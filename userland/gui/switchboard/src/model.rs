//! Build the live [`SwitchboardModel`] the panel renders from a [`Sample`],
//! the session's [`SeatReport`], and the service's own [`Activities`]
//! grouping state, and map each interactive [`SwitchboardAction`] the
//! composed control reports back onto the outbound [`Effect`]s it implies.
//!
//! This module performs none of these effects itself and never fabricates
//! a row it cannot back with a real reading: `jobs`, `services`, and
//! `system_actions` are left empty because the OS exposes no background-job
//! registry, no service-enumeration query in the System Information API,
//! and no power/lock interface this service may drive (see the crate
//! docs). [`crate::panel`] applies the effects through its host seam; this
//! module decides *what* to do, never *how*.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport};
use tairix_abi::sysinfo::{CrashFaultBucket, CrashFaultClass, ProcessState};
use tairix_abi::{CapabilityId, CapabilityQuery, Duration64, ProcId, SchedPriority, Signal};
use tairix_controls::{
    ActivityState, PressureKind, PressureState, ProgressValue, RecoveryState, WindowControlKind,
    MAX_CHART_SAMPLES,
};

use crate::activities::Activities;
use crate::derive::{memory_pressured, Hysteresis};
use crate::format::{format_bytes, format_duration, format_rate, percent};
use crate::sample::{DegradedField, ProcessSummary, Sample};
use crate::system_report::{build_system_report, reading, HeadlinePressure};
use crate::view::{
    ActionVerdict, ActivityControl, ActivityMember, ActivitySummary, CrashSnapshot, FaultImpact,
    FaultMark, PressureAction, PressureCause, PressureControl, Reading, RecoveryControl,
    RecoveryItem, Section, SwitchboardAction, SwitchboardModel, TaskAuthority, TaskControl,
    TaskKind, TaskSummary, Unmeasured,
};

/// Convert a wire [`CommandSection`] into the shared control's own
/// [`Section`] — the two are defined independently because `lib/abi` may
/// not depend on the userland control library, so the service is the one
/// place that maps between them.
#[must_use]
pub const fn map_section(command: CommandSection) -> Section {
    match command {
        CommandSection::Tasks => Section::Tasks,
        CommandSection::Jobs => Section::Jobs,
        CommandSection::Pressure => Section::Pressure,
        CommandSection::Activities => Section::Activities,
        CommandSection::Recovery => Section::Recovery,
        CommandSection::System => Section::System,
    }
}

/// Narrow a sampled scheduler task id to the signed process id the
/// `signal` syscall takes, refusing rather than truncating.
///
/// A truncated id would name a *different*, arbitrary process, so an id
/// beyond the syscall's width yields [`None`] and the action is not
/// attempted at all.
#[must_use]
pub fn signal_pid(owner: u64) -> Option<i32> {
    i32::try_from(owner).ok()
}

/// A process name rendered as display text: valid UTF-8 with a lossy
/// replacement for anything that is not, exactly as a hover readout or a
/// row label — display text carrying no authority — is built elsewhere in
/// this crate.
pub(crate) fn display_name(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
}

/// Whether a row owned by `uid` is controllable under `self_uid`/`authority`:
/// the row is the caller's own (the same-uid rule kill(2) itself uses) or
/// the caller holds the administrative capability. An unknown `self_uid`
/// (the service could not find its own row in the sample) is never
/// controllable through the same-uid rule — only the capability can still
/// grant it — so a lookup failure narrows authority rather than widening it.
fn controllable(uid: u32, self_uid: Option<u32>, authority: &dyn CapabilityQuery) -> bool {
    self_uid == Some(uid) || authority.holds(CapabilityId::PROC_CONTROL)
}

/// The rolling instrument state the panel's resource rows need that no single
/// [`Sample`] carries: the CPU chart's bounded history, and the pressure
/// verdicts the tray summary's own derivation already reached for the same
/// readings.
///
/// The history is held inline, capped at the chart's own
/// [`MAX_CHART_SAMPLES`] window, so recording a sample never allocates
/// and a service that runs for weeks never grows an unbounded log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveMeters {
    cpu_history: [u16; MAX_CHART_SAMPLES],
    cpu_len: usize,
    cpu_pressured: bool,
    memory_pressured: bool,
}

impl Default for LiveMeters {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveMeters {
    /// Empty history, neither resource pressured.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpu_history: [0; MAX_CHART_SAMPLES],
            cpu_len: 0,
            cpu_pressured: false,
            memory_pressured: false,
        }
    }

    /// Record `sample`'s readings and the CPU-pressure verdict `hysteresis`
    /// latched for the very same reading, so the panel's meter and the tray
    /// icon's rail can never disagree.
    ///
    /// Only a measured CPU reading enters the history: an interval the service
    /// could not measure contributes no point rather than a zero one, which
    /// would plot as a genuine idle moment. The oldest point is dropped once
    /// the window is full.
    pub fn record(&mut self, sample: &Sample, hysteresis: Hysteresis) {
        self.cpu_pressured = hysteresis.cpu_pressured();
        self.memory_pressured = memory_pressured(sample);
        let Some(busy) = sample.cpu_busy_permille else {
            return;
        };
        if self.cpu_len == MAX_CHART_SAMPLES {
            self.cpu_history.copy_within(1.., 0);
            self.cpu_len -= 1;
        }
        self.cpu_history[self.cpu_len] = busy;
        self.cpu_len += 1;
    }

    /// The recorded CPU readings, oldest first.
    #[must_use]
    pub fn cpu_history(&self) -> &[u16] {
        &self.cpu_history[..self.cpu_len]
    }

    /// Whether CPU pressure is latched active.
    #[must_use]
    pub const fn cpu_pressured(&self) -> bool {
        self.cpu_pressured
    }

    /// Whether the last memory reading was at or beyond the pressure band.
    #[must_use]
    pub const fn memory_pressured(&self) -> bool {
        self.memory_pressured
    }
}

/// The number of CPU readings kept per task for its row's sparkline.
///
/// The same window the resource charts plot, so a per-task spark and the
/// system CPU chart beside it cover the same span of time rather than two
/// arbitrary ones.
pub const TASK_HISTORY_LEN: usize = MAX_CHART_SAMPLES;

/// What one task's tracking holds between samples: the previous reading of
/// its own storage counters, and its recent CPU readings.
///
/// The counters are cumulative-since-start, so a *rate* needs the previous
/// reading and the interval between the two; the history is the row's own
/// sparkline, which no single sample carries.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskTrack {
    /// The task's total storage bytes (read plus written) as of the last
    /// recorded sample, which the next sample deltas against.
    io_bytes: u64,
    /// The rate that delta produced, in bytes per second, or `None` when
    /// this sample had nothing honest to divide.
    disk_bytes_per_sec: Option<u64>,
    /// Recent CPU readings, oldest first, capped at [`TASK_HISTORY_LEN`].
    cpu_history: Vec<u16>,
}

/// The per-task rolling state the task rows need that no single [`Sample`]
/// carries: each task's previous storage-counter reading (for an honest
/// bytes-per-second rate) and its own bounded CPU history (for the row's
/// sparkline).
///
/// Keyed by [`ProcId`] — the never-reused process identity — rather than by
/// pid or row index, so a recycled pid can never inherit a dead task's
/// history and a re-sorted or filtered table can never mis-attribute one.
///
/// Every entry is dropped the first sample its task is absent from
/// ([`Self::record`] rebuilds the map from the sample rather than mutating
/// it in place), so a machine that churns short-lived processes for weeks
/// accumulates nothing: the map is exactly as large as the live process
/// list, and each entry holds at most [`TASK_HISTORY_LEN`] readings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskMeters {
    tracks: BTreeMap<ProcId, TaskTrack>,
}

