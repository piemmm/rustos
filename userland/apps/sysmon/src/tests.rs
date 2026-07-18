//! Behaviour tests for the `sysmon` monitor: the model's focus/scroll/
//! interval/degradation logic and the renderer/loop driven over in-memory
//! `sysinfo` and tty channels.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::sysinfo::{
    CpuLoadRecord, CpuLoadRequest, CpuTimeListRequest, CpuTimeRecord, IrqListRequest, IrqRecord,
    KernelMemoryStats, LoadAverage, MemoryPressureStats, ProcessListRequest, ProcessRecord,
    ProcessState, RamzipStats, ReclaimClassRecord, ReclaimListRequest, SysinfoQueryId,
    SysinfoRequestHeader, Uptime, IRQ_FLAG_QUARANTINED, RECLAIM_CLASS_COUNT,
};
use tairix_abi::{Duration64, Errno, ProcId};
use tairix_curses::{Event, Screen, Size, Tty};
use tairix_procinfo::Transport;
use tairix_termcap::TermType;

use crate::app::{detail_capacity, render, run};
use crate::command::{parse, Command, DEFAULT_DELAY_TENTHS, MAX_DELAY_TENTHS, MIN_DELAY_TENTHS};
use crate::error::SysmonError;
use crate::model::{Action, Focus, Gauge, Model, PinState};

// ---- Fixtures --------------------------------------------------------------

/// An in-memory `sysinfod` stand-in answering every query the monitor
/// issues from fixed data, decoding each request exactly as the real
/// service; each gated query can be independently denied or failed.
struct FakeService {
    uptime_ns: RefCell<Option<u64>>,
    load: Option<LoadAverage>,
    memory: Option<KernelMemoryStats>,
    pressure: RefCell<Option<MemoryPressureStats>>,
    ramzip: Option<RamzipStats>,
    reclaim: Option<Vec<ReclaimClassRecord>>,
    cpu_times: RefCell<Option<Vec<CpuTimeRecord>>>,
    cpu_loads: Option<Vec<CpuLoadRecord>>,
    irqs: Option<Vec<IrqRecord>>,
    processes: RefCell<Vec<ProcessRecord>>,
    deny: RefCell<Vec<SysinfoQueryId>>,
}

impl FakeService {
    /// A service serving a full, healthy snapshot.
    fn healthy() -> Self {
        Self {
            uptime_ns: RefCell::new(Some(1_000_000_000)),
            load: Some(LoadAverage {
                load1: 1 << tairix_abi::sysinfo::LOAD_FIXED_SHIFT,
                ..LoadAverage::default()
            }),
            memory: Some(KernelMemoryStats {
                total_bytes: 1024 * 1024 * 1024,
                free_bytes: 512 * 1024 * 1024,
                kernel_heap_bytes: 64 * 1024 * 1024,
                user_resident_bytes: 128 * 1024 * 1024,
                page_size: 4096,
                reserved: 0,
            }),
            pressure: RefCell::new(Some(MemoryPressureStats {
                band: 2,
                total_bytes: 1024 * 1024 * 1024,
                free_bytes: 512 * 1024 * 1024,
                reserve_bytes: 32 * 1024 * 1024,
                band_entries: [4, 3, 2, 1, 0],
                ..MemoryPressureStats::default()
            })),
            ramzip: Some(RamzipStats {
                stored_bytes: 4096,
                logical_bytes: 16384,
                pinned_bytes: 8 * 1024 * 1024,
                fault_ins: 7,
                ..RamzipStats::default()
            }),
            reclaim: Some(
                (0..RECLAIM_CLASS_COUNT)
                    .map(|class| ReclaimClassRecord {
                        class: u8::try_from(class).unwrap_or(0),
                        payload_bytes: 1024 * (class as u64 + 1),
                        entries: class as u64,
                        ..ReclaimClassRecord::default()
                    })
                    .collect(),
            ),
            cpu_times: RefCell::new(Some(vec![cpu_time(0, 750, 250), cpu_time(1, 100, 900)])),
            cpu_loads: Some(vec![
                CpuLoadRecord {
                    cpu: 0,
                    reserved: 0,
                    queue_depth: 3,
                    switches: 100,
                    preemptions: 7,
                },
                CpuLoadRecord {
                    cpu: 1,
                    reserved: 0,
                    queue_depth: 0,
                    switches: 50,
                    preemptions: 2,
                },
            ]),
            irqs: Some(vec![
                IrqRecord {
                    line: 27,
                    flags: 0,
                    owner: 14,
                    count: 1234,
                },
                IrqRecord {
                    line: 111,
                    flags: IRQ_FLAG_QUARANTINED,
                    owner: 13,
                    count: 200_000,
                },
            ]),
            processes: RefCell::new(vec![
                record_with(1, b"init", 100, 4096),
                record_with(2, b"shell", 900, 1024 * 1024),
            ]),
            deny: RefCell::new(Vec::new()),
        }
    }

