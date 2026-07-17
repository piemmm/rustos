//! The shared CPU-time paging walk.
//!
//! `top`-style viewers page through the per-CPU execution-time accounting
//! and derive a busy/idle utilisation split from the deltas of two samples.
//! The paging is the generic [`walk_pages`](crate::list) used by the process
//! and mount lists, so only the per-record decode lives here.

use tairix_abi::sysinfo::{CpuTimeListRequest, CpuTimeRecord, SysinfoQueryId};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError};
use crate::request::CallError;
use crate::transport::Transport;

/// Number of [`CpuTimeRecord`]s requested per CPU-time page.
///
/// A page bounds the reply size so the transport never has to carry every
/// CPU at once; [`for_each_cpu_time`] walks pages until a short page ends
/// the list.
pub const CPU_TIME_PAGE: u16 = 64;

/// Page through the per-CPU execution-time accounting and hand each decoded
/// [`CpuTimeRecord`] to `sink`.
///
/// The query is [`SysinfoQueryId::CPU_TIME_STATS`], which the service serves
/// ungated: the busy/idle utilisation split is system-wide and secret-free.
/// Records are delivered in the order the service returns them (ascending
/// CPU index).
///
/// The walk **fails closed**: a reply whose length is not a whole number of
/// [`CpuTimeRecord::WIRE_LEN`] records, or one that would overflow the page
/// offset, is rejected rather than partially rendered.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_cpu_time(
    transport: &dyn Transport,
    mut sink: impl FnMut(&CpuTimeRecord) -> Result<(), Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::CPU_TIME_STATS,
        CpuTimeRecord::WIRE_LEN,
        CPU_TIME_PAGE,
        |offset, limit| {
            CpuTimeListRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = CpuTimeRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{for_each_cpu_time, CPU_TIME_PAGE};
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::sysinfo::{
        CpuTimeListRequest, CpuTimeRecord, SysinfoQueryId, SysinfoRequestHeader,
    };
    use tairix_abi::Errno;

    /// An in-memory `sysinfod` stand-in answering CPU-time queries from a
    /// fixed record set, decoding the request exactly as the real service.
    struct Fixture {
        records: Vec<CpuTimeRecord>,
        malformed: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<CpuTimeRecord>) -> Self {
            Self {
                records,
                malformed: false,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.malformed {
                return Ok(alloc::vec![0u8; CpuTimeRecord::WIRE_LEN + 1]);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = CpuTimeListRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * CpuTimeRecord::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    fn record(cpu: u32, busy_ns: u64, idle_ns: u64) -> CpuTimeRecord {
        CpuTimeRecord {
            cpu,
            reserved: 0,
            busy_ns,
            idle_ns,
        }
    }

    fn collect(fixture: &Fixture) -> Result<Vec<CpuTimeRecord>, ListError> {
        let seen = RefCell::new(Vec::new());
        for_each_cpu_time(fixture, |r| {
            seen.borrow_mut().push(*r);
            Ok(())
        })?;
        Ok(seen.into_inner())
    }

    #[test]
    fn walk_routes_the_cpu_time_query_and_yields_records() {
        let fixture = Fixture::new(alloc::vec![record(0, 750, 250), record(1, 100, 900)]);
        let got = collect(&fixture).expect("ok");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].busy_ns, 750);
        assert_eq!(got[1].cpu, 1);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::CPU_TIME_STATS]
        );
    }

    #[test]
    fn walk_pages_until_a_short_page() {
        let mut records = Vec::new();
        for cpu in 0..=u32::from(CPU_TIME_PAGE) {
            records.push(record(cpu, 1, 1));
        }
        let fixture = Fixture::new(records);
        let got = collect(&fixture).expect("ok");
        assert_eq!(got.len(), usize::from(CPU_TIME_PAGE) + 1);
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn no_cpus_yields_nothing() {
        let fixture = Fixture::new(Vec::new());
        assert!(collect(&fixture).expect("ok").is_empty());
        assert_eq!(fixture.seen.borrow().len(), 1);
    }

    #[test]
    fn malformed_reply_fails_closed() {
        let mut fixture = Fixture::new(alloc::vec![record(0, 1, 1)]);
        fixture.malformed = true;
        assert_eq!(
            collect(&fixture),
            Err(ListError::Call(CallError::Service(Errno::BadMagic)))
        );
    }

    #[test]
    fn sink_error_stops_the_walk() {
        let fixture = Fixture::new(alloc::vec![record(0, 1, 1), record(1, 2, 2)]);
        let count = RefCell::new(0usize);
        let result = for_each_cpu_time(&fixture, |_| {
            *count.borrow_mut() += 1;
            Err(Errno::NotFound)
        });
        assert_eq!(result, Err(ListError::Sink(Errno::NotFound)));
        assert_eq!(*count.borrow(), 1);
    }
}
