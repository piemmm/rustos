//! The production socket-backed DNS transport and the convenience
//! [`resolve`] entry point.
//!
//! [`RtDnsTransport`] implements the pure engine's
//! [`DnsTransport`](tairix_net::dns::DnsTransport) over the `netsock-v1` UDP
//! datagram socket (`tairix_rt::net`): it binds an app-local delivery port,
//! opens the datagram socket for a server's address family on demand with a
//! CSPRNG-drawn ephemeral source port (the RFC 5452 source-port randomisation
//! the socket layer contributes), sends each encoded query, and parks on the
//! delivery port for the reply — never a busy spin. Every received datagram is
//! checked against the network stack's kernel-attested [`Origin`]; a datagram
//! from any other sender is dropped (the delivery port is otherwise an
//! unauthenticated inbox — fail closed).
//!
//! [`host_address`] is the entry point a connecting tool uses for its target
//! operand: it answers an address literal without opening a socket, so a
//! literal target works with no resolver configured.
//!
//! This module is compiled only for a freestanding userland program that
//! opts into the `program` feature; the pure orchestration in the crate root
//! and its host tests never pull the runtime.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::net::{SocketAddr, SocketDatagram, SocketId};
use tairix_abi::net_ipc::NetAddrFamily;
use tairix_abi::time::Duration64;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{Errno, Origin, RandomFlags};
use tairix_net::addr::IpAddr;
use tairix_net::dns::{DnsTransport, RecordType, Resolution, Wait, PORT};
use tairix_procinfo::IpcTransport;

use crate::{resolve_name, ResolveError};

/// The client's delivery-port endpoint id — an app-local, unrestricted
/// well-known value (not a reserved kernel id), so binding it needs no
/// capability. The stack posts this socket's inbound datagrams here.
/// (`0x_646e_7371` spells "dnsq".)
const DELIVER_PORT: u64 = 0x_646e_7371;

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

/// A socket-backed [`DnsTransport`]: the monotonic clock, an on-demand UDP
/// datagram socket per address family, and the delivery-port park.
pub struct RtDnsTransport {
    /// The wait-set the delivery port is registered with; `wait` parks on it.
    set: u64,
    /// The IPv4 datagram socket, opened on the first query to a v4 server.
    v4: Option<SocketId>,
    /// The IPv6 datagram socket, opened on the first query to a v6 server.
    v6: Option<SocketId>,
    /// The stack's kernel-attested origin, captured from the first received
    /// datagram so every later one can be required to match it (fail closed).
    stack: Option<Origin>,
    /// The receive scratch buffer (reused across datagrams), sized to the
    /// largest datagram frame the stack can deliver.
    scratch: Vec<u8>,
}

impl RtDnsTransport {
    /// Bind the delivery port and register it with a fresh wait-set.
    ///
    /// The datagram sockets themselves are opened lazily on the first query
    /// to each family, so a lookup that only ever talks to one family opens
    /// only that socket.
    ///
    /// # Errors
    ///
    /// [`Errno`] if the delivery port cannot be bound or the wait-set cannot
    /// be created or armed.
    pub fn open() -> Result<Self, Errno> {
        if tairix_rt::port_bind(DELIVER_PORT, SocketDatagram::MAX_WIRE_LEN, DELIVER_CAPACITY) < 0 {
            return Err(Errno::AddressInUse);
        }
        let set = tairix_rt::waitset_create();
        let Ok(set) = u64::try_from(set) else {
            return Err(Errno::from_syscall(set));
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            DELIVER_PORT,
            DELIVER_TOKEN,
        ) != 0
        {
            return Err(Errno::NotImplemented);
        }
        Ok(Self {
            set,
            v4: None,
            v6: None,
            stack: None,
            scratch: vec![0u8; SocketDatagram::MAX_WIRE_LEN],
        })
    }

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
        let socket = tairix_rt::net::socket(family, DELIVER_PORT)?;
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

    /// Resolve `name`/`record_type` over this transport, reusing its bound
    /// delivery port and open sockets across calls.
    ///
    /// A resolving tool that looks a name up under several record types (the
    /// `host` A+AAAA default) drives one transport through this method for
    /// each type, so the delivery port is bound once and the per-family
    /// datagram sockets are opened once and shared — never rebinding the port
    /// per query. The server set and the CSPRNG come from the production
    /// seams: the real System Information API transport and the kernel
    /// random subsystem.
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
        record_type: RecordType,
    ) -> Result<Resolution, ResolveError> {
        let mut rng = || {
            let mut bytes = [0u8; 4];
            let _ = tairix_rt::random_get(&mut bytes, RandomFlags::empty());
            u32::from_le_bytes(bytes)
        };
        resolve_name(name, record_type, &IpcTransport, self, &mut rng)
    }

    /// Resolve a command-line host operand to one address over this
    /// transport, reusing its bound delivery port and open sockets.
    ///
    /// The literal-first, family-preference policy itself is the shared
    /// [`crate::resolve_host`]; this only supplies the query.
    pub fn host_address(&mut self, host: &str, family: Option<NetAddrFamily>) -> Option<IpAddr> {
        let mut query = |name: &str, record: RecordType| self.resolve(name, record).ok();
        crate::resolve_host(host, family, &mut query)
    }
}

impl Drop for RtDnsTransport {
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

impl DnsTransport for RtDnsTransport {
    fn now(&mut self) -> Duration64 {
        Duration64::from_nanos(tairix_rt::clock_get())
    }

    fn send(&mut self, server: IpAddr, query: &[u8]) -> Result<(), Errno> {
        let dest = server_socket_addr(server);
        let socket = self.socket_for(dest.family)?;
        tairix_rt::net::send(socket, Some(dest), query)
    }

    fn wait(&mut self, deadline: Duration64, buf: &mut [u8]) -> Result<Wait, Errno> {
        let deadline_ns = deadline_nanos(deadline);
        loop {
            let now = tairix_rt::clock_get();
            if now >= deadline_ns {
                return Ok(Wait::TimedOut);
            }
            match tairix_rt::net::recv(DELIVER_PORT, &mut self.scratch) {
                Ok((datagram, origin)) => {
                    // Authenticate the sender: capture the stack's origin on
                    // the first datagram, then require every later one to
                    // match it. A datagram from any other origin is dropped
                    // (fail closed) — the engine would reject a mismatched
                    // reply anyway, but a forged sender never even reaches it.
                    match self.stack {
                        Some(known) if known != origin => continue,
                        None => self.stack = Some(origin),
                        _ => {}
                    }
                    let len = datagram.payload.len().min(buf.len());
                    buf[..len].copy_from_slice(&datagram.payload[..len]);
                    return Ok(Wait::Datagram(len));
                }
                // The mailbox is momentarily empty: park until the stack posts
                // a datagram or the remaining budget elapses, then re-check.
                Err(Errno::WouldBlock) => self.park(deadline_ns - now),
                Err(other) => return Err(other),
            }
        }
    }
}

/// Resolve `name`/`record_type` over the production seams: the real
/// System Information API transport for the configured server set, a
/// freshly opened [`RtDnsTransport`] for the UDP queries, and the kernel
/// CSPRNG for the query id and retransmit jitter.
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
pub fn resolve(name: &str, record_type: RecordType) -> Result<Resolution, ResolveError> {
    let mut udp = RtDnsTransport::open().map_err(ResolveError::Transport)?;
    udp.resolve(name, record_type)
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
    let (family, addr) = crate::address_parts(server);
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
