//! The `sysmon` view model: the sampled kernel-statistics snapshot plus the
//! panel focus, scroll, refresh interval, pin state, and help-overlay state,
//! and how an input [`Event`] moves them.
//!
//! The model is deliberately free of any terminal I/O so it is exhaustively
//! testable on the host: the [`crate::app`] glue feeds it events and asks
//! [`crate::app::render`] to draw it. Every System Information query
//! degrades independently — a refused capability records the refusal, a
//! failed call records the absence — so the monitor keeps running under the
//! very stress it observes.

use alloc::vec::Vec;

use rustos_abi::sysinfo::{
    CpuLoadRecord, KernelMemoryStats, LoadAverage, MemoryPressureStats, ProcessRecord, RamzipStats,
    ReclaimClassRecord, SysinfoQueryId, Uptime,
};
use rustos_curses::Event;
use rustos_procinfo::{
    call, for_each_cpu_load, for_each_cpu_time, for_each_process, for_each_reclaim_class,
    memory_pressure, ramzip_stats, CallError, ListError, Transport,
};

use crate::command::{DELAY_STEP_TENTHS, MAX_DELAY_TENTHS, MIN_DELAY_TENTHS};

/// How many refresh-interval band samples the pressure history strip keeps.
///
/// Bounds the strip at two minutes of three-second refreshes; the renderer
/// shows however many fit its width, newest rightmost.
pub const BAND_HISTORY_MAX: usize = 120;

/// One gated query's outcome: the decoded value, the stated capability
/// refusal, or the honest absence (transport/decode failure) — never a
/// fabricated figure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Gauge<T> {
    /// The service answered and the reply decoded.
    Ready(T),
    /// The service refused the query for want of `CAP_SYSINFO_KERNEL` (or
    /// `CAP_SYSINFO_GLOBAL` for the process summary) — rendered as the
    /// refusal it is, never a silent blank.
    Denied,
    /// The call failed or the reply did not decode; the figure is absent.
    Unavailable,
}

impl<T> Gauge<T> {
    /// The decoded value, when the query answered.
    pub const fn ready(&self) -> Option<&T> {
        match self {
            Gauge::Ready(value) => Some(value),
            _ => None,
        }
    }

    /// Whether the query was refused for want of a capability.
    pub const fn is_denied(&self) -> bool {
        matches!(self, Gauge::Denied)
    }
}

/// Map one scalar query outcome onto its [`Gauge`].
fn gauge_call<T>(result: Result<T, CallError>) -> Gauge<T> {
    match result {
        Ok(value) => Gauge::Ready(value),
        Err(CallError::PermissionDenied) => Gauge::Denied,
        Err(CallError::Service(_)) => Gauge::Unavailable,
    }
}

/// Map one paged-walk outcome (with its collected records) onto a [`Gauge`].
fn gauge_walk<T>(result: Result<(), ListError>, records: Vec<T>) -> Gauge<Vec<T>> {
    match result {
        Ok(()) => Gauge::Ready(records),
        Err(ListError::Call(CallError::PermissionDenied)) => Gauge::Denied,
        Err(_) => Gauge::Unavailable,
    }
}

/// Which detail panel the lower half of the screen expands.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Focus {
    /// The per-class reclaimable-cache ledger table (`RECLAIM_STATS`).
    Reclaim,
    /// The `ramzip` compressed-tier counters (`RAMZIP_STATS`).
    Ramzip,
    /// The per-CPU load table (`CPU_LOAD` + the `CPU_TIME_STATS` split).
    Cpu,
    /// The process census and top consumers (the process lists).
    Processes,
}

impl Focus {
    /// The next panel in the `p` cycling order.
    #[must_use]
    pub const fn next(self) -> Focus {
        match self {
            Focus::Reclaim => Focus::Ramzip,
            Focus::Ramzip => Focus::Cpu,
            Focus::Cpu => Focus::Processes,
            Focus::Processes => Focus::Reclaim,
        }
    }