impl TaskMeters {
    /// No tasks tracked yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tracks: BTreeMap::new(),
        }
    }

    /// Fold `sample`'s process list in: delta each task's storage counters
    /// against its previous reading, extend its CPU history, and keep only
    /// the tasks this sample names.
    ///
    /// This runs *before* the rows are built, so a row reads the figures
    /// this sample produced rather than the last one's — and the delta is
    /// taken against the stored previous reading before that reading is
    /// replaced, so the two can never be confused.
    ///
    /// A task's CPU reading enters its history only when the sample
    /// measured one: an unmeasurable interval contributes no point rather
    /// than a zero one, which would plot as a genuine idle moment.
    pub fn record(&mut self, sample: &Sample) {
        let mut next = BTreeMap::new();
        for process in &sample.processes {
            let io_bytes = process
                .io_bytes_read
                .saturating_add(process.io_bytes_written);
            let previous = self.tracks.remove(&process.proc_id);
            let disk_bytes_per_sec = previous
                .as_ref()
                .and_then(|track| rate_per_sec(io_bytes, track.io_bytes, sample.elapsed_ns));
            let mut cpu_history = previous.map_or_else(Vec::new, |track| track.cpu_history);
            if let Some(permille) = process.cpu_permille {
                if cpu_history.len() >= TASK_HISTORY_LEN {
                    cpu_history.remove(0);
                }
                cpu_history.push(permille);
            }
            next.insert(
                process.proc_id,
                TaskTrack {
                    io_bytes,
                    disk_bytes_per_sec,
                    cpu_history,
                },
            );
        }
        self.tracks = next;
    }

    /// The task's recorded CPU readings, oldest first; empty for a task
    /// that has never been measured.
    #[must_use]
    pub fn cpu_history(&self, proc_id: ProcId) -> &[u16] {
        self.tracks
            .get(&proc_id)
            .map_or(&[][..], |track| track.cpu_history.as_slice())
    }

    /// The task's storage throughput as of the last [`Self::record`], in
    /// bytes per second.
    ///
    /// `None` — which the row renders as an explicit unmeasured mark, never
    /// a zero — when there was nothing honest to divide: the very first
    /// sample (no interval to measure over), a task appearing for the first
    /// time (no previous reading *of this task*), or a task this sample did
    /// not name at all. A counter that did not move over a real interval is
    /// a genuine measurement of `0` bytes per second and is reported as
    /// such.
    #[must_use]
    pub fn disk_rate(&self, proc_id: ProcId) -> Option<u64> {
        self.tracks.get(&proc_id)?.disk_bytes_per_sec
    }
}

/// When each faulted task was first observed to be faulted, so the surface
/// can say how long a fault has stood.
///
/// Nothing in the System Information API records *when* a task entered its
/// current state: a process record carries the state but no transition
/// timestamp. The only honest way to say "stopped 4m ago" is therefore for
/// this service to note the first sample it saw the fault in and measure
/// from there — which is what this does, clocked off the monotonic uptime
/// reading rather than a wall clock that an administrator can move.
///
/// Keyed by [`ProcId`] for the same reason [`TaskMeters`] is: a recycled pid
/// must never inherit a dead task's fault age. Entries are dropped the first
/// sample their task is no longer faulted, so a machine that churns faults
/// accumulates nothing and a task that recovers and faults again is timed
/// from its *new* fault, not its old one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FaultClock {
    first_seen: BTreeMap<ProcId, Duration64>,
    resolved: usize,
}

impl FaultClock {
    /// No faults observed yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            first_seen: BTreeMap::new(),
            resolved: 0,
        }
    }

    /// Note which tasks are faulted as of `sample`, keeping the instant
    /// each was *first* seen faulted and forgetting the rest.
    ///
    /// With no uptime reading there is no clock to measure against, so no
    /// instant is recorded and every age reads as unmeasured — never a
    /// fabricated zero. The map is still pruned in that case, so a stale
    /// instant cannot outlive the fault it belonged to.
    pub fn record(&mut self, sample: &Sample, seat_report: &SeatReport) {
        let now = sample.uptime.map(|uptime| uptime.since_boot);
        let mut next = BTreeMap::new();
        for process in &sample.processes {
            if process_recovery(process, seat_report) == RecoveryState::None {
                continue;
            }
            let Some(now) = now else {
                continue;
            };
            let first = self
                .first_seen
                .get(&process.proc_id)
                .copied()
                .unwrap_or(now);
            next.insert(process.proc_id, first);
        }
        let cleared = self
            .first_seen
            .keys()
            .filter(|proc_id| !next.contains_key(*proc_id))
            .count();
        self.resolved = self.resolved.saturating_add(cleared);
        self.first_seen = next;
    }

    /// How many faults this service has watched clear since it started.
    ///
    /// A fault that is gone from a sample it was in last time has cleared,
    /// which only the thing that folds one sample into the next can see. It
    /// is counted here rather than in the screen so that a screen refreshed
    /// with a model and a screen built from that same model are the same
    /// screen, and so a count of *observed* history is never mistaken for a
    /// figure the kernel reported.
    #[must_use]
    pub const fn resolved(&self) -> usize {
        self.resolved
    }

    /// How long the task has been faulted as of `now`, or `None` when no
    /// instant was ever recorded for it (no uptime reading to measure
    /// against, or a task that is not faulted).
    #[must_use]
    pub fn elapsed(&self, proc_id: ProcId, now: Option<Duration64>) -> Option<Duration64> {
        elapsed_since(self.first_seen.get(&proc_id).copied(), now)
    }
}

/// The duration between `first` and `now`, or `None` when either is
/// unmeasured or the clock appears to have moved backwards — which a
/// monotonic reading should never do — so a bad pair yields no duration
/// rather than a negative or wrapped one.
///
/// The one interval arithmetic every band-start clock on this screen
/// shares ([`FaultClock`] and [`PressureClock`]), so a fault's age and a
/// pressure cause's age can never silently diverge on how "how long" is
/// computed.
fn elapsed_since(first: Option<Duration64>, now: Option<Duration64>) -> Option<Duration64> {
    let first = first?;
    let now = now?;
    let (now, first) = (now.saturating_total_nanos(), first.saturating_total_nanos());
    (now >= first).then(|| Duration64::from_nanos(now.saturating_sub(first)))
}

/// When each pressured resource last entered its current pressure band, so
/// the Pressure section can say how long a cause has stood rather than
/// guessing.
///
/// The System Information API reports only whether a resource is pressured
/// right now, not when it became so, so — exactly as [`FaultClock`] does
/// for a standing task fault — this service notes the first sample it saw
/// a resource cross into pressure and measures from there. There are only
/// ever two resources this section flags (CPU, memory), so a fixed pair of
/// slots serves the purpose [`FaultClock`]'s map serves for an unbounded
/// set of tasks; a resource that leaves its band drops its instant
/// immediately, so a later re-entry is timed from its *new* start, not its
/// old one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PressureClock {
    cpu_since: Option<Duration64>,
    memory_since: Option<Duration64>,
}

