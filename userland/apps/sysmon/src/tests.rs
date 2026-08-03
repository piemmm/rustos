//! Behaviour tests for the `sysmon` monitor: the model's focus/scroll/
//! interval/degradation logic and the renderer/loop driven over in-memory
//! `sysinfo` and tty channels.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
use tairix_abi::sysinfo::{
    fold_cache_ledgers, CacheLedgerListRequest, CacheLedgerOrigin, CacheLedgerRecord,
    CacheOwnerKind, CpuLoadRecord, CpuLoadRequest, CpuTimeListRequest, CpuTimeRecord,
    IrqListRequest, IrqRecord, KernelMemoryStats, LoadAverage, MemoryPressureStats,
    MountAvailability, MountListRequest, MountRecord, MountVolumeState, ProcessListRequest,
    ProcessRecord, ProcessState, RamzipStats, ReclaimClassRecord, ReclaimListRequest,
    SysinfoQueryId, SysinfoRequestHeader, Uptime, IRQ_FLAG_QUARANTINED, RECLAIM_CLASS_COUNT,
};
use tairix_abi::{Duration64, Errno, ProcId, SchedPriority};
use tairix_curses::{Event, Screen, Size, Tty};
use tairix_procinfo::Transport;
use tairix_termcap::TermType;

use crate::app::{detail_capacity, elide_middle, render, run};
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
    caches: Option<Vec<CacheLedgerRecord>>,
    cpu_times: RefCell<Option<Vec<CpuTimeRecord>>>,
    cpu_loads: Option<Vec<CpuLoadRecord>>,
    irqs: Option<Vec<IrqRecord>>,
    mounts: Option<Vec<MountRecord>>,
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
            // The class totals are the fold of the cache rows below, as the
            // real service guarantees: a double that let the two views
            // disagree could hide exactly the arithmetic the panel shows.
            reclaim: Some(fold_cache_ledgers(&healthy_caches()).to_vec()),
            caches: Some(healthy_caches()),
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
            mounts: Some(healthy_mounts()),
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
            SysinfoQueryId::CACHE_LEDGERS => {
                let Some(records) = &self.caches else {
                    return Err(Errno::NotFound);
                };
                let req = CacheLedgerListRequest::from_bytes(payload)?;
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
            SysinfoQueryId::MOUNT_LIST => {
                let Some(records) = &self.mounts else {
                    return Err(Errno::NotFound);
                };
                let req = MountListRequest::from_bytes(payload)?;
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
        SchedPriority::Normal,
        cpu_time_ns,
        mem_bytes,
        name,
    )
    .expect("record")
}

/// The healthy-snapshot cache ledgers: one kernel-measured row per reclaim
/// class, and one row a userland process filed for its own glyph cache in
/// the `disposable-ui` class — the class the kernel cannot see into, and
/// the reason the self-report path exists.
///
/// The per-class figures are the ones the class table has always shown
/// (payload `1024 * (class + 1)`, `class` entries, hits and misses 10:1 for
/// a 90% ratio). The reported row holds three times the kernel-measured
/// resident bytes of its class — a 75% self share — and keeps the same
/// 10:1 effectiveness so the folded class ratio is still 90%.
fn healthy_caches() -> Vec<CacheLedgerRecord> {
    let mut rows: Vec<CacheLedgerRecord> = (0..RECLAIM_CLASS_COUNT)
        .map(|class| {
            let class = u8::try_from(class).unwrap_or(0);
            let scale = u64::from(class) + 1;
            let mut row = cache_row(
                &alloc::format!("kernel.slab{class}"),
                CacheOwnerKind::KernelSubsystem,
                0,
                class,
            );
            row.origin = CacheLedgerOrigin::Kernel;
            row.payload_bytes = 1024 * scale;
            row.entries = u64::from(class);
            row.hits = 100 * scale;
            row.misses = 10 * scale;
            row
        })
        .collect();
    let mut reported = cache_row(
        "font.client.glyphs",
        CacheOwnerKind::UserlandProcess,
        0,
        disposable_ui_class(),
    );
    reported.origin = CacheLedgerOrigin::SelfReported;
    reported.reporter_pid = 41;
    reported.payload_bytes = 2560;
    reported.metadata_bytes = 512;
    reported.entries = 12;
    reported.hits = 300;
    reported.misses = 30;
    rows.push(reported);
    rows
}

/// A zeroed cache-ledger row for the fixtures.
fn cache_row(
    label: &str,
    owner_kind: CacheOwnerKind,
    owner_id: u64,
    class: u8,
) -> CacheLedgerRecord {
    CacheLedgerRecord::new(label.as_bytes(), owner_kind, owner_id, class).expect("cache ledger")
}

/// The `disposable-ui` class id, looked up by its stable name so the
/// fixtures cannot drift from the ABI's class order.
fn disposable_ui_class() -> u8 {
    tairix_abi::sysinfo::reclaim_class_from_name("disposable-ui").expect("disposable-ui class")
}