    /// A short human label for the panel header.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Focus::Reclaim => "reclaimable caches",
            Focus::Ramzip => "compressed tier (ramzip)",
            Focus::Cpu => "per-cpu load",
            Focus::Processes => "processes",
        }
    }
}

/// Whether the monitor's own memory is pinned out of the swap tiers, and —
/// when it is not — the stated reason (a capability or limit refusal is an
/// answer, never a fatal error).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PinState {
    /// `mem_pin` succeeded: the monitor never stalls on its own fault-in.
    Pinned,
    /// `mem_pin` was refused; the session continues unpinned with the
    /// reason shown on the title line.
    Unpinned(&'static str),
}

/// What the [`crate::app`] loop should do after handling an event.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Nothing changed; no redraw is required.
    Ignore,
    /// View state changed; redraw from the existing snapshot.
    Redraw,
    /// The user asked for an immediate re-query.
    Refresh,
    /// The user asked to quit.
    Quit,
}

/// One CPU's busy share over the last refresh interval, in tenths of a
/// percent, derived from two `CPU_TIME_STATS` samples' deltas (the first
/// sample falls back to the honest cumulative since-boot ratio).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CpuBusy {
    /// The CPU index.
    pub cpu: u32,
    /// Busy share of the interval, in tenths of a percent.
    pub busy_tenths: u32,
}

/// The sampled statistics snapshot one refresh produces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// Monotonic nanoseconds since boot, when the ungated `UPTIME` answered.
    pub uptime_ns: Option<u64>,
    /// The damped run-queue averages and task/user census, when the ungated
    /// `LOAD_AVERAGE` answered.
    pub load: Option<LoadAverage>,
    /// Kernel memory statistics (`KERNEL_MEMORY_STATS`, gated).
    pub memory: Gauge<KernelMemoryStats>,
    /// The live pressure gauge (`MEMORY_PRESSURE`, gated).
    pub pressure: Gauge<MemoryPressureStats>,
    /// The per-class reclaim ledger (`RECLAIM_STATS`, gated).
    pub reclaim: Gauge<Vec<ReclaimClassRecord>>,
    /// The `ramzip` tier counters (`RAMZIP_STATS`, gated).
    pub ramzip: Gauge<RamzipStats>,
    /// The per-CPU scheduler load records (`CPU_LOAD`, gated).
    pub cpu_loads: Gauge<Vec<CpuLoadRecord>>,
    /// The process records backing the census and top-consumer lists: the
    /// system-wide list when `CAP_SYSINFO_GLOBAL` is held, else the
    /// caller's own with [`Snapshot::global_denied`] recorded.
    pub processes: Vec<ProcessRecord>,
    /// Whether the system-wide process list was refused and the census fell
    /// back to the caller's own processes.
    pub global_denied: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            uptime_ns: None,
            load: None,
            memory: Gauge::Unavailable,
            pressure: Gauge::Unavailable,
            reclaim: Gauge::Unavailable,
            ramzip: Gauge::Unavailable,
            cpu_loads: Gauge::Unavailable,
            processes: Vec::new(),
            global_denied: false,
        }
    }
}

/// Per-process carry-over from the previous refresh: the cumulative CPU
/// time keyed by the never-reused `proc_id`, so a numeric-PID reuse can
/// never inherit another lifetime's statistics.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ProcHistory {
    proc_id: rustos_abi::ProcId,
    cpu_time_ns: u64,
}

/// The live `sysmon` view state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Model {
    snapshot: Snapshot,
    band_history: Vec<u8>,
    prev_cpu_times: Vec<(u32, u64, u64)>,
    cpu_busy: Vec<CpuBusy>,
    proc_history: Vec<ProcHistory>,
    prev_uptime_ns: Option<u64>,
    proc_pct: Vec<(rustos_abi::ProcId, u32)>,
    focus: Focus,
    scroll: usize,
    viewport: usize,
    delay_tenths: u32,
    pin: PinState,
    show_help: bool,
}