    /// Deny one query with `PermissionDenied`.
    fn deny(&self, query: SysinfoQueryId) {
        self.deny.borrow_mut().push(query);
    }

    /// Advance the served monotonic clock and replace the process set.
    fn tick(&self, now_ns: u64, records: Vec<ProcessRecord>) {
        *self.uptime_ns.borrow_mut() = Some(now_ns);
        *self.processes.borrow_mut() = records;
    }

    /// Replace the served per-CPU time records.
    fn set_cpu_times_for_test(&self, records: Vec<CpuTimeRecord>) {
        *self.cpu_times.borrow_mut() = Some(records);
    }
}

/// Encode `records[offset..offset+limit]` as concatenated wire records.
fn page_bytes<T>(
    records: &[T],
    offset: u32,
    limit: u16,
    encode: impl Fn(&T) -> Vec<u8>,
) -> Vec<u8> {
    let offset = offset as usize;
    if offset >= records.len() {
        return Vec::new();
    }
    let take = core::cmp::min(records.len() - offset, limit as usize);
    let mut out = Vec::new();
    for record in &records[offset..offset + take] {
        out.extend_from_slice(&encode(record));
    }
    out
}

impl Transport for FakeService {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        let header = SysinfoRequestHeader::from_bytes(request)?;
        if self.deny.borrow().contains(&header.query) {
            return Err(Errno::PermissionDenied);
        }
        let payload = &request[SysinfoRequestHeader::WIRE_LEN
            ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
        match header.query {
            SysinfoQueryId::UPTIME => {
                let Some(ns) = *self.uptime_ns.borrow() else {
                    return Err(Errno::NotFound);
                };
                let uptime = Uptime {
                    since_boot: Duration64::from_nanos(ns),
                    ..Uptime::default()
                };
                Ok(uptime.to_le_bytes().to_vec())
            }
            SysinfoQueryId::LOAD_AVERAGE => match &self.load {
                Some(load) => Ok(load.to_le_bytes().to_vec()),
                None => Err(Errno::NotFound),
            },
            SysinfoQueryId::KERNEL_MEMORY_STATS => match &self.memory {
                Some(memory) => Ok(memory.to_le_bytes().to_vec()),
                None => Err(Errno::NotFound),
            },
            SysinfoQueryId::MEMORY_PRESSURE => match &*self.pressure.borrow() {
                Some(pressure) => Ok(pressure.to_le_bytes().to_vec()),
                None => Err(Errno::NotFound),
            },
            SysinfoQueryId::RAMZIP_STATS => match &self.ramzip {
                Some(stats) => Ok(stats.to_le_bytes().to_vec()),
                None => Err(Errno::NotFound),
            },
            SysinfoQueryId::RECLAIM_STATS => {
                let Some(records) = &self.reclaim else {
                    return Err(Errno::NotFound);
                };
                let req = ReclaimListRequest::from_bytes(payload)?;
                Ok(page_bytes(records, req.offset, req.limit, |r| {
                    r.to_le_bytes().to_vec()
                }))
            }
            SysinfoQueryId::CPU_LOAD => {
                let Some(records) = &self.cpu_loads else {
                    return Err(Errno::NotFound);
                };
                let req = CpuLoadRequest::from_bytes(payload)?;
                Ok(page_bytes(records, req.offset, req.limit, |r| {
                    r.to_le_bytes().to_vec()
                }))
            }
            SysinfoQueryId::CPU_TIME_STATS => {
                let cpu_times = self.cpu_times.borrow();
                let Some(records) = cpu_times.as_ref() else {
                    return Err(Errno::NotFound);
                };
                let req = CpuTimeListRequest::from_bytes(payload)?;
                Ok(page_bytes(records, req.offset, req.limit, |r| {
                    r.to_le_bytes().to_vec()
                }))
            }
            SysinfoQueryId::IRQ_LIST => {
                let Some(records) = &self.irqs else {
                    return Err(Errno::NotFound);
                };
                let req = IrqListRequest::from_bytes(payload)?;
                Ok(page_bytes(records, req.offset, req.limit, |r| {
                    r.to_le_bytes().to_vec()
                }))
            }
            SysinfoQueryId::GLOBAL_PROCESS_LIST | SysinfoQueryId::SELF_PROCESS_LIST => {
                let req = ProcessListRequest::from_bytes(payload)?;
                let records = self.processes.borrow();
                Ok(page_bytes(&records, req.offset, req.limit, |r| {
                    r.to_le_bytes().to_vec()
                }))
            }
            _ => Err(Errno::NotFound),
        }
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
    /// returned nothing (the elapsed refresh interval).
    fn with_chunks(chunks: &[&[u8]]) -> Self {
        Self {
            chunks: chunks.iter().rev().map(|c| c.to_vec()).collect(),
            output: Vec::new(),
        }
    }
}

impl Tty for FakeTty {
    fn write(&mut self, bytes: &[u8]) -> tairix_curses::Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self) -> tairix_curses::Result<Vec<u8>> {
        Ok(self.chunks.pop().unwrap_or_default())
    }
}

