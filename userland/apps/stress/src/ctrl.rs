//! The controller's event-driven state machine (`plans/STRESSTEST.md`
//! §7.1).
//!
//! Pure bookkeeping, host-testable end to end: the program half feeds it
//! the events its one wait-set observes — a child exit, an observed
//! signal, the run timeout, the teardown grace deadline — and executes
//! the typed [`Action`]s it returns. The machine itself performs no
//! syscall, so every teardown path (`^C`, `Terminate`, timeout, monitor
//! quit, worker failure, external kill) is provable on the host.
//!
//! Lifecycle: `Running` → (`signal` / timeout / monitor quit / workers
//! done) → `Draining` (every live worker asked to `Terminate`, the grace
//! clock armed) → on grace expiry `Killing` (`Kill` the stragglers) →
//! `Done` once every child is reaped. The run's own completion tears down
//! **workers only** — a `--monitor` session stays up until the user quits
//! it, and the summary is reported when it exits — while a signal end
//! tears the monitor down too.

use alloc::vec::Vec;

use tairix_abi::Signal;

use crate::worker::{WorkerKind, REFUSED_EXIT};

/// The `128 + n` wait status of a child ended by `Terminate` — read from
/// the one ABI definition so the classification can never drift.
const TERMINATE_STATUS: i32 = match Signal::Terminate.termination_status() {
    Some(status) => status,
    None => 0,
};

/// The `128 + n` wait status of a child ended by `Kill`.
const KILL_STATUS: i32 = match Signal::Kill.termination_status() {
    Some(status) => status,
    None => 0,
};

/// One child the controller is supervising.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Child {
    pid: i64,
    kind: ChildKind,
}

/// What a supervised child is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildKind {
    Worker(WorkerKind),
    Monitor,
}

/// Where the run is in its lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Load running; nothing has asked the run to end yet.
    Running,
    /// Teardown requested: every live worker (and, on a signal end, the
    /// monitor) has been sent `Terminate`; the grace clock is armed.
    Draining,
    /// The grace deadline passed: stragglers have been sent `Kill`.
    Killing,
}

/// One thing the program half must do in response to an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Send `signal` to child `pid`.
    Signal {
        /// The child to signal.
        pid: i64,
        /// The signal to send (`Terminate` on drain, `Kill` on grace
        /// expiry).
        signal: Signal,
    },
    /// Arm the teardown grace deadline (the machine entered `Draining`
    /// with live children still to reap).
    ArmGrace,
}

/// One thing the controller's wait-set observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    /// A supervised child was reaped with `code`.
    ChildExited {
        /// The reaped child's PID.
        pid: i64,
        /// Its wait status (exit code, or the `128+n` signal status).
        code: i32,
    },
    /// The signal intake drained an observed termination request.
    Signalled(Signal),
    /// The `--timeout` deadline elapsed.
    TimeoutElapsed,
    /// The teardown grace deadline elapsed with children still live.
    GraceElapsed,
}

/// How the reaped workers fared, for the summary line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tally {
    /// Workers that exited 0 (terminated cleanly on request, or ended
    /// their bounded run).
    pub clean: u32,
    /// Workers that reported a typed resource refusal
    /// ([`REFUSED_EXIT`]) — expected outcomes, especially under
    /// `--overcommit`.
    pub refused: u32,
    /// Workers that failed outright (any other non-zero status,
    /// including an external kill) — these fail the run.
    pub failed: u32,
}

/// The controller state machine. See the module docs for the lifecycle.
#[derive(Debug)]
pub struct Controller {
    children: Vec<Child>,
    phase: Phase,
    tally: Tally,
    /// The first observed termination request, deciding the exit status
    /// (130 for `Interrupt`, 143 for `Terminate`).
    signalled: Option<Signal>,
    /// Set once the run's end has been decided (signal, timeout, monitor
    /// quit, or all workers gone) — the summary is reported when the
    /// machine completes.
    monitor_quit: bool,
}

