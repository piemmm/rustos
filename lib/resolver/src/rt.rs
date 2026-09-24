//! The production socket-backed DNS transport and the convenience
//! [`resolve`] entry point.
//!
//! [`RtDnsTransport`] drives the pure engine's
//! [`DnsTransport`](tairix_net::dns::DnsTransport) over the `netsock-v1` UDP
//! datagram socket (`tairix_rt::net`): it draws query ids from a generator
//! keyed once from the kernel CSPRNG, binds a process-private delivery
//! port ([`tairix_rt::bind_private_port`] — the kernel's port registry is
//! machine-wide, so a fixed id would let one long-lived client deny
//! resolution to every other process for the boot),
//! opens the datagram socket for a server's address family on demand with a
//! CSPRNG-drawn ephemeral source port (the RFC 5452 source-port randomisation
//! the socket layer contributes), sends each encoded query, and parks on the
//! delivery port for the reply — never a busy spin. Only the network stack's
//! own deliveries reach the engine: the socket receive authenticates the
//! sender's kernel-attested service account and discards any other post.
//!
//! [`host_address`] is the entry point a connecting tool uses for its target
//! operand: it answers an address literal without opening a socket, so a
//! literal target works with no resolver configured. [`reverse_name`] is its
//! mirror: the display name an address maps back to, for a tool that shows
//! peers by name unless asked to stay numeric.
//!
//! This module is compiled only for a freestanding userland program that
//! opts into the `program` feature; the pure orchestration in the crate root
//! and its host tests never pull the runtime.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::discovery_ipc::Families;
use tairix_abi::net::{SocketAddr, SocketDatagram, SocketDelivery, SocketId};
use tairix_abi::net_ipc::NetAddrFamily;
use tairix_abi::time::Duration64;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::Errno;
use tairix_discovery::DiscoveryError;
use tairix_net::addr::IpAddr;
use tairix_net::dns::{
    AddrList, Answer, DnsTransport, LookupType, Name, Resolution, ResolveStatus, Wait, PORT,
};
use tairix_procinfo::IpcTransport;
use tairix_rng::{FastRng, RandU64};

use crate::{nothing, pointer_name, resolve_name, resolve_pointer, LinkLookup, ResolveError};

/// How long a lookup the link answers waits for its first answer: the
/// question's first transmission and the one a second later (RFC 6762 §5.2),
/// with time for a responder's answer to each.
const LINK_WINDOW_NS: u64 = 3 * ONE_SEC_NANOS;

/// The lookups the link answers, through `discoveryd`.
///
/// A machine with no discovery service answers every one of them with
/// nothing: a `.local` name is still never sent to a server.
pub struct RtLinkLookup;

impl LinkLookup for RtLinkLookup {
    fn host(&mut self, name: &Name, record_type: LookupType) -> Result<Resolution, ResolveError> {
        let families = match record_type {
            LookupType::A => Families {
                v4: true,
                v6: false,
            },
            LookupType::Aaaa => Families {
                v4: false,
                v6: true,
            },
            LookupType::Ptr => return Ok(nothing(record_type)),
        };
        let until = tairix_rt::clock_get().saturating_add(LINK_WINDOW_NS);
        match tairix_discovery::host_addresses(name.as_wire(), families, until) {
            Ok(addresses) if !addresses.is_empty() => Ok(Resolution {
                status: ResolveStatus::Success,
                answer: Answer::Addresses(AddrList::from_addrs(&addresses)),
                ttl_secs: 0,
            }),
            Ok(_) | Err(DiscoveryError::Unavailable) => Ok(nothing(record_type)),
            Err(error) => Err(link_error(error)),
        }
    }

    fn pointer(&mut self, address: IpAddr) -> Result<Resolution, ResolveError> {
        let until = tairix_rt::clock_get().saturating_add(LINK_WINDOW_NS);
        match tairix_discovery::reverse_name(address, until) {
            Ok(Some(target)) => Ok(Name::from_wire(&target).map_or_else(
                || nothing(LookupType::Ptr),
                |name| Resolution {
                    status: ResolveStatus::Success,
                    answer: Answer::Pointer(Some(name)),
                    ttl_secs: 0,
                },
            )),
            Ok(None) | Err(DiscoveryError::Unavailable) => Ok(nothing(LookupType::Ptr)),
            Err(error) => Err(link_error(error)),
        }
    }
}