impl PressureClock {
    /// Neither resource pressured yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpu_since: None,
            memory_since: None,
        }
    }

    /// Note this sample's latched pressure verdicts, entering or clearing
    /// each resource's band-start instant.
    ///
    /// With no uptime reading there is no clock to measure against, so a
    /// resource pressured this sample without one recorded before stays
    /// without a start instant and its age reads as unmeasured — never a
    /// fabricated zero.
    pub fn record(&mut self, cpu_pressured: bool, memory_pressured: bool, now: Option<Duration64>) {
        Self::latch(&mut self.cpu_since, cpu_pressured, now);
        Self::latch(&mut self.memory_since, memory_pressured, now);
    }

    /// Enter or clear one resource's band-start instant.
    fn latch(slot: &mut Option<Duration64>, pressured: bool, now: Option<Duration64>) {
        if !pressured {
            *slot = None;
        } else if slot.is_none() {
            *slot = now;
        }
    }

    /// How long CPU pressure has stood as of `now`, or `None` when CPU is
    /// not pressured or no start instant was ever recorded for it.
    #[must_use]
    pub fn cpu_elapsed(&self, now: Option<Duration64>) -> Option<Duration64> {
        elapsed_since(self.cpu_since, now)
    }

    /// How long memory pressure has stood as of `now`, under the same
    /// conditions as [`Self::cpu_elapsed`].
    #[must_use]
    pub fn memory_elapsed(&self, now: Option<Duration64>) -> Option<Duration64> {
        elapsed_since(self.memory_since, now)
    }
}

/// Everything the monitor carries *between* samples — every reading that no
/// single [`Sample`] can produce on its own.
///
/// A rate is a difference over an interval and a history is a sequence, so
/// neither can be read from one sample: both are folded forward from the
/// previous cycle. Keeping the whole-system meters and the per-task meters
/// as one thing means they are always folded forward from the *same*
/// sample, so a task's rate and the system chart beside it can never end up
/// describing two different intervals.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RollingMeters {
    /// The whole-system readings: the CPU chart's history and the latched
    /// pressure verdicts.
    pub system: LiveMeters,
    /// The per-task readings, keyed by the task's never-reused identity.
    pub tasks: TaskMeters,
    /// When each standing fault was first observed, so its age is measured
    /// rather than guessed.
    pub faults: FaultClock,
    /// When each pressured resource entered its current band, so the
    /// Pressure section's "how long" is measured rather than guessed.
    pub pressure: PressureClock,
}

impl RollingMeters {
    /// Nothing measured yet: no history either side, no pressure latched.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            system: LiveMeters::new(),
            tasks: TaskMeters::new(),
            faults: FaultClock::new(),
            pressure: PressureClock::new(),
        }
    }

    /// Fold `sample` in on both sides at once, with `hysteresis` the
    /// pressure verdict latched for that very reading.
    ///
    /// Called before the rows are built, so every figure a row shows came
    /// from this sample rather than the last one. A task the sample does
    /// not name is dropped here, so an exited task leaks neither its
    /// counters nor its history.
    pub fn record(&mut self, sample: &Sample, hysteresis: Hysteresis, seat_report: &SeatReport) {
        self.system.record(sample, hysteresis);
        self.tasks.record(sample);
        self.faults.record(sample, seat_report);
        let now = sample.uptime.map(|uptime| uptime.since_boot);
        self.pressure.record(
            self.system.cpu_pressured(),
            self.system.memory_pressured(),
            now,
        );
    }
}

/// Nanoseconds in one second, the scale a per-second rate is derived at.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// A per-second rate from two readings of one cumulative counter and the
/// interval between them, or `None` when the interval is unmeasured or
/// zero (there is nothing to divide by).
///
/// A counter that went *backwards* — which a cumulative counter should
/// never do — yields `0` rather than a wrapped, enormous rate: the delta
/// saturates, so a bad reading understates rather than invents. The
/// multiplication is done in [`u128`] so a fast device over a short
/// interval cannot overflow before the division brings it back in range.
fn rate_per_sec(now: u64, previous: u64, elapsed_ns: Option<u64>) -> Option<u64> {
    let elapsed_ns = elapsed_ns.filter(|ns| *ns > 0)?;
    let delta = u128::from(now.saturating_sub(previous));
    let per_sec = delta.saturating_mul(u128::from(NANOS_PER_SEC)) / u128::from(elapsed_ns);
    Some(u64::try_from(per_sec).unwrap_or(u64::MAX))
}

/// The identity backing one rendered task row, for the grouping actions a
/// task index alone cannot resolve.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskIdent {
    /// The row's never-reused process identity.
    proc_id: ProcId,
    /// The row's scheduler task id.
    pid: u64,
    /// The row's validated display name.
    name: String,
}

/// The live [`SwitchboardModel`] plus the per-row identity the model's own
/// row indices do not carry, so [`apply_action`] can resolve a reported
/// [`SwitchboardAction`] back to the scheduler task id(s) or activity it
/// names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PanelModel {
    /// The model the panel renders.
    pub model: SwitchboardModel,
    /// `task_owners[i]` is the pid backing `model.tasks[i]`.
    task_owners: Vec<u64>,
    /// `recovery_owners[i]` is the pid backing `model.recovery[i]`.
    recovery_owners: Vec<u64>,
    /// `pressure_targets[i]` is the culprit pid backing `model.pressure[i]`,
    /// or `None` when that cause named no culprit.
    pressure_targets: Vec<Option<u64>>,
    /// `task_idents[i]` is the identity backing `model.tasks[i]`.
    task_idents: Vec<TaskIdent>,
    /// `activity_ids[i]` is the stable activity id backing
    /// `model.activities[i]`.
    activity_ids: Vec<u64>,
    /// `activity_members[i]` is the scheduler task ids of `model.activities[i]`'s
    /// members that are joined to the current sample, in group order — the
    /// signal-sweep and activate targets a rendered [`ActivityMember`] (name/
    /// detail/activity only) cannot carry.
    activity_members: Vec<Vec<u64>>,
}

impl PanelModel {
    /// The pid backing `model.tasks[index]`, or `None` for an out-of-range
    /// index (fail closed — never a guess).
    #[must_use]
    pub fn task_owner(&self, index: usize) -> Option<u64> {
        self.task_owners.get(index).copied()
    }

    /// The pid backing `model.recovery[index]`, or `None` for an
    /// out-of-range index (fail closed).
    #[must_use]
    pub fn recovery_owner(&self, index: usize) -> Option<u64> {
        self.recovery_owners.get(index).copied()
    }

    /// The culprit pid backing `model.pressure[index]`, or `None` when the
    /// cause named no culprit, or the index is out of range (fail closed).
    #[must_use]
    pub fn pressure_target(&self, index: usize) -> Option<u64> {
        self.pressure_targets.get(index).copied().flatten()
    }

    /// The `(proc_id, pid, name)` backing `model.tasks[index]`, or `None`
    /// for an out-of-range index (fail closed).
    #[must_use]
    pub fn task_ident(&self, index: usize) -> Option<(ProcId, u64, &str)> {
        self.task_idents
            .get(index)
            .map(|ident| (ident.proc_id, ident.pid, ident.name.as_str()))
    }

    /// The stable activity id backing `model.activities[index]`, or `None`
    /// for an out-of-range index (fail closed).
    #[must_use]
    pub fn activity_id(&self, index: usize) -> Option<u64> {
        self.activity_ids.get(index).copied()
    }

    /// The joined member pids of `model.activities[index]`, in group order,
    /// or an empty slice for an out-of-range index (fail closed).
    #[must_use]
    pub fn activity_members(&self, index: usize) -> &[u64] {
        self.activity_members
            .get(index)
            .map_or(&[][..], Vec::as_slice)
    }
}

/// The service's own uid, found by its own pid's row in `sample`.
///
/// `None` when the row is not present this sample — a fresh service before
/// its first process-list read, or one whose process-list query itself
/// degraded this cycle. An unknown uid narrows authority rather than
/// widening it: `controllable` never grants the same-uid rule on a `None`,
/// only the administrative capability can still act.
#[must_use]
pub fn derive_self_uid(sample: &Sample, self_pid: u64) -> Option<u32> {
    sample
        .processes
        .iter()
        .find(|process| process.pid == self_pid)
        .map(|process| process.uid)
}