impl Model {
    /// A fresh, empty model refreshing every `delay_tenths` tenths of a
    /// second (clamped into the in-session bounds). Call [`Model::refresh`]
    /// to populate it.
    #[must_use]
    pub fn new(delay_tenths: u32) -> Model {
        Model {
            snapshot: Snapshot::default(),
            band_history: Vec::new(),
            prev_cpu_times: Vec::new(),
            cpu_busy: Vec::new(),
            proc_history: Vec::new(),
            prev_uptime_ns: None,
            proc_pct: Vec::new(),
            focus: Focus::Reclaim,
            scroll: 0,
            viewport: 1,
            delay_tenths: delay_tenths.clamp(MIN_DELAY_TENTHS, MAX_DELAY_TENTHS),
            pin: PinState::Unpinned("not attempted"),
            show_help: false,
        }
    }

    /// Record whether the startup `mem_pin` succeeded, and the stated
    /// reason when it did not (shown on the title line for the session).
    pub fn set_pin_state(&mut self, pin: PinState) {
        self.pin = pin;
    }

    /// The recorded pin state.
    #[must_use]
    pub const fn pin(&self) -> PinState {
        self.pin
    }

    /// The sampled snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// The pressure-band history, oldest first (one sample per refresh
    /// whose pressure query answered).
    #[must_use]
    pub fn band_history(&self) -> &[u8] {
        &self.band_history
    }

    /// The per-CPU busy shares derived from the last two CPU-time samples.
    #[must_use]
    pub fn cpu_busy(&self) -> &[CpuBusy] {
        &self.cpu_busy
    }

    /// The `%CPU` share (tenths of a percent, over the last refresh
    /// interval) derived for `proc_id`, when one could be measured.
    #[must_use]
    pub fn proc_pct(&self, proc_id: rustos_abi::ProcId) -> Option<u32> {
        self.proc_pct
            .iter()
            .find(|(id, _)| *id == proc_id)
            .map(|(_, pct)| *pct)
    }

    /// The focused detail panel.
    #[must_use]
    pub const fn focus(&self) -> Focus {
        self.focus
    }

    /// The detail-panel scroll offset (clamped by the renderer against the
    /// panel's actual line count).
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    /// Clamp the scroll offset against the focused panel's `lines`, keeping
    /// at least one viewport of content reachable. The renderer calls this
    /// with the lines it is about to draw, so the offset can never point
    /// past the panel however the snapshot shrank.
    pub fn clamp_scroll(&mut self, lines: usize) {
        let max = lines.saturating_sub(self.viewport);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    /// The refresh interval in tenths of a second.
    #[must_use]
    pub const fn delay_tenths(&self) -> u32 {
        self.delay_tenths
    }

    /// Whether the help overlay is showing.
    #[must_use]
    pub const fn help_visible(&self) -> bool {
        self.show_help
    }

    /// Tell the model how many detail rows fit on screen, so scrolling
    /// clamps correctly. A zero is treated as one row.
    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
    }