/// A link lookup's failure, as the resolver reports one.
fn link_error(error: DiscoveryError) -> ResolveError {
    match error {
        DiscoveryError::Refused(errno) | DiscoveryError::Transport(errno) => {
            ResolveError::Transport(errno)
        }
        DiscoveryError::Unavailable | DiscoveryError::Malformed => {
            ResolveError::Transport(Errno::BadMagic)
        }
    }
}

/// Delivery-port mailbox depth. A resolution has one query outstanding at a
/// time, but retransmission and failover can leave a couple of late replies
/// in flight; this headroom lets them queue rather than back-pressure the
/// stack.
const DELIVER_CAPACITY: usize = 8;

/// Wait-set token for the delivery port (one source, so any non-zero token
/// identifies it).
const DELIVER_TOKEN: u64 = 1;

/// One second in nanoseconds — the widening used to turn a monotonic
/// [`Duration64`] deadline into the `u64` nanosecond count the wait-set and
/// clock syscalls speak.
const ONE_SEC_NANOS: u64 = 1_000_000_000;

/// A socket-backed resolver: the [`DnsTransport`] the engine drives, and the
/// generator its query ids and retransmit jitter are drawn from.
pub struct RtDnsTransport {
    sockets: Sockets,
    /// Keyed from the kernel CSPRNG when the transport opened, so no query
    /// id is ever drawn from a source that could not be keyed.
    rng: FastRng,
}

/// The monotonic clock, an on-demand UDP datagram socket per address family,
/// and the delivery-port park.
struct Sockets {
    /// This transport's process-private delivery port
    /// ([`tairix_rt::bind_private_port`]).
    deliver: u64,
    /// The wait-set the delivery port is registered with; `wait` parks on it.
    set: u64,
    /// The IPv4 datagram socket, opened on the first query to a v4 server.
    v4: Option<SocketId>,
    /// The IPv6 datagram socket, opened on the first query to a v6 server.
    v6: Option<SocketId>,
    /// The receive scratch buffer (reused across datagrams), sized to the
    /// largest datagram frame the stack can deliver.
    scratch: Vec<u8>,
}

impl RtDnsTransport {
    /// Key the query-id generator, bind the delivery port, and register it
    /// with a fresh wait-set.
    ///
    /// The datagram sockets themselves are opened lazily on the first query
    /// to each family, so a lookup that only ever talks to one family opens
    /// only that socket.
    ///
    /// # Errors
    ///
    /// [`Errno`] if the kernel CSPRNG cannot key the generator, the delivery
    /// port cannot be bound, or the wait-set cannot be created or armed.
    pub fn open() -> Result<Self, Errno> {
        let rng = FastRng::keyed_by(tairix_rt::random_fill)?;
        let deliver = tairix_rt::bind_private_port(SocketDatagram::MAX_WIRE_LEN, DELIVER_CAPACITY)?;
        let set = tairix_rt::waitset_create();
        let Ok(set) = u64::try_from(set) else {
            return Err(Errno::from_syscall(set));
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            deliver,
            DELIVER_TOKEN,
        ) != 0
        {
            return Err(Errno::NotImplemented);
        }
        Ok(Self {
            sockets: Sockets {
                deliver,
                set,
                v4: None,
                v6: None,
                scratch: tairix_util::fallible::filled(SocketDatagram::MAX_WIRE_LEN, 0u8)
                    .ok_or(Errno::OutOfMemory)?,
            },
            rng,
        })
    }