/// Build the live [`PanelModel`] from this sample, the monitor's
/// [`RollingMeters`], the seat's latest unresponsive-owner report, and the
/// service's own [`Activities`] grouping state.
///
/// `self_uid` is the service's own uid, derived by the caller from its own
/// pid's row in `sample` (an unknown uid narrows authority rather than
/// widening it — see `controllable`). `meters` must already have this
/// sample folded in, so the rows carry this cycle's rates and histories
/// rather than the previous cycle's. `authority` is queried once per
/// action kind that needs it, so the rendered [`SwitchboardModel`] and the
/// effect [`apply_action`] later produces from the same authority can never
/// disagree about whether an action is available.
#[must_use]
pub fn build_model(
    title: &str,
    sample: &Sample,
    seat_report: &SeatReport,
    meters: &RollingMeters,
    authority: &dyn CapabilityQuery,
    activities: &Activities,
    self_uid: Option<u32>,
) -> PanelModel {
    let mut model = SwitchboardModel::new(title);
    let can_force = authority.holds(CapabilityId::PROC_CONTROL);

    let (tasks, task_owners, task_idents) = build_tasks(
        &sample.processes,
        seat_report,
        &meters.tasks,
        activities,
        can_force,
    );
    let (recovery, recovery_owners) = build_recovery(sample, seat_report, meters, can_force);
    let (pressure, pressure_targets) = build_pressure(sample, meters, self_uid, authority);
    let (activity_summaries, activity_ids, activity_members) =
        build_activities(activities, sample, &meters.tasks, self_uid, authority);

    model.tasks = tasks;
    model.system = build_system_report(
        sample,
        meters.system.cpu_history(),
        HeadlinePressure {
            cpu: meters.system.cpu_pressured(),
            memory: meters.system.memory_pressured(),
        },
        authority,
    );
    model.recovery = recovery;
    model.recovery_resolved = meters.faults.resolved();
    model.pressure = pressure;
    model.activities = activity_summaries;
    model.can_create_activity = activities.can_create();

    PanelModel {
        model,
        task_owners,
        recovery_owners,
        pressure_targets,
        task_idents,
        activity_ids,
        activity_members,
    }
}

/// One [`TaskSummary`] per sampled process, in sampled order, each row
/// naming its own pid and identity so [`apply_action`] can resolve it.
///
/// A row carries no Pressure Rail: the System Information API reports a
/// process's CPU time, not which resource that process is straining, so
/// naming one would be a guess dressed as a measurement.
///
/// Every row's kind is [`TaskKind::Process`], because that is what the
/// process list reports. Nothing here reads a background-job registry or a
/// service registry — neither exists — so no row claims to be a job or a
/// service, and the counts for those two are honest zeroes rather than a
/// classification of processes nobody measured.
///
/// The disk rate and the sparkline history come from `meters`, which the
/// caller has already folded this sample into.
fn build_tasks(
    processes: &[ProcessSummary],
    seat_report: &SeatReport,
    meters: &TaskMeters,
    activities: &Activities,
    can_force: bool,
) -> (Vec<TaskSummary>, Vec<u64>, Vec<TaskIdent>) {
    let mut tasks = Vec::with_capacity(processes.len());
    let mut owners = Vec::with_capacity(processes.len());
    let mut idents = Vec::with_capacity(processes.len());
    for process in processes {
        let name = display_name(&process.name);
        tasks.push(TaskSummary {
            proc_id: process.proc_id,
            name: name.clone(),
            kind: TaskKind::Process,
            lifecycle: Some(process.state),
            cpu_permille: process.cpu_permille,
            memory_bytes: Some(process.mem_bytes),
            disk_bytes_per_sec: meters.disk_rate(process.proc_id),
            cpu_history: meters.cpu_history(process.proc_id).to_vec(),
            pressure: PressureState::None,
            activity: process_activity(process.state),
            recovery: process_recovery(process, seat_report),
            authority: task_authority(process.state, can_force),
            group: activities.group_index_of(process.proc_id),
        });
        owners.push(process.pid);
        idents.push(TaskIdent {
            proc_id: process.proc_id,
            pid: process.pid,
            name,
        });
    }
    (tasks, owners, idents)
}

/// What the caller may do to one sampled process.
///
/// Two things decide each command: the caller's own authority, and the
/// task's lifecycle state. Signalling a task — pausing it, continuing it,
/// ending it — needs `PROC_CONTROL`, so without it those wear the Authority
/// Mark; with it, the state still rules out the ones that make no sense for
/// this task, which is a plain disablement rather than a refusal of
/// authority. A task that has already exited can be signalled to no effect,
/// so nothing is offered for it at all.
///
/// Raising a task's window is a plain session request needing no capability
/// to *attempt* — the session is free to refuse it — and lowering a priority
/// is the same signal-level authority as the rest.
fn task_authority(state: ProcessState, can_force: bool) -> TaskAuthority {
    let signal = |permitted: bool| match (can_force, permitted) {
        (false, _) => ActionVerdict::DeniedByAuthority,
        (true, false) => ActionVerdict::DisabledByState,
        (true, true) => ActionVerdict::Ready,
    };
    let live = !matches!(state, ProcessState::Zombie);
    TaskAuthority {
        switch: if live {
            ActionVerdict::Ready
        } else {
            ActionVerdict::DisabledByState
        },
        pause: signal(live && state != ProcessState::Stopped),
        resume: signal(state == ProcessState::Stopped),
        lower_priority: signal(live && state != ProcessState::Stopped),
        force_quit: signal(live),
    }
}

/// The recovery posture a sampled process is in.
///
/// The one definition of "this task is in trouble", shared by the task
/// row's Signal Bead (and the fault filter that counts it) and the
/// Recovery section's own list, so the table can never disagree with the
/// list about which tasks are faulted.
///
/// A process the scheduler reports stopped is recoverable; one the seat
/// named unresponsive is hung. Stopped wins when both hold, matching the
/// Recovery list, which reports a stopped process once rather than twice.
fn process_recovery(process: &ProcessSummary, seat_report: &SeatReport) -> RecoveryState {
    if process.state == ProcessState::Stopped {
        return RecoveryState::Recoverable;
    }
    if seat_report.owners().contains(&process.pid) {
        return RecoveryState::Hung;
    }
    RecoveryState::None
}

/// The [`ActivityState`] a process's [`ProcessState`] implies.
///
/// A live process — on a CPU, waiting for one, or blocked awaiting an
/// event it will be woken by — has work in progress whose extent the row
/// does not quantify. A process that has exited but not yet been reaped,
/// or that a job-control signal stopped, has none.
const fn process_activity(state: ProcessState) -> ActivityState {
    match state {
        ProcessState::Runnable | ProcessState::Running | ProcessState::Blocked => {
            ActivityState::Working
        }
        ProcessState::Zombie | ProcessState::Stopped => ActivityState::Idle,
    }
}