/// The healthy-snapshot mount table: a used root volume and a
/// no-capacity, surprise-removed volume, exercising both the usage and the
/// degraded rows of the storage panel.
fn healthy_mounts() -> Vec<MountRecord> {
    vec![
        // A healthy root volume: 20 GiB, a fifth used (4 of 20).
        mount_record(
            b"rootfs",
            b"/",
            b"arxfs",
            MountFlags::READ_ONLY,
            VolumeStats {
                block_size: 4096,
                total_blocks: 5 * 1024 * 1024,
                free_blocks: 4 * 1024 * 1024,
                avail_blocks: 4 * 1024 * 1024,
                files: 0,
                files_free: 0,
            },
            MountAvailability::Available,
        ),
        // A volume whose driver reports no capacity: honest "unknown".
        mount_record(
            b"data",
            b"/Storage/data",
            b"fat32",
            MountFlags::default(),
            VolumeStats::default(),
            MountAvailability::UnavailableDirty,
        ),
    ]
}

/// A mount-table record for the storage-panel fixtures.
fn mount_record(
    source: &[u8],
    target: &[u8],
    fstype: &[u8],
    flags: MountFlags,
    usage: VolumeStats,
    availability: MountAvailability,
) -> MountRecord {
    MountRecord::new(
        source,
        target,
        fstype,
        flags,
        MountVolumeState {
            usage,
            availability,
            medium: None,
        },
        [0u8; 16],
    )
    .expect("mount")
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

/// Reconstruct the painted screen as one trimmed `String` per row by
/// composing the base window and reading its cells directly — no tty, no
/// escape-sequence decoding.
fn grid_lines(model: &mut Model, rows: u16, cols: u16) -> Vec<alloc::string::String> {
    use crate::app::{compose, Theme};
    let mut screen = screen_with(FakeTty::with_input(b""), rows, cols);
    let theme = Theme::resolve(&mut screen);
    let win = compose(model, &theme, Size::new(rows, cols));
    let buffer = win.buffer();
    let mut lines = Vec::with_capacity(usize::from(rows));
    for row in 0..rows {
        let mut text = alloc::string::String::new();
        if let Some(cells) = buffer.row(row) {
            for cell in cells {
                text.push(cell.ch);
            }
        }
        while text.ends_with(' ') {
            text.pop();
        }
        lines.push(text);
    }
    lines
}

/// The row of the reconstructed grid whose text contains `needle`, or a
/// panic naming the whole grid so a failure is legible.
fn grid_row<'a>(lines: &'a [alloc::string::String], needle: &str) -> &'a str {
    for line in lines {
        if line.contains(needle) {
            return line;
        }
    }
    panic!("no row contains {needle:?} in:\n{}", lines.join("\n"))
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
    // `p` visits every panel in `Focus::ALL` order, once each, before
    // wrapping back to the first.
    let mut seen = vec![model.focus()];
    for _ in 1..Focus::ALL.len() {
        assert_eq!(model.handle_event(&Event::Char('p')), Action::Redraw);
        seen.push(model.focus());
    }
    assert_eq!(seen, Focus::ALL.to_vec());
    // Scroll was reset by the first cycle.
    assert_eq!(model.scroll(), 0);
    // The cycle wraps.
    assert_eq!(model.handle_event(&Event::Char('p')), Action::Redraw);
    assert_eq!(model.focus(), Focus::Reclaim);
}