    /// Resolve `name`/`record_type` over this transport, reusing its bound
    /// delivery port and open sockets across calls.
    ///
    /// A resolving tool that looks a name up under several record types (the
    /// `host` A+AAAA default) drives one transport through this method for
    /// each type, so the delivery port is bound once and the per-family
    /// datagram sockets are opened once and shared — never rebinding the port
    /// per query. The server set comes from the real System Information API
    /// transport.
    ///
    /// # Errors
    ///
    /// A [`ResolveError`] describing why resolution could not proceed (an
    /// invalid name, no configured server, a failed server-set query, or a
    /// UDP transport failure). A negative or timed-out resolution is returned
    /// as a [`Resolution`], not an error.
    pub fn resolve(
        &mut self,
        name: &str,
        record_type: LookupType,
    ) -> Result<Resolution, ResolveError> {
        let Self { sockets, rng } = self;
        resolve_name(
            name,
            record_type,
            &IpcTransport,
            sockets,
            &mut RtLinkLookup,
            &mut || rng.next_u32(),
        )
    }

    /// Resolve the domain name `address` maps back to over this transport,
    /// reusing its bound delivery port and open sockets.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve).
    pub fn resolve_reverse(&mut self, address: IpAddr) -> Result<Resolution, ResolveError> {
        let Self { sockets, rng } = self;
        resolve_pointer(
            address,
            &IpcTransport,
            sockets,
            &mut RtLinkLookup,
            &mut || rng.next_u32(),
        )
    }

    /// The display name `address` maps back to, or [`None`] when it has no
    /// `PTR` record or the lookup did not conclude — the shape a tool that
    /// falls back to the numeric address wants.
    ///
    /// Reusing one transport across many addresses is what lets a table
    /// renderer resolve a whole listing without rebinding a port per row.
    pub fn reverse_name(&mut self, address: IpAddr) -> Option<String> {
        pointer_name(&self.resolve_reverse(address).ok()?)
    }

    /// Resolve a command-line host operand to one address over this
    /// transport, reusing its bound delivery port and open sockets.
    ///
    /// The literal-first, family-preference policy itself is the shared
    /// [`crate::resolve_host`]; this only supplies the query.
    pub fn host_address(&mut self, host: &str, family: Option<NetAddrFamily>) -> Option<IpAddr> {
        let mut query = |name: &str, record: LookupType| self.resolve(name, record).ok();
        crate::resolve_host(host, family, &mut query)
    }
}

impl Sockets {
    /// The datagram socket for `family`, opened (and bound to a CSPRNG-drawn
    /// ephemeral source port) on first use and cached thereafter.
    fn socket_for(&mut self, family: NetAddrFamily) -> Result<SocketId, Errno> {
        let cached = match family {
            NetAddrFamily::V4 => &mut self.v4,
            NetAddrFamily::V6 => &mut self.v6,
        };
        if let Some(socket) = *cached {
            return Ok(socket);
        }
        let socket = tairix_rt::net::socket(family, self.deliver)?;
        // A local port of 0 asks the stack for a CSPRNG-drawn ephemeral port
        // — the RFC 5452 source-port randomisation that widens an off-path
        // spoofer's search space beyond the query id alone.
        let local = SocketAddr {
            family,
            addr: [0u8; 16],
            port: 0,
        };
        tairix_rt::net::bind(socket, local)?;
        *cached = Some(socket);
        Ok(socket)
    }

    /// Park on the delivery port for up to `nanos`, giving the CPU up until
    /// the stack posts a datagram or the one-shot timer elapses.
    fn park(&self, nanos: u64) {
        let mut token = 0u64;
        let _ = tairix_rt::waitset_wait(self.set, nanos, &mut token);
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        // Best-effort teardown: release the datagram sockets so their handles
        // and ephemeral ports do not linger past the resolution.
        if let Some(socket) = self.v4.take() {
            let _ = tairix_rt::net::close(socket);
        }
        if let Some(socket) = self.v6.take() {
            let _ = tairix_rt::net::close(socket);
        }
    }
}

impl DnsTransport for Sockets {
    fn now(&mut self) -> Duration64 {
        Duration64::from_nanos(tairix_rt::clock_get())
    }

    fn send(&mut self, server: IpAddr, query: &[u8]) -> Result<(), Errno> {
        let dest = server_socket_addr(server);
        let socket = self.socket_for(dest.family)?;
        tairix_rt::net::send(socket, Some(dest), None, query)
    }

