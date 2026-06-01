//! The shared process-list paging walk and row rendering.
//!
//! Both the `sysinfo` and `ps` tools page through the process list the same
//! way — fixed-size pages walked until a short page ends the list — and
//! render each [`ProcessRecord`] into the same fixed-column row. That logic
//! lives here, in one place, rather than being copied (`AGENTS.md` §2.2).

use alloc::format;
use alloc::string::String;

use rustos_abi::sysinfo::{ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId};
use rustos_abi::Errno;

use crate::request::{call, CallError};
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

/// Why a process-list walk did not complete.
///
/// [`Call`](ProcessListError::Call) carries a transport/capability failure
/// (including a structurally invalid reply, reported as
/// [`Errno::BadMagic`]); [`Sink`](ProcessListError::Sink) carries the
/// [`Errno`] a caller's per-record sink raised (typically a terminal write).
/// Distinguishing them lets a consuming tool map each onto the right line of
/// its own error enum (`AGENTS.md` §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessListError {
    /// The query failed, was denied, or returned a structurally invalid
    /// reply.
    Call(CallError),
    /// The per-record sink raised an error (e.g. a failed terminal write).
    Sink(Errno),
}

/// Page through the process list and hand each decoded [`ProcessRecord`] to
/// `sink`.
///
/// `all` selects the system-wide view
/// ([`SysinfoQueryId::GLOBAL_PROCESS_LIST`], which the service gates on
/// `CAP_SYSINFO_GLOBAL`); otherwise the caller's own processes
/// ([`SysinfoQueryId::SELF_PROCESS_LIST`], ungated). Records are delivered in
/// the order the service returns them.
///
/// The walk **fails closed** (`AGENTS.md` §5.4 / §2.9): a reply whose length
/// is not a whole number of [`ProcessRecord::WIRE_LEN`] records, or one that
/// would overflow the page offset, is rejected rather than partially
/// rendered.
///
/// # Errors
///
/// * [`ProcessListError::Call`] — the transport failed, the service denied
///   the query, or the reply was structurally invalid.
/// * [`ProcessListError::Sink`] — `sink` returned an error for some record;
///   the walk stops at that record.
pub fn for_each_process(
    transport: &dyn Transport,
    all: bool,
    mut sink: impl FnMut(&ProcessRecord) -> Result<(), Errno>,
) -> Result<(), ProcessListError> {
    let query = if all {
        SysinfoQueryId::GLOBAL_PROCESS_LIST
    } else {
        SysinfoQueryId::SELF_PROCESS_LIST
    };
    let mut offset: u32 = 0;
    loop {
        let request = ProcessListRequest {
            offset,
            limit: PROCESS_PAGE,
            flags: 0,
        };
        let reply =
            call(transport, query, &request.to_le_bytes()).map_err(ProcessListError::Call)?;
        if reply.len() % ProcessRecord::WIRE_LEN != 0 {
            return Err(ProcessListError::Call(CallError::Service(Errno::BadMagic)));
        }
        let count = reply.len() / ProcessRecord::WIRE_LEN;
        for chunk in reply.chunks_exact(ProcessRecord::WIRE_LEN) {
            let record = ProcessRecord::from_bytes(chunk)
                .map_err(|errno| ProcessListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ProcessListError::Sink)?;
        }
        if count < usize::from(PROCESS_PAGE) {
            return Ok(());
        }
        let advanced = u32::try_from(count)
            .map_err(|_| ProcessListError::Call(CallError::Service(Errno::LengthOutOfRange)))?;
        offset = offset
            .checked_add(advanced)
            .ok_or(ProcessListError::Call(CallError::Service(
                Errno::LengthOutOfRange,
            )))?;
    }
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
        name_lossy(record.name_bytes()),
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

/// Decode an inline name buffer for display, substituting `U+FFFD` for any
/// invalid byte rather than failing (a display routine never panics, §2.9).
fn name_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        for_each_process, render_process, state_char, ProcessListError, PROCESS_HEADER,
        PROCESS_PAGE,
    };
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::sysinfo::{
        ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId, SysinfoRequestHeader,
    };
    use rustos_abi::Errno;

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
        ProcessRecord::new(pid, 1, 1000, 1000, state, 0, name).expect("record")
    }

    fn collect(all: bool, fixture: &Fixture) -> Result<Vec<ProcessRecord>, ProcessListError> {
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
            Err(ProcessListError::Call(CallError::PermissionDenied))
        );
    }

    #[test]
    fn malformed_reply_fails_closed() {
        let mut fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Running)]);
        fixture.malformed = true;
        assert_eq!(
            collect(false, &fixture),
            Err(ProcessListError::Call(CallError::Service(Errno::BadMagic)))
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
        assert_eq!(result, Err(ProcessListError::Sink(Errno::NotFound)));
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
