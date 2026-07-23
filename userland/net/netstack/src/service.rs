//! The request dispatcher: the one place a `netstack-v1` request is
//! decoded, capability-checked, audited, and answered.

use tairix_abi::net_ipc::{encode_page_reply, NetstackRequest, IF_NAME_LEN};
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::{CapabilityId, CapabilityQuery, Duration64, Errno, Origin};
use tairix_log::{log, Event, EventId, Field, Level, Sink};

use crate::events;
use crate::iface::Netstack;

/// The authenticated principal on whose behalf a request is served.
///
/// The identity is the kernel-attested [`Origin`] of the requesting
/// task — obtained from the IPC layer (`call_peer_origin`), never from
/// the caller's own payload — so `netstack` trusts the kernel's view,
/// not bytes on the wire.
pub struct Caller {
    origin: Origin,
}

impl Caller {
    /// Wrap a kernel-attested [`Origin`] as the serving principal.
    #[must_use]
    pub fn new(origin: Origin) -> Self {
        Self { origin }
    }

    /// The caller's kernel-attested origin.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The caller's effective capability set, as the object-safe
    /// [`CapabilityQuery`] seam the dispatcher gates on.
    #[must_use]
    pub fn capabilities(&self) -> &dyn CapabilityQuery {
        self.origin.capabilities()
    }
}

/// Serve one `netstack-v1` request.
///
/// Decodes the fixed-width [`NetstackRequest`] from `request`, enforces
/// the operation's required capability against `caller` **before any
/// state is touched**, applies it against `stack`, emits the audit
/// record, and writes the encoded reply into `response`, returning the
/// number of bytes written.
///
/// The pipeline fails closed: a malformed frame, a missing capability,
/// or a refused mutation each return a typed [`Errno`] and leave
/// `response` unspecified — the transport loop frames the error as a
/// status reply so the client sees the exact refusal.
///
/// # Capabilities
///
/// * `InterfaceList` / `AddrAdd` / `RouteAdd` / `Counters` —
///   [`CapabilityId::NET_ADMIN`].
/// * `InterfaceFacts` / `InterfaceState` —
///   [`CapabilityId::SYSINFO_INTROSPECT`] (the sysinfo broker;
///   per-client narrowing lives in that audited broker, exactly as it
///   does above the kernel's introspection primitive).
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] / [`Errno::BadMagic`] /
///   [`Errno::AbiVersionUnsupported`] / [`Errno::OutOfRange`] /
///   [`Errno::LengthOutOfRange`] — the frame failed to decode.
/// * [`Errno::PermissionDenied`] — the caller lacks the operation's
///   required capability.
/// * [`Errno::NotFound`] — a mutation named an unmanaged interface.
pub fn serve(
    stack: &mut Netstack,
    caller: &Caller,
    audit: &dyn Sink,
    request: &[u8],
    response: &mut [u8],
    now: Duration64,
) -> Result<usize, Errno> {
    let decoded = match NetstackRequest::from_bytes(request) {
        Ok(decoded) => decoded,
        Err(err) => {
            emit(
                audit,
                Level::Warn,
                events::REQUEST_MALFORMED,
                "netstack request rejected: frame decode failed",
                &[],
            );
            return Err(err);
        }
    };

    let required = match decoded {
        NetstackRequest::InterfaceFacts { .. }
        | NetstackRequest::InterfaceState { .. }
        | NetstackRequest::InterfaceCounters { .. }
        | NetstackRequest::InterfaceRates { .. } => CapabilityId::SYSINFO_INTROSPECT,
        _ => CapabilityId::NET_ADMIN,
    };
    if !caller.capabilities().holds(required) {
        emit(
            audit,
            Level::Warn,
            events::REQUEST_DENIED,
            "netstack request denied: caller lacks required capability",
            &[op_field(&decoded)],
        );
        return Err(Errno::PermissionDenied);
    }

    match decoded {
        NetstackRequest::InterfaceList => {
            let names = stack.names();
            encode_page_reply(&names, response)
        }
        NetstackRequest::AddrAdd {
            iface,
            family,
            prefix,
            addr,
        } => {
            let applied = stack.addr_add(iface, family, prefix, addr, now);
            audit_mutation(audit, "addr add", iface, applied)?;
            write_status(response)
        }
        NetstackRequest::RouteAdd {
            iface,
            family,
            prefix,
            dest,
            next_hop,
        } => {
            let applied = stack.route_add(iface, family, prefix, dest, next_hop);
            audit_mutation(audit, "route add", iface, applied)?;
            write_status(response)
        }
        NetstackRequest::InterfaceCounters { offset, limit } => {
            let records: alloc::vec::Vec<_> = stack
                .counters_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetInterfaceCountersRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, response)
        }
        NetstackRequest::InterfaceFacts { offset, limit } => {
            let records: alloc::vec::Vec<_> = stack
                .facts_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetInterfaceFactsRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, response)
        }
        NetstackRequest::InterfaceState { offset, limit } => {
            let records: alloc::vec::Vec<_> = stack
                .state_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetInterfaceStateRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, response)
        }
        NetstackRequest::InterfaceRates {
            offset,
            limit,
            window,
        } => {
            let records: alloc::vec::Vec<_> = stack
                .rates_records(offset, limit, window, now)
                .iter()
                .map(tairix_abi::net_ipc::NetInterfaceRatesRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, response)
        }
        NetstackRequest::BindDriver { .. } => {
            // Binding a NIC driver's device channel creates and grants a
            // shared-memory region and issues IPC to the driver — I/O the
            // pure engine dispatcher cannot perform. The freestanding
            // transport loop intercepts `BindDriver` and carries it out over
            // its live syscall seams before ever reaching this dispatcher;
            // arriving here means it was routed to a path that cannot
            // service it, so refuse fail-closed rather than pretend.
            Err(Errno::NotSupported)
        }
    }
}

