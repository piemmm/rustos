//! The request/render engine: turn a [`Command`] into the `sysinfo-v1`
//! process-list query, page through the reply, and render one row per
//! process.

use rustos_help::{own_short_help, HelpSource};
use rustos_procinfo::{for_each_process, render_process, Output, Transport, PROCESS_HEADER};

use crate::command::Command;
use crate::error::PsError;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `ps`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: ps [-e | -A | --all]

  (default)   list your own processes
  -e, -A      list every process (needs CAP_SYSINFO_GLOBAL)
  -h, -?      show this help";

/// `ps`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "ps";

/// Run one [`Command`], issuing its query through `transport` and writing the
/// rendered listing to `out`. `locale` is the user's `LANG` preference, if
/// set; `help` is the tool's own `Help/` tree, read by the short-help
/// switches.
///
/// The page walk and row rendering are the shared helpers from
/// `lib/procinfo`; `ps` only supplies the column header and the per-row sink. The capability gate lives in `sysinfod`, not here: a
/// denied global listing comes back as
/// [`Errno::PermissionDenied`](rustos_abi::Errno::PermissionDenied), which
/// the tool renders honestly as [`PsError::PermissionDenied`].
///
/// # Errors
///
/// * [`PsError::PermissionDenied`] — the service refused the global listing
///   for want of `CAP_SYSINFO_GLOBAL`.
/// * [`PsError::Service`] — the transport failed or the reply did not decode
///   against `sysinfo-v1`.
/// * [`PsError::Output`] — writing the terminal failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), PsError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::List { all } => run_list(all, transport, out),
    }
}

/// Render `ps`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails. The rendered page is written as one multi-line `write_line`;
/// the seam owns the final newline.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), PsError> {
    let bytes = own_short_help(help, locale, OWN_WORD);
    let text = bytes
        .as_deref()
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or(USAGE);
    out.write_line(text.trim_end_matches('\n'))
        .map_err(PsError::Output)
}

/// Page through the process list and render one row per process.
fn run_list(all: bool, transport: &dyn Transport, out: &dyn Output) -> Result<(), PsError> {
    out.write_line(PROCESS_HEADER).map_err(PsError::Output)?;
    for_each_process(transport, all, |record| {
        out.write_line(&render_process(record))
    })
    .map_err(PsError::from)
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::Command;
    use crate::error::PsError;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::sysinfo::{
        ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId, SysinfoRequestHeader,
    };
    use rustos_abi::{Errno, ProcId};
    use rustos_help::{HelpSource, SourceError};
    use rustos_procinfo::{Output, Transport};

    /// A Help tree with no documents at all: the short-help fallback path.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    /// A Help tree holding one canonical `ps.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\nps — list processes\n\n\
                       ## SYNOPSIS\n\n`ps [-e | -A | --all] [-h | -?]`\n\n\
                       ## DESCRIPTION\n\nLists processes.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("default")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "default" && file_name == "ps.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// An in-memory `sysinfod` stand-in: it decodes a request the same way
    /// the real service does and answers process-list queries from fixtures.
    struct Fixture {
        records: Vec<ProcessRecord>,
        deny_global: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<ProcessRecord>) -> Self {
            Self {
                records,
                deny_global: false,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.deny_global && header.query == SysinfoQueryId::GLOBAL_PROCESS_LIST {
                return Err(Errno::PermissionDenied);
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

    /// Captures rendered lines; optionally fails on the Nth write.
    struct Recorder {
        lines: RefCell<Vec<String>>,
        fail_at: Option<usize>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                fail_at: None,
            }
        }

        fn failing_at(index: usize) -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                fail_at: Some(index),
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.borrow().clone()
        }
    }

    impl Output for Recorder {
        fn write_line(&self, line: &str) -> Result<(), Errno> {
            let mut lines = self.lines.borrow_mut();
            if self.fail_at == Some(lines.len()) {
                return Err(Errno::NotFound);
            }
            lines.push(line.to_string());
            Ok(())
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
        .unwrap()
    }

    #[test]
    fn help_prints_the_usage_fallback_and_touches_no_query() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Help, None, &fixture, &NoHelp, &out), Ok(()));
        assert_eq!(out.lines(), alloc::vec![USAGE.to_string()]);
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Help, None, &fixture, &OneDoc, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("list processes"));
        assert!(lines[0].contains("ps [-e | -A | --all]"));
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn self_list_renders_header_and_rows_and_routes_self() {
        let fixture = Fixture::new(alloc::vec![
            record(1, b"init", ProcessState::Running),
            record(7, b"shell", ProcessState::Blocked),
        ]);
        let out = Recorder::new();
        assert_eq!(
            run(Command::List { all: false }, None, &fixture, &NoHelp, &out),
            Ok(())
        );
        let lines = out.lines();
        assert_eq!(lines.len(), 3); // header + two rows
        assert!(lines[0].contains("PID"));
        assert!(lines[1].contains("init"));
        assert!(lines[1].contains(" R "));
        assert!(lines[2].contains("shell"));
        assert!(lines[2].contains(" D "));
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::SELF_PROCESS_LIST]
        );
    }

    #[test]
    fn all_list_routes_the_global_query() {
        let fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Runnable)]);
        let out = Recorder::new();
        assert_eq!(
            run(Command::List { all: true }, None, &fixture, &NoHelp, &out),
            Ok(())
        );
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::GLOBAL_PROCESS_LIST]
        );
    }

    #[test]
    fn empty_list_renders_only_the_header() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(Command::List { all: false }, None, &fixture, &NoHelp, &out),
            Ok(())
        );
        assert_eq!(out.lines().len(), 1);
    }

    #[test]
    fn denied_global_list_maps_to_permission_denied() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.deny_global = true;
        let out = Recorder::new();
        assert_eq!(
            run(Command::List { all: true }, None, &fixture, &NoHelp, &out),
            Err(PsError::PermissionDenied)
        );
        // Only the header was written before the query failed.
        assert_eq!(out.lines().len(), 1);
    }

    #[test]
    fn output_failure_on_the_header_propagates() {
        let fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Running)]);
        let out = Recorder::failing_at(0);
        assert_eq!(
            run(Command::List { all: false }, None, &fixture, &NoHelp, &out),
            Err(PsError::Output(Errno::NotFound))
        );
    }

    #[test]
    fn output_failure_on_a_row_propagates() {
        let fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Running)]);
        // Header is index 0; the first row is index 1.
        let out = Recorder::failing_at(1);
        assert_eq!(
            run(Command::List { all: false }, None, &fixture, &NoHelp, &out),
            Err(PsError::Output(Errno::NotFound))
        );
    }
}
