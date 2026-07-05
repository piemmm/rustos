//! The request/render engine: turn a [`Command`] into typed `sysinfo-v1`
//! requests, decode the typed replies, and render human-readable lines.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use rustos_abi::sysinfo::{
    KernelMemoryStats, ResourceLimitRecord, SysinfoQueryId, SystemIdentity, Uptime,
};
use rustos_abi::{Errno, LimitKind};

use rustos_help::{own_short_help, HelpSource};
use rustos_procinfo::{
    call, emit_self_scope_omission, for_each_process, render_limit_bound, render_process, Output,
    Transport, PROCESS_HEADER,
};

use crate::command::Command;
use crate::error::SysinfoError;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `sysinfo`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: sysinfo <query>

queries:
  processes [--all]   list processes (--all: every process, needs CAP_SYSINFO_GLOBAL)
  memory              kernel memory statistics (needs CAP_SYSINFO_KERNEL)
  hardware            detected hardware tree (needs CAP_SYSINFO_HW)
  identity            machine identity and OS version
  uptime              time since boot and boot wall-clock time
  limits              your effective resource limits and live usage
  help, -h, -?        show this help";

/// `sysinfo`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "sysinfo";

/// Run one [`Command`], issuing its query through `transport` and writing the
/// rendered result to `out`. `locale` is the user's `LANG` preference, if
/// set; `help` is the tool's own `Help/` tree, read by the short-help
/// switches.
///
/// # Errors
///
/// * [`SysinfoError::PermissionDenied`] — the service refused the query for
///   want of its declared capability.
/// * [`SysinfoError::Service`] — the transport failed or the reply did not
///   decode against `sysinfo-v1`.
/// * [`SysinfoError::Output`] — writing the terminal failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Processes { all } => run_processes(all, transport, out),
        Command::Memory => run_memory(transport, out),
        Command::Hardware => run_hardware(transport, out),
        Command::Identity => run_identity(transport, out),
        Command::Uptime => run_uptime(transport, out),
        Command::Limits => run_limits(transport, out),
    }
}

/// Render `sysinfo`'s own short help (`NAME` + `SYNOPSIS` + compact
/// `OPTIONS`) from its own Help tree through the one shared engine; when no
/// document can be served (a build without the bundle's documents) the
/// usage banner stands in — the tool's own text, not fabricated help
/// content — so `-h` never fails. The rendered page is written as one
/// multi-line `write_line`; the seam owns the final newline.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    let bytes = own_short_help(help, locale, OWN_WORD);
    let text = bytes
        .as_deref()
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or(USAGE);
    emit(out, text.trim_end_matches('\n'))
}

/// Issue `query` with `payload` through the shared client helper and map a
/// capability denial or transport failure onto the CLI's error vocabulary.
fn service_call(
    transport: &dyn Transport,
    query: SysinfoQueryId,
    payload: &[u8],
) -> Result<Vec<u8>, SysinfoError> {
    call(transport, query, payload).map_err(SysinfoError::from)
}

/// Write `line` to `out`, mapping a console failure onto
/// [`SysinfoError::Output`].
fn emit(out: &dyn Output, line: &str) -> Result<(), SysinfoError> {
    out.write_line(line).map_err(SysinfoError::Output)
}

/// Page through the process list and render one row per process. The
/// default self scope also notes on the advisory stream (fd 3) that the
/// listing is not system-wide, so a tool or user knows stdout is not
/// exhaustive.
///
/// The page walk, the row rendering, and the self-scope advisory are the
/// shared helpers from `lib/procinfo` (the same record definition `ps`
/// emits); the CLI only supplies the column header, the per-row sink, and
/// its own widening spelling.
fn run_processes(
    all: bool,
    transport: &dyn Transport,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    emit(out, PROCESS_HEADER)?;
    for_each_process(transport, all, |record| {
        out.write_line(&render_process(record))
    })
    .map_err(SysinfoError::from)?;
    if !all {
        emit_self_scope_omission(out, OWN_WORD, &[OWN_WORD, "processes", "--all"]);
    }
    Ok(())
}

