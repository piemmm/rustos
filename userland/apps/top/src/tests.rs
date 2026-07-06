//! Behaviour tests for the `top` viewer: the model's selection/scroll/scope
//! and statistics logic and the renderer/loop driven over in-memory
//! `sysinfo` and tty channels.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use rustos_abi::sysinfo::{
    CpuTimeListRequest, CpuTimeRecord, KernelMemoryStats, LoadAverage, ProcessListRequest,
    ProcessRecord, ProcessState, SysinfoQueryId, SysinfoRequestHeader, Uptime,
};
use rustos_abi::{Duration64, Errno, ProcId};
use rustos_curses::{Event, Screen, Size, Tty};
use rustos_procinfo::Transport;
use rustos_termcap::TermType;

use crate::app::{list_capacity, render, run};
use crate::error::TopError;
use crate::model::{Action, CpuSplit, Model, Scope, ALL_DENIED_NOTICE};

// ---- Fixtures --------------------------------------------------------------

/// An in-memory `sysinfod` stand-in answering the viewer's queries from a
/// fixed record set plus a settable monotonic clock, decoding each request
/// exactly as the real service.
struct FakeService {
    records: RefCell<Vec<ProcessRecord>>,
    deny_global: bool,
    /// Monotonic "now" served to `UPTIME`; `None` fails the query.
    uptime_ns: RefCell<Option<u64>>,
    load: Option<LoadAverage>,
    memory: Option<KernelMemoryStats>,
    deny_memory: bool,
    /// Per-CPU records served to `CPU_TIME_STATS`; `None` fails the query
    /// (the degraded default — most tests exercise other figures).
    cpu_times: RefCell<Option<Vec<CpuTimeRecord>>>,
    seen: RefCell<Vec<SysinfoQueryId>>,
}

impl FakeService {
    fn new(records: Vec<ProcessRecord>) -> Self {
        Self {
            records: RefCell::new(records),
            deny_global: false,
            uptime_ns: RefCell::new(Some(1_000_000_000)),
            load: None,
            memory: None,
            deny_memory: false,
            cpu_times: RefCell::new(None),
            seen: RefCell::new(Vec::new()),
        }
    }

    /// Advance the served monotonic clock and replace the record set, as a
    /// live system would between two refreshes.
    fn tick(&self, now_ns: u64, records: Vec<ProcessRecord>) {
        *self.uptime_ns.borrow_mut() = Some(now_ns);
        *self.records.borrow_mut() = records;
    }

    /// Replace the served per-CPU time records.
    fn set_cpu_times(&self, records: Vec<CpuTimeRecord>) {
        *self.cpu_times.borrow_mut() = Some(records);
    }
}

impl Transport for FakeService {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        let header = SysinfoRequestHeader::from_bytes(request)?;
        self.seen.borrow_mut().push(header.query);
        if self.deny_global && header.query == SysinfoQueryId::GLOBAL_PROCESS_LIST {
            return Err(Errno::PermissionDenied);
        }
        if header.query == SysinfoQueryId::UPTIME {
            let Some(ns) = *self.uptime_ns.borrow() else {
                return Err(Errno::NotFound);
            };
            let uptime = Uptime {
                since_boot: Duration64::from_nanos(ns),
                ..Uptime::default()
            };
            return Ok(uptime.to_le_bytes().to_vec());
        }
        if header.query == SysinfoQueryId::LOAD_AVERAGE {
            return match &self.load {
                Some(load) => Ok(load.to_le_bytes().to_vec()),
                None => Err(Errno::NotFound),
            };
        }
        if header.query == SysinfoQueryId::KERNEL_MEMORY_STATS {
            if self.deny_memory {
                return Err(Errno::PermissionDenied);
            }
            return match &self.memory {
                Some(memory) => Ok(memory.to_le_bytes().to_vec()),
                None => Err(Errno::NotFound),
            };
        }
        if header.query == SysinfoQueryId::CPU_TIME_STATS {
            let cpu_times = self.cpu_times.borrow();
            let Some(records) = cpu_times.as_ref() else {
                return Err(Errno::NotFound);
            };
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = CpuTimeListRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * CpuTimeRecord::WIRE_LEN);
            for record in &records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            return Ok(out);
        }
        let payload = &request[SysinfoRequestHeader::WIRE_LEN
            ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
        let req = ProcessListRequest::from_bytes(payload)?;
        let records = self.records.borrow();
        let offset = req.offset as usize;
        if offset >= records.len() {
            return Ok(Vec::new());
        }
        let take = core::cmp::min(records.len() - offset, req.limit as usize);
        let mut out = Vec::with_capacity(take * ProcessRecord::WIRE_LEN);
        for record in &records[offset..offset + take] {
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }
}

