//! The shared process-list paging walk and row rendering.
//!
//! Both the `sysinfo` and `ps` tools page through the process list the same
//! way — fixed-size pages walked until a short page ends the list — and
//! render each [`ProcessRecord`] into the same fixed-column row. That logic
//! lives here, in one place, rather than being copied.

use alloc::format;
use alloc::string::String;

use rustos_abi::sysinfo::{ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId};
use rustos_abi::Errno;

use crate::list::{field_lossy, walk_pages, ListError};
use crate::request::CallError;
use crate::transport::Transport;

/// Number of [`ProcessRecord`]s requested per process-list page.
///
/// A page bounds the reply size so the transport never has to carry every
/// process at once; [`for_each_process`] walks pages until a short page ends
/// the list.
pub const PROCESS_PAGE: u16 = 64;

/// The column header for a process listing, matching the columns
/// [`render_process`] produces.
pub const PROCESS_HEADER: &str = "  PID  PPID   UID   GID S CPU NAME";

/// Page through the process list and hand each decoded [`ProcessRecord`] to
/// `sink`.
///
/// `all` selects the system-wide view
/// ([`SysinfoQueryId::GLOBAL_PROCESS_LIST`], which the service gates on
/// `CAP_SYSINFO_GLOBAL`); otherwise the caller's own processes
/// ([`SysinfoQueryId::SELF_PROCESS_LIST`], ungated). Records are delivered in
/// the order the service returns them.
///
/// The walk **fails closed**: a reply whose length
/// is not a whole number of [`ProcessRecord::WIRE_LEN`] records, or one that
/// would overflow the page offset, is rejected rather than partially
/// rendered.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_process(
    transport: &dyn Transport,
    all: bool,
    mut sink: impl FnMut(&ProcessRecord) -> Result<(), Errno>,
) -> Result<(), ListError> {
    let query = if all {
        SysinfoQueryId::GLOBAL_PROCESS_LIST
    } else {
        SysinfoQueryId::SELF_PROCESS_LIST
    };
    walk_pages(
        transport,
        query,
        ProcessRecord::WIRE_LEN,
        PROCESS_PAGE,
        |offset, limit| {
            ProcessListRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = ProcessRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Render one [`ProcessRecord`] as a fixed-column row matching
/// [`PROCESS_HEADER`].
#[must_use]
pub fn render_process(record: &ProcessRecord) -> String {
    format!(
        "{:>5} {:>5} {:>5} {:>5} {} {:>3} {}",
        record.pid,
        record.parent_pid,
        record.uid,
        record.gid,
        state_char(record.state),
        record.cpu,
        field_lossy(record.name_bytes()),
    )
}

/// A single-letter process-state code, in the spirit of `ps`.
#[must_use]
pub fn state_char(state: ProcessState) -> char {
    match state {
        ProcessState::Runnable => 'r',
        ProcessState::Running => 'R',
        ProcessState::Blocked => 'D',
        ProcessState::Zombie => 'Z',
        ProcessState::Stopped => 'T',
    }
}

#[cfg(test)]
mod tests {
    use super::{for_each_process, render_process, state_char, PROCESS_HEADER, PROCESS_PAGE};
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::sysinfo::{
        ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId, SysinfoRequestHeader,
    };
    use rustos_abi::{Errno, ProcId};

    /// An in-memory `sysinfod` stand-in answering process-list queries from a
    /// fixed record set, decoding the request exactly as the real service.
    struct Fixture {
        records: Vec<ProcessRecord>,
        deny: bool,
        malformed: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<ProcessRecord>) -> Self {
            Self {
                records,
                deny: false,
                malformed: false,
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
            if self.malformed {
                return Ok(alloc::vec![0u8; ProcessRecord::WIRE_LEN + 1]);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = ProcessListRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * ProcessRecord::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    fn record(pid: u64, name: &[u8], state: ProcessState) -> ProcessRecord {
        ProcessRecord::new(
            pid,
            1,
            ProcId::KERNEL,
            ProcId::KERNEL,
            1000,
            1000,
            state,
            0,
            name,
        )
        .expect("record")
    }

    fn collect(all: bool, fixture: &Fixture) -> Result<Vec<ProcessRecord>, ListError> {
        let seen = RefCell::new(Vec::new());
        for_each_process(fixture, all, |r| {
            seen.borrow_mut().push(*r);
            Ok(())
        })?;
        Ok(seen.into_inner())
    }

    #[test]
    fn self_walk_routes_self_query_and_yields_records() {
        let fixture = Fixture::new(alloc::vec![
            record(1, b"init", ProcessState::Running),
            record(7, b"shell", ProcessState::Blocked),
        ]);
        let got = collect(false, &fixture).expect("ok");
        assert_eq!(got.len(), 2);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::SELF_PROCESS_LIST]
        );
    }

    #[test]
    fn all_walk_routes_global_query() {
        let fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Runnable)]);
        let _ = collect(true, &fixture).expect("ok");
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::GLOBAL_PROCESS_LIST]
        );
    }

    #[test]
    fn walk_pages_until_a_short_page() {
        let mut records = Vec::new();
        for pid in 0..=u64::from(PROCESS_PAGE) {
            records.push(record(pid, b"p", ProcessState::Runnable));
        }
        let fixture = Fixture::new(records);
        let got = collect(false, &fixture).expect("ok");
        assert_eq!(got.len(), usize::from(PROCESS_PAGE) + 1);
        // A full page plus a short page: two requests.
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn empty_list_yields_nothing() {
        let fixture = Fixture::new(Vec::new());
        let got = collect(false, &fixture).expect("ok");
        assert!(got.is_empty());
        assert_eq!(fixture.seen.borrow().len(), 1);
    }

    #[test]
    fn denied_query_maps_to_call_permission_denied() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.deny = true;
        assert_eq!(
            collect(true, &fixture),
            Err(ListError::Call(CallError::PermissionDenied))
        );
    }

    #[test]
    fn malformed_reply_fails_closed() {
        let mut fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Running)]);
        fixture.malformed = true;
        assert_eq!(
            collect(false, &fixture),
            Err(ListError::Call(CallError::Service(Errno::BadMagic)))
        );
    }

    #[test]
    fn sink_error_stops_the_walk_and_is_reported() {
        let fixture = Fixture::new(alloc::vec![
            record(1, b"init", ProcessState::Running),
            record(2, b"two", ProcessState::Running),
        ]);
        let count = RefCell::new(0usize);
        let result = for_each_process(&fixture, false, |_| {
            let mut c = count.borrow_mut();
            *c += 1;
            if *c == 1 {
                Err(Errno::NotFound)
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(ListError::Sink(Errno::NotFound)));
        // The walk stopped at the first record.
        assert_eq!(*count.borrow(), 1);
    }

    #[test]
    fn render_process_columns_and_state_chars() {
        let r = render_process(&record(42, b"daemon", ProcessState::Blocked));
        assert!(r.contains("42"));
        assert!(r.contains("daemon"));
        assert!(r.contains(" D "));
        assert_eq!(state_char(ProcessState::Running), 'R');
        assert_eq!(state_char(ProcessState::Runnable), 'r');
        assert_eq!(state_char(ProcessState::Zombie), 'Z');
        assert_eq!(state_char(ProcessState::Stopped), 'T');
        assert!(PROCESS_HEADER.contains("PID"));
        assert!(PROCESS_HEADER.contains("NAME"));
    }

    #[test]
    fn render_process_is_lossy_on_invalid_name_bytes() {
        let r = render_process(&record(1, &[0xFF, 0xFE], ProcessState::Running));
        // Lossy decoding never panics and yields a replacement char.
        assert!(r.contains('\u{FFFD}'));
    }
}
