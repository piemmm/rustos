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

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport};
use tairix_abi::sysinfo::ProcessState;
use tairix_abi::{CapabilityId, CapabilityQuery, ProcId, SchedPriority, Signal};
use tairix_controls::{
    ActionVerdict, ActivityControl, ActivityMember, ActivityState, ActivitySummary, MeterValue,
    PressureAction, PressureCause, PressureControl, PressureKind, PressureState, ProgressValue,
    RecoveryControl, RecoveryItem, RecoveryState, ResourceSummary, Section, SwitchboardAction,
    SwitchboardModel, TaskSummary, WindowControlKind, MAX_CHART_SAMPLES,
};

use crate::activities::Activities;
use crate::derive::{memory_pressured, Hysteresis};
use crate::sample::{ProcessSummary, Sample};

/// The task row action's label: every task's row action is the same
/// switch-to-window request, so the label never varies per row.
const SWITCH_LABEL: &str = "Switch";

/// The reading text of a resource the service could not measure this
/// cycle. It reads as "unknown", never as a fabricated `0%`, and the
/// meter beside it stays [`MeterValue::Unmeasured`].
const UNREAD_READING: &str = "unknown";

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
        CommandSection::Overview => Section::Overview,
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

/// A permille fraction as whole-percent display text.
fn percent(permille: u16) -> String {
    format!("{}%", permille / 10)
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

/// The rolling instrument state the panel's header band needs that no single
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

/// Build the live [`PanelModel`] from this sample, the rolling meter state,
/// the seat's latest unresponsive-owner report, and the service's own
/// [`Activities`] grouping state.
///
/// `self_uid` is the service's own uid, derived by the caller from its own
/// pid's row in `sample` (an unknown uid narrows authority rather than
/// widening it — see `controllable`). `authority` is queried once per
/// action kind that needs it, so the rendered [`SwitchboardModel`] and the
/// effect [`apply_action`] later produces from the same authority can never
/// disagree about whether an action is available.
#[must_use]
pub fn build_model(
    title: &str,
    sample: &Sample,
    seat_report: &SeatReport,
    meters: &LiveMeters,
    authority: &dyn CapabilityQuery,
    activities: &Activities,
    self_uid: Option<u32>,
) -> PanelModel {
    let mut model = SwitchboardModel::new(title);
    let can_force = authority.holds(CapabilityId::PROC_CONTROL);

    let (tasks, task_owners, task_idents) = build_tasks(&sample.processes, activities);
    let (recovery, recovery_owners) = build_recovery(&sample.processes, seat_report, can_force);
    let (pressure, pressure_targets) = build_pressure(sample, meters, self_uid, authority);
    let (activity_summaries, activity_ids, activity_members) =
        build_activities(activities, &sample.processes, self_uid, authority);

    model.tasks = tasks;
    model.resources = build_resources(sample, meters);
    model.recovery = recovery;
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
fn build_tasks(
    processes: &[ProcessSummary],
    activities: &Activities,
) -> (Vec<TaskSummary>, Vec<u64>, Vec<TaskIdent>) {
    let mut tasks = Vec::with_capacity(processes.len());
    let mut owners = Vec::with_capacity(processes.len());
    let mut idents = Vec::with_capacity(processes.len());
    for process in processes {
        let detail = process.cpu_permille.map_or_else(String::new, percent);
        let name = display_name(&process.name);
        tasks.push(TaskSummary {
            name: name.clone(),
            detail,
            pressure: PressureState::None,
            activity: process_activity(process.state),
            recovery: RecoveryState::None,
            action: SWITCH_LABEL.to_string(),
            // Every task's switch action is a plain session request that
            // needs no capability of its own to attempt; the session is
            // free to refuse it.
            action_allowed: true,
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
/// but this sample never saw contributes no row (never a fabricated one).
fn build_recovery(
    processes: &[ProcessSummary],
    seat_report: &SeatReport,
    can_force: bool,
) -> (Vec<RecoveryItem>, Vec<u64>) {
    let mut recovery = Vec::new();
    let mut owners = Vec::new();

    for process in processes {
        if process.state == ProcessState::Stopped {
            recovery.push(RecoveryItem {
                name: display_name(&process.name),
                detail: String::from("stopped"),
                recovery: RecoveryState::Recoverable,
                can_restart: true,
                can_force,
            });
            owners.push(process.pid);
        }
    }

    for &owner in seat_report.owners() {
        // Never trust a name from the wire: the report carries ids only,
        // joined here against the names this service attested itself.
        let Some(process) = processes.iter().find(|process| process.pid == owner) else {
            continue;
        };
        if owners.contains(&owner) {
            // Already listed as stopped; a hung-but-running process is a
            // distinct, separately reported condition below.
            continue;
        }
        recovery.push(RecoveryItem {
            name: display_name(&process.name),
            detail: String::from("not responding"),
            recovery: RecoveryState::Hung,
            can_restart: true,
            can_force,
        });
        owners.push(owner);
    }

    (recovery, owners)
}

/// The resource rows the header band and the Overview section show: one per
/// resource this service actually queries, in a fixed order so the band
/// never reshuffles between samples.
///
/// Disk and network get no row at all: the System Information API's
/// process/CPU-time/memory-pressure queries carry no throughput reading for
/// either, and this service's own capability ceiling and sampling budget
/// stop at the resources the tray's own pressure latches cover, so a row
/// for either would be an invented reading rather than an honest one.
fn build_resources(sample: &Sample, meters: &LiveMeters) -> Vec<ResourceSummary> {
    alloc::vec![
        resource(
            "CPU",
            PressureKind::Cpu,
            sample.cpu_busy_permille,
            meters.cpu_pressured(),
            meters.cpu_history(),
        ),
        resource(
            "Memory",
            PressureKind::Memory,
            sample.memory_pressure.map(|memory| memory.used_permille),
            meters.memory_pressured(),
            // The memory gauge is refreshed on its own slower cadence and
            // carried forward between samples, so a per-sample history would
            // plot one carried reading repeatedly as though every point were a
            // fresh measurement.
            &[],
        ),
    ]
}

/// One resource row: measured when `permille` is a real reading, honestly
/// unmeasured when the query failed or its capability was never granted —
/// never a fabricated zero, which reads as "idle" when the truth is
/// "unknown".
fn resource(
    name: &str,
    kind: PressureKind,
    permille: Option<u16>,
    pressured: bool,
    history: &[u16],
) -> ResourceSummary {
    let reading = permille.map_or_else(|| String::from(UNREAD_READING), percent);
    let meter = permille.map_or(MeterValue::Unmeasured, |value| {
        MeterValue::Measured(ProgressValue::new(value))
    });
    let pressure = if pressured {
        PressureState::Under(kind)
    } else {
        PressureState::None
    };
    ResourceSummary::new(name, reading, kind, load_activity(permille)).with_meter(
        meter,
        pressure,
        history.iter().copied(),
    )
}

/// The Overview card's Heat Seam for a resource reading: any measured load
/// is work in progress whose extent the seam does not itself quantify (the
/// meter beside it carries the number). A zero reading, and a reading the
/// service could not take at all, leave the seam quiet.
const fn load_activity(permille: Option<u16>) -> ActivityState {
    match permille {
        Some(reading) if reading > 0 => ActivityState::Working,
        _ => ActivityState::Idle,
    }
}

/// A byte count as compact binary-unit text with one decimal place
/// (`"1.9 GiB"`, `"640.0 KiB"`), or plain whole bytes below 1 KiB
/// (`"512 B"`) — the pressure card's cause text names how much memory the
/// culprit holds without pretending to more precision than a permille-scale
/// reading warrants.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    let (scale, unit) = if bytes >= GIB {
        (GIB, "GiB")
    } else if bytes >= MIB {
        (MIB, "MiB")
    } else if bytes >= KIB {
        (KIB, "KiB")
    } else {
        return format!("{bytes} B");
    };
    let whole = bytes / scale;
    let tenths = (bytes % scale) * 10 / scale;
    format!("{whole}.{tenths} {unit}")
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
fn build_pressure(
    sample: &Sample,
    meters: &LiveMeters,
    self_uid: Option<u32>,
    authority: &dyn CapabilityQuery,
) -> (Vec<PressureCause>, Vec<Option<u64>>) {
    let mut causes = Vec::new();
    let mut targets = Vec::new();
    if meters.cpu_pressured() {
        let (cause, target) = cpu_pressure_cause(sample, self_uid, authority);
        causes.push(cause);
        targets.push(target);
    }
    if meters.memory_pressured() {
        let (cause, target) = memory_pressure_cause(sample);
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
) -> (PressureCause, Option<u64>) {
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
fn memory_pressure_cause(sample: &Sample) -> (PressureCause, Option<u64>) {
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
        actions: alloc::vec![show_tasks_action(true)],
    };
    (cause, Some(process.pid))
}

/// The Activities section's summaries, one per tracked group in group
/// order, alongside each group's stable id and its joined members' pids
/// (in group order) for the actions a rendered [`ActivityMember`] cannot
/// resolve on its own.
fn build_activities(
    activities: &Activities,
    processes: &[ProcessSummary],
    self_uid: Option<u32>,
    authority: &dyn CapabilityQuery,
) -> (Vec<ActivitySummary>, Vec<u64>, Vec<Vec<u64>>) {
    let mut summaries = Vec::with_capacity(activities.len());
    let mut ids = Vec::with_capacity(activities.len());
    let mut member_pids = Vec::with_capacity(activities.len());

    for group in activities.iter() {
        let mut members = Vec::with_capacity(group.members.len());
        let mut joined_pids = Vec::new();
        let mut any_working = false;
        let mut joined_count = 0usize;
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
                });
                continue;
            };
            joined_count += 1;
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
            });
        }

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
    /// [`tairix_controls::Switchboard::submitted_activity_name`].
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
        SwitchboardAction::Task { index } => {
            panel.task_owner(index).map_or_else(Vec::new, |owner| {
                alloc::vec![Effect::ActivateOwner { owner }]
            })
        }
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
