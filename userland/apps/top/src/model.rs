//! The `top` view model: the sampled system summary and per-process rows
//! plus the cursor, scroll position, scope, and help-overlay state, and how
//! an input [`Event`] moves them.
//!
//! The model is deliberately free of any terminal I/O so it is exhaustively
//! testable on the host: the [`crate::app`] glue feeds it
//! events and asks [`crate::app::render`] to draw it. All of curses'
//! geometry quirks (clamping a selection, keeping it on screen) live here
//! once, as do the GNU-`top`-style statistics: the per-refresh `%CPU`
//! delta, the exponentially weighted `WCPU` average, and the
//! sort-by-`%CPU` row order.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::sysinfo::{KernelMemoryStats, LoadAverage, ProcessRecord, SysinfoQueryId, Uptime};
use tairix_abi::ProcId;
use tairix_curses::Event;
use tairix_procinfo::{call, for_each_process, CallError, CpuTotals, Transport, WalkStep};

use crate::error::TopError;

/// Which processes the viewer is showing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// The caller's own processes (the ungated `SELF_PROCESS_LIST`).
    Own,
    /// Every process system-wide (`GLOBAL_PROCESS_LIST`, which the service
    /// gates on `CAP_SYSINFO_GLOBAL`).
    All,
}

impl Scope {
    /// Whether this scope is the system-wide view.
    #[must_use]
    pub const fn is_all(self) -> bool {
        matches!(self, Scope::All)
    }

    /// A short human label for the status line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Scope::Own => "own",
            Scope::All => "all",
        }
    }
}

/// What the [`crate::app`] loop should do after handling an event.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Nothing changed; no redraw is required.
    Ignore,
    /// View state changed; redraw from the existing snapshot.
    Redraw,
    /// The snapshot is stale (the scope changed or a refresh was asked for);
    /// re-query the service, then redraw.
    Refresh,
    /// The user asked to quit.
    Quit,
}

/// The one-line notice shown when the system-wide view was refused and the
/// viewer fell back to the caller's own processes.
pub const ALL_DENIED_NOTICE: &str = "all view denied: capability not held";

/// One process row: the sampled record plus the statistics derived from the
/// previous refresh.
///
/// Percentages are carried in **tenths of a percent** (`123` renders as
/// `12.3`) so the model stays integer-only; the renderer owns the decimal
/// point.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Row {
    /// The sampled `sysinfo-v1` record.
    pub record: ProcessRecord,
    /// CPU share over the interval since the previous refresh, in tenths
    /// of a percent. Zero on a process's first appearance (there is no
    /// interval to measure yet) and when no interval could be measured.
    pub pct_cpu_tenths: u32,
    /// Weighted CPU share: an exponentially weighted moving average of the
    /// per-refresh `%CPU` samples (`wcpu' = (wcpu + pct) / 2`), in tenths
    /// of a percent. Smooths bursts the instantaneous column whipsaws on.
    pub wcpu_tenths: u32,
}

/// The busy/idle utilisation split in tenths of a percent (summing to
/// 1000 when defined), derived from two [`CpuTotals`] samples.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CpuSplit {
    /// Share of the interval the CPUs spent running tasks, in tenths of a
    /// percent.
    pub busy_tenths: u32,
    /// Share of the interval the CPUs sat idle, in tenths of a percent.
    pub idle_tenths: u32,
}

/// The sampled system summary drawn above the process list.
///
/// Each figure is independently optional so a query the service refuses or
/// cannot answer degrades that one line, never the whole session: the
/// memory query is gated on `CAP_SYSINFO_KERNEL` and its refusal is the
/// expected answer for an unentitled user.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Summary {
    /// Monotonic nanoseconds since boot, when the `UPTIME` query answered.
    pub uptime_ns: Option<u64>,
    /// The damped run-queue averages and task/user census, when the
    /// `LOAD_AVERAGE` query answered.
    pub load: Option<LoadAverage>,
    /// Kernel memory statistics, when `KERNEL_MEMORY_STATS` answered.
    pub memory: Option<KernelMemoryStats>,
    /// Whether the memory query was refused for want of
    /// `CAP_SYSINFO_KERNEL` (rendered as an explicit refusal, never a
    /// silent blank).
    pub memory_denied: bool,
    /// The aggregate CPU-time sample, when `CPU_TIME_STATS` answered.
    pub cpu: Option<CpuTotals>,
}