/// An in-memory tty: queued input chunks (one per read, so a test can
/// model an elapsed input wait as an empty chunk), captured output bytes.
struct FakeTty {
    chunks: Vec<Vec<u8>>,
    output: Vec<u8>,
}

impl FakeTty {
    fn with_input(bytes: &[u8]) -> Self {
        Self::with_chunks(&[bytes])
    }

    /// One queued chunk per read; an empty chunk models a read that
    /// returned nothing (the elapsed refresh delay).
    fn with_chunks(chunks: &[&[u8]]) -> Self {
        Self {
            chunks: chunks.iter().rev().map(|c| c.to_vec()).collect(),
            output: Vec::new(),
        }
    }
}

impl Tty for FakeTty {
    fn write(&mut self, bytes: &[u8]) -> rustos_curses::Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self) -> rustos_curses::Result<Vec<u8>> {
        Ok(self.chunks.pop().unwrap_or_default())
    }
}

/// A record with the stable fixture identity fields; `proc_id` is derived
/// from the pid so cross-refresh correlation works the way the kernel's
/// never-reused ids do.
fn record_with(pid: u64, name: &[u8], cpu_time_ns: u64, mem_bytes: u64) -> ProcessRecord {
    let mut raw = [0u8; 16];
    raw[..8].copy_from_slice(&pid.to_le_bytes());
    raw[8] = 1;
    ProcessRecord::new(
        pid,
        1,
        ProcId::from_raw(raw),
        ProcId::KERNEL,
        1000,
        1000,
        ProcessState::Running,
        0,
        cpu_time_ns,
        mem_bytes,
        name,
    )
    .expect("record")
}

fn record(pid: u64, name: &[u8]) -> ProcessRecord {
    record_with(pid, name, 0, 0)
}

fn records(n: u64) -> Vec<ProcessRecord> {
    (1..=n).map(|pid| record(pid, b"proc")).collect()
}

/// Whether `haystack` contains the byte run `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---- Model -----------------------------------------------------------------

#[test]
fn an_empty_model_has_no_selection() {
    let model = Model::new(Scope::Own);
    assert_eq!(model.selected(), None);
    assert!(model.rows().is_empty());
}