impl Controller {
    /// A controller supervising no children yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            children: Vec::new(),
            phase: Phase::Running,
            tally: Tally {
                clean: 0,
                refused: 0,
                failed: 0,
            },
            signalled: None,
            monitor_quit: false,
        }
    }

    /// Record a spawned worker child.
    pub fn add_worker(&mut self, pid: i64, kind: WorkerKind) {
        self.children.push(Child {
            pid,
            kind: ChildKind::Worker(kind),
        });
    }

    /// Record the spawned `--monitor` child.
    pub fn add_monitor(&mut self, pid: i64) {
        self.children.push(Child {
            pid,
            kind: ChildKind::Monitor,
        });
    }

    /// Feed one observed event, collecting the actions to execute.
    pub fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::ChildExited { pid, code } => self.on_child_exited(pid, code),
            Event::Signalled(signal) => self.on_signalled(signal),
            Event::TimeoutElapsed => self.begin_drain(false),
            Event::GraceElapsed => self.on_grace_elapsed(),
        }
    }

    /// The run is fully over: every supervised child is reaped and an
    /// end was decided. The program half reports the summary, removes
    /// the scratch files, and exits with [`Self::exit_code`].
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.children.is_empty()
    }

    /// Whether any worker is still live.
    #[must_use]
    pub fn live_workers(&self) -> usize {
        self.children
            .iter()
            .filter(|child| matches!(child.kind, ChildKind::Worker(_)))
            .count()
    }

    /// The reaped workers' outcome counts.
    #[must_use]
    pub const fn tally(&self) -> Tally {
        self.tally
    }

    /// The process exit status of the whole run: the observed signal's
    /// `128 + n` status (`^C` → 130, `Terminate` → 143) when one ended
    /// the run, otherwise 1 if any worker failed outright, otherwise 0 —
    /// typed refusals are expected outcomes and do not fail the run (the
    /// GNU `stress` convention).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if let Some(signal) = self.signalled {
            return signal.termination_status().unwrap_or(1);
        }
        i32::from(self.tally.failed > 0)
    }

    /// Whether the `--monitor` child ended the run by quitting.
    #[must_use]
    pub const fn monitor_quit(&self) -> bool {
        self.monitor_quit
    }

    fn on_child_exited(&mut self, pid: i64, code: i32) -> Vec<Action> {
        let Some(position) = self.children.iter().position(|child| child.pid == pid) else {
            // Not ours (already reaped, or a stray report): nothing to do.
            return Vec::new();
        };
        let child = self.children.swap_remove(position);
        match child.kind {
            ChildKind::Monitor => {
                // The user quit the monitor: the run is over — tear the
                // workers down and report when they are reaped.
                self.monitor_quit = true;
                self.begin_drain(false)
            }
            ChildKind::Worker(_) => {
                // A worker asked to die during teardown exits with the
                // signal's `128 + n` status; that is the *requested*
                // outcome, counted clean. The same statuses while the run
                // is still going mean someone outside killed a worker —
                // a failure the controller reports.
                let asked_to_die = self.phase != Phase::Running
                    && (code == TERMINATE_STATUS || code == KILL_STATUS);
                if code == 0 || asked_to_die {
                    self.tally.clean += 1;
                } else if code == REFUSED_EXIT {
                    self.tally.refused += 1;
                } else {
                    self.tally.failed += 1;
                }
                // The last worker ending on its own ends the run (every
                // worker refused or failed — there is nothing left to
                // load). A completed run's teardown targets workers only:
                // a still-open monitor stays up, and the report waits for
                // the user to quit it.
                if self.phase == Phase::Running && self.live_workers() == 0 {
                    return self.begin_drain(false);
                }
                Vec::new()
            }
        }
    }

    fn on_signalled(&mut self, signal: Signal) -> Vec<Action> {
        if self.signalled.is_none()
            && matches!(signal, Signal::Interrupt | Signal::Terminate | Signal::Kill)
        {
            self.signalled = Some(signal);
        }
        // A signal ends the whole session: the monitor is torn down too.
        self.begin_drain(true)
    }

    /// Ask every live worker — and, when `include_monitor`, the monitor —
    /// to terminate, arming the grace clock if anything is still live.
    /// Idempotent: a second end request while draining adds nothing.
    fn begin_drain(&mut self, include_monitor: bool) -> Vec<Action> {
        if self.phase != Phase::Running && !include_monitor {
            return Vec::new();
        }
        let drain_workers = self.phase == Phase::Running;
        if self.phase == Phase::Running {
            // Never regress the phase: a signal landing after the grace
            // deadline already escalated to `Kill` must not re-open the
            // drain window.
            self.phase = Phase::Draining;
        }
        let mut actions = Vec::new();
        for child in &self.children {
            let ask = match child.kind {
                ChildKind::Worker(_) => drain_workers,
                ChildKind::Monitor => include_monitor,
            };
            if ask {
                actions.push(Action::Signal {
                    pid: child.pid,
                    signal: Signal::Terminate,
                });
            }
        }
        if !actions.is_empty() {
            actions.push(Action::ArmGrace);
        }
        actions
    }

    fn on_grace_elapsed(&mut self) -> Vec<Action> {
        if self.phase != Phase::Draining {
            return Vec::new();
        }
        self.phase = Phase::Killing;
        self.children
            .iter()
            .map(|child| Action::Signal {
                pid: child.pid,
                signal: Signal::Kill,
            })
            .collect()
    }
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}