/// A per-CPU time record.
fn cpu_time(cpu: u32, busy_ns: u64, idle_ns: u64) -> CpuTimeRecord {
    CpuTimeRecord {
        cpu,
        reserved: 0,
        busy_ns,
        idle_ns,
    }
}

/// A process record whose `proc_id` is derived from the pid, mirroring the
/// kernel's never-reused ids for cross-refresh correlation.
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

/// Whether `haystack` contains the byte run `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A refreshed model over a healthy service.
fn refreshed() -> (FakeService, Model) {
    let service = FakeService::healthy();
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    (service, model)
}

// ---- Command line ------------------------------------------------------

#[test]
fn no_arguments_runs_at_the_default_interval() {
    assert_eq!(
        parse(&[]),
        Ok(Command::Run {
            delay_tenths: DEFAULT_DELAY_TENTHS
        })
    );
}

#[test]
fn help_flags_win() {
    assert_eq!(parse(&["-h"]), Ok(Command::Help));
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
    assert_eq!(parse(&["-d", "1", "-h"]), Ok(Command::Help));
}

#[test]
fn delay_accepts_the_gnu_spellings_and_clamps_zero() {
    for args in [
        &["-d", "5"][..],
        &["-d5"][..],
        &["--delay", "5"][..],
        &["--delay=5"][..],
    ] {
        assert_eq!(parse(args), Ok(Command::Run { delay_tenths: 50 }));
    }
    assert_eq!(
        parse(&["-d", "0"]),
        Ok(Command::Run {
            delay_tenths: MIN_DELAY_TENTHS
        })
    );
}

#[test]
fn malformed_delays_and_operands_are_usage_errors() {
    for args in [
        &["-d"][..],
        &["-d", "abc"][..],
        &["-d", "1.2.3"][..],
        &["-x"][..],
        &["operand"][..],
    ] {
        assert_eq!(parse(args), Err(SysmonError::Usage));
    }
}

/// Every locale's `OPTIONS` section documents exactly the switches this
/// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
/// language-neutral, so each translated document must carry the same keys
/// as the canonical one. The documents are read from the bundle's own
/// on-disk `Help/` tree — the single source the image builder plants —
/// never a copy embedded in this crate.
#[test]
fn help_documents_the_parser_switches() {
    extern crate std;
    use alloc::format;
    use std::fs;

    let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    for locale in tairix_help::REQUIRED_LOCALES {
        let path = format!("{help_root}/{locale}/sysmon.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for switch in ["`-d, --delay <seconds>`", "`-h, -?`"] {
            assert!(
                text.contains(switch),
                "{locale}/sysmon.md must document {switch}"
            );
        }
    }
}

