//! Shared supervision witness for the spawn-session QEMU verticals.
//!
//! Both freestanding guests (`tests/integration/spawn_session_qemu_aarch64`
//! and `..._x86_64`) drive their audit sink through this one state machine,
//! so the two ports assert the same property and neither can drift.
//!
//! # What it witnesses
//!
//! That PID 1 **supervises** the login session rather than spawning it and
//! forgetting it: the session ran and exited, PID 1 was waiting on its
//! children, and PID 1 then built a replacement.
//!
//! # Why it keys on identity, not on event counts
//!
//! The obvious encoding — "PASS once N processes have been built and M
//! syscalls audited" — silently couples the assertion to the *length of the
//! boot-service list*. Adding one service shifts both totals, the thresholds
//! are then met at the session's **first** spawn, and the guest reports PASS
//! having observed no supervision whatsoever. That is not a hypothetical: it
//! is what the counting version did once a time service joined the startup
//! set, and only the runner's serial-script guard caught it.
//!
//! So nothing here counts. Each step is recognised by *who* acted and *what*
//! they did, which is invariant under any change to the service list.

#![no_std]
#![deny(missing_docs)]

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use tairix_log::{Event, EventId, FieldValue};

/// Process name PID 1 is admitted under.
pub const SUPERVISOR_COMM: &str = tairix_kernel_core::init::INIT_PROC_NAME;

/// Process name the login session is admitted under.
///
/// The kernel derives it from the session bundle's path by stripping the
/// generic `Run` entry point and the `.app` suffix, so
/// `/System/Services/login.app/Run` is attested as `login`.
pub const SESSION_COMM: &str = "login";

/// Syscall name recorded when a process terminates itself.
pub const EXIT_SYSCALL: &str = "exit";

/// Syscall name recorded when a process blocks reaping its children.
pub const WAIT_SYSCALL: &str = "wait";

/// Audit field naming the acting process.
const COMM_FIELD: &str = "comm";

/// Audit field naming the dispatched syscall.
const SYSCALL_FIELD: &str = "sc";

/// Emitted once an EL0/ring-3 image has been built.
const PROCESS_SPAWNED: EventId = tairix_kernel_core::AuditEvent::ProcessSpawned.id();

/// Emitted for each successfully dispatched audited syscall.
const SYSCALL_INVOKED: EventId = tairix_kernel_syscall::AuditEvent::SyscallInvoked.id();

/// How far along the supervision cycle the witness has got.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Stage {
    /// Waiting for the first session instance to exit.
    AwaitSessionExit,
    /// The session exited; waiting for PID 1 to build its replacement.
    AwaitRelaunch,
    /// The full launch → run → exit → reap → relaunch cycle was observed.
    Complete,
}

impl Stage {
    /// Discriminant held in the witness's atomic.
    const fn code(self) -> u8 {
        match self {
            Self::AwaitSessionExit => 0,
            Self::AwaitRelaunch => 1,
            Self::Complete => 2,
        }
    }

    /// Inverse of [`Self::code`]; an unknown code cannot arise because the
    /// atomic is only ever written from this type.
    const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::AwaitSessionExit,
            1 => Self::AwaitRelaunch,
            _ => Self::Complete,
        }
    }
}

/// Observes an audit stream and reports when PID 1 has demonstrably
/// supervised the login session.
///
/// Lives in a `static`, so it is driven through `&self` and keeps its state
/// in atomics.
pub struct SupervisionWitness {
    stage: AtomicU8,
    supervisor_waited: AtomicBool,
}

impl SupervisionWitness {
    /// A witness that has observed nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stage: AtomicU8::new(Stage::AwaitSessionExit.code()),
            supervisor_waited: AtomicBool::new(false),
        }
    }

    /// How far along the cycle the witness has got.
    #[must_use]
    pub fn stage(&self) -> Stage {
        Stage::from_code(self.stage.load(Ordering::Acquire))
    }

    /// Fold one audit record in, returning whether the cycle is now complete.
    ///
    /// Idempotent once complete, so a sink may keep calling it.
    pub fn observe(&self, event: &Event<'_>) -> bool {
        if event.id == SYSCALL_INVOKED {
            self.observe_syscall(
                str_field(event, COMM_FIELD).unwrap_or_default(),
                str_field(event, SYSCALL_FIELD).unwrap_or_default(),
            );
        } else if event.id == PROCESS_SPAWNED {
            self.observe_spawn();
        }
        self.stage() == Stage::Complete
    }

    /// Fold in an audited syscall by acting process and syscall name.
    pub fn observe_syscall(&self, comm: &str, syscall: &str) {
        if comm == SUPERVISOR_COMM && syscall == WAIT_SYSCALL {
            self.supervisor_waited.store(true, Ordering::Release);
        }
        if comm == SESSION_COMM
            && syscall == EXIT_SYSCALL
            && self.stage() == Stage::AwaitSessionExit
        {
            self.stage
                .store(Stage::AwaitRelaunch.code(), Ordering::Release);
        }
    }

    /// Fold in a process-image build.
    ///
    /// Only meaningful once the session has exited: the next image built
    /// after that is the replacement, and a process that has exited cannot
    /// be the one running it. The supervisor must also have been seen
    /// waiting, so a spawn-and-forget that never reaps cannot pass.
    pub fn observe_spawn(&self) {
        if self.stage() == Stage::AwaitRelaunch && self.supervisor_waited.load(Ordering::Acquire) {
            self.stage.store(Stage::Complete.code(), Ordering::Release);
        }
    }
}

impl Default for SupervisionWitness {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a string-valued audit field, or `None` if absent or another type.
fn str_field<'a>(event: &Event<'a>, key: &str) -> Option<&'a str> {
    event
        .fields
        .iter()
        .find(|field| field.key == key)
        .and_then(|field| match field.value {
            FieldValue::Str(text) => Some(text),
            _ => None,
        })
}

#[cfg(test)]
mod tests;
