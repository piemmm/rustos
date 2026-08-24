//! TAIRiX userland DNS stub-resolver client (`plans/DNS.md` DNS2).
//!
//! This crate is the small seam that turns the pure, host-tested DNS engine in
//! [`tairix_net::dns`] into a working name lookup for a userland program. It
//! owns **no** DNS logic of its own: the RFC 1035 wire codec, the RFC
//! 5452-hardened response validation, and the retransmit/failover state machine
//! all live in `tairix-net` (the one-definition rule), and the active
//! recursive-server set comes from the one System Information API query
//! (`NET_RESOLVER_SERVERS`) that the `state:net/resolver/servers` read also
//! uses, so a resolver client and an operator inspecting the config can never
//! disagree.
//!
//! # What this crate is
//!
//! * [`resolve_name`] — the pure, host-testable orchestration: fetch the
//!   configured recursive servers through a [`tairix_procinfo::Transport`],
//!   then drive [`tairix_net::dns::resolve`] over a caller-supplied
//!   [`tairix_net::dns::DnsTransport`] and CSPRNG. Both seams are injected,
//!   so the whole path is exercised against in-memory fakes with no kernel.
//! * `RtDnsTransport` and `resolve` (the `program` feature; documented on a
//!   freestanding target) — the production glue: a [`DnsTransport`] over
//!   the `netsock-v1` UDP datagram socket (`tairix_rt::net`), with RFC 5452
//!   source-port randomisation from the port-0 bind and a kernel-attested
//!   stack-origin check on every received datagram (fail closed — the
//!   delivery port is otherwise an unauthenticated inbox), plus a
//!   convenience entry point that wires it to the real sysinfo transport and
//!   the kernel CSPRNG.
//!
//! # Security
//!
//! The resolver adds no authority: opening the UDP socket is capability-gated
//! stack-side ([`CAP_NET`](tairix_abi::CapabilityId::NET)), the server-set
//! query is ungated public host configuration, and every response is validated
//! by the pure engine before an address is surfaced. A DNS server and every
//! packet on the wire are treated as hostile: off-path spoofing is bounded by
//! the engine's random query id and strict question match, and by this crate's
//! source-port randomisation and origin check.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); as a `lib/*` crate it depends only on other `lib/*`
//! crates. No `unsafe`, and no `unwrap`/`expect`/`panic!` on a production path.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use tairix_abi::net_ipc::{NetAddrFamily, NetResolverServer, MAX_RESOLVER_SERVERS};
use tairix_abi::Errno;
use tairix_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::dns::{self, DnsError, DnsTransport, Name, RecordType, Resolution};
use tairix_procinfo::{for_each_resolver_server, CallError, ListError, Transport, WalkStep};

#[cfg(all(feature = "program", target_os = "none"))]
mod rt;
#[cfg(all(feature = "program", target_os = "none"))]
pub use rt::{resolve, RtDnsTransport};

/// Why a name resolution did not produce an answer.
///
/// Each variant is a distinct, actionable cause a consuming tool renders
/// precisely; there is no catch-all that hides a wire-level [`Errno`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveError {
    /// The requested name is not a syntactically valid DNS name (a label or
    /// the whole name exceeds the RFC 1035 length limits, or a label holds
    /// an illegal octet). Carries the engine's precise reason.
    InvalidName(DnsError),
    /// No recursive DNS server is configured for the host, so there is
    /// nothing to query. Distinct from a query that timed out: the fix is to
    /// configure a server (DHCP-learned or a `<iface>.dns.servers` key), not
    /// to retry.
    NoServers,
    /// Fetching the configured server set from the System Information API
    /// failed. Carries the underlying [`Errno`] (for example
    /// [`Errno::PermissionDenied`], though the query is normally ungated).
    ServerSource(Errno),
    /// The UDP transport failed part-way through the resolution (for example
    /// the network became unreachable). Carries the underlying [`Errno`];
    /// the resolution is abandoned fail-closed rather than reported as a
    /// spurious answer.
    Transport(Errno),
}