// ---- Model: sampling and degradation ------------------------------------

#[test]
fn a_healthy_refresh_populates_every_panel() {
    let (_service, model) = refreshed();
    let snapshot = model.snapshot();
    assert!(snapshot.uptime_ns.is_some());
    assert!(snapshot.load.is_some());
    assert!(snapshot.memory.ready().is_some());
    assert!(snapshot.pressure.ready().is_some());
    assert_eq!(
        snapshot.reclaim.ready().map(Vec::len),
        Some(RECLAIM_CLASS_COUNT)
    );
    assert!(snapshot.ramzip.ready().is_some());
    assert_eq!(snapshot.cpu_loads.ready().map(Vec::len), Some(2));
    assert_eq!(snapshot.irqs.ready().map(Vec::len), Some(2));
    assert_eq!(snapshot.processes.len(), 2);
    assert!(!snapshot.global_denied);
    assert_eq!(model.band_history(), &[2]);
}

#[test]
fn each_denied_query_records_the_refusal_and_the_rest_still_serve() {
    let service = FakeService::healthy();
    service.deny(SysinfoQueryId::KERNEL_MEMORY_STATS);
    service.deny(SysinfoQueryId::MEMORY_PRESSURE);
    service.deny(SysinfoQueryId::RECLAIM_STATS);
    service.deny(SysinfoQueryId::RAMZIP_STATS);
    service.deny(SysinfoQueryId::CPU_LOAD);
    service.deny(SysinfoQueryId::IRQ_LIST);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let snapshot = model.snapshot();
    assert!(snapshot.memory.is_denied());
    assert!(snapshot.pressure.is_denied());
    assert!(snapshot.reclaim.is_denied());
    assert!(snapshot.ramzip.is_denied());
    assert!(snapshot.cpu_loads.is_denied());
    assert!(snapshot.irqs.is_denied());
    // The ungated figures still serve; the session is alive.
    assert!(snapshot.uptime_ns.is_some());
    assert!(!model.cpu_busy().is_empty());
    assert_eq!(snapshot.processes.len(), 2);
    // No pressure sample was recorded for the strip.
    assert!(model.band_history().is_empty());
}

#[test]
fn a_failed_query_is_unavailable_not_denied() {
    let mut service = FakeService::healthy();
    service.ramzip = None;
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    assert_eq!(model.snapshot().ramzip, Gauge::Unavailable);
}

#[test]
fn a_denied_global_census_falls_back_to_own_and_records_it() {
    let service = FakeService::healthy();
    service.deny(SysinfoQueryId::GLOBAL_PROCESS_LIST);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    assert!(model.snapshot().global_denied);
    assert_eq!(model.snapshot().processes.len(), 2);
}

#[test]
fn the_band_history_tracks_refreshes_and_stays_bounded() {
    let (service, mut model) = refreshed();
    for _ in 0..(crate::model::BAND_HISTORY_MAX + 5) {
        model.refresh(&service);
    }
    assert_eq!(model.band_history().len(), crate::model::BAND_HISTORY_MAX);
    assert!(model.band_history().iter().all(|&band| band == 2));
}

#[test]
fn cpu_busy_differences_two_samples() {
    let (service, mut model) = refreshed();
    // First sample: the cumulative ratio (750/1000 = 75.0%).
    assert_eq!(model.cpu_busy()[0].busy_tenths, 750);
    // Second sample: CPU 0 gained 100 busy / 0 idle => 100%.
    service.set_cpu_times_for_test(vec![cpu_time(0, 850, 250), cpu_time(1, 100, 1900)]);
    model.refresh(&service);
    assert_eq!(model.cpu_busy()[0].busy_tenths, 1000);
    assert_eq!(model.cpu_busy()[1].busy_tenths, 0);
}