#[test]
fn the_arrow_keys_step_the_panel_ring_in_both_directions() {
    let (_service, mut model) = refreshed();
    assert_eq!(model.focus(), Focus::Reclaim);
    model.set_viewport(2);
    // Right steps forward exactly as `p` does, and resets the scroll.
    assert_eq!(model.handle_event(&Event::Down), Action::Redraw);
    assert_eq!(model.scroll(), 1);
    assert_eq!(model.handle_event(&Event::Right), Action::Redraw);
    assert_eq!(model.focus(), Focus::Ramzip);
    assert_eq!(model.scroll(), 0);
    // Left steps backward and wraps from the first panel to the last.
    assert_eq!(model.handle_event(&Event::Left), Action::Redraw);
    assert_eq!(model.focus(), Focus::Reclaim);
    assert_eq!(model.handle_event(&Event::Left), Action::Redraw);
    assert_eq!(model.focus(), Focus::Processes);
    // Left then Right returns to where it started.
    assert_eq!(model.handle_event(&Event::Right), Action::Redraw);
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
    // The memory gauge's trailing figures carry the compact used/total
    // figures and the kernel-heap size.
    assert!(contains(&out, b"512.0M"));
    assert!(contains(&out, b"used"));
    assert!(contains(&out, b"kernel"));
    // The pressure gauge is labelled `Pres` and names its current band.
    assert!(contains(&out, b"Pres"));
    assert!(contains(&out, b"moderate"));
    // The panel tab bar shows the focused `caches` panel, whose table lists
    // the reclaim classes by name.
    assert!(contains(&out, b"caches"));
    assert!(contains(&out, b"disposable-ui"));
    // The per-cache breakdown follows the class table on the same page.
    assert!(contains(&out, b"origin"));
    assert!(contains(&out, b"kernel.slab0"));
    // The caches table leads with the effectiveness columns: hits, misses,
    // and the hit ratio (100 : 10 per class renders as 90%).
    assert!(contains(&out, b"hits"));
    assert!(contains(&out, b"misses"));
    assert!(contains(&out, b"hit%"));
    assert!(contains(&out, b"90%"));
    // The task-census row.
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
    focus_on(&mut model, Focus::Ramzip);
    let out = rendered(&mut model);
    // The tab bar highlights the `ramzip` panel; its sectioned counters
    // follow below, carrying the compression accept rate.
    assert!(contains(&out, b"ramzip"));
    assert!(contains(&out, b"stored"));
    assert!(contains(&out, b"16.0K"));
    assert!(contains(&out, b"accept-rate"));
    focus_on(&mut model, Focus::Storage);
    let out = rendered(&mut model);
    // The `disks` panel: the mounted root volume with its usage figures.
    assert!(contains(&out, b"disks"));
    assert!(contains(&out, b"mounted on"));
    assert!(contains(&out, b"arxfs"));
    focus_on(&mut model, Focus::Cpu);
    let out = rendered(&mut model);
    // The `cpu` tab's table carries the switch/preemption columns.
    assert!(contains(&out, b"cpu"));
    assert!(contains(&out, b"switches"));
    focus_on(&mut model, Focus::Irqs);
    let out = rendered(&mut model);
    // The quarantined line 111 renders its owner, count, and state.
    assert!(contains(&out, b"111"));
    assert!(contains(&out, b"quarantined"));
    focus_on(&mut model, Focus::Processes);
    let out = rendered(&mut model);
    // The `procs` panel: the census top consumers by %cpu.
    assert!(contains(&out, b"procs"));
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
    focus_on(&mut model, Focus::Processes);
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
    // The overlay carries the bar key: the memory categories, the CPU busy
    // glyph, and the severity thresholds, so a reader can decode the gauges.
    assert!(contains(&out, b"bar key"));
    assert!(contains(&out, b"user"));
    assert!(contains(&out, b"kernel"));
    assert!(contains(&out, b"busy"));
    assert!(contains(&out, b"free"));
    assert!(contains(&out, b"<60%"));
    assert!(contains(&out, b">=85%"));
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

// ---- Rendering: the redesigned gauges, tabs, and styled rows ---------------

use crate::app::{detail_rows, RowStyle};

/// Cycle the focused panel to `target` from the default (`Reclaim`).
fn focus_on(model: &mut Model, target: Focus) {
    for _ in 0..Focus::ALL.len() {
        if model.focus() == target {
            return;
        }
        let _ = model.handle_event(&Event::Char('p'));
    }
    assert_eq!(model.focus(), target);
}

#[test]
fn the_memory_gauge_draws_a_stacked_bar_with_a_percentage() {
    // On a roomy screen the adaptive bar grows wide enough to show every
    // category, so the stacked composition is asserted directly on the
    // reconstructed memory row: a `[`…`]` bar decomposing the 512M of the
    // 1G total used into `#` user-resident (128M), `K` kernel heap (64M),
    // and `=` other-in-use cells, then the compact used/total figures and
    // the exact used percentage.
    let (_service, mut model) = refreshed();
    let lines = grid_lines(&mut model, 25, 120);
    let mem = grid_row(&lines, "Mem");
    assert!(mem.contains('['), "bar open: {mem:?}");
    assert!(mem.contains(']'), "bar close: {mem:?}");
    assert!(mem.contains('#'), "user-resident cells: {mem:?}");
    assert!(mem.contains('K'), "kernel-heap cells: {mem:?}");
    assert!(mem.contains('='), "other-in-use cells: {mem:?}");
    assert!(mem.contains("50% used"), "used percentage: {mem:?}");
    assert!(mem.contains("512.0M/1.0G"), "compact figures: {mem:?}");
}

#[test]
fn the_memory_trailing_annotates_the_overlapping_ramzip_and_pinned_figures() {
    let mut service = FakeService::healthy();
    // ramzip stored and pinned bytes overlap the bar's disjoint buckets, so
    // they are reported as trailing figures rather than double-counted slices.
    service.ramzip = Some(RamzipStats {
        stored_bytes: 8 * 1024 * 1024,
        pinned_bytes: 4 * 1024 * 1024,
        ..RamzipStats::default()
    });
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let out = rendered(&mut model);
    assert!(contains(&out, b"ramzip"));
    assert!(contains(&out, b"pinned"));
}

#[test]
fn a_table_header_renders_in_the_inverted_rendition() {
    let (_service, mut model) = refreshed();
    // The storage panel opens with a column header, drawn as a full-width
    // inverted (reverse-video) bar. Reverse video is the only reverse the
    // screen emits (the coloured bars/tabs use colour, not reverse), so its
    // SGR marks the header unambiguously.
    focus_on(&mut model, Focus::Storage);
    let out = rendered(&mut model);
    assert!(
        contains(&out, b"\x1b[7m"),
        "a table header must render inverted (reverse video)"
    );
}

#[test]
fn the_cpu_gauge_names_the_busy_share_and_cpu_count() {
    let (_service, mut model) = refreshed();
    let out = rendered(&mut model);
    assert!(contains(&out, b"busy"));
    assert!(contains(&out, b"cpus"));
}

#[test]
fn the_tab_bar_lists_every_panel() {
    let (_service, mut model) = refreshed();
    let out = rendered(&mut model);
    for label in ["caches", "ramzip", "disks", "cpu", "irqs", "procs"] {
        assert!(
            contains(&out, label.as_bytes()),
            "tab bar must show {label}"
        );
    }
}

#[test]
fn the_ramzip_panel_reports_the_cache_hit_ratios() {
    let mut service = FakeService::healthy();
    service.ramzip = Some(RamzipStats {
        logical_bytes: 16384,
        stored_bytes: 4096,
        attempts: 10,
        accepted: 7,
        fault_ins: 6,
        warm_restored: 2,
        cluster_restored: 0,
        auth_failures: 1,
        decode_failures: 1,
        ..RamzipStats::default()
    });
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    focus_on(&mut model, Focus::Ramzip);
    let text: alloc::string::String = detail_rows(&model)
        .iter()
        .map(|r| r.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    // Compression accept rate 7/10 = 70%; restore success (6+2)/(8+2) = 80%;
    // stored 4K of 16K logical saves 12K = 75%.
    assert!(text.contains("accept-rate 70%"), "accept-rate: {text}");
    assert!(text.contains("success-rate 80%"), "success-rate: {text}");
    assert!(text.contains("75% of logical"), "saved ratio: {text}");
}

#[test]
fn an_idle_ramzip_tier_reports_no_ratio_rather_than_a_fabricated_one() {
    let mut service = FakeService::healthy();
    service.ramzip = Some(RamzipStats::default());
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    focus_on(&mut model, Focus::Ramzip);
    let text: alloc::string::String = detail_rows(&model)
        .iter()
        .map(|r| r.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    // Nothing offered and nothing restored: the ratios are `-`, never 0% or
    // 100% invented from an empty denominator.
    assert!(text.contains("accept-rate -"), "idle accept-rate: {text}");
    assert!(text.contains("success-rate -"), "idle success-rate: {text}");
}

#[test]
fn the_storage_panel_lists_volumes_with_usage_and_marks_the_dead_one() {
    let (_service, mut model) = refreshed();
    focus_on(&mut model, Focus::Storage);
    let rows = detail_rows(&model);
    assert_eq!(rows[0].style, RowStyle::Header);
    // The healthy 20 GiB root is a fifth used: a filled usage bar and 20%.
    let root = rows
        .iter()
        .find(|r| r.text.contains("arxfs"))
        .expect("root");
    assert_eq!(root.style, RowStyle::Body);
    assert!(root.text.contains("20%"), "root use%: {}", root.text);
    assert!(root.text.contains('|'), "root usage bar: {}", root.text);
    // The no-capacity dirty volume states its unknown size and is warned.
    let data = rows
        .iter()
        .find(|r| r.text.contains("/Storage/data"))
        .expect("data");
    assert_eq!(data.style, RowStyle::Warn);
    assert!(data.text.contains("capacity unknown"));
    assert!(data.text.contains("[unavailable-dirty]"));
}

/// A storage-panel model serving exactly `mounts`.
fn storage_model(mounts: Vec<MountRecord>) -> Model {
    let mut service = FakeService::healthy();
    service.mounts = Some(mounts);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    focus_on(&mut model, Focus::Storage);
    model
}

#[test]
fn two_deep_mount_points_stay_distinguishable_in_their_column() {
    // Two volumes whose paths agree for the whole width of the column and
    // differ only at the leaf. Telling them apart is the column's entire
    // job, so the middle is what goes.
    let model = storage_model(
        [
            &b"/Storage/backup-2024-vol1"[..],
            &b"/Storage/backup-2024-vol2"[..],
        ]
        .into_iter()
        .map(|target| {
            mount_record(
                b"backup",
                target,
                b"arxfs",
                MountFlags::default(),
                VolumeStats::default(),
                MountAvailability::Available,
            )
        })
        .collect(),
    );
    let rows = detail_rows(&model);
    let targets: Vec<&str> = rows
        .iter()
        .filter(|row| row.style != RowStyle::Header)
        .map(|row| columns(row)[0])
        .collect();
    assert_eq!(targets, ["/Storage/b~2024-vol1", "/Storage/b~2024-vol2"]);
}

#[test]
fn a_hundred_terabyte_volume_keeps_its_figures_inside_their_columns() {
    // The charter's storage floor is volumes well past 100 TB, so the
    // capacity figures must climb into the tebibyte and exbibyte bands
    // rather than growing digits and shunting the usage bar off the line.
    const BLOCK: u32 = 4096;
    let volume = |bytes: u64, free: u64| VolumeStats {
        block_size: BLOCK,
        total_blocks: bytes / u64::from(BLOCK),
        free_blocks: free / u64::from(BLOCK),
        avail_blocks: free / u64::from(BLOCK),
        files: 0,
        files_free: 0,
    };
    let tib = 1024u64.pow(4);
    let eib = 1024u64.pow(6);
    let model = storage_model(vec![
        mount_record(
            b"vast",
            b"/Storage/vast",
            b"arxfs",
            MountFlags::default(),
            volume(200 * tib, 160 * tib),
            MountAvailability::Available,
        ),
        mount_record(
            b"vaster",
            b"/Storage/vaster",
            b"arxfs",
            MountFlags::default(),
            volume(2 * eib, eib),
            MountAvailability::Available,
        ),
    ]);
    let rows = detail_rows(&model);
    assert_eq!(
        columns(row_starting(&rows, "/Storage/vast"))[2..6],
        ["200.0T", "40.0T", "160.0T", "20%"]
    );
    assert_eq!(
        columns(row_starting(&rows, "/Storage/vaster"))[2..6],
        ["2.0E", "1.0E", "1.0E", "50%"]
    );
    for row in rows.iter().filter(|row| row.style != RowStyle::Header) {
        for figure in &columns(row)[2..5] {
            assert!(
                figure.chars().count() <= tairix_procinfo::SIZE_WIDTH,
                "figure {figure:?} overruns its column: {}",
                row.text
            );
        }
    }
}

#[test]
fn a_failed_mount_walk_renders_a_single_absence_row() {
    let service = FakeService::healthy();
    service.deny(SysinfoQueryId::MOUNT_LIST);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    focus_on(&mut model, Focus::Storage);
    let rows = detail_rows(&model);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].style, RowStyle::Denied);
}

#[test]
fn the_irq_panel_styles_the_header_and_quarantined_line() {
    let (_service, mut model) = refreshed();
    focus_on(&mut model, Focus::Irqs);
    let rows = detail_rows(&model);
    // The first row is the column header; the quarantined line 111 is drawn
    // in the warn rendition; the healthy line 27 is an ordinary body row.
    assert_eq!(rows[0].style, RowStyle::Header);
    let quarantined = rows
        .iter()
        .find(|r| r.text.contains("111"))
        .expect("quarantined row");
    assert_eq!(quarantined.style, RowStyle::Warn);
    let active = rows
        .iter()
        .find(|r| r.text.contains(" 27 "))
        .expect("active row");
    assert_eq!(active.style, RowStyle::Body);
}

/// The per-cache breakdown rows: everything below the breakdown table's own
/// column header, which the `origin` column names uniquely.
fn breakdown_rows(rows: &[crate::app::PanelRow]) -> &[crate::app::PanelRow] {
    let header = rows
        .iter()
        .position(|row| row.style == RowStyle::Header && row.text.contains("origin"))
        .expect("breakdown header");
    &rows[header + 1..]
}

/// A row's columns, so an assertion names the column it means rather than a
/// substring that could match anywhere on the line.
fn columns(row: &crate::app::PanelRow) -> Vec<&str> {
    row.text.split_whitespace().collect()
}

/// The panel row whose first column is `first`, or a panic naming the rows
/// so a failure is legible.
fn row_starting<'a>(rows: &'a [crate::app::PanelRow], first: &str) -> &'a crate::app::PanelRow {
    rows.iter()
        .find(|row| columns(row).first() == Some(&first))
        .unwrap_or_else(|| panic!("no row for {first}: {rows:?}"))
}