    /// Re-query every panel's figures and adopt the fresh sample. Each
    /// query degrades independently: a capability refusal is recorded as
    /// the refusal it is, a failed call as the figure's absence — the
    /// monitor never dies because the service refused or hiccuped
    /// (observing a machine under stress is its purpose).
    pub fn refresh(&mut self, transport: &dyn Transport) {
        let uptime_ns = call(transport, SysinfoQueryId::UPTIME, &[])
            .ok()
            .and_then(|bytes| Uptime::from_bytes(&bytes).ok())
            .and_then(|uptime| {
                let secs = u64::try_from(uptime.since_boot.secs()).ok()?;
                secs.checked_mul(1_000_000_000)
                    .map(|ns| ns + u64::from(uptime.since_boot.subsec_nanos()))
            });
        let load = call(transport, SysinfoQueryId::LOAD_AVERAGE, &[])
            .ok()
            .and_then(|bytes| LoadAverage::from_bytes(&bytes).ok());

        let memory = gauge_call(
            call(transport, SysinfoQueryId::KERNEL_MEMORY_STATS, &[]).and_then(|bytes| {
                KernelMemoryStats::from_bytes(&bytes).map_err(CallError::Service)
            }),
        );
        let pressure = gauge_call(memory_pressure(transport));
        let ramzip = gauge_call(ramzip_stats(transport));

        let mut reclaim_records = Vec::new();
        let reclaim = gauge_walk(
            for_each_reclaim_class(transport, |record| {
                reclaim_records.push(*record);
                Ok(())
            }),
            reclaim_records,
        );

        let mut load_records = Vec::new();
        let cpu_loads = gauge_walk(
            for_each_cpu_load(transport, |record| {
                load_records.push(*record);
                Ok(())
            }),
            load_records,
        );

        self.sample_cpu_times(transport);

        let (processes, global_denied) = Self::sample_processes(transport);
        let interval_ns = match (self.prev_uptime_ns, uptime_ns) {
            (Some(prev), Some(now)) if now > prev => now - prev,
            _ => 0,
        };
        self.derive_proc_pct(&processes, interval_ns);
        self.proc_history = processes
            .iter()
            .map(|record| ProcHistory {
                proc_id: record.proc_id,
                cpu_time_ns: record.cpu_time_ns,
            })
            .collect();
        self.prev_uptime_ns = uptime_ns;

        if let Gauge::Ready(stats) = &pressure {
            if self.band_history.len() == BAND_HISTORY_MAX {
                self.band_history.remove(0);
            }
            self.band_history.push(stats.band);
        }

        self.snapshot = Snapshot {
            uptime_ns,
            load,
            memory,
            pressure,
            reclaim,
            ramzip,
            cpu_loads,
            processes,
            global_denied,
        };
    }

    /// Sample the per-CPU cumulative busy/idle times and derive each CPU's
    /// busy share: the delta against the previous sample when one exists
    /// (the interval view), else the cumulative since-boot ratio (the
    /// honest first frame). A failed walk leaves the shares empty.
    fn sample_cpu_times(&mut self, transport: &dyn Transport) {
        let mut now: Vec<(u32, u64, u64)> = Vec::new();
        let walked = for_each_cpu_time(transport, |record| {
            now.push((record.cpu, record.busy_ns, record.idle_ns));
            Ok(())
        });
        if walked.is_err() {
            self.cpu_busy = Vec::new();
            return;
        }
        self.cpu_busy = now
            .iter()
            .map(|&(cpu, busy, idle)| {
                let (dbusy, didle) = match self
                    .prev_cpu_times
                    .iter()
                    .find(|(prev_cpu, _, _)| *prev_cpu == cpu)
                {
                    Some(&(_, pbusy, pidle)) => {
                        (busy.saturating_sub(pbusy), idle.saturating_sub(pidle))
                    }
                    None => (busy, idle),
                };
                let total = dbusy.saturating_add(didle);
                let busy_tenths = if total == 0 {
                    0
                } else {
                    u32::try_from(u128::from(dbusy) * 1000 / u128::from(total)).unwrap_or(1000)
                };
                CpuBusy {
                    cpu,
                    busy_tenths: busy_tenths.min(1000),
                }
            })
            .collect();
        self.prev_cpu_times = now;
    }

    /// Sample the process list: the system-wide census when the service
    /// grants it, else the caller's own with the refusal recorded. Any
    /// other failure leaves the census empty (its honest absence).
    fn sample_processes(transport: &dyn Transport) -> (Vec<ProcessRecord>, bool) {
        let mut records = Vec::new();
        match for_each_process(transport, true, |record| {
            records.push(*record);
            Ok(())
        }) {
            Ok(()) => (records, false),
            Err(ListError::Call(CallError::PermissionDenied)) => {
                records.clear();
                let _ = for_each_process(transport, false, |record| {
                    records.push(*record);
                    Ok(())
                });
                (records, true)
            }
            Err(_) => (Vec::new(), false),
        }
    }