/// One [`RecoveryItem`] per stopped process this service sampled itself,
/// plus one per unresponsive owner the seat report names that this
/// service's own sample can still put a name to; an owner the report names
/// but this sample never saw contributes no row (never a fabricated one) —
/// the join is against the names this service attested itself, never a
/// name taken from the wire.
///
/// Each item carries everything the Recovery screen shows about that one
/// fault: its stable identity, how long it has stood (from `meters`'
/// [`FaultClock`], the only honest source), what to do about it, the marks
/// this service observed, the kernel's crash record where the fault raised
/// one, and the task's own resource cost.
fn build_recovery(
    sample: &Sample,
    seat_report: &SeatReport,
    meters: &RollingMeters,
    can_force: bool,
) -> (Vec<RecoveryItem>, Vec<u64>) {
    let mut recovery = Vec::new();
    let mut owners = Vec::new();
    let now = sample.uptime.map(|uptime| uptime.since_boot);

    // Stopped processes first, then the unresponsive ones, so the list
    // groups by condition rather than by sampled order. Both conditions
    // are read through the one shared classifier, so this list and the
    // task rows' own Signal Beads can never disagree about which tasks
    // are faulted. A process that is both is reported once, as stopped,
    // because that is what the classifier resolves it to.
    for state in [RecoveryState::Recoverable, RecoveryState::Hung] {
        for process in &sample.processes {
            if process_recovery(process, seat_report) != state {
                continue;
            }
            let since = elapsed_reading(meters.faults.elapsed(process.proc_id, now), sample);
            recovery.push(RecoveryItem {
                proc_id: process.proc_id,
                pid: process.pid,
                name: display_name(&process.name),
                detail: String::from(fault_detail(state)),
                since: since.clone(),
                recovery: state,
                impact: FaultImpact::of(state),
                status: String::from(fault_status(state)),
                recommendation: String::from(fault_recommendation(state, can_force)),
                marks: fault_marks(state, &since),
                crash: crash_snapshot(sample, process.proc_id),
                cpu: cpu_reading(process.cpu_permille, sample),
                memory: Reading::measured(format_bytes(process.mem_bytes)),
                disk: disk_reading(meters.tasks.disk_rate(process.proc_id), sample),
                network: Reading::Absent(Unmeasured::NoInterface),
                can_restart: true,
                can_force,
            });
            owners.push(process.pid);
        }
    }

    (recovery, owners)
}

/// The trailing detail a fault's card shows: what happened, in the fewest
/// words that distinguish the two conditions.
const fn fault_detail(state: RecoveryState) -> &'static str {
    match state {
        RecoveryState::Hung => "not responding",
        _ => "stopped",
    }
}

/// The plain statement of what state a fault is in.
const fn fault_status(state: RecoveryState) -> &'static str {
    match state {
        RecoveryState::Hung => "The task is running but has stopped answering its seat.",
        _ => "The task has been stopped and is holding what it held.",
    }
}

/// What a reader should do about a fault.
///
/// The recommendation names only a command the caller can actually take: a
/// caller without process control is told to restart rather than pointed at
/// a force they would be refused.
const fn fault_recommendation(state: RecoveryState, can_force: bool) -> &'static str {
    match (state, can_force) {
        (RecoveryState::Hung, true) => "Restart it; force it only if the restart does not take.",
        (RecoveryState::Hung, false) => "Restart it.",
        (_, _) => "Restart it to release what it is holding.",
    }
}

/// How long something has stood, or why that cannot be said.
///
/// A duration is only ever a measured one: with no uptime reading there is
/// no clock to measure against, and the sample's own verdict explains why
/// that reading is absent rather than this deriving a second opinion.
/// Shared by Recovery's fault age and Pressure's band age, so the two
/// screens can never render the same absence two different ways.
fn elapsed_reading(elapsed: Option<Duration64>, sample: &Sample) -> Reading {
    reading(sample, DegradedField::Uptime, elapsed, format_duration)
}

/// A CPU share, or why there is none.
///
/// A share is unmeasured on the very first reading — there is no interval
/// to divide by — which the sample's own verdict explains rather than this
/// reporting a zero nothing ever idled at. Shared by a faulted task's own
/// share and the whole processor's busy share, so a per-task figure and a
/// whole-resource one can never render the same absence two different ways.
fn cpu_reading(permille: Option<u16>, sample: &Sample) -> Reading {
    reading(sample, DegradedField::CpuTime, permille, percent)
}

/// How much of the machine's memory is in use, or why that cannot be said.
///
/// The share comes from the one memory-pressure reading the sampler takes,
/// so an absent reading is explained by that field's own verdict rather
/// than by this deriving a second opinion — and never by a plausible
/// percentage of a total nobody read.
fn memory_share_reading(sample: &Sample) -> Reading {
    reading(
        sample,
        DegradedField::MemoryPressure,
        sample.memory_pressure.map(|memory| memory.used_permille),
        percent,
    )
}

/// A faulted task's own storage throughput, or why there is none.
///
/// Absent for exactly the reason a rate is absent anywhere else: no
/// previous reading of *this* task to delta against. A counter that did not
/// move over a real interval is a genuine `0 B/s` and is reported as one.
fn disk_reading(bytes_per_sec: Option<u64>, sample: &Sample) -> Reading {
    reading(
        sample,
        DegradedField::ProcessList,
        bytes_per_sec,
        format_rate,
    )
}

/// A resident-memory total, or why there is none.
///
/// The process list is what carries a footprint, so an absent total is
/// explained by that field's own verdict — a group whose members this
/// sample never saw has no total, rather than a nought none of them holds.
fn memory_bytes_reading(bytes: Option<u64>, sample: &Sample) -> Reading {
    reading(sample, DegradedField::ProcessList, bytes, format_bytes)
}

/// The marks a fault's timeline carries.
///
/// Only what this service observed: the fault itself, stamped with the age
/// it has stood, and — where the age is known — the observation that it is
/// still standing now. A history this service did not see is not invented.
fn fault_marks(state: RecoveryState, since: &Reading) -> Vec<FaultMark> {
    let mut marks = alloc::vec![FaultMark {
        stamp: match since {
            Reading::Measured(age) => alloc::format!("{age} ago"),
            Reading::Absent(_) => String::from("when observed"),
        },
        text: String::from(match state {
            RecoveryState::Hung => "Stopped answering its seat",
            _ => "Stopped by the kernel",
        }),
        is_fault: true,
    }];
    if matches!(since, Reading::Measured(_)) {
        marks.push(FaultMark {
            stamp: String::from("now"),
            text: String::from("Still faulted"),
            is_fault: false,
        });
    }
    marks
}

/// The kernel's crash record for `proc_id`, matched by the task's own
/// stable identity.
///
/// The match is on [`ProcId`] and nothing else: a numeric pid is reused, so
/// matching on one could attribute a dead task's crash to a live task that
/// inherited its number. A fault with no record answers [`None`], which the
/// screen states plainly rather than drawing an empty table.
fn crash_snapshot(sample: &Sample, proc_id: ProcId) -> Option<CrashSnapshot> {
    let crash = sample
        .crashes
        .as_ref()?
        .iter()
        .find(|record| record.proc_id == proc_id)?;
    Some(CrashSnapshot {
        cause: String::from(crash_cause(crash.fault_class)),
        location: crash_location(crash.fault_bucket, crash.fault_offset),
        write: crash.is_write(),
        owner: alloc::format!("uid {}, gid {}", crash.uid, crash.gid),
        pc: alloc::format!(
            "{:#018x} ({})",
            crash.pc,
            if crash.load_base_known() {
                "program-relative"
            } else {
                "absolute"
            }
        ),
        sp: alloc::format!("{:#018x}", crash.sp),
        fp: if crash.fp_valid() {
            alloc::format!("{:#018x}", crash.fp)
        } else {
            String::from("not meaningful for this frame")
        },
        registers: crash
            .regs()
            .iter()
            .map(|reg| (display_name(reg.name_bytes()), reg.value))
            .collect(),
        frames: crash.frames().to_vec(),
    })
}

