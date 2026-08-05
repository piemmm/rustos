//! The shared socket-listing paging walk.
//!
//! TAIRiX has no `/proc/net`: the open-socket table is read through the
//! typed, versioned, capability-checked `sysinfo-v1`
//! [`SysinfoQueryId::NET_SOCKETS`] query, served by `sysinfod` (which
//! forwards to `netstack`'s broker read). The `ss` socket-statistics tool
//! pages that query; the paging loop is the generic
//! [`walk_pages`](crate::list) the process and mount walks use, so only
//! the per-record decode lives here — never a second query client
//! (`plans/NETWORK.md` §5).

use tairix_abi::net_ipc::NetSocketRecord;
use tairix_abi::sysinfo::{NetInterfaceListRequest, SysinfoQueryId};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError, WalkStep};
use crate::request::CallError;
use crate::transport::Transport;

/// Number of [`NetSocketRecord`]s requested per socket-listing page.
///
/// Bounded so a reply frame never has to carry the whole socket table at
/// once; [`for_each_net_socket`] walks pages until a short page ends the
/// list. It matches the netstack list limit, so a page is one round trip.
pub const NET_SOCKET_PAGE: u16 = 32;

/// Page through the open-socket table and hand each decoded
/// [`NetSocketRecord`] to `sink`.
///
/// The query is [`SysinfoQueryId::NET_SOCKETS`], which the broker serves
/// only to a `CAP_SYSINFO_GLOBAL` holder and audits: the records name
/// every principal's sockets and every connection's peer address. A
/// caller without the capability receives [`ListError::Call`] carrying
/// [`CallError::PermissionDenied`] — never a fabricated empty table.
///
/// `sink` answers [`WalkStep::Continue`] to be given the next record or
/// [`WalkStep::Stop`] to end the walk there, which is how a caller bounds
/// how much of a long or hostile table it will accept. Stopping is an
/// ordinary success, so it stays distinguishable from a failure.
///
/// The walk **fails closed**: a reply whose length is not a whole number
/// of [`NetSocketRecord::WIRE_LEN`] records, or one that would overflow
/// the page offset, is rejected rather than partially delivered.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_net_socket(
    transport: &dyn Transport,
    mut sink: impl FnMut(&NetSocketRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::NET_SOCKETS,
        NetSocketRecord::WIRE_LEN,
        NET_SOCKET_PAGE,
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
            let record = NetSocketRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::CallError;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::net_ipc::{NetAddrFamily, NetSockProto, NetSockState};
    use tairix_abi::sysinfo::SysinfoRequestHeader;

    /// An in-memory `sysinfod` stand-in answering socket-list queries from a
    /// fixed record set, decoding the request exactly as the real service
    /// and returning the reply as packed records (the sysinfo reply shape).
    struct Fixture {
        records: Vec<NetSocketRecord>,
        deny: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<NetSocketRecord>) -> Self {
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
            let mut out = Vec::with_capacity(take * NetSocketRecord::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    fn sample() -> NetSocketRecord {
        let mut local = [0u8; 16];
        local[..4].copy_from_slice(&[10, 0, 2, 15]);
        NetSocketRecord {
            proto: NetSockProto::Udp,
            state: NetSockState::Unconnected,
            family: NetAddrFamily::V4,
            local_addr: local,
            local_port: 5353,
            peer_addr: [0u8; 16],
            peer_port: 0,
            owner: 7,
            recv_q: 0,
            send_q: 0,
        }
    }

    fn collect(fixture: &Fixture) -> Result<Vec<NetSocketRecord>, ListError> {
        let seen = RefCell::new(Vec::new());
        for_each_net_socket(fixture, |r| {
            seen.borrow_mut().push(*r);
            Ok(WalkStep::Continue)
        })?;
        Ok(seen.into_inner())
    }

    #[test]
    fn walk_routes_the_socket_query_and_yields_records() {
        let fixture = Fixture::new(alloc::vec![sample()]);
        let got = collect(&fixture).expect("ok");
        assert_eq!(got, alloc::vec![sample()]);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::NET_SOCKETS]
        );
    }

    #[test]
    fn walk_pages_until_a_short_page() {
        let mut records = Vec::new();
        for _ in 0..=NET_SOCKET_PAGE {
            records.push(sample());
        }
        let fixture = Fixture::new(records);
        let got = collect(&fixture).expect("ok");
        assert_eq!(got.len(), usize::from(NET_SOCKET_PAGE) + 1);
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn a_sink_error_stops_the_walk() {
        let fixture = Fixture::new(alloc::vec![sample()]);
        let result = for_each_net_socket(&fixture, |_| Err(Errno::BufferTooSmall));
        assert_eq!(result, Err(ListError::Sink(Errno::BufferTooSmall)));
    }

    #[test]
    fn a_denial_is_surfaced_not_swallowed() {
        let mut fixture = Fixture::new(alloc::vec![sample()]);
        fixture.deny = true;
        let result = for_each_net_socket(&fixture, |_| Ok(WalkStep::Continue));
        assert_eq!(result, Err(ListError::Call(CallError::PermissionDenied)));
    }
}