#[test]
fn refresh_populates_and_selects_the_first_row() {
    let service = FakeService::new(vec![record(1, b"init"), record(2, b"shell")]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    assert_eq!(model.rows().len(), 2);
    assert_eq!(model.selected(), Some(0));
    assert!(service
        .seen
        .borrow()
        .contains(&SysinfoQueryId::SELF_PROCESS_LIST));
    assert!(!service
        .seen
        .borrow()
        .contains(&SysinfoQueryId::GLOBAL_PROCESS_LIST));
}

#[test]
fn arrows_move_and_clamp_the_selection() {
    let service = FakeService::new(records(3));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(10);

    assert_eq!(model.handle_event(&Event::Up), Action::Ignore); // already at top
    assert_eq!(model.handle_event(&Event::Down), Action::Redraw);
    assert_eq!(model.selected(), Some(1));
    assert_eq!(model.handle_event(&Event::End), Action::Redraw);
    assert_eq!(model.selected(), Some(2));
    assert_eq!(model.handle_event(&Event::Down), Action::Ignore); // clamped at bottom
    assert_eq!(model.handle_event(&Event::Home), Action::Redraw);
    assert_eq!(model.selected(), Some(0));
}

#[test]
fn the_selection_scrolls_to_stay_visible() {
    let service = FakeService::new(records(20));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(5);
    assert_eq!(model.scroll_top(), 0);

    model.handle_event(&Event::End);
    assert_eq!(model.selected(), Some(19));
    // The last row must be visible: top is clamped so the viewport ends at 19.
    assert_eq!(model.scroll_top(), 15);

    model.handle_event(&Event::Home);
    assert_eq!(model.scroll_top(), 0);
}

#[test]
fn page_keys_move_a_viewport_at_a_time() {
    let service = FakeService::new(records(20));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(5);
    model.handle_event(&Event::PageDown);
    assert_eq!(model.selected(), Some(5));
    model.handle_event(&Event::PageUp);
    assert_eq!(model.selected(), Some(0));
}

#[test]
fn toggling_scope_asks_for_a_refresh_and_flips_the_view() {
    let mut model = Model::new(Scope::Own);
    assert_eq!(model.handle_event(&Event::Char('a')), Action::Refresh);
    assert_eq!(model.scope(), Scope::All);
    assert_eq!(model.handle_event(&Event::Char('a')), Action::Refresh);
    assert_eq!(model.scope(), Scope::Own);
}

#[test]
fn the_help_key_toggles_the_overlay() {
    let mut model = Model::new(Scope::Own);
    assert!(!model.help_visible());
    assert_eq!(model.handle_event(&Event::Char('?')), Action::Redraw);
    assert!(model.help_visible());
    assert_eq!(model.handle_event(&Event::Char('h')), Action::Redraw);
    assert!(!model.help_visible());
}

#[test]
fn quitting_is_reported() {
    let mut model = Model::new(Scope::Own);
    assert_eq!(model.handle_event(&Event::Char('q')), Action::Quit);
}

#[test]
fn an_unmapped_key_is_ignored() {
    let mut model = Model::new(Scope::Own);
    assert_eq!(model.handle_event(&Event::Tab), Action::Ignore);
}

#[test]
fn refresh_clamps_a_selection_past_a_shrunken_list() {
    let service = FakeService::new(records(10));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.set_viewport(10);
    model.handle_event(&Event::End);
    assert_eq!(model.selected(), Some(9));

    // The list shrinks under the cursor; refresh must clamp the selection.
    let smaller = FakeService::new(records(3));
    model.refresh(&smaller).expect("ok");
    assert_eq!(model.selected(), Some(2));
}

#[test]
fn a_denied_global_refresh_reports_permission_denied() {
    let mut service = FakeService::new(records(2));
    service.deny_global = true;
    let mut model = Model::new(Scope::All);
    assert_eq!(model.refresh(&service), Err(TopError::PermissionDenied));
}

#[test]
fn a_recovering_refresh_falls_back_to_own_and_posts_the_notice() {
    let mut service = FakeService::new(records(2));
    service.deny_global = true;
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("own view is ungated");
    // 'a' asks for the system-wide view; the service refuses it, so the
    // recovering refresh reverts to the caller's own processes and says why
    // instead of failing the session.
    assert_eq!(model.handle_event(&Event::Char('a')), Action::Refresh);
    assert_eq!(model.refresh_recovering(&service), Ok(()));
    assert_eq!(model.scope(), Scope::Own);
    assert_eq!(model.notice(), Some(ALL_DENIED_NOTICE));
    assert_eq!(model.rows().len(), 2);
    // Both queries reached the service: the refused global one, then the
    // own-scope fallback.
    assert!(service
        .seen
        .borrow()
        .contains(&SysinfoQueryId::GLOBAL_PROCESS_LIST));
}

#[test]
fn the_next_key_clears_the_notice() {
    let mut service = FakeService::new(records(2));
    service.deny_global = true;
    let mut model = Model::new(Scope::All);
    model.refresh_recovering(&service).expect("recovers");
    assert_eq!(model.notice(), Some(ALL_DENIED_NOTICE));
    model.handle_event(&Event::Down);
    assert_eq!(model.notice(), None);
}

#[test]
fn a_recovering_refresh_propagates_a_service_failure() {
    // Only the *capability refusal of the global view* is recoverable; a
    // transport failure is a real error and must end the session.
    struct FailingService;
    impl Transport for FailingService {
        fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
            Err(Errno::NotFound)
        }
    }
    let mut model = Model::new(Scope::All);
    assert_eq!(
        model.refresh_recovering(&FailingService),
        Err(TopError::Service(Errno::NotFound))
    );
    assert_eq!(model.notice(), None);
}