/// Why the resolver refused the faulting access, in the kernel's own terms.
const fn crash_cause(class: CrashFaultClass) -> &'static str {
    match class {
        CrashFaultClass::Stack => "stack growth the kernel could not back",
        CrashFaultClass::StackLimit => "stack growth refused by the task's stack bound",
        CrashFaultClass::FileRegion => "refused access inside a file mapping",
        CrashFaultClass::Anon => "reserved memory the kernel could not back",
        CrashFaultClass::Wild => "outside every mapping the task owns",
    }
}

/// Where the faulting address sat, as a distance from its anchor rather
/// than an absolute address.
///
/// Two of the buckets carry no meaningful distance, and say so rather than
/// printing the `0` the record holds — which a reader would take for a
/// measured offset of nothing.
fn crash_location(bucket: CrashFaultBucket, offset: u64) -> String {
    match bucket {
        CrashFaultBucket::NullPage => alloc::format!("{offset} bytes into the null page"),
        CrashFaultBucket::BelowStackGuard => {
            alloc::format!("{offset} bytes below the stack guard")
        }
        CrashFaultBucket::PastRegion => alloc::format!("{offset} bytes past its region"),
        CrashFaultBucket::Wild => String::from("far from every mapping (no distance to give)"),
        CrashFaultBucket::InRegion => String::from("inside a region it owns (no distance to give)"),
    }
}

/// The "Show tasks" relief action every pressure cause offers: it always
/// works (a section switch needs no authority), so it is always
/// [`ActionVerdict::Ready`].
fn show_tasks_action(recommended: bool) -> PressureAction {
    PressureAction {
        label: String::from("Show tasks"),
        control: PressureControl::ShowTasks,
        verdict: ActionVerdict::Ready,
        recommended,
    }
}

/// The Pressure section's cards, built from the same latches the tray
/// icon's rail uses ([`LiveMeters::cpu_pressured`] /
/// [`LiveMeters::memory_pressured`]) — measured pressure, never guessed —
/// alongside the culprit pid each card's relief actions target.
///
/// How long each cause has stood comes from `meters`' [`PressureClock`],
/// the only honest source: the System Information API reports that a
/// resource *is* pressured, never when it became so, so an age this
/// service did not itself observe reads as unmeasured.
fn build_pressure(
    sample: &Sample,
    meters: &RollingMeters,
    self_uid: Option<u32>,
    authority: &dyn CapabilityQuery,
) -> (Vec<PressureCause>, Vec<Option<u64>>) {
    let mut causes = Vec::new();
    let mut targets = Vec::new();
    let now = sample.uptime.map(|uptime| uptime.since_boot);
    if meters.system.cpu_pressured() {
        let since = elapsed_reading(meters.pressure.cpu_elapsed(now), sample);
        let (cause, target) = cpu_pressure_cause(sample, self_uid, authority, since);
        causes.push(cause);
        targets.push(target);
    }
    if meters.system.memory_pressured() {
        let since = elapsed_reading(meters.pressure.memory_elapsed(now), sample);
        let (cause, target) = memory_pressure_cause(sample, since);
        causes.push(cause);
        targets.push(target);
    }
    (causes, targets)
}

/// The CPU pressure card: culprit is the sampled row with the highest
/// measured CPU share, or a culprit-less card naming the resource itself
/// when no per-process rate has been measured yet.
fn cpu_pressure_cause(
    sample: &Sample,
    self_uid: Option<u32>,
    authority: &dyn CapabilityQuery,
    since: Reading,
) -> (PressureCause, Option<u64>) {
    let amount = cpu_reading(sample.cpu_busy_permille, sample);
    let culprit = sample
        .processes
        .iter()
        .enumerate()
        .filter_map(|(index, process)| {
            process
                .cpu_permille
                .map(|permille| (index, process, permille))
        })
        .max_by_key(|(_, _, permille)| *permille);

    let Some((index, process, permille)) = culprit else {
        return (
            PressureCause {
                resource: String::from("CPU"),
                kind: PressureKind::Cpu,
                culprit: String::from("CPU"),
                cause: String::from(
                    "The processor is saturated; per-task rates are not measured yet.",
                ),
                activity: ActivityState::Idle,
                task_index: None,
                amount,
                since,
                actions: alloc::vec![show_tasks_action(false)],
            },
            None,
        );
    };

    let can_control = controllable(process.uid, self_uid, authority);
    let lower_verdict = if process.priority == SchedPriority::Low {
        ActionVerdict::DisabledByState
    } else if can_control {
        ActionVerdict::Ready
    } else {
        ActionVerdict::DeniedByAuthority
    };
    let pause_verdict = if process.state == ProcessState::Stopped {
        ActionVerdict::DisabledByState
    } else if can_control {
        ActionVerdict::Ready
    } else {
        ActionVerdict::DeniedByAuthority
    };

    let cause = PressureCause {
        resource: String::from("CPU"),
        kind: PressureKind::Cpu,
        culprit: display_name(&process.name),
        cause: format!("Using {}% of the CPU over the last sample.", permille / 10),
        activity: ActivityState::Progress(ProgressValue::new(permille)),
        task_index: Some(index),
        amount,
        since,
        actions: alloc::vec![
            PressureAction {
                label: String::from("Lower priority"),
                control: PressureControl::LowerPriority,
                verdict: lower_verdict,
                recommended: true,
            },
            PressureAction {
                label: String::from("Pause"),
                control: PressureControl::Pause,
                verdict: pause_verdict,
                recommended: false,
            },
            show_tasks_action(false),
        ],
    };
    (cause, Some(process.pid))
}

/// The memory pressure card: culprit is the sampled row holding the most
/// memory, or a culprit-less card naming the resource itself when no row
/// carries a measured footprint.
///
/// Pausing a process does not free the memory it already holds, so the
/// only relief this card ever offers is Show tasks; force-quitting a
/// culprit stays the Recovery section's job.
fn memory_pressure_cause(sample: &Sample, since: Reading) -> (PressureCause, Option<u64>) {
    let amount = memory_share_reading(sample);
    let culprit = sample
        .processes
        .iter()
        .enumerate()
        .filter(|(_, process)| process.mem_bytes > 0)
        .max_by_key(|(_, process)| process.mem_bytes);

    let Some((index, process)) = culprit else {
        return (
            PressureCause {
                resource: String::from("Memory"),
                kind: PressureKind::Memory,
                culprit: String::from("Memory"),
                cause: String::from("Memory pressure is high."),
                activity: ActivityState::Idle,
                task_index: None,
                amount,
                since,
                actions: alloc::vec![show_tasks_action(false)],
            },
            None,
        );
    };

    let total_bytes = sample
        .memory_pressure
        .map_or(0, |memory| memory.total_bytes);
    let used_permille = sample
        .memory_pressure
        .map_or(0, |memory| memory.used_permille);
    let share_clause = if total_bytes > 0 {
        let share = (u128::from(process.mem_bytes) * 1000 / u128::from(total_bytes)).min(1000);
        let share = u16::try_from(share).unwrap_or(1000);
        format!(" ({}% of memory)", share / 10)
    } else {
        String::new()
    };
    let cause_text = format!(
        "Using {} of RAM{}.",
        format_bytes(process.mem_bytes),
        share_clause
    );

    let cause = PressureCause {
        resource: String::from("Memory"),
        kind: PressureKind::Memory,
        culprit: display_name(&process.name),
        cause: cause_text,
        activity: ActivityState::Progress(ProgressValue::new(used_permille)),
        task_index: Some(index),
        amount,
        since,
        actions: alloc::vec![show_tasks_action(true)],
    };
    (cause, Some(process.pid))
}