#[test]
fn proc_pct_is_the_delta_share_between_refreshes() {
    let (service, mut model) = refreshed();
    // One second passes; pid 2 consumes half of it.
    service.tick(
        2_000_000_000,
        vec![
            record_with(1, b"init", 100, 4096),
            record_with(2, b"shell", 900 + 500_000_000, 1024 * 1024),
        ],
    );
    model.refresh(&service);
    let shell = model.snapshot().processes[1];
    assert_eq!(model.proc_pct(shell.proc_id), Some(500));
}

// ---- Model: keys ---------------------------------------------------------

#[test]
fn quit_refresh_and_unmapped_keys_report_their_actions() {
    let (_service, mut model) = refreshed();
    assert_eq!(model.handle_event(&Event::Char('q')), Action::Quit);
    assert_eq!(model.handle_event(&Event::Char('r')), Action::Refresh);
    assert_eq!(model.handle_event(&Event::Char('z')), Action::Ignore);
}

#[test]
fn the_panel_key_cycles_every_panel_and_resets_scroll() {
    let (_service, mut model) = refreshed();
    assert_eq!(model.focus(), Focus::Reclaim);
    model.set_viewport(2);
    assert_eq!(model.handle_event(&Event::Down), Action::Redraw);
    assert_eq!(model.scroll(), 1);
    let mut seen = vec![model.focus()];
    for _ in 0..4 {
        assert_eq!(model.handle_event(&Event::Char('p')), Action::Redraw);
        seen.push(model.focus());
    }
    assert_eq!(
        seen,
        vec![
            Focus::Reclaim,
            Focus::Ramzip,
            Focus::Cpu,
            Focus::Irqs,
            Focus::Processes
        ]
    );
    // Scroll was reset by the first cycle.
    assert_eq!(model.scroll(), 0);
    // The cycle wraps.
    assert_eq!(model.handle_event(&Event::Char('p')), Action::Redraw);
    assert_eq!(model.focus(), Focus::Reclaim);
}

#[test]
fn the_interval_keys_step_within_the_bounds() {
    let (_service, mut model) = refreshed();
    assert_eq!(model.delay_tenths(), DEFAULT_DELAY_TENTHS);
    assert_eq!(model.handle_event(&Event::Char('+')), Action::Redraw);
    assert_eq!(model.delay_tenths(), DEFAULT_DELAY_TENTHS + 10);
    assert_eq!(model.handle_event(&Event::Char('-')), Action::Redraw);
    assert_eq!(model.delay_tenths(), DEFAULT_DELAY_TENTHS);
    // The lower bound clamps and reports no change once reached.
    for _ in 0..100 {
        let _ = model.handle_event(&Event::Char('-'));
    }
    assert_eq!(model.delay_tenths(), MIN_DELAY_TENTHS);
    assert_eq!(model.handle_event(&Event::Char('-')), Action::Ignore);
    // The upper bound clamps too.
    for _ in 0..100 {
        let _ = model.handle_event(&Event::Char('+'));
    }
    assert_eq!(model.delay_tenths(), MAX_DELAY_TENTHS);
    assert_eq!(model.handle_event(&Event::Char('+')), Action::Ignore);
}

#[test]
fn scrolling_clamps_at_zero_and_against_the_panel_length() {
    let (_service, mut model) = refreshed();
    model.set_viewport(3);
    assert_eq!(model.handle_event(&Event::Up), Action::Ignore);
    assert_eq!(model.handle_event(&Event::Down), Action::Redraw);
    assert_eq!(model.handle_event(&Event::PageDown), Action::Redraw);
    assert_eq!(model.scroll(), 4);
    // Clamping against the panel's length only caps the offset; an
    // in-range offset is untouched.
    model.clamp_scroll(RECLAIM_CLASS_COUNT + 1);
    assert_eq!(model.scroll(), 4);
    assert_eq!(model.handle_event(&Event::Home), Action::Redraw);
    assert_eq!(model.scroll(), 0);
    assert_eq!(model.handle_event(&Event::End), Action::Redraw);
    model.clamp_scroll(RECLAIM_CLASS_COUNT + 1);
    assert_eq!(model.scroll(), RECLAIM_CLASS_COUNT + 1 - 3);
}