// ---- Statistics ------------------------------------------------------------

#[test]
fn pct_cpu_is_the_delta_share_between_refreshes() {
    // First sample at t=1s: both tasks have run 0ns. Second sample at t=2s:
    // pid 1 consumed 500ms of the 1s interval (50.0%), pid 2 consumed 100ms
    // (10.0%).
    let service = FakeService::new(vec![record(1, b"busy"), record(2, b"calm")]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("first sample");
    assert!(model.rows().iter().all(|row| row.pct_cpu_tenths == 0));

    service.tick(
        2_000_000_000,
        vec![
            record_with(1, b"busy", 500_000_000, 0),
            record_with(2, b"calm", 100_000_000, 0),
        ],
    );
    model.refresh(&service).expect("second sample");
    let busy = model
        .rows()
        .iter()
        .find(|r| r.record.pid == 1)
        .expect("busy");
    let calm = model
        .rows()
        .iter()
        .find(|r| r.record.pid == 2)
        .expect("calm");
    assert_eq!(busy.pct_cpu_tenths, 500);
    assert_eq!(calm.pct_cpu_tenths, 100);
    // The busiest process sorts to the top, GNU-style.
    assert_eq!(model.rows()[0].record.pid, 1);
}

#[test]
fn wcpu_smooths_across_refreshes_and_decays_when_idle() {
    let service = FakeService::new(vec![record(1, b"burst")]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("first sample");

    // One busy interval: 100% of the 1s window. WCPU averages the previous
    // smoothed value (0) with the new sample (1000 tenths).
    service.tick(
        2_000_000_000,
        vec![record_with(1, b"burst", 1_000_000_000, 0)],
    );
    model.refresh(&service).expect("second sample");
    assert_eq!(model.rows()[0].pct_cpu_tenths, 1000);
    assert_eq!(model.rows()[0].wcpu_tenths, 500);

    // An idle interval: %CPU drops straight to zero, WCPU only halves —
    // the weighted column decays instead of whipsawing.
    service.tick(
        3_000_000_000,
        vec![record_with(1, b"burst", 1_000_000_000, 0)],
    );
    model.refresh(&service).expect("third sample");
    assert_eq!(model.rows()[0].pct_cpu_tenths, 0);
    assert_eq!(model.rows()[0].wcpu_tenths, 250);
}

#[test]
fn a_reused_pid_with_a_new_proc_id_starts_its_statistics_afresh() {
    // The first lifetime of numeric pid 1 accrued 900ms; a new lifetime
    // reusing the number (different proc_id) must not inherit that history
    // as a negative or giant delta — it starts at zero.
    let service = FakeService::new(vec![record_with(1, b"old", 900_000_000, 0)]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("first sample");

    let mut reused = record_with(1, b"new", 50_000_000, 0);
    let mut raw = [0u8; 16];
    raw[0] = 0xEE; // a different, never-before-seen instance identity
    reused.proc_id = ProcId::from_raw(raw);
    service.tick(2_000_000_000, vec![reused]);
    model.refresh(&service).expect("second sample");
    assert_eq!(
        model.rows()[0].pct_cpu_tenths,
        0,
        "no interval observed yet"
    );
    assert_eq!(model.rows()[0].wcpu_tenths, 0);
}

#[test]
fn summary_records_uptime_and_the_denied_memory_query() {
    let mut service = FakeService::new(records(1));
    service.deny_memory = true;
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    assert_eq!(model.summary().uptime_ns, Some(1_000_000_000));
    assert!(model.summary().memory.is_none());
    assert!(model.summary().memory_denied);
    assert!(model.summary().load.is_none());
}

#[test]
fn summary_carries_load_and_memory_when_served() {
    let mut service = FakeService::new(records(1));
    service.load = Some(LoadAverage {
        load1: 1 << 11, // 1.00 fixed-point
        load5: 0,
        load15: 0,
        runnable: 1,
        total_tasks: 3,
        users: 2,
    });
    service.memory = Some(KernelMemoryStats {
        total_bytes: 1024 * 1024 * 1024,
        free_bytes: 512 * 1024 * 1024,
        kernel_heap_bytes: 8 * 1024 * 1024,
        user_resident_bytes: 0,
        page_size: 4096,
        reserved: 0,
    });
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let summary = model.summary();
    assert_eq!(summary.load.map(|l| l.users), Some(2));
    assert_eq!(
        summary.memory.map(|m| m.total_bytes),
        Some(1024 * 1024 * 1024)
    );
    assert!(!summary.memory_denied);
}

/// One per-CPU record with zeroed reserved bits.
fn cpu_time(cpu: u32, busy_ns: u64, idle_ns: u64) -> CpuTimeRecord {
    CpuTimeRecord {
        cpu,
        reserved: 0,
        busy_ns,
        idle_ns,
    }
}

#[test]
fn the_first_cpu_sample_shows_the_since_boot_split() {
    let service = FakeService::new(records(1));
    service.set_cpu_times(vec![cpu_time(0, 750, 250), cpu_time(1, 250, 750)]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    // 1000ns busy of 2000ns total across both CPUs: 50.0% / 50.0%.
    assert_eq!(
        model.cpu_split(),
        Some(CpuSplit {
            busy_tenths: 500,
            idle_tenths: 500,
        })
    );
    assert_eq!(model.summary().cpu.map(|c| c.cpus), Some(2));
}

#[test]
fn the_cpu_split_differences_two_samples() {
    let service = FakeService::new(records(1));
    service.set_cpu_times(vec![cpu_time(0, 1_000, 9_000)]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("first sample");
    // Over the next interval the CPU was busy 900 of 1000 ns: 90.0%.
    service.set_cpu_times(vec![cpu_time(0, 1_900, 9_100)]);
    model.refresh(&service).expect("second sample");
    assert_eq!(
        model.cpu_split(),
        Some(CpuSplit {
            busy_tenths: 900,
            idle_tenths: 100,
        })
    );
}

#[test]
fn an_unanswered_cpu_query_yields_no_split() {
    let service = FakeService::new(records(1));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    assert_eq!(model.summary().cpu, None);
    assert_eq!(model.cpu_split(), None);
}

#[test]
fn user_names_resolve_and_absent_entries_degrade_to_the_uid() {
    let service = FakeService::new(records(1));
    let mut model = Model::new(Scope::Own);
    model.set_user_names(vec![(1000, String::from("alice"))]);
    model.refresh(&service).expect("ok");
    assert_eq!(model.user_name(1000), Some("alice"));
    assert_eq!(model.user_name(1001), None);
}

// ---- Formatting ------------------------------------------------------------

#[test]
fn tenths_format_with_one_decimal_and_saturate() {
    assert_eq!(crate::app::format_tenths(0), "0.0");
    assert_eq!(crate::app::format_tenths(123), "12.3");
    assert_eq!(crate::app::format_tenths(1000), "100.0");
    assert_eq!(crate::app::format_tenths(u32::MAX), "999.9");
}

#[test]
fn sizes_format_by_magnitude() {
    assert_eq!(crate::app::format_size(0), "0");
    assert_eq!(crate::app::format_size(4096), "4K");
    assert_eq!(crate::app::format_size(5 * 1024 * 1024), "5120K");
    assert_eq!(crate::app::format_size(200 * 1024 * 1024), "200.0M");
    assert_eq!(crate::app::format_size(20 * 1024 * 1024 * 1024), "20.0G");
}

#[test]
fn time_plus_formats_minutes_seconds_hundredths() {
    assert_eq!(crate::app::format_time_plus(0), "0:00.00");
    assert_eq!(crate::app::format_time_plus(1_230_000_000), "0:01.23");
    assert_eq!(crate::app::format_time_plus(61_500_000_000), "1:01.50");
    assert_eq!(crate::app::format_time_plus(3_600_000_000_000), "60:00.00");
}

#[test]
fn uptime_formats_hours_minutes_and_days() {
    assert_eq!(crate::app::format_uptime(0), "0:00");
    assert_eq!(crate::app::format_uptime(3 * 60_000_000_000), "0:03");
    assert_eq!(
        crate::app::format_uptime(26 * 3_600_000_000_000),
        "1 day, 2:00"
    );
    assert_eq!(
        crate::app::format_uptime(50 * 3_600_000_000_000),
        "2 days, 2:00"
    );
}

// ---- Rendering -------------------------------------------------------------

#[test]
fn list_capacity_subtracts_header_and_footer() {
    assert_eq!(list_capacity(Size::new(24, 80)), 18);
    // A screen with no room for the list still reports zero, never underflows.
    assert_eq!(list_capacity(Size::new(2, 80)), 0);
}

#[test]
fn render_draws_the_summary_header_and_rows() {
    let service = FakeService::new(vec![record_with(1, b"init", 0, 4096), record(2, b"shell")]);
    let mut model = Model::new(Scope::Own);
    model.set_user_names(vec![(1000, String::from("alice"))]);
    model.refresh(&service).expect("ok");
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(12, 80),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");

    let out = screen.into_tty().output;
    assert!(contains(&out, b"top"));
    assert!(contains(&out, b"Tasks:"));
    assert!(contains(&out, b"%Cpu(s):"));
    assert!(contains(&out, b"Mem"));
    assert!(contains(&out, b"PID"));
    assert!(contains(&out, b"USER"));
    assert!(contains(&out, b"SIZE"));
    assert!(contains(&out, b"%CPU"));
    assert!(contains(&out, b"WCPU"));
    assert!(contains(&out, b"TIME+"));
    assert!(contains(&out, b"COMMAND"));
    assert!(contains(&out, b"init"));
    assert!(contains(&out, b"shell"));
    assert!(contains(&out, b"alice"));
}

#[test]
fn an_unavailable_cpu_split_renders_its_absence() {
    let service = FakeService::new(records(1));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(12, 80),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");
    let out = screen.into_tty().output;
    assert!(contains(&out, b"unavailable"));
}

#[test]
fn a_served_cpu_split_renders_busy_and_idle() {
    let service = FakeService::new(records(1));
    service.set_cpu_times(vec![cpu_time(0, 123, 877)]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(12, 80),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");
    let out = screen.into_tty().output;
    assert!(contains(&out, b"12.3"));
    assert!(contains(&out, b"busy,"));
    assert!(contains(&out, b"87.7"));
    assert!(contains(&out, b"idle"));
}

#[test]
fn a_denied_memory_query_renders_the_refusal() {
    let mut service = FakeService::new(records(1));
    service.deny_memory = true;
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(12, 80),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");
    let out = screen.into_tty().output;
    assert!(contains(&out, b"CAP_SYSINFO_KERNEL"));
}

#[test]
fn state_letters_are_coloured_on_a_colour_terminal_only() {
    let mut colour = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(12, 80),
    );
    let running = crate::app::state_attributes(&mut colour, 'R').expect("coloured");
    assert!(running.bold, "a running task is emphasised");
    assert!(crate::app::state_attributes(&mut colour, 'Z').is_some());
    assert!(crate::app::state_attributes(&mut colour, 'T').is_some());
    assert!(crate::app::state_attributes(&mut colour, 'r').is_some());
    // A sleeping task stays plain even in colour.
    assert!(crate::app::state_attributes(&mut colour, 'S').is_none());

    // A monochrome terminal never colours: the letter itself carries the
    // information.
    let mut mono = Screen::new(FakeTty::with_input(b""), TermType::Dumb, Size::new(12, 80));
    assert!(crate::app::state_attributes(&mut mono, 'R').is_none());
}

#[test]
fn process_rows_carry_the_new_columns() {
    let service = FakeService::new(vec![record_with(7, b"worker", 61_500_000_000, 4096)]);
    let mut model = Model::new(Scope::Own);
    model.set_user_names(vec![(1000, String::from("alice"))]);
    model.refresh(&service).expect("ok");
    let line = crate::app::process_row(&model, &model.rows()[0]);
    assert!(line.contains("      7"));
    assert!(line.contains("alice"));
    assert!(line.contains("4K"));
    assert!(line.contains(" R "));
    assert!(line.contains("1:01.50"));
    assert!(line.ends_with("worker"));
}

#[test]
fn an_unmapped_uid_renders_numerically() {
    let service = FakeService::new(records(1));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let line = crate::app::process_row(&model, &model.rows()[0]);
    assert!(line.contains("1000"), "numeric uid stands in: {line}");
}

#[test]
fn render_shows_the_help_overlay_when_toggled() {
    let service = FakeService::new(records(3));
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    model.handle_event(&Event::Char('?'));
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(16, 60),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");
    let out = screen.into_tty().output;
    // The renderer skips unchanged blank cells, so only contiguous
    // non-space runs are guaranteed to appear verbatim in the byte stream.
    assert!(contains(&out, b"Keys"));
    assert!(contains(&out, b"refresh"));
}

#[test]
fn a_wide_process_name_does_not_break_rendering() {
    // A double-width (CJK) process name must render without panicking and the
    // narrow column header must still be present.
    let service = FakeService::new(vec![record(1, "世界".as_bytes())]);
    let mut model = Model::new(Scope::Own);
    model.refresh(&service).expect("ok");
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(8, 60),
    );
    model.set_viewport(list_capacity(screen.size()));
    render(&model, &mut screen).expect("render ok");
    let out = screen.into_tty().output;
    assert!(contains(&out, "世界".as_bytes()));
}

// ---- The run loop ----------------------------------------------------------

#[test]
fn run_quits_on_q_after_refreshing() {
    let service = FakeService::new(vec![record(1, b"init")]);
    let mut model = Model::new(Scope::Own);
    let mut screen = Screen::new(
        FakeTty::with_input(b"q"),
        TermType::Xterm256Color,
        Size::new(10, 60),
    );
    assert_eq!(run(&mut model, &service, &mut screen), Ok(()));
    // It refreshed before drawing, so the snapshot is populated.
    assert_eq!(model.rows().len(), 1);
    let out = screen.into_tty().output;
    assert!(contains(&out, b"init"));
}

#[test]
fn run_auto_refreshes_when_the_input_wait_elapses() {
    // A read that returns nothing is the elapsed refresh delay: the loop
    // re-queries the service (never quits, never spins in the test — the
    // real wait is the kernel-bounded read) and keeps going until 'q'.
    let service = FakeService::new(records(3));
    let mut model = Model::new(Scope::Own);
    let mut screen = Screen::new(
        FakeTty::with_chunks(&[b"\x1b[B", b"", b"q"]),
        TermType::Xterm256Color,
        Size::new(10, 60),
    );
    assert_eq!(run(&mut model, &service, &mut screen), Ok(()));
    // The down-arrow moved the selection, and it survived the auto-refresh.
    assert_eq!(model.selected(), Some(1));
    // The elapsed wait re-queried the process list: once up front, once for
    // the tick.
    let listings = service
        .seen
        .borrow()
        .iter()
        .filter(|&&q| q == SysinfoQueryId::SELF_PROCESS_LIST)
        .count();
    assert_eq!(listings, 2);
}

#[test]
fn run_toggling_to_a_denied_global_view_recovers_and_shows_why() {
    let mut service = FakeService::new(records(2));
    service.deny_global = true;
    let mut model = Model::new(Scope::Own);
    // 'a' toggles to the system-wide scope and triggers a refresh, which the
    // service denies. That is not fatal: the viewer falls back to the own
    // view, keeps running (here until the 'q' that follows), and the redraw
    // carries the reason on the status line (a 100-column screen fits the
    // whole title; a narrower one truncates it like any other line).
    let mut screen = Screen::new(
        FakeTty::with_input(b"aq"),
        TermType::Xterm256Color,
        Size::new(10, 100),
    );
    assert_eq!(run(&mut model, &service, &mut screen), Ok(()));
    assert_eq!(model.scope(), Scope::Own);
    // The 'q' that ended the session cleared the notice (any handled key
    // does), so the proof it was shown is the rendered frame between the
    // two keys. The diff renderer skips cells unchanged since the previous
    // frame (the spaces between words), so only the notice's contiguous
    // non-space runs are guaranteed to appear verbatim in the byte stream.
    let out = screen.into_tty().output;
    assert!(contains(&out, b"denied:"));
    assert!(contains(&out, b"capability"));
    assert!(contains(&out, b"held"));
}