/// Record an admin mutation's outcome, propagating its refusal.
fn audit_mutation(
    audit: &dyn Sink,
    what: &'static str,
    iface: [u8; IF_NAME_LEN],
    applied: Result<(), Errno>,
) -> Result<(), Errno> {
    let name_len = iface.iter().position(|&b| b == 0).unwrap_or(IF_NAME_LEN);
    let name = core::str::from_utf8(&iface[..name_len]).unwrap_or("?");
    match applied {
        Ok(()) => {
            emit(
                audit,
                Level::Info,
                events::ADMIN_APPLIED,
                what,
                &[iface_field(name)],
            );
            Ok(())
        }
        Err(err) => {
            emit(
                audit,
                Level::Warn,
                events::ADMIN_REFUSED,
                what,
                &[iface_field(name)],
            );
            Err(err)
        }
    }
}

/// The interface alias an audit record carries.
fn iface_field(name: &str) -> Field<'_> {
    Field {
        key: "iface",
        value: tairix_log::FieldValue::Str(name),
    }
}

/// Write the success status frame.
fn write_status(response: &mut [u8]) -> Result<usize, Errno> {
    if response.len() < STATUS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    response[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
    Ok(STATUS_REPLY_LEN)
}

/// The operation name an audit record carries.
fn op_field(request: &NetstackRequest) -> Field<'static> {
    let op = match request {
        NetstackRequest::InterfaceList => "interface list",
        NetstackRequest::AddrAdd { .. } => "addr add",
        NetstackRequest::RouteAdd { .. } => "route add",
        NetstackRequest::InterfaceCounters { .. } => "interface counters",
        NetstackRequest::InterfaceFacts { .. } => "interface facts",
        NetstackRequest::InterfaceState { .. } => "interface state",
        NetstackRequest::InterfaceRates { .. } => "interface rates",
        NetstackRequest::BindDriver { .. } => "bind driver",
    };
    Field {
        key: "op",
        value: tairix_log::FieldValue::Str(op),
    }
}

/// Emit one structured audit record.
fn emit(audit: &dyn Sink, level: Level, id: EventId, message: &str, fields: &[Field<'_>]) {
    log(
        audit,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}
