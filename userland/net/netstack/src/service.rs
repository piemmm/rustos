//! The request dispatcher: the one place a `netstack-v1` request is
//! decoded, capability-checked, audited, and answered.

use tairix_abi::net_ipc::{NetstackRequest, IF_NAME_LEN, NETSTACK_LIST_LIMIT_MAX};
use tairix_abi::reply::{encode_page_reply, encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::{CapabilityId, CapabilityQuery, Duration64, Errno, Origin};
use tairix_log::{log, Event, EventId, Field, Level, Sink};

use crate::events;
use crate::iface::Netstack;
use crate::socket::SocketService;

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
#[allow(clippy::too_many_arguments)]
pub fn serve(
    stack: &mut Netstack,
    sockets: &SocketService,
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
        | NetstackRequest::InterfaceRates { .. }
        | NetstackRequest::Sockets { .. }
        | NetstackRequest::BondMembers { .. }
        | NetstackRequest::ResolverServers
        | NetstackRequest::TimeServers
        | NetstackRequest::StackDefence => CapabilityId::SYSINFO_INTROSPECT,
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
            encode_page_reply(&names, NETSTACK_LIST_LIMIT_MAX, response)
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
        NetstackRequest::InterfaceFacts { .. }
        | NetstackRequest::InterfaceState { .. }
        | NetstackRequest::InterfaceCounters { .. }
        | NetstackRequest::InterfaceRates { .. }
        | NetstackRequest::Sockets { .. }
        | NetstackRequest::BondMembers { .. }
        | NetstackRequest::ResolverServers
        | NetstackRequest::TimeServers
        | NetstackRequest::StackDefence => serve_read(stack, sockets, decoded, response, now),
        NetstackRequest::ApplyNetworkSettings(settings) => {
            // Pure state mutation (no I/O), so unlike `BindDriver` it is
            // served here: store the policy and re-apply the family
            // switches to every managed interface. The `CAP_NET_ADMIN`
            // gate above already ran; record the audited outcome.
            stack.apply_settings(settings, now);
            emit(
                audit,
                Level::Info,
                events::NETWORK_SETTINGS_APPLIED,
                "network settings applied",
                &[],
            );
            write_status(response)
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

/// Serve one paged broker *read* (interface facts/state/counters/rates or
/// the socket listing) into `response`.
///
/// Split out of [`serve`] so the dispatcher stays small: every arm here
/// encodes its records with the one [`encode_page_reply`] page codec, and
/// the caller has already gated the read on `CAP_SYSINFO_INTROSPECT`. A
/// non-read request never reaches this function.
fn serve_read(
    stack: &mut Netstack,
    sockets: &SocketService,
    request: NetstackRequest,
    response: &mut [u8],
    now: Duration64,
) -> Result<usize, Errno> {
    match request {
        NetstackRequest::InterfaceFacts { offset, limit } => {
            let records: alloc::vec::Vec<_> = stack
                .facts_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetInterfaceFactsRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
        }
        NetstackRequest::InterfaceState { offset, limit } => {
            let records: alloc::vec::Vec<_> = stack
                .state_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetInterfaceStateRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
        }
        NetstackRequest::InterfaceCounters { offset, limit } => {
            let records: alloc::vec::Vec<_> = stack
                .counters_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetInterfaceCountersRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
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
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
        }
        NetstackRequest::Sockets { offset, limit } => {
            let records: alloc::vec::Vec<_> = sockets
                .socket_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetSocketRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
        }
        NetstackRequest::BondMembers { offset, limit } => {
            let records: alloc::vec::Vec<_> = stack
                .bond_member_records(offset, limit)
                .iter()
                .map(tairix_abi::net_ipc::NetBondMemberRecord::to_le_bytes)
                .collect();
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
        }
        NetstackRequest::ResolverServers => {
            // The active resolver set is small and closed, so it is served
            // whole in one page (never larger than `MAX_RESOLVER_SERVERS`);
            // the shared page codec carries it like every other broker read.
            let records: alloc::vec::Vec<_> = stack
                .resolver_servers()
                .iter()
                .map(tairix_abi::net_ipc::NetServerAddr::to_le_bytes)
                .collect();
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
        }
        NetstackRequest::TimeServers => {
            let records: alloc::vec::Vec<_> = stack
                .time_servers()
                .iter()
                .map(tairix_abi::net_ipc::NetServerAddr::to_le_bytes)
                .collect();
            encode_page_reply(&records, NETSTACK_LIST_LIMIT_MAX, response)
        }
        NetstackRequest::StackDefence => {
            // One record, not a page: the defence totals belong to the
            // socket table as a whole and name no interface.
            let payload = sockets.defence_counters().to_le_bytes();
            if response.len() < payload.len() {
                return Err(Errno::BufferTooSmall);
            }
            response[..payload.len()].copy_from_slice(&payload);
            Ok(payload.len())
        }
        // The dispatcher only routes the paged read ops here.
        _ => Err(Errno::NotSupported),
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
        NetstackRequest::Sockets { .. } => "sockets",
        NetstackRequest::BondMembers { .. } => "bond members",
        NetstackRequest::ApplyNetworkSettings(_) => "apply network settings",
        NetstackRequest::BindDriver { .. } => "bind driver",
        NetstackRequest::ResolverServers => "resolver servers",
        NetstackRequest::TimeServers => "time servers",
        NetstackRequest::StackDefence => "stack defence",
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