#[test]
fn a_refused_class_query_states_the_refusal_rather_than_a_fabricated_table() {
    let service = FakeService::healthy();
    service.deny(SysinfoQueryId::RECLAIM_STATS);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    // The default focus is the caches page; the refused class query renders
    // as one stated-refusal row, never an invented class table.
    let rows = detail_rows(&model);
    assert_eq!(rows[0].style, RowStyle::Denied);
    assert!(rows[0].text.contains("CAP_SYSINFO_KERNEL"));
    assert!(
        !rows.iter().any(|row| row.text.contains("self%")),
        "no class table may be drawn for a refused query: {rows:?}"
    );
    // The per-cache breakdown is a separate query and still serves: each
    // query degrades on its own.
    assert_eq!(breakdown_rows(&rows).len(), RECLAIM_CLASS_COUNT + 1);
}

#[test]
fn the_caches_page_breaks_each_class_total_down_per_cache() {
    let (_service, model) = refreshed();
    let rows = detail_rows(&model);
    let breakdown = breakdown_rows(&rows);
    // One row per registered ledger: the nine kernel-measured caches and the
    // one a process reported for itself.
    assert_eq!(breakdown.len(), RECLAIM_CLASS_COUNT + 1);
    // Both tables are budgeted for the 80-column serial fallback, including
    // the rows only a scrolled panel shows.
    for row in &rows {
        assert!(
            tairix_curses::str_width(&row.text) <= 80,
            "row wider than 80 columns: {}",
            row.text
        );
    }

    // The kernel-measured `disposable-ui` slab: a kernel-subsystem owner
    // carries no id, its figures are attested, 1 KiB is resident, and 100
    // hits to 10 misses is a 90% ratio.
    let kernel = row_starting(breakdown, "kernel.slab0");
    assert_eq!(
        columns(kernel),
        [
            "kernel.slab0",
            "kernel",
            "kernel",
            "disposable-ui",
            "0",
            "1.0K",
            "90%"
        ]
    );
    assert_eq!(kernel.style, RowStyle::Body);

    // The reported glyph cache: the reporting pid names its owner, and 2560
    // payload plus 512 metadata bytes are 3 KiB resident. The label is a
    // column too long, so its middle goes and both ends stay — the leaf is
    // what tells one glyph cache from another.
    let reported = row_starting(breakdown, "font.cli~.glyphs");
    assert_eq!(
        columns(reported),
        [
            "font.cli~.glyphs",
            "proc@41",
            "self",
            "disposable-ui",
            "12",
            "3.0K",
            "90%"
        ]
    );
}