/// The total of one measured reading across every joined member of an
/// activity, or `None` when the group has no joined member at all or any
/// joined member's own reading is unmeasured.
///
/// One missing part makes the whole absent: a total that quietly skipped an
/// unmeasured member would understate the group while reading as a
/// measurement. The sum saturates rather than wrapping, so an implausible
/// set of readings overstates at the ceiling instead of folding back to a
/// small, believable figure.
fn member_total(
    joined: &[&ProcessSummary],
    read: impl Fn(&ProcessSummary) -> Option<u64>,
) -> Option<u64> {
    if joined.is_empty() {
        return None;
    }
    joined.iter().try_fold(0u64, |total, member| {
        read(member).map(|value| total.saturating_add(value))
    })
}

/// One activity's combined CPU share across its joined members, or why
/// there is none.
///
/// A group spanning several cores legitimately totals past 100%, so the
/// share is not clamped; it saturates at the widest figure the share type
/// carries, which a group bounded by
/// [`MAX_ACTIVITY_MEMBERS`](crate::activities::MAX_ACTIVITY_MEMBERS) cannot
/// reach.
fn activity_cpu_reading(joined: &[&ProcessSummary], sample: &Sample) -> Reading {
    let total = member_total(joined, |process| process.cpu_permille.map(u64::from))
        .map(|total| u16::try_from(total).unwrap_or(u16::MAX));
    cpu_reading(total, sample)
}

/// The Activities section's summaries, one per tracked group in group
/// order, alongside each group's stable id and its joined members' pids
/// (in group order) for the actions a rendered [`ActivityMember`] cannot
/// resolve on its own.
///
/// Each summary also carries what the group costs the machine, totalled
/// from its joined members' own measured readings rather than from any
/// per-group accounting: there is none, and inventing one would be a figure
/// nobody measured. Network is always unmeasured because no per-process
/// network accounting exists to total.
fn build_activities(
    activities: &Activities,
    sample: &Sample,
    meters: &TaskMeters,
    self_uid: Option<u32>,
    authority: &dyn CapabilityQuery,
) -> (Vec<ActivitySummary>, Vec<u64>, Vec<Vec<u64>>) {
    let mut summaries = Vec::with_capacity(activities.len());
    let mut ids = Vec::with_capacity(activities.len());
    let mut member_pids = Vec::with_capacity(activities.len());
    let processes = &sample.processes;

    for group in activities.iter() {
        let mut members = Vec::with_capacity(group.members.len());
        let mut joined_pids = Vec::new();
        let mut joined = Vec::new();
        let mut any_working = false;
        let mut all_joined_controllable = true;

        for member in group.members {
            let Some(process) = processes
                .iter()
                .find(|process| process.proc_id == member.proc_id)
            else {
                members.push(ActivityMember {
                    name: member.name.clone(),
                    detail: String::new(),
                    activity: ActivityState::Idle,
                    joined: false,
                });
                continue;
            };
            joined.push(process);
            joined_pids.push(process.pid);
            let activity = process_activity(process.state);
            if activity == ActivityState::Working {
                any_working = true;
            }
            if !controllable(process.uid, self_uid, authority) {
                all_joined_controllable = false;
            }
            let detail = process.cpu_permille.map_or_else(String::new, percent);
            members.push(ActivityMember {
                name: display_name(&process.name),
                detail,
                activity,
                joined: true,
            });
        }
        let joined_count = joined.len();

        let can_control = if joined_count == 0 {
            authority.holds(CapabilityId::PROC_CONTROL)
        } else {
            all_joined_controllable
        };
        let activity = if !group.paused && any_working {
            ActivityState::Working
        } else {
            ActivityState::Idle
        };
        let count = group.members.len();
        let detail = if count == 1 {
            String::from("1 task")
        } else {
            format!("{count} tasks")
        };

        summaries.push(ActivitySummary {
            id: group.id,
            name: String::from(group.name),
            detail,
            activity,
            paused: group.paused,
            can_control,
            can_accept_member: count < crate::activities::MAX_ACTIVITY_MEMBERS,
            cpu: activity_cpu_reading(&joined, sample),
            memory: memory_bytes_reading(
                member_total(&joined, |process| Some(process.mem_bytes)),
                sample,
            ),
            disk: disk_reading(
                member_total(&joined, |process| meters.disk_rate(process.proc_id)),
                sample,
            ),
            network: Reading::Absent(Unmeasured::NoInterface),
            members,
        });
        ids.push(group.id);
        member_pids.push(joined_pids);
    }

    (summaries, ids, member_pids)
}

/// A grouping-state edit produced by a task or activity action, applied by
/// [`crate::service::Service`] to its own [`Activities`] — the panel and
/// its [`Effect`]s stay stateless about grouping, and the current
/// [`PanelModel`] is what resolves every index they carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupingEdit {
    /// Assign `task` to `activity`, or to a freshly created activity when
    /// `activity` is `None`.
    Assign {
        /// The task's index within the model.
        task: usize,
        /// The activity's index within the model, or `None` to create one.
        activity: Option<usize>,
    },
    /// Remove `task` from its activity.
    Unassign {
        /// The task's index within the model.
        task: usize,
    },
    /// Commit the pending rename, read from the widget with
    /// [`Switchboard::submitted_activity_name`](crate::view::Switchboard::submitted_activity_name).
    Rename {
        /// The activity's index within the model.
        activity: usize,
    },
    /// Set an activity's paused flag.
    SetPaused {
        /// The activity's index within the model.
        activity: usize,
        /// The new paused state.
        paused: bool,
    },
    /// Close an activity (its members are handled by a separate signal
    /// sweep alongside this edit).
    Close {
        /// The activity's index within the model.
        activity: usize,
    },
}

/// The outbound effect an interactive [`SwitchboardAction`] implies.
/// [`crate::panel`] applies every entry of the [`Vec`] [`apply_action`]
/// returns, in order; an empty vector means nothing to do beyond letting
/// the caller re-render.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Close the window and return to headless sampling.
    CloseWindow,
    /// Ask the session to raise the named owner's front window.
    ActivateOwner {
        /// The owner's scheduler task id.
        owner: u64,
    },
    /// Ask the session to re-launch the named owner.
    RestartOwner {
        /// The owner's scheduler task id.
        owner: u64,
    },
    /// Deliver a control signal to the named process directly.
    Signal {
        /// The scheduler task id to signal.
        pid: u64,
        /// The signal to deliver.
        signal: Signal,
    },
    /// Lower the named process's scheduling priority.
    LowerPriority {
        /// The scheduler task id to lower.
        pid: u64,
    },
    /// Deliver one signal to each of several processes, continuing past an
    /// individual refusal rather than aborting the sweep.
    SignalMany {
        /// The scheduler task ids to signal.
        pids: Vec<u64>,
        /// The signal to deliver to each.
        signal: Signal,
    },
    /// Ask the session to raise each owner's window in turn.
    ActivateOwners {
        /// The owners' scheduler task ids, in group order.
        owners: Vec<u64>,
    },
    /// Apply a grouping-state edit to the service's own [`Activities`].
    Grouping(GroupingEdit),
}