    fn wait(&mut self, deadline: Duration64, buf: &mut [u8]) -> Result<Wait, Errno> {
        let deadline_ns = deadline_nanos(deadline);
        loop {
            let now = tairix_rt::clock_get();
            if now >= deadline_ns {
                return Ok(Wait::TimedOut);
            }
            // Only the stack's own deliveries come back: the receive
            // discards a forged sender before the engine could see it.
            match tairix_rt::net::recv(self.deliver, &mut self.scratch) {
                Ok(SocketDelivery::Datagram(datagram)) => {
                    let len = datagram.payload.len().min(buf.len());
                    buf[..len].copy_from_slice(&datagram.payload[..len]);
                    return Ok(Wait::Datagram(len));
                }
                // A resolver's sockets join no group, so no link edge is
                // ever delivered; one that were would answer nothing.
                Ok(SocketDelivery::Link(_)) => {}
                // The mailbox is momentarily empty: park until the stack posts
                // a datagram or the remaining budget elapses, then re-check.
                Err(Errno::WouldBlock) => self.park(deadline_ns - now),
                Err(other) => return Err(other),
            }
        }
    }
}

/// Resolve `name`/`record_type` over the production seams: the real
/// System Information API transport for the configured server set and a
/// freshly opened [`RtDnsTransport`] for the UDP queries and their ids.
///
/// This is the one call a resolving program makes; the pure
/// [`resolve_name`] orchestration it delegates to is what the host tests
/// exercise, so there is no second driver.
///
/// # Errors
///
/// A [`ResolveError`] describing why resolution could not proceed (an
/// invalid name, no configured server, a failed server-set query, or a UDP
/// transport failure). A negative or timed-out resolution is returned as a
/// [`Resolution`], not an error.
pub fn resolve(name: &str, record_type: LookupType) -> Result<Resolution, ResolveError> {
    let mut udp = RtDnsTransport::open().map_err(ResolveError::Transport)?;
    udp.resolve(name, record_type)
}

/// The display name `address` maps back to over the production seams, or
/// [`None`] when it has no `PTR` record or the lookup did not conclude.
///
/// The one call a tool makes for a *single* reverse lookup; a tool resolving
/// many addresses opens one [`RtDnsTransport`] and calls its
/// [`reverse_name`](RtDnsTransport::reverse_name) instead, so the delivery
/// port is bound once.
#[must_use]
pub fn reverse_name(address: IpAddr) -> Option<String> {
    RtDnsTransport::open().ok()?.reverse_name(address)
}

/// Resolve a command-line host operand to one address over the production
/// seams — the one call a connecting tool makes for its target operand.
///
/// An address literal is answered without opening a socket at all, so a
/// literal target keeps working on a machine with no resolver configured.
#[must_use]
pub fn host_address(host: &str, family: Option<NetAddrFamily>) -> Option<IpAddr> {
    if let Some(address) = crate::literal_address(host, family) {
        return Some(address);
    }
    RtDnsTransport::open().ok()?.host_address(host, family)
}

/// Turn a resolver [`IpAddr`] into the `netsock-v1` destination
/// [`SocketAddr`] on DNS [`PORT`] (53).
fn server_socket_addr(server: IpAddr) -> SocketAddr {
    let (family, addr) = tairix_abi::net_ipc::address_parts(server);
    SocketAddr {
        family,
        addr,
        port: PORT,
    }
}

/// Widen a non-negative monotonic [`Duration64`] deadline to the `u64`
/// nanosecond count the clock and wait-set syscalls use, saturating rather
/// than wrapping at the extremes (a negative or overflowing value the
/// monotonic clock never produces clamps to a safe bound).
fn deadline_nanos(deadline: Duration64) -> u64 {
    let secs = u64::try_from(deadline.secs()).unwrap_or(0);
    secs.saturating_mul(ONE_SEC_NANOS)
        .saturating_add(u64::from(deadline.subsec_nanos()))
}