#[test]
fn a_self_reported_cache_says_so_in_words_and_in_the_class_self_share() {
    let (_service, model) = refreshed();
    let rows = detail_rows(&model);
    let breakdown = breakdown_rows(&rows);
    let reported = row_starting(breakdown, "font.cli~.glyphs");
    // The mark is a word in the row itself, legible on a monochrome serial
    // console; the notice rendition reinforces it, never carries it alone.
    assert_eq!(columns(reported).get(2), Some(&"self"));
    assert_eq!(reported.style, RowStyle::Notice);
    // Every attested row says where its figures came from too, and is drawn
    // as an ordinary body row.
    for row in breakdown.iter().filter(|row| row.style == RowStyle::Body) {
        assert_eq!(columns(row).get(2), Some(&"kernel"), "{}", row.text);
    }
    // The class table carries the share of the class total that is taken on
    // trust: 3 KiB reported of the 4 KiB resident.
    assert_eq!(
        columns(row_starting(&rows, "disposable-ui")).get(3),
        Some(&"75%")
    );
    // A class no process reports for is wholly attested, and says 0% rather
    // than a blank that could read as "unknown".
    assert_eq!(
        columns(row_starting(&rows, "fs-metadata")).get(3),
        Some(&"0%")
    );
}

#[test]
fn a_class_total_is_the_sum_of_its_cache_rows() {
    let (_service, model) = refreshed();
    let rows = detail_rows(&model);
    // `disposable-ui` holds two caches: the kernel-measured 1 KiB slab and
    // the reported 3 KiB glyph cache. The class row is their sum — 12
    // entries, 4 KiB resident, 400 hits and 40 misses — so the two tables
    // can never tell an operator two different stories.
    assert_eq!(
        columns(row_starting(&rows, "disposable-ui")),
        [
            "disposable-ui",
            "12",
            "4.0K",
            "75%",
            "400",
            "40",
            "90%",
            "0",
            "0",
            "0"
        ]
    );
    let breakdown = breakdown_rows(&rows);
    let residents: Vec<&str> = breakdown
        .iter()
        .filter(|row| columns(row).get(3) == Some(&"disposable-ui"))
        .filter_map(|row| columns(row).get(5).copied())
        .collect();
    assert_eq!(residents, ["1.0K", "3.0K"], "rows of the class");
}