#[test]
fn the_help_key_toggles_the_overlay() {
    let (_service, mut model) = refreshed();
    assert!(!model.help_visible());
    assert_eq!(model.handle_event(&Event::Char('?')), Action::Redraw);
    assert!(model.help_visible());
    assert_eq!(model.handle_event(&Event::Char('h')), Action::Redraw);
    assert!(!model.help_visible());
}

#[test]
fn the_pin_state_defaults_honest_and_records_the_outcome() {
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    assert_eq!(model.pin(), PinState::Unpinned("not attempted"));
    model.set_pin_state(PinState::Pinned);
    assert_eq!(model.pin(), PinState::Pinned);
    model.set_pin_state(PinState::Unpinned("CAP_MEM_PIN not held"));
    assert_eq!(model.pin(), PinState::Unpinned("CAP_MEM_PIN not held"));
}

#[test]
fn a_delay_outside_the_session_bounds_is_clamped_at_construction() {
    assert_eq!(Model::new(0).delay_tenths(), MIN_DELAY_TENTHS);
    assert_eq!(Model::new(10_000).delay_tenths(), MAX_DELAY_TENTHS);
}

// ---- Rendering -----------------------------------------------------------

/// A screen over a captured in-memory tty.
fn screen_with(tty: FakeTty, rows: u16, cols: u16) -> Screen<FakeTty> {
    Screen::new(tty, TermType::Xterm256Color, Size::new(rows, cols))
}

/// Render `model` and return the emitted bytes.
fn rendered(model: &mut Model) -> Vec<u8> {
    let mut screen = screen_with(FakeTty::with_input(b""), 24, 80);
    render(model, &mut screen).expect("render");
    screen.into_tty().output
}

#[test]
fn render_draws_the_summary_and_the_reclaim_panel() {
    let (_service, mut model) = refreshed();
    model.set_pin_state(PinState::Pinned);
    let out = rendered(&mut model);
    // The styled title row is emitted contiguously; plain rows are diffed
    // cell-by-cell (unchanged blanks skipped), so their assertions bind to
    // single tokens.
    assert!(contains(&out, b"sysmon - up"));
    assert!(contains(&out, b"[pinned]"));
    assert!(contains(&out, b"MiB"));
    assert!(contains(&out, b"kernel,"));
    assert!(contains(&out, b"Pressure:"));
    assert!(contains(&out, b"moderate"));
    assert!(contains(&out, b"reclaimable caches"));
    assert!(contains(&out, b"disposable-ui"));
    assert!(contains(&out, b"Tasks"));
    assert!(contains(&out, b"total,"));
}

#[test]
fn a_denied_kernel_query_renders_the_refusal_and_the_session_continues() {
    let service = FakeService::healthy();
    service.deny(SysinfoQueryId::KERNEL_MEMORY_STATS);
    service.deny(SysinfoQueryId::MEMORY_PRESSURE);
    service.deny(SysinfoQueryId::RECLAIM_STATS);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let out = rendered(&mut model);
    assert!(contains(&out, b"CAP_SYSINFO_KERNEL)"));
    // The ungated census still renders.
    assert!(contains(&out, b"Tasks"));
    assert!(contains(&out, b"total,"));
}

#[test]
fn a_refused_pin_renders_its_stated_reason() {
    let (_service, mut model) = refreshed();
    model.set_pin_state(PinState::Unpinned("CAP_MEM_PIN not held"));
    let out = rendered(&mut model);
    // The 80-column title row may truncate the tail of the reason; the
    // stated capability is the load-bearing token.
    assert!(contains(&out, b"[unpinned: CAP_MEM_PIN"));
}

#[test]
fn each_panel_renders_its_detail_lines() {
    let (_service, mut model) = refreshed();
    let _ = model.handle_event(&Event::Char('p'));
    let out = rendered(&mut model);
    assert!(contains(&out, b"compressed tier (ramzip)"));
    assert!(contains(&out, b"stored"));
    assert!(contains(&out, b"16K"));
    let _ = model.handle_event(&Event::Char('p'));
    let out = rendered(&mut model);
    assert!(contains(&out, b"per-cpu load"));
    assert!(contains(&out, b"switches"));
    let _ = model.handle_event(&Event::Char('p'));
    let out = rendered(&mut model);
    assert!(contains(&out, b"interrupt lines"));
    // The quarantined line 111 renders its owner, count, and state.
    assert!(contains(&out, b"111"));
    assert!(contains(&out, b"quarantined"));
    let _ = model.handle_event(&Event::Char('p'));
    let out = rendered(&mut model);
    assert!(contains(&out, b"processes"));
    assert!(contains(&out, b"%cpu"));
    assert!(contains(&out, b"shell"));
}