/// Per-process carry-over from the previous refresh: the cumulative CPU
/// time and the smoothed `WCPU`, keyed by the never-reused [`ProcId`] so a
/// numeric-PID reuse can never inherit another lifetime's statistics.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct History {
    proc_id: ProcId,
    cpu_time_ns: u64,
    wcpu_tenths: u32,
}

/// The live `top` view state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Model {
    rows: Vec<Row>,
    summary: Summary,
    history: Vec<History>,
    prev_uptime_ns: Option<u64>,
    prev_cpu: Option<CpuTotals>,
    cpu_split: Option<CpuSplit>,
    users: Vec<(u32, String)>,
    scope: Scope,
    selected: usize,
    top: usize,
    viewport: usize,
    show_help: bool,
    notice: Option<&'static str>,
}

impl Model {
    /// A fresh, empty model for the given `scope`. Call [`Model::refresh`]
    /// to populate it.
    #[must_use]
    pub fn new(scope: Scope) -> Model {
        Model {
            rows: Vec::new(),
            summary: Summary::default(),
            history: Vec::new(),
            prev_uptime_ns: None,
            prev_cpu: None,
            cpu_split: None,
            users: Vec::new(),
            scope,
            selected: 0,
            top: 0,
            viewport: 1,
            show_help: false,
            notice: None,
        }
    }

    /// Install the uid → username map resolved from the user database, used
    /// by the `USER` column. An empty map (the database was unreadable, or
    /// the caller lacks `CAP_USERS_READ`) degrades every row to its numeric
    /// uid — the honest answer, never a fabricated name.
    pub fn set_user_names(&mut self, users: Vec<(u32, String)>) {
        self.users = users;
    }

    /// The username for `uid`, when the installed map knows it.
    #[must_use]
    pub fn user_name(&self, uid: u32) -> Option<&str> {
        self.users
            .iter()
            .find(|(id, _)| *id == uid)
            .map(|(_, name)| name.as_str())
    }

    /// Post a status-line notice (cleared by the next handled key).
    pub fn set_notice(&mut self, notice: &'static str) {
        self.notice = Some(notice);
    }

    /// Re-query the process list and summary figures for the current
    /// [`Scope`] and adopt the fresh sample: rows gain their `%CPU` /
    /// `WCPU` statistics against the previous sample and are sorted by
    /// `%CPU` descending (cumulative time, then PID, break ties), and the
    /// selection is clamped into the new bounds.
    ///
    /// The optional summary queries degrade line-by-line: a refused memory
    /// query is recorded as the refusal it is, and a failed uptime or load
    /// query leaves that figure absent. Only the process list itself is
    /// load-bearing.
    ///
    /// # Errors
    ///
    /// * [`TopError::PermissionDenied`] — the system-wide view was refused
    ///   for want of `CAP_SYSINFO_GLOBAL`.
    /// * [`TopError::Service`] — the transport failed or the reply did not
    ///   decode against `sysinfo-v1`.
    pub fn refresh(&mut self, transport: &dyn Transport) -> Result<(), TopError> {
        let mut records = Vec::new();
        for_each_process(transport, self.scope.is_all(), |record| {
            records.push(*record);
            Ok(WalkStep::Continue)
        })?;

        self.summary = Self::sample_summary(transport);
        self.cpu_split = Self::split_cpu(self.prev_cpu, self.summary.cpu);
        self.prev_cpu = self.summary.cpu.or(self.prev_cpu);

        // The wall-clock interval since the previous refresh, from the same
        // monotonic uptime source on both sides. Absent (zero) on the first
        // sample and when either uptime read failed.
        let interval_ns = match (self.prev_uptime_ns, self.summary.uptime_ns) {
            (Some(prev), Some(now)) if now > prev => now - prev,
            _ => 0,
        };

        let mut rows: Vec<Row> = records
            .iter()
            .map(|record| self.build_row(record, interval_ns))
            .collect();
        // GNU top's default order: the biggest CPU consumer first. The
        // cumulative-time and PID tie-breaks keep the order stable between
        // refreshes of an idle system.
        rows.sort_by(|a, b| {
            b.pct_cpu_tenths
                .cmp(&a.pct_cpu_tenths)
                .then(b.record.cpu_time_ns.cmp(&a.record.cpu_time_ns))
                .then(a.record.pid.cmp(&b.record.pid))
        });

        self.history = rows
            .iter()
            .map(|row| History {
                proc_id: row.record.proc_id,
                cpu_time_ns: row.record.cpu_time_ns,
                wcpu_tenths: row.wcpu_tenths,
            })
            .collect();
        self.prev_uptime_ns = self.summary.uptime_ns;
        self.rows = rows;
        self.clamp_selection();
        Ok(())
    }