/// Fetch and render the kernel memory statistics.
fn run_memory(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::KERNEL_MEMORY_STATS, &[])?;
    let stats = KernelMemoryStats::from_bytes(&reply).map_err(SysinfoError::Service)?;
    emit(out, &format!("total bytes:     {}", stats.total_bytes))?;
    emit(out, &format!("free bytes:      {}", stats.free_bytes))?;
    emit(
        out,
        &format!("kernel heap:     {}", stats.kernel_heap_bytes),
    )?;
    emit(
        out,
        &format!("user resident:   {}", stats.user_resident_bytes),
    )?;
    emit(out, &format!("page size:       {}", stats.page_size))
}

/// Fetch the hardware tree and report its size.
///
/// The hardware-tree wire format is owned by `lib/abi` and is not built
/// yet, so the CLI does not pretend to decode it: it honestly reports the
/// byte length the service returned (no faking).
fn run_hardware(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::HARDWARE_TREE, &[])?;
    emit(out, &format!("hardware tree: {} bytes", reply.len()))
}

/// Fetch and render the machine identity.
fn run_identity(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::SYSTEM_IDENTITY, &[])?;
    let identity = SystemIdentity::from_bytes(&reply).map_err(SysinfoError::Service)?;
    emit(
        out,
        &format!("hostname:    {}", name_lossy(identity.hostname_bytes())),
    )?;
    emit(out, &format!("machine id:  {}", hex(&identity.machine_id)))?;
    emit(
        out,
        &format!(
            "os version:  {}.{}.{}",
            identity.version_major, identity.version_minor, identity.version_patch
        ),
    )
}

/// Fetch and render system uptime.
fn run_uptime(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::UPTIME, &[])?;
    let uptime = Uptime::from_bytes(&reply).map_err(SysinfoError::Service)?;
    emit(
        out,
        &format!(
            "since boot:  {}.{:09}s",
            uptime.since_boot.secs(),
            uptime.since_boot.subsec_nanos()
        ),
    )?;
    emit(
        out,
        &format!(
            "boot time:   {}.{:09}s since the Unix epoch",
            uptime.boot_time.secs(),
            uptime.boot_time.subsec_nanos()
        ),
    )
}

/// Fetch and render the caller's effective resource limits and live usage.
///
/// The reply is exactly [`LimitKind::COUNT`] [`ResourceLimitRecord`]s in
/// discriminant order; the CLI decodes them positionally and prints one
/// aligned row per resource. A reply of the wrong length fails closed rather than rendering a partial table.
fn run_limits(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::RESOURCE_LIMITS, &[])?;
    if reply.len() != ResourceLimitRecord::WIRE_LEN * LimitKind::COUNT {
        return Err(SysinfoError::Service(Errno::BufferTooSmall));
    }
    emit(out, "resource              soft         hard         usage")?;
    for index in 0..LimitKind::COUNT {
        let base = index * ResourceLimitRecord::WIRE_LEN;
        let record =
            ResourceLimitRecord::from_bytes(&reply[base..base + ResourceLimitRecord::WIRE_LEN])
                .map_err(SysinfoError::Service)?;
        emit(
            out,
            &format!(
                "{:<20}  {:>11}  {:>11}  {:>11}",
                record.kind.name(),
                render_limit_bound(record.limit.soft),
                render_limit_bound(record.limit.hard),
                record.usage,
            ),
        )?;
    }
    Ok(())
}