    /// Derive each process's `%CPU` over the refresh interval from the
    /// previous sample's history, keyed by the never-reused `proc_id`. A
    /// process first seen this refresh (or an unmeasurable interval) has no
    /// observed share — zero, never a fabricated since-boot guess.
    fn derive_proc_pct(&mut self, processes: &[ProcessRecord], interval_ns: u64) {
        self.proc_pct = processes
            .iter()
            .map(|record| {
                let pct = match self
                    .proc_history
                    .iter()
                    .find(|entry| entry.proc_id == record.proc_id)
                {
                    Some(entry) if interval_ns > 0 => {
                        let delta = record.cpu_time_ns.saturating_sub(entry.cpu_time_ns);
                        u32::try_from(u128::from(delta) * 1000 / u128::from(interval_ns))
                            .unwrap_or(u32::MAX)
                    }
                    _ => 0,
                };
                (record.proc_id, pct)
            })
            .collect();
    }

    /// Handle one decoded input [`Event`] and report what the loop should
    /// do.
    pub fn handle_event(&mut self, event: &Event) -> Action {
        match event {
            Event::Char('q' | 'Q') => Action::Quit,
            Event::Char('r' | 'R') => Action::Refresh,
            Event::Char('p' | 'P') => {
                self.focus = self.focus.next();
                self.scroll = 0;
                Action::Redraw
            }
            Event::Char('+' | '=') => self.bump_delay(true),
            Event::Char('-' | '_') => self.bump_delay(false),
            Event::Up => self.scroll_by(-1),
            Event::Down => self.scroll_by(1),
            Event::PageUp => self.scroll_by(-self.page_step()),
            Event::PageDown => self.scroll_by(self.page_step()),
            Event::Home => {
                if self.scroll == 0 {
                    Action::Ignore
                } else {
                    self.scroll = 0;
                    Action::Redraw
                }
            }
            Event::End => {
                // The renderer clamps against the panel's line count, so
                // "end" just asks for the largest offset it will allow.
                self.scroll = usize::MAX;
                Action::Redraw
            }
            Event::Char('?' | 'h') => {
                self.show_help = !self.show_help;
                Action::Redraw
            }
            _ => Action::Ignore,
        }
    }

    /// One page of movement: a viewport's worth of rows.
    fn page_step(&self) -> isize {
        isize::try_from(self.viewport).unwrap_or(1).max(1)
    }

    /// Move the detail scroll by `delta` rows (clamped at zero; the upper
    /// bound is clamped by the renderer against the panel's line count).
    fn scroll_by(&mut self, delta: isize) -> Action {
        let current = isize::try_from(self.scroll).unwrap_or(isize::MAX);
        let target = current.saturating_add(delta).max(0);
        let target = usize::try_from(target).unwrap_or(0);
        if target == self.scroll {
            return Action::Ignore;
        }
        self.scroll = target;
        Action::Redraw
    }

    /// Lengthen (`up = true`) or shorten the refresh interval by one step,
    /// clamped into `[MIN_DELAY_TENTHS, MAX_DELAY_TENTHS]`.
    fn bump_delay(&mut self, up: bool) -> Action {
        let next = if up {
            self.delay_tenths
                .saturating_add(DELAY_STEP_TENTHS)
                .min(MAX_DELAY_TENTHS)
        } else {
            self.delay_tenths
                .saturating_sub(DELAY_STEP_TENTHS)
                .max(MIN_DELAY_TENTHS)
        };
        if next == self.delay_tenths {
            return Action::Ignore;
        }
        self.delay_tenths = next;
        Action::Redraw
    }
}