#[test]
fn a_refused_cache_ledger_query_states_the_refusal_rather_than_an_empty_table() {
    let service = FakeService::healthy();
    // The breakdown is gated on the kernel-observability capability exactly
    // as the class table is, so a caller without it is refused both.
    service.deny(SysinfoQueryId::RECLAIM_STATS);
    service.deny(SysinfoQueryId::CACHE_LEDGERS);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let rows = detail_rows(&model);
    let refusals = rows
        .iter()
        .filter(|row| row.style == RowStyle::Denied)
        .count();
    assert_eq!(refusals, 2, "each table states its own refusal: {rows:?}");
    for row in rows.iter().filter(|row| row.style == RowStyle::Denied) {
        assert!(row.text.contains("CAP_SYSINFO_KERNEL"), "{}", row.text);
    }
    // Not one cache row, and no "no caches registered" notice: the page never
    // implies the machine holds no caches when it was refused the answer.
    assert!(
        !rows.iter().any(|row| row.text.contains("no caches")),
        "a refusal must not read as an empty table: {rows:?}"
    );
}

#[test]
fn an_empty_cache_ledger_is_stated_rather_than_left_blank() {
    let mut service = FakeService::healthy();
    let rows = Vec::new();
    service.reclaim = Some(fold_cache_ledgers(&rows).to_vec());
    service.caches = Some(rows);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let rows = detail_rows(&model);
    let breakdown = breakdown_rows(&rows);
    assert_eq!(breakdown.len(), 1);
    assert_eq!(breakdown[0].style, RowStyle::Notice);
    assert!(breakdown[0].text.contains("no caches registered"));
}

