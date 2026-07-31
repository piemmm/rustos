//! Build the live [`SwitchboardModel`] the panel renders from a [`Sample`]
//! and the session's [`SeatReport`], and map each interactive
//! [`SwitchboardAction`] the composed control reports back onto the
//! outbound [`Effect`] it implies.
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
use tairix_abi::{CapabilityId, CapabilityQuery, Signal};
use tairix_controls::{
    ActivityState, MeterValue, PressureKind, PressureState, ProgressValue, RecoveryControl,
    RecoveryItem, RecoveryState, ResourceSummary, Section, SwitchboardAction, SwitchboardModel,
    TaskSummary, WindowControlKind, MAX_HISTORY_SAMPLES,
};

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
fn display_name(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
}

/// A permille fraction as whole-percent display text.
fn percent(permille: u16) -> String {
    format!("{}%", permille / 10)
}

/// The rolling meter state the panel's header band needs that no single
/// [`Sample`] carries: the CPU sparkline's bounded history, and the
/// pressure verdicts the tray summary's own derivation already reached for
/// the same readings.
///
/// The history is held inline, capped at the meter's own
/// [`MAX_HISTORY_SAMPLES`] window, so recording a sample never allocates
/// and a service that runs for weeks never grows an unbounded log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveMeters {
    cpu_history: [u16; MAX_HISTORY_SAMPLES],
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
            cpu_history: [0; MAX_HISTORY_SAMPLES],
            cpu_len: 0,
            cpu_pressured: false,
            memory_pressured: false,
        }
    }

    /// Record `sample`'s readings and the CPU-pressure verdict `hysteresis`
    /// latched for the very same reading, so the panel's meter and the tray
    /// icon's rail can never disagree.
    ///
    /// Only a measured CPU reading enters the sparkline: an interval the
    /// service could not measure contributes no bar rather than a zero one,
    /// which would draw as a genuine idle moment. The oldest bar is dropped
    /// once the window is full.
    pub fn record(&mut self, sample: &Sample, hysteresis: Hysteresis) {
        self.cpu_pressured = hysteresis.cpu_pressured();
        self.memory_pressured = memory_pressured(sample);
        let Some(busy) = sample.cpu_busy_permille else {
            return;
        };
        if self.cpu_len == MAX_HISTORY_SAMPLES {
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

/// The live [`SwitchboardModel`] plus the per-row process identity the
/// model's own row indices do not carry, so [`apply_action`] can resolve a
/// [`SwitchboardAction::Task`] or [`SwitchboardAction::Recovery`] index back
/// to the scheduler task id ([`ProcessSummary::pid`]) it names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PanelModel {
    /// The model the panel renders.
    pub model: SwitchboardModel,
    /// `task_owners[i]` is the pid backing `model.tasks[i]`.
    task_owners: Vec<u64>,
    /// `recovery_owners[i]` is the pid backing `model.recovery[i]`.
    recovery_owners: Vec<u64>,
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
}

/// Build the live [`PanelModel`] from this sample, the rolling meter state,
/// and the seat's latest unresponsive-owner report.
///
/// `authority` is queried once per action kind that needs it (today, only
/// [`CapabilityId::PROC_CONTROL`] for the recovery Force action), so the
/// rendered [`SwitchboardModel`] and the effect [`apply_action`] later
/// produces from the same authority can never disagree about whether an
/// action is available.
#[must_use]
pub fn build_model(
    title: &str,
    sample: &Sample,
    seat_report: &SeatReport,
    meters: &LiveMeters,
    authority: &dyn CapabilityQuery,
) -> PanelModel {
    let mut model = SwitchboardModel::new(title);
    let can_force = authority.holds(CapabilityId::PROC_CONTROL);

    let (tasks, task_owners) = build_tasks(&sample.processes);
    let (recovery, recovery_owners) = build_recovery(&sample.processes, seat_report, can_force);

    model.tasks = tasks;
    model.resources = build_resources(sample, meters);
    model.recovery = recovery;

    PanelModel {
        model,
        task_owners,
        recovery_owners,
    }
}

/// One [`TaskSummary`] per sampled process, in sampled order, each row
/// action naming its own pid so [`apply_action`] can resolve it.
///
/// A row carries no Pressure Rail: the System Information API reports a
/// process's CPU time, not which resource that process is straining, so
/// naming one would be a guess dressed as a measurement.
fn build_tasks(processes: &[ProcessSummary]) -> (Vec<TaskSummary>, Vec<u64>) {
    let mut tasks = Vec::with_capacity(processes.len());
    let mut owners = Vec::with_capacity(processes.len());
    for process in processes {
        let detail = process.cpu_permille.map_or_else(String::new, percent);
        tasks.push(TaskSummary {
            name: display_name(&process.name),
            detail,
            pressure: PressureState::None,
            activity: process_activity(process.state),
            recovery: RecoveryState::None,
            action: SWITCH_LABEL.to_string(),
            // Every task's switch action is a plain session request that
            // needs no capability of its own to attempt; the session is
            // free to refuse it.
            action_allowed: true,
        });
        owners.push(process.pid);
    }
    (tasks, owners)
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
/// Disk and network get no row at all: the System Information API exposes
/// no throughput query for either, and a row for a resource with no query
/// behind it would be an invented row rather than an honest reading.
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
            // carried forward between samples, so a per-sample sparkline
            // would draw one carried reading repeatedly as though every bar
            // were a fresh measurement.
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

/// The outbound effect an interactive [`SwitchboardAction`] implies.
/// [`crate::panel`] applies exactly one of these per action; every variant
/// that carries no genuine effect (a furniture gesture the window channel
/// has no wire mechanism for today, a section change, a scroll, or a row
/// this service leaves empty) maps to [`Effect::None`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Nothing to do beyond letting the caller re-render.
    None,
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
}

/// Map an interactive [`SwitchboardAction`] the composed control reported
/// to the [`Effect`] it implies, resolving row indices against `panel` and
/// re-checking `authority` for the one action that needs a capability
/// ([`RecoveryControl::Force`]) — so an action whose authority is absent is
/// never attempted, matching the render-time check [`build_model`] already
/// applied.
#[must_use]
pub fn apply_action(
    panel: &PanelModel,
    action: SwitchboardAction,
    authority: &dyn CapabilityQuery,
) -> Effect {
    match action {
        SwitchboardAction::Task { index } => panel
            .task_owner(index)
            .map_or(Effect::None, |owner| Effect::ActivateOwner { owner }),
        SwitchboardAction::Recovery { index, control } => {
            let Some(owner) = panel.recovery_owner(index) else {
                return Effect::None;
            };
            match control {
                RecoveryControl::Restart => Effect::RestartOwner { owner },
                RecoveryControl::Force => {
                    if authority.holds(CapabilityId::PROC_CONTROL) {
                        Effect::Signal {
                            pid: owner,
                            signal: Signal::Kill,
                        }
                    } else {
                        Effect::None
                    }
                }
            }
        }
        SwitchboardAction::Window(WindowControlKind::Close) => Effect::CloseWindow,
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
        | SwitchboardAction::Scrolled { .. } => Effect::None,
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
