//! The request/render engine: turn a [`Command`] into typed `sysinfo-v1`
//! requests, decode the typed replies, and render human-readable lines.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use rustos_abi::sysinfo::{KernelMemoryStats, SysinfoQueryId, SystemIdentity, Uptime};

use rustos_procinfo::{call, for_each_process, render_process, Output, Transport, PROCESS_HEADER};

use crate::command::Command;
use crate::error::SysinfoError;

/// The usage banner printed by [`Command::Help`] and on a usage error.
pub const USAGE: &str = "\
usage: sysinfo <query>

queries:
  processes [--all]   list processes (--all: every process, needs CAP_SYSINFO_GLOBAL)
  memory              kernel memory statistics (needs CAP_SYSINFO_KERNEL)
  hardware            detected hardware tree (needs CAP_SYSINFO_HW)
  identity            machine identity and OS version
  uptime              time since boot and boot wall-clock time
  help                show this message";

/// Run one [`Command`], issuing its query through `transport` and writing the
/// rendered result to `out`.
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
    transport: &dyn Transport,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    match command {
        Command::Help => emit(out, USAGE),
        Command::Processes { all } => run_processes(all, transport, out),
        Command::Memory => run_memory(transport, out),
        Command::Hardware => run_hardware(transport, out),
        Command::Identity => run_identity(transport, out),
        Command::Uptime => run_uptime(transport, out),
    }
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

/// Page through the process list and render one row per process.
///
/// The page walk and row rendering are the shared helpers from
/// `lib/procinfo`; the CLI only supplies the column header and the per-row
/// sink (`AGENTS.md` §2.2).
fn run_processes(
    all: bool,
    transport: &dyn Transport,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    emit(out, PROCESS_HEADER)?;
    for_each_process(transport, all, |record| {
        out.write_line(&render_process(record))
    })
    .map_err(SysinfoError::from)
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
/// The hardware-tree wire format is owned by `lib/abi` §18 and is not built
/// yet, so the CLI does not pretend to decode it: it honestly reports the
/// byte length the service returned (`AGENTS.md` §2.1 — no faking).
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
/// invalid byte rather than failing (a display routine never panics, §2.9).
fn name_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::Command;
    use crate::error::SysinfoError;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::sysinfo::{
        KernelMemoryStats, ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId,
        SysinfoRequestHeader, SystemIdentity, Uptime,
    };
    use rustos_abi::time::{Duration64, Time64};
    use rustos_abi::Errno;
    use rustos_procinfo::{Output, Transport};

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
            } else {
                Err(Errno::NotImplemented)
            }
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
        ProcessRecord::new(pid, 1, 1000, 1000, state, 0, name).unwrap()
    }

    #[test]
    fn help_prints_usage() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Help, &fixture, &out), Ok(()));
        assert_eq!(out.lines(), alloc::vec![USAGE.to_string()]);
        // Help touches no query.
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
