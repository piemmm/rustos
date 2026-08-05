//! The shared active-resolver-server walk.
//!
//! TAIRiX has no `/etc/resolv.conf`: the host's active recursive-resolver
//! server set is read through the typed, versioned, capability-checked
//! `sysinfo-v1` [`SysinfoQueryId::NET_RESOLVER_SERVERS`] query, served by
//! `sysinfod` (which forwards to `netstack`'s broker read). The set is the
//! aggregated, deduplicated DHCP-learned ∪ statically-configured DNS servers
//! the stack maintains (`plans/DNS.md` DNS2), the one source both this read
//! and a userland resolver client share — so the two can never disagree
//! (`AGENTS.md` §2.2). The paging loop is the generic
//! [`walk_pages`](crate::list) the socket and mount walks use, so only the
//! per-record decode lives here.

use tairix_abi::net_ipc::{NetResolverServer, MAX_RESOLVER_SERVERS};
use tairix_abi::sysinfo::{NetInterfaceListRequest, SysinfoQueryId};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError, WalkStep};
use crate::request::CallError;
use crate::transport::Transport;

/// Number of [`NetResolverServer`]s requested per page.
///
/// The active set is a small, closed set bounded by
/// [`MAX_RESOLVER_SERVERS`], so one page always suffices; the walk still
/// terminates on the first short page like any other list. Tied to
/// [`MAX_RESOLVER_SERVERS`] by a compile-time assertion so the two cannot
/// drift (the widening `u16 -> usize` comparison avoids a truncating cast).
pub const RESOLVER_SERVER_PAGE: u16 = 4;
const _: () = assert!(RESOLVER_SERVER_PAGE as usize == MAX_RESOLVER_SERVERS);

/// Page the host's active recursive-resolver server set and hand each
/// decoded [`NetResolverServer`] to `sink`.
///
/// The query is [`SysinfoQueryId::NET_RESOLVER_SERVERS`], which the broker
/// serves ungated: the recursive DNS servers a host queries are public host
/// configuration (the resolv.conf analogue), exposing no per-principal
/// secret.
///
/// `sink` answers [`WalkStep::Continue`] to be given the next record or
/// [`WalkStep::Stop`] to end the walk there, which is how a caller bounds
/// how much of a long or hostile list it will accept. Stopping is an
/// ordinary success, so it stays distinguishable from a failure.
///
/// The walk **fails closed**: a reply whose length is not a whole number of
/// [`NetResolverServer::WIRE_LEN`] records, or one that would overflow the
/// page offset, is rejected rather than partially delivered.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_resolver_server(
    transport: &dyn Transport,
    mut sink: impl FnMut(&NetResolverServer) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::NET_RESOLVER_SERVERS,
        NetResolverServer::WIRE_LEN,
        RESOLVER_SERVER_PAGE,
        |offset, limit| {
            NetInterfaceListRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = NetResolverServer::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::net_ipc::NetAddrFamily;
    use tairix_abi::sysinfo::SysinfoRequestHeader;

    /// An in-memory `sysinfod` stand-in answering resolver-server queries
    /// from a fixed record set, decoding the request exactly as the real
    /// service and returning the reply as packed records.
    struct Fixture {
        records: Vec<NetResolverServer>,
        deny: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<NetResolverServer>) -> Self {
            Self {
                records,
                deny: false,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.deny {
                return Err(Errno::PermissionDenied);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = NetInterfaceListRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * NetResolverServer::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> NetResolverServer {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[a, b, c, d]);
        NetResolverServer {
            family: NetAddrFamily::V4,
            addr,
        }
    }

    fn collect(fixture: &Fixture) -> Result<Vec<NetResolverServer>, ListError> {
        let seen = RefCell::new(Vec::new());
        for_each_resolver_server(fixture, |r| {
            seen.borrow_mut().push(*r);
            Ok(WalkStep::Continue)
        })?;
        Ok(seen.into_inner())
    }

    #[test]
    fn walk_routes_the_resolver_query_and_yields_records() {
        let servers = alloc::vec![
            v4(10, 0, 2, 3),
            NetResolverServer {
                family: NetAddrFamily::V6,
                addr: [0x26; 16],
            },
        ];
        let fixture = Fixture::new(servers.clone());
        let got = collect(&fixture).expect("ok");
        assert_eq!(got, servers);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::NET_RESOLVER_SERVERS]
        );
    }

    #[test]
    fn an_empty_set_yields_no_records_in_one_call() {
        let fixture = Fixture::new(Vec::new());
        let got = collect(&fixture).expect("ok");
        assert!(got.is_empty());
        assert_eq!(fixture.seen.borrow().len(), 1);
    }

    #[test]
    fn a_denial_is_surfaced_not_swallowed() {
        let mut fixture = Fixture::new(alloc::vec![v4(9, 9, 9, 9)]);
        fixture.deny = true;
        let result = for_each_resolver_server(&fixture, |_| Ok(WalkStep::Continue));
        assert_eq!(result, Err(ListError::Call(CallError::PermissionDenied)));
    }
}