impl core::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidName(_) => f.write_str("not a valid domain name"),
            Self::NoServers => f.write_str("no DNS server is configured"),
            Self::ServerSource(errno) => {
                write!(f, "cannot read the configured DNS servers: {errno}")
            }
            Self::Transport(errno) => write!(f, "the DNS query could not be sent: {errno}"),
        }
    }
}

/// Fetch the host's active recursive-resolver server set through `sysinfo`,
/// converting each [`NetResolverServer`] record to an [`IpAddr`].
///
/// The set is the aggregated, deduplicated statically-configured ∪
/// DHCP-learned servers the network stack maintains, read through the
/// ungated `NET_RESOLVER_SERVERS` query (`plans/DNS.md` DNS2). It is bounded
/// by [`MAX_RESOLVER_SERVERS`], so the returned vector never grows beyond
/// that fixed cap.
///
/// # Errors
///
/// [`Errno`] describing why the query failed (a transport failure, or the
/// service refusing the query), mapped from the [`ListError`] the walk
/// raised.
pub fn configured_servers(sysinfo: &dyn Transport) -> Result<Vec<IpAddr>, Errno> {
    let mut servers = Vec::with_capacity(MAX_RESOLVER_SERVERS);
    for_each_resolver_server(sysinfo, |record| {
        servers.push(server_addr(record));
        Ok(WalkStep::Continue)
    })
    .map_err(list_error_to_errno)?;
    Ok(servers)
}

/// Resolve `name`/`record_type` by fetching the configured recursive servers
/// through `sysinfo` and driving the pure [`tairix_net::dns::resolve`] loop
/// over `udp`.
///
/// This is the one shared orchestration the production `resolve` entry point
/// and the host tests both exercise, so there is no second copy of the
/// "fetch servers, then drive the engine" logic. `rng` supplies the CSPRNG
/// draws the engine needs (the query id and retransmit jitter) and is kept
/// distinct from the transports so neither aliases the other.
///
/// # Errors
///
/// * [`ResolveError::InvalidName`] — `name` is not a valid DNS name.
/// * [`ResolveError::NoServers`] — no recursive server is configured.
/// * [`ResolveError::ServerSource`] — the server-set query failed.
/// * [`ResolveError::Transport`] — the UDP transport failed mid-resolution.
///
/// A resolution that concludes negatively (NXDOMAIN, NODATA) or as a timeout
/// is **not** an error: it is returned as the corresponding
/// [`Resolution`] so the caller can render the
/// difference between "does not exist" and "could not reach a server".
pub fn resolve_name(
    name: &str,
    record_type: RecordType,
    sysinfo: &dyn Transport,
    udp: &mut dyn DnsTransport,
    rng: &mut dyn FnMut() -> u32,
) -> Result<Resolution, ResolveError> {
    let name = Name::encode(name).map_err(ResolveError::InvalidName)?;
    let servers = configured_servers(sysinfo).map_err(ResolveError::ServerSource)?;
    if servers.is_empty() {
        return Err(ResolveError::NoServers);
    }
    dns::resolve(name, record_type, &servers, udp, rng).map_err(ResolveError::Transport)
}

/// Convert a [`NetResolverServer`] record to an [`IpAddr`].
fn server_addr(record: &NetResolverServer) -> IpAddr {
    match record.family {
        NetAddrFamily::V4 => IpAddr::V4(Ipv4Addr::new(
            record.addr[0],
            record.addr[1],
            record.addr[2],
            record.addr[3],
        )),
        NetAddrFamily::V6 => IpAddr::V6(Ipv6Addr::from(record.addr)),
    }
}

/// Collapse the paged-walk [`ListError`] onto the wire-level [`Errno`] the
/// caller renders — a capability denial keeps its distinguished
/// [`Errno::PermissionDenied`] spelling rather than being folded away.
fn list_error_to_errno(error: ListError) -> Errno {
    match error {
        ListError::Call(CallError::PermissionDenied) => Errno::PermissionDenied,
        ListError::Call(CallError::Service(errno)) | ListError::Sink(errno) => errno,
    }
}

#[cfg(test)]
mod tests;