#[test]
fn the_cache_breakdown_pages_until_the_service_runs_out() {
    let total = usize::from(tairix_procinfo::CACHE_LEDGER_PAGE) + 6;
    let mut service = FakeService::healthy();
    // More ledgers than one page holds: the walk must ask again from the
    // next offset until a short page ends it, or the tail is invisible.
    let ledgers: Vec<CacheLedgerRecord> = (0..total)
        .map(|n| {
            let mut row = cache_row(
                &alloc::format!("cache{n}"),
                CacheOwnerKind::FilesystemVolume,
                n as u64,
                disposable_ui_class(),
            );
            row.origin = CacheLedgerOrigin::Kernel;
            row.payload_bytes = 1024;
            row
        })
        .collect();
    service.reclaim = Some(fold_cache_ledgers(&ledgers).to_vec());
    service.caches = Some(ledgers);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let rows = detail_rows(&model);
    let breakdown = breakdown_rows(&rows);
    assert_eq!(breakdown.len(), total);
    assert!(breakdown[0].text.starts_with("cache0"));
    let last = alloc::format!("cache{}", total - 1);
    assert!(
        breakdown[total - 1].text.starts_with(&last),
        "last row: {}",
        breakdown[total - 1].text
    );
    // The volume-owned rows name their volume id, which fits the owner
    // column whole.
    assert!(
        breakdown[1].text.contains("vol:1"),
        "owner id: {}",
        breakdown[1].text
    );
}

#[test]
fn a_huge_owner_id_is_elided_rather_than_shifting_the_columns_after_it() {
    let mut service = FakeService::healthy();
    // The largest owner and reporter a `u64` can carry: 46 columns of owner
    // text for a 13-column cell. Nothing stops a service sending it, and a
    // cell that overran would push every figure after it off the line.
    let mut row = cache_row(
        "kernel.slab0",
        CacheOwnerKind::Task,
        u64::MAX,
        disposable_ui_class(),
    );
    row.origin = CacheLedgerOrigin::SelfReported;
    row.reporter_pid = u64::MAX;
    row.payload_bytes = 1024;
    row.entries = 3;
    row.hits = 90;
    row.misses = 10;
    let ledgers = vec![row];
    service.reclaim = Some(fold_cache_ledgers(&ledgers).to_vec());
    service.caches = Some(ledgers);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let rows = detail_rows(&model);
    let breakdown = breakdown_rows(&rows);
    assert_eq!(breakdown.len(), 1);
    let row = &breakdown[0];
    // The whole line, cell by cell: both ends of both ids survive inside
    // the owner's own thirteen columns, so the row can neither be read as
    // naming a smaller task or pid, nor shift the figures after it.
    assert_eq!(
        row.text,
        alloc::format!(
            "{:<16} {:<13} {:<6} {:<21} {:>7} {:>7} {:>4}",
            "kernel.slab0",
            "task:1~551615",
            "self",
            "disposable-ui",
            "3",
            "1.0K",
            "90%"
        )
    );
    assert_eq!(tairix_curses::str_width(&row.text), 80);
}

#[test]
fn a_name_too_long_for_its_column_keeps_its_head_and_its_leaf() {
    // A name that fits is the name.
    assert_eq!(elide_middle("clean_fs.data", 16), "clean_fs.data");
    assert_eq!(elide_middle("exactly16colums!", 16), "exactly16colums!");
    // A longer one loses its middle, not its leaf: the leaf is what tells
    // two caches in the same namespace apart.
    assert_eq!(elide_middle("clean_fs.metadata", 16), "clean_fs~etadata");
    assert_eq!(elide_middle("font.client.glyphs", 16), "font.cli~.glyphs");
    // Whatever the width, an elided name is exactly the column it was given.
    for width in 3..=32 {
        let elided = elide_middle("session.desktop-artwork", width);
        assert_eq!(
            tairix_curses::str_width(&elided),
            width.min(23),
            "width {width}: {elided}"
        );
    }
}

#[test]
fn a_column_too_narrow_to_hold_both_ends_cuts_rather_than_overruns() {
    // Under three columns there is no room for a head, a mark and a leaf,
    // so the name is cut to the budget — never widened past it, never a
    // panic on the arithmetic.
    assert_eq!(elide_middle("clean_fs.metadata", 2), "cl");
    assert_eq!(elide_middle("clean_fs.metadata", 1), "c");
    assert_eq!(elide_middle("clean_fs.metadata", 0), "");
    assert_eq!(elide_middle("", 0), "");
}