    /// Derive one row's statistics from the previous sample's history.
    fn build_row(&self, record: &ProcessRecord, interval_ns: u64) -> Row {
        let previous = self
            .history
            .iter()
            .find(|entry| entry.proc_id == record.proc_id);
        let (pct_cpu_tenths, wcpu_tenths) = match previous {
            Some(entry) if interval_ns > 0 => {
                let delta = record.cpu_time_ns.saturating_sub(entry.cpu_time_ns);
                // Tenths of a percent: delta/interval * 1000, in 128-bit
                // space so a long interval can never overflow.
                let pct = u128::from(delta) * 1000 / u128::from(interval_ns);
                let pct = u32::try_from(pct).unwrap_or(u32::MAX);
                (pct, u32::midpoint(entry.wcpu_tenths, pct))
            }
            // A process first seen this refresh (or an unmeasurable
            // interval) has no observed share yet; zero is the truthful
            // answer, never a fabricated since-boot guess.
            Some(entry) => (0, entry.wcpu_tenths / 2),
            None => (0, 0),
        };
        Row {
            record: *record,
            pct_cpu_tenths,
            wcpu_tenths,
        }
    }

    /// Derive the busy/idle split from the previous and current aggregate
    /// samples.
    ///
    /// With a previous sample the split describes the interval between the
    /// two (counter deltas, exactly as GNU `top` differences `/proc/stat`);
    /// the first sample falls back to the cumulative since-boot ratio, and
    /// an absent or empty sample yields no split at all — the line renders
    /// its absence, never a fabricated figure.
    fn split_cpu(prev: Option<CpuTotals>, now: Option<CpuTotals>) -> Option<CpuSplit> {
        let now = now?;
        let busy_tenths = u32::from(CpuTotals::busy_permille(prev.unwrap_or_default(), now)?);
        Some(CpuSplit {
            busy_tenths,
            idle_tenths: 1000 - busy_tenths,
        })
    }

    /// Sample the summary queries, each degrading independently.
    fn sample_summary(transport: &dyn Transport) -> Summary {
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
        let (memory, memory_denied) =
            match call(transport, SysinfoQueryId::KERNEL_MEMORY_STATS, &[]) {
                Ok(bytes) => (KernelMemoryStats::from_bytes(&bytes).ok(), false),
                Err(CallError::PermissionDenied) => (None, true),
                Err(CallError::Service(_)) => (None, false),
            };
        let cpu = CpuTotals::fetch_all(transport).ok().flatten();
        Summary {
            uptime_ns,
            load,
            memory,
            memory_denied,
            cpu,
        }
    }

    /// Re-query like [`Model::refresh`], but treat a *refused system-wide
    /// view* as the non-fatal answer it is: fall back to the caller's own
    /// processes, requery, and post [`ALL_DENIED_NOTICE`] on the status
    /// line so the user learns why the scope did not change — the viewer
    /// keeps running. Every other failure (and a refusal of the caller's
    /// *own* listing, which no capability gates) stays fatal and
    /// propagates.
    ///
    /// # Errors
    ///
    /// As [`Model::refresh`], except [`TopError::PermissionDenied`] for the
    /// system-wide scope, which is absorbed into the fallback.
    pub fn refresh_recovering(&mut self, transport: &dyn Transport) -> Result<(), TopError> {
        match self.refresh(transport) {
            Err(TopError::PermissionDenied) if self.scope.is_all() => {
                self.scope = Scope::Own;
                self.refresh(transport)?;
                self.notice = Some(ALL_DENIED_NOTICE);
                Ok(())
            }
            outcome => outcome,
        }
    }