#[test]
fn a_denied_global_census_renders_the_fallback_reason() {
    let service = FakeService::healthy();
    service.deny(SysinfoQueryId::GLOBAL_PROCESS_LIST);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    // The tasks line carries the short scope marker on every panel.
    let out = rendered(&mut model);
    assert!(contains(&out, b"(own)"));
    // The processes panel opens with the full stated refusal.
    for _ in 0..4 {
        let _ = model.handle_event(&Event::Char('p'));
    }
    let out = rendered(&mut model);
    assert!(contains(&out, b"CAP_SYSINFO_GLOBAL)"));
}

#[test]
fn render_shows_the_help_overlay_when_toggled() {
    let (_service, mut model) = refreshed();
    let _ = model.handle_event(&Event::Char('?'));
    let out = rendered(&mut model);
    assert!(contains(&out, b"lengthen"));
    assert!(contains(&out, b"shorten"));
}

#[test]
fn detail_capacity_subtracts_header_and_footer() {
    assert_eq!(detail_capacity(Size::new(24, 80)), 16);
    assert_eq!(detail_capacity(Size::new(8, 80)), 0);
    assert_eq!(detail_capacity(Size::new(50, 132)), 42);
}

#[test]
fn the_history_strip_renders_band_glyphs() {
    let (service, mut model) = refreshed();
    model.refresh(&service);
    let out = rendered(&mut model);
    // Two refreshes in the moderate band: two adjacent `=` glyphs.
    assert!(contains(&out, b"=="));
}

// ---- The loop --------------------------------------------------------------

#[test]
fn run_quits_on_q_after_refreshing() {
    let service = FakeService::healthy();
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    let mut screen = screen_with(FakeTty::with_input(b"q"), 24, 80);
    run(&mut model, &service, &mut screen).expect("run");
    let out = screen.into_tty().output;
    assert!(contains(&out, b"sysmon - up"));
    assert!(!model.snapshot().processes.is_empty());
}

#[test]
fn run_auto_refreshes_when_the_input_wait_elapses() {
    let service = FakeService::healthy();
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    // First read: nothing (the elapsed interval). Second read: quit. The
    // elapsed wait must have re-queried: the served clock advanced between
    // the reads, so the second render shows the moved band history.
    let mut screen = screen_with(FakeTty::with_chunks(&[b"", b"q"]), 24, 80);
    run(&mut model, &service, &mut screen).expect("run");
    // Two pressure samples were recorded: the up-front refresh plus the
    // elapsed-wait one.
    assert_eq!(model.band_history().len(), 2);
}

#[test]
fn run_survives_a_service_that_refuses_everything() {
    let service = FakeService::healthy();
    for query in [
        SysinfoQueryId::KERNEL_MEMORY_STATS,
        SysinfoQueryId::MEMORY_PRESSURE,
        SysinfoQueryId::RECLAIM_STATS,
        SysinfoQueryId::RAMZIP_STATS,
        SysinfoQueryId::CPU_LOAD,
        SysinfoQueryId::GLOBAL_PROCESS_LIST,
        SysinfoQueryId::SELF_PROCESS_LIST,
        SysinfoQueryId::UPTIME,
        SysinfoQueryId::LOAD_AVERAGE,
        SysinfoQueryId::CPU_TIME_STATS,
        SysinfoQueryId::IRQ_LIST,
    ] {
        service.deny(query);
    }
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    let mut screen = screen_with(FakeTty::with_input(b"q"), 24, 80);
    run(&mut model, &service, &mut screen).expect("run");
    let out = screen.into_tty().output;
    // Every gated panel states its refusal; nothing dies.
    assert!(contains(&out, b"CAP_SYSINFO_KERNEL)"));
}