#[test]
fn the_process_panel_notice_is_styled_when_the_global_census_is_refused() {
    let service = FakeService::healthy();
    service.deny(SysinfoQueryId::GLOBAL_PROCESS_LIST);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    focus_on(&mut model, Focus::Processes);
    let rows = detail_rows(&model);
    assert_eq!(rows[0].style, RowStyle::Notice);
    assert!(rows[0].text.contains("CAP_SYSINFO_GLOBAL"));
}

// ---- 80-column fit ---------------------------------------------------------

/// The conventional serial fallback and the smallest grid the tool must
/// serve without clipping a figure: 80 columns by 25 rows.

#[test]
fn every_composed_row_fits_the_eighty_column_grid() {
    // The renderer clips each line to the width, so an over-wide line can
    // never physically overflow — this instead guards against a future
    // double-width miscount, and pairs with the content assertions below
    // that prove no *figure* is lost to that clipping.
    let (_service, mut model) = refreshed();
    for line in grid_lines(&mut model, 25, 80) {
        assert!(
            tairix_curses::str_width(&line) <= 80,
            "row wider than 80 columns: {line:?}"
        );
    }
}

#[test]
fn the_memory_line_keeps_every_figure_at_eighty_columns() {
    // ramzip and pinned are the figures the old fixed-width bar pushed off
    // the 80-column line; the adaptive bar yields its cells so they stay on.
    let mut service = FakeService::healthy();
    service.ramzip = Some(RamzipStats {
        stored_bytes: 16 * 1024 * 1024,
        pinned_bytes: 32 * 1024 * 1024,
        ..RamzipStats::default()
    });
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let lines = grid_lines(&mut model, 25, 80);
    let mem = grid_row(&lines, "Mem");
    // Every mandatory memory figure is present in full, not truncated.
    assert!(mem.contains("% used"), "used share: {mem:?}");
    assert!(mem.contains("kernel 64.0M"), "kernel heap: {mem:?}");
    assert!(mem.contains("ramzip 16.0M"), "ramzip stored: {mem:?}");
    assert!(mem.contains("pinned 32.0M"), "pinned: {mem:?}");
}

#[test]
fn large_figures_are_not_truncated_at_eighty_columns() {
    // A big, busy machine: multi-terabyte RAM, hundred-gigabyte ramzip and
    // pinned aggregates, million-plus scheduler counters, half-a-billion
    // band-entry transitions. The compact units keep every figure on the
    // line and the adaptive bar simply gives up its cells.
    let mut service = FakeService::healthy();
    service.memory = Some(KernelMemoryStats {
        total_bytes: 2 * 1024 * 1024 * 1024 * 1024,
        free_bytes: 1024 * 1024 * 1024 * 1024,
        kernel_heap_bytes: 20 * 1024 * 1024 * 1024,
        user_resident_bytes: 512 * 1024 * 1024 * 1024,
        page_size: 4096,
        reserved: 0,
    });
    service.ramzip = Some(RamzipStats {
        stored_bytes: 100 * 1024 * 1024 * 1024,
        pinned_bytes: 200 * 1024 * 1024 * 1024,
        ..RamzipStats::default()
    });
    *service.pressure.borrow_mut() = Some(MemoryPressureStats {
        band: 4,
        total_bytes: 2 * 1024 * 1024 * 1024 * 1024,
        free_bytes: 1024 * 1024 * 1024 * 1024,
        reserve_bytes: 64 * 1024 * 1024 * 1024,
        band_entries: [500_000_000, 0, 0, 0, 0],
        ..MemoryPressureStats::default()
    });
    service.cpu_loads = Some(vec![
        CpuLoadRecord {
            cpu: 0,
            reserved: 0,
            queue_depth: 9,
            switches: 1_500_000,
            preemptions: 12_000_000,
        },
        CpuLoadRecord {
            cpu: 1,
            reserved: 0,
            queue_depth: 9,
            switches: 0,
            preemptions: 0,
        },
    ]);
    let mut model = Model::new(DEFAULT_DELAY_TENTHS);
    model.refresh(&service);
    let lines = grid_lines(&mut model, 25, 80);

    for line in &lines {
        assert!(
            tairix_curses::str_width(line) <= 80,
            "row wider than 80 columns: {line:?}"
        );
    }

    let mem = grid_row(&lines, "Mem");
    assert!(mem.contains("kernel 20.0G"), "kernel heap: {mem:?}");
    assert!(mem.contains("ramzip 100.0G"), "ramzip: {mem:?}");
    assert!(mem.contains("pinned 200.0G"), "pinned: {mem:?}");

    let pres = grid_row(&lines, "Pres");
    assert!(pres.contains("500.0M entries"), "band entries: {pres:?}");

    let cpu = grid_row(&lines, "CPU");
    assert!(cpu.contains("1.5M sw"), "switches: {cpu:?}");
    assert!(cpu.contains("12.0M preempt"), "preemptions: {cpu:?}");
}