/// Render `bytes` as lowercase hex with no separators.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a `String` is infallible; the byte format is fixed-width.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decode an inline name buffer for display, substituting `U+FFFD` for any
/// invalid byte rather than failing (a display routine never panics).
fn name_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{run as engine_run, USAGE};
    use crate::command::Command;
    use crate::error::SysinfoError;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::sysinfo::{
        KernelMemoryStats, ProcessListRequest, ProcessRecord, ProcessState, ResourceLimitRecord,
        SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime,
    };
    use rustos_abi::time::{Duration64, Time64};
    use rustos_abi::{Errno, LimitKind, ProcId, ResourceLimit, RLIMIT_INFINITY};
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

    /// A Help tree holding one canonical `sysinfo.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\nsysinfo — query system information\n\n\
                       ## SYNOPSIS\n\n`sysinfo <query>`\n\n\
                       ## DESCRIPTION\n\nQueries things.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("default")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "default" && file_name == "sysinfo.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// The engine under the fixtures' default seams: no locale preference
    /// and an empty Help tree, so every existing scenario exercises the
    /// query paths unchanged.
    fn run(
        command: Command,
        transport: &dyn Transport,
        out: &dyn Output,
    ) -> Result<(), SysinfoError> {
        engine_run(command, None, transport, &NoHelp, out)
    }

    /// An in-memory `sysinfod` stand-in: it decodes a request the same way
    /// the real service does and answers from fixed fixtures.
    struct Fixture {
        records: Vec<ProcessRecord>,
        memory: KernelMemoryStats,
        identity: SystemIdentity,
        uptime: Uptime,
        hardware: Vec<u8>,
        deny: Option<SysinfoQueryId>,
        malformed_process_list: bool,
        short_scalar: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<ProcessRecord>) -> Self {
            Self {
                records,
                memory: KernelMemoryStats {
                    total_bytes: 4096,
                    free_bytes: 1024,
                    kernel_heap_bytes: 512,
                    user_resident_bytes: 256,
                    page_size: 4096,
                    reserved: 0,
                },
                identity: SystemIdentity::new([0xAB; 16], 1, 2, 3, b"rustbox").unwrap(),
                uptime: Uptime {
                    since_boot: Duration64::from_nanos(9),
                    boot_time: Time64::from_secs(1000),
                },
                hardware: Vec::new(),
                deny: None,
                malformed_process_list: false,
                short_scalar: false,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.deny == Some(header.query) {
                return Err(Errno::PermissionDenied);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            if header.query == SysinfoQueryId::SELF_PROCESS_LIST
                || header.query == SysinfoQueryId::GLOBAL_PROCESS_LIST
            {
                if self.malformed_process_list {
                    return Ok(alloc::vec![0u8; ProcessRecord::WIRE_LEN + 1]);
                }
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
            } else if self.short_scalar {
                Ok(alloc::vec![0u8; 3])
            } else if header.query == SysinfoQueryId::KERNEL_MEMORY_STATS {
                Ok(self.memory.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::HARDWARE_TREE {
                Ok(self.hardware.clone())
            } else if header.query == SysinfoQueryId::SYSTEM_IDENTITY {
                Ok(self.identity.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::UPTIME {
                Ok(self.uptime.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::RESOURCE_LIMITS {
                let mut out = Vec::new();
                for (index, kind) in LimitKind::ALL.iter().enumerate() {
                    let usage = index as u64;
                    let limit = ResourceLimit::new(index as u64, RLIMIT_INFINITY).unwrap();
                    out.extend_from_slice(
                        &ResourceLimitRecord::new(*kind, limit, usage).to_le_bytes(),
                    );
                }
                Ok(out)
            } else {
                Err(Errno::NotImplemented)
            }
        }
    }

    /// Captures rendered lines; optionally fails on the Nth write.
    struct Recorder {
        lines: RefCell<Vec<String>>,
        infos: RefCell<Vec<String>>,
        fail_at: Option<usize>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                infos: RefCell::new(Vec::new()),
                fail_at: None,
            }
        }

        fn failing_at(index: usize) -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                infos: RefCell::new(Vec::new()),
                fail_at: Some(index),
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.borrow().clone()
        }

        fn infos(&self) -> Vec<String> {
            self.infos.borrow().clone()
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

        fn info(&self, record: &[u8]) {
            let text = core::str::from_utf8(record).expect("JSONL is UTF-8");
            self.infos.borrow_mut().push(text.to_string());
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
    fn help_prints_the_usage_fallback() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Help, &fixture, &out), Ok(()));
        assert_eq!(out.lines(), alloc::vec![USAGE.to_string()]);
        // Help touches no query.
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            engine_run(Command::Help, None, &fixture, &OneDoc, &out),
            Ok(())
        );
        let lines = out.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("query system information"));
        assert!(lines[0].contains("sysinfo <query>"));
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn self_process_list_renders_rows_and_routes_self() {
        let fixture = Fixture::new(alloc::vec![
            record(1, b"init", ProcessState::Running),
            record(7, b"shell", ProcessState::Blocked),
        ]);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: false }, &fixture, &out),
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
        // The default self scope announces its omission on the advisory
        // stream (fd 3) through the shared record definition, suggesting
        // this tool's own widening spelling; stdout is untouched.
        let infos = out.infos();
        assert_eq!(infos.len(), 1);
        assert!(infos[0].contains("\"producer\":\"sysinfo\""));
        assert!(infos[0].contains("\"kind\":\"omission\""));
        assert!(infos[0].contains("proc.self_scope_only"));
        assert!(infos[0].contains("\"argv\":[\"sysinfo\",\"processes\",\"--all\"]"));
    }

    #[test]
    fn global_process_list_routes_global_query() {
        let fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Runnable)]);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: true }, &fixture, &out),
            Ok(())
        );
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::GLOBAL_PROCESS_LIST]
        );
        // The global view omits nothing, so no advisory record is emitted.
        assert!(out.infos().is_empty());
    }

    #[test]
    fn process_list_pages_until_a_short_page() {
        // 65 records forces a full 64-record page plus a 1-record page.
        let mut records = Vec::new();
        for pid in 0..65u64 {
            records.push(record(pid, b"p", ProcessState::Runnable));
        }
        let fixture = Fixture::new(records);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: false }, &fixture, &out),
            Ok(())
        );
        // Header + 65 rows.
        assert_eq!(out.lines().len(), 66);
        // Two paged requests were issued.
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn denied_scalar_query_maps_to_permission_denied() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.deny = Some(SysinfoQueryId::KERNEL_MEMORY_STATS);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Memory, &fixture, &out),
            Err(SysinfoError::PermissionDenied)
        );
        assert!(out.lines().is_empty());
    }

    #[test]
    fn malformed_process_reply_fails_closed() {
        let mut fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Running)]);
        fixture.malformed_process_list = true;
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: false }, &fixture, &out),
            Err(SysinfoError::Service(Errno::BadMagic))
        );
        // A failed walk renders no listing, so it announces no omission.
        assert!(out.infos().is_empty());
    }

    #[test]
    fn truncated_scalar_reply_fails_closed() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.short_scalar = true;
        let out = Recorder::new();
        assert_eq!(
            run(Command::Memory, &fixture, &out),
            Err(SysinfoError::Service(Errno::BufferTooSmall))
        );
    }

    #[test]
    fn memory_renders_every_field() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Memory, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("4096"));
        assert!(lines[1].contains("1024"));
    }

    #[test]
    fn identity_renders_hostname_machine_id_and_version() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Identity, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("rustbox"));
        assert!(lines[1].contains("abababab"));
        assert!(lines[2].contains("1.2.3"));
    }

    #[test]
    fn uptime_renders() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Uptime, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert!(lines[0].contains('9'));
        assert!(lines[1].contains("1000"));
    }

    #[test]
    fn limits_render_one_row_per_kind() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Limits, &fixture, &out), Ok(()));
        let lines = out.lines();
        // Header + one row per LimitKind.
        assert_eq!(lines.len(), 1 + LimitKind::COUNT);
        assert!(lines[0].contains("resource"));
        assert!(lines[1].contains(LimitKind::AddressSpaceBytes.name()));
        // The infinite hard bound renders as `unlimited`.
        assert!(lines[1].contains("unlimited"));
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::RESOURCE_LIMITS]
        );
    }

    #[test]
    fn limits_short_reply_fails_closed() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.short_scalar = true;
        let out = Recorder::new();
        assert_eq!(
            run(Command::Limits, &fixture, &out),
            Err(SysinfoError::Service(Errno::BufferTooSmall))
        );
    }

    #[test]
    fn hardware_reports_the_byte_count() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.hardware = alloc::vec![0u8; 42];
        let out = Recorder::new();
        assert_eq!(run(Command::Hardware, &fixture, &out), Ok(()));
        assert_eq!(
            out.lines(),
            alloc::vec!["hardware tree: 42 bytes".to_string()]
        );
    }

    #[test]
    fn output_failure_propagates() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::failing_at(0);
        assert_eq!(
            run(Command::Help, &fixture, &out),
            Err(SysinfoError::Output(Errno::NotFound))
        );
    }
}