/// Map an interactive [`SwitchboardAction`] the composed control reported
/// to the [`Effect`]s it implies, resolving every index against `panel` and
/// re-checking authority for every authority-gated action from the very
/// same [`PanelModel`] verdicts [`build_model`] already computed under the
/// same authority — so an action whose authority is absent is never
/// attempted, matching the render-time check the model already applied. An
/// out-of-range index yields no effect (fail closed) rather than guessing.
#[must_use]
pub fn apply_action(
    panel: &PanelModel,
    action: SwitchboardAction,
    authority: &dyn CapabilityQuery,
) -> Vec<Effect> {
    match action {
        SwitchboardAction::Task { index, control } => apply_task(panel, index, control),
        SwitchboardAction::Recovery { index, control } => {
            let Some(owner) = panel.recovery_owner(index) else {
                return Vec::new();
            };
            match control {
                RecoveryControl::Restart => alloc::vec![Effect::RestartOwner { owner }],
                RecoveryControl::Force => {
                    if authority.holds(CapabilityId::PROC_CONTROL) {
                        alloc::vec![Effect::Signal {
                            pid: owner,
                            signal: Signal::Kill,
                        }]
                    } else {
                        Vec::new()
                    }
                }
            }
        }
        SwitchboardAction::Window(WindowControlKind::Close) => alloc::vec![Effect::CloseWindow],
        SwitchboardAction::Pressure { index, control } => apply_pressure(panel, index, control),
        SwitchboardAction::TaskGrouped { task, activity } => {
            if panel.task_ident(task).is_none() {
                return Vec::new();
            }
            if let Some(activity_index) = activity {
                if panel.activity_id(activity_index).is_none() {
                    return Vec::new();
                }
            }
            alloc::vec![Effect::Grouping(GroupingEdit::Assign { task, activity })]
        }
        SwitchboardAction::TaskUngrouped { task } => {
            if panel.task_ident(task).is_none() {
                return Vec::new();
            }
            alloc::vec![Effect::Grouping(GroupingEdit::Unassign { task })]
        }
        SwitchboardAction::Activity { index, control } => apply_activity(panel, index, control),
        SwitchboardAction::ActivityRenamed { index } => {
            if panel.activity_id(index).is_none() {
                return Vec::new();
            }
            alloc::vec![Effect::Grouping(GroupingEdit::Rename { activity: index })]
        }
        // No background-job registry exists, so a job action can never
        // resolve to a real target; no service-enumeration query or
        // power/lock interface exists either, so service and system-action
        // rows are never populated and their actions never fire in
        // practice. A furniture gesture the window channel cannot yet
        // carry (move/resize/activate/minimize/put-to-back), a section
        // change, and a scroll all need only the re-render the caller
        // already performs.
        SwitchboardAction::Window(_)
        | SwitchboardAction::Activate
        | SwitchboardAction::MoveBegin
        | SwitchboardAction::MoveTo { .. }
        | SwitchboardAction::MoveEnd
        | SwitchboardAction::ResizeBegin
        | SwitchboardAction::ResizeTo { .. }
        | SwitchboardAction::ResizeEnd
        | SwitchboardAction::ResizeCancel
        | SwitchboardAction::SectionChanged { .. }
        | SwitchboardAction::Job { .. }
        | SwitchboardAction::Service { .. }
        | SwitchboardAction::System { .. }
        | SwitchboardAction::Scrolled { .. } => Vec::new(),
    }
}

/// One task command, re-checked against the verdict [`build_model`] already
/// computed for that very task rather than re-deriving authority from
/// scratch — the model's verdict *is* the server-side check, computed once
/// under the real authority, so a command the rail drew as denied or
/// disabled can never be carried out by a scripted or otherwise unexpected
/// report of it (fail closed).
///
/// [`TaskControl::Reveal`] is the same request of the session as
/// [`TaskControl::Switch`]: raising a task's window is how this system shows
/// the reader where it is, and there is no separate "highlight without
/// raising" interface to invent one for. [`TaskControl::OpenLogs`] resolves
/// to nothing at all: no capability-gated query for a task's own log entries
/// exists, which is exactly why its verdict is permanently disabled.
fn apply_task(panel: &PanelModel, index: usize, control: TaskControl) -> Vec<Effect> {
    let Some(task) = panel.model.tasks.get(index) else {
        return Vec::new();
    };
    if task.authority.verdict(control) != ActionVerdict::Ready {
        return Vec::new();
    }
    let Some(owner) = panel.task_owner(index) else {
        return Vec::new();
    };
    match control {
        TaskControl::Switch | TaskControl::Reveal => {
            alloc::vec![Effect::ActivateOwner { owner }]
        }
        TaskControl::Pause => alloc::vec![Effect::Signal {
            pid: owner,
            signal: Signal::Stop,
        }],
        TaskControl::Resume => alloc::vec![Effect::Signal {
            pid: owner,
            signal: Signal::Continue,
        }],
        TaskControl::LowerPriority => alloc::vec![Effect::LowerPriority { pid: owner }],
        TaskControl::ForceQuit => alloc::vec![Effect::Signal {
            pid: owner,
            signal: Signal::Kill,
        }],
        TaskControl::OpenLogs => Vec::new(),
    }
}

/// A pressure cause's relief action, re-checked against the verdict
/// [`build_model`] already computed for it rather than re-deriving
/// authority from scratch — the model's verdict *is* the server-side
/// check, computed once under the real authority so it can never disagree
/// with what is rendered.
///
/// [`PressureControl::ShowTasks`] never reaches here: the composed control
/// resolves it internally into a section change, so seeing it here (a
/// scripted or otherwise unexpected report) yields no effect rather than
/// acting on an action this service never renders as its own.
fn apply_pressure(panel: &PanelModel, index: usize, control: PressureControl) -> Vec<Effect> {
    let Some(cause) = panel.model.pressure.get(index) else {
        return Vec::new();
    };
    let Some(pid) = panel.pressure_target(index) else {
        return Vec::new();
    };
    let Some(action) = cause
        .actions
        .iter()
        .find(|action| action.control == control)
    else {
        return Vec::new();
    };
    if action.verdict != ActionVerdict::Ready {
        return Vec::new();
    }
    match control {
        PressureControl::Pause => alloc::vec![Effect::Signal {
            pid,
            signal: Signal::Stop,
        }],
        PressureControl::LowerPriority => alloc::vec![Effect::LowerPriority { pid }],
        PressureControl::ShowTasks => Vec::new(),
    }
}

/// An activity's action, re-checked against the `can_control` verdict
/// [`build_model`] already computed for it. A stored pid whose process
/// exited may have been reused by an unrelated process, so a sweep or an
/// activation only ever names members still joined to the *current*
/// sample — acting on an unjoined pid would risk the pid-reuse hazard.
fn apply_activity(panel: &PanelModel, index: usize, control: ActivityControl) -> Vec<Effect> {
    let Some(summary) = panel.model.activities.get(index) else {
        return Vec::new();
    };
    let pids = panel.activity_members(index).to_vec();
    match control {
        ActivityControl::Switch => alloc::vec![Effect::ActivateOwners { owners: pids }],
        ActivityControl::Pause => {
            if !summary.can_control {
                return Vec::new();
            }
            alloc::vec![
                Effect::SignalMany {
                    pids,
                    signal: Signal::Stop,
                },
                Effect::Grouping(GroupingEdit::SetPaused {
                    activity: index,
                    paused: true,
                }),
            ]
        }
        ActivityControl::Resume => {
            if !summary.can_control {
                return Vec::new();
            }
            alloc::vec![
                Effect::SignalMany {
                    pids,
                    signal: Signal::Continue,
                },
                Effect::Grouping(GroupingEdit::SetPaused {
                    activity: index,
                    paused: false,
                }),
            ]
        }
        ActivityControl::Close => {
            if !summary.can_control {
                return Vec::new();
            }
            alloc::vec![
                Effect::SignalMany {
                    pids,
                    signal: Signal::Terminate,
                },
                Effect::Grouping(GroupingEdit::Close { activity: index }),
            ]
        }
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