    /// The status-line notice posted by the last recovered refusal, if any.
    /// Cleared by the next handled key.
    #[must_use]
    pub const fn notice(&self) -> Option<&'static str> {
        self.notice
    }

    /// The process rows currently held, in display order.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The sampled system summary.
    #[must_use]
    pub const fn summary(&self) -> &Summary {
        &self.summary
    }

    /// The busy/idle utilisation split derived from the last refresh, when
    /// the CPU-time query answered.
    #[must_use]
    pub const fn cpu_split(&self) -> Option<CpuSplit> {
        self.cpu_split
    }

    /// The current scope.
    #[must_use]
    pub const fn scope(&self) -> Scope {
        self.scope
    }

    /// The index of the selected row, or `None` when the list is empty.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// The index of the first process row drawn (the scroll offset).
    #[must_use]
    pub const fn scroll_top(&self) -> usize {
        self.top
    }

    /// Whether the help overlay is showing.
    #[must_use]
    pub const fn help_visible(&self) -> bool {
        self.show_help
    }

    /// Tell the model how many process rows fit on screen, so it can keep
    /// the selection visible. A zero is treated as one row.
    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        self.scroll_into_view();
    }

    /// Handle one decoded input [`Event`] and report what the loop should
    /// do.
    ///
    /// Any handled key clears the status-line notice: a notice describes
    /// the previous action's outcome, and the next action supersedes it.
    pub fn handle_event(&mut self, event: &Event) -> Action {
        self.notice = None;
        match event {
            Event::Char('q' | 'Q') => Action::Quit,
            Event::Up => self.move_selection(-1),
            Event::Down => self.move_selection(1),
            Event::PageUp => self.move_selection(-(self.page_step())),
            Event::PageDown => self.move_selection(self.page_step()),
            Event::Home => self.move_to(0),
            Event::End => self.move_to(self.rows.len().saturating_sub(1)),
            Event::Char('a' | 'A') => self.toggle_scope(),
            Event::Char('r' | 'R') => Action::Refresh,
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

    /// Flip between the own-processes and system-wide views, asking for a
    /// re-query against the new scope.
    fn toggle_scope(&mut self) -> Action {
        self.scope = match self.scope {
            Scope::Own => Scope::All,
            Scope::All => Scope::Own,
        };
        Action::Refresh
    }

    /// Move the selection by `delta` rows (clamped to the list), keeping it
    /// on screen. Returns [`Action::Redraw`] when something moved.
    fn move_selection(&mut self, delta: isize) -> Action {
        if self.rows.is_empty() {
            return Action::Ignore;
        }
        let last = self.rows.len() - 1;
        let current = isize::try_from(self.selected).unwrap_or(0);
        let target = (current + delta).clamp(0, isize::try_from(last).unwrap_or(0));
        let target = usize::try_from(target).unwrap_or(0);
        self.move_to(target)
    }

    /// Move the selection to an absolute row, keeping it on screen.
    fn move_to(&mut self, index: usize) -> Action {
        if self.rows.is_empty() {
            return Action::Ignore;
        }
        let clamped = index.min(self.rows.len() - 1);
        if clamped == self.selected {
            return Action::Ignore;
        }
        self.selected = clamped;
        self.scroll_into_view();
        Action::Redraw
    }

    /// Clamp the selection (and scroll) into the current list bounds, used
    /// after a refresh changes the row count.
    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            self.top = 0;
            return;
        }
        self.selected = self.selected.min(self.rows.len() - 1);
        self.scroll_into_view();
    }

    /// Adjust the scroll offset so the selected row lies within the
    /// viewport.
    fn scroll_into_view(&mut self) {
        if self.rows.is_empty() {
            self.top = 0;
            return;
        }
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + self.viewport {
            self.top = self.selected + 1 - self.viewport;
        }
        let max_top = self.rows.len().saturating_sub(self.viewport);
        self.top = self.top.min(max_top);
    }
}
