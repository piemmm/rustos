//! The kernel-side process-wait seam the `wait` (`abi-v1` number 16)
//! syscall uses (`plans/SPAWN.md` SP6).
//!
//! [`ProcessWait`] is the one object-safe boundary between the
//! arch-neutral syscall handler in `kernel/core` and the producer that
//! blocks the caller until one of its children exits, reaps the zombie,
//! and reports the child's exit code. Reaping a child requires walking the
//! scheduler's parent/child + exit-status bookkeeping and cooperatively
//! parking the caller until a child is reapable — work that belongs with
//! the live scheduler integration, not in the decoupled handler — so, like
//! the [`ProcessSpawn`](crate::spawn::ProcessSpawn) and
//! [`MemMap`](crate::memmap::MemMap) producers, the concrete implementation
//! is installed at boot through a `with_*` builder and the handler reaches
//! it through this trait.
//!
//! Until a producer is installed the handler holds [`NULL_PROCESS_WAIT`],
//! which fails closed with [`Errno::NotImplemented`] (`AGENTS.md` §2.9). A
//! build whose scheduler-side wait producer is not yet wired (the state
//! before `plans/SPAWN.md` `SP6b` lands a real producer) therefore
//! announces an intentionally inert interface rather than fabricating an
//! exit code — exactly as [`NULL_MEM_MAP`](crate::memmap::NULL_MEM_MAP) and
//! [`NULL_PROCESS_SPAWN`](crate::spawn::NULL_PROCESS_SPAWN) do for their
//! syscalls.

use rustos_abi::Errno;
use rustos_kernel_sec::TaskId;

/// A child process reaped by [`ProcessWait::wait`].
///
/// Carries the reaped child's PID (the value the `wait` syscall returns to
/// the caller) and its exit code (the value the kernel writes to the
/// caller's `status` out-pointer).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ReapedChild {
    /// PID of the child that was reaped.
    pub pid: u32,
    /// The exit code the child passed to `exit` (`AGENTS.md` §20 — the
    /// program's terminating status).
    pub code: i32,
}

/// The kernel-side producer of the `wait` syscall.
///
/// Implemented by the scheduler-port-installed producer that blocks
/// `parent` until one of its children exits, reaps that child, and reports
/// it. The trait is deliberately minimal — the single already-validated
/// user-facing operation — so `kernel/core` stays free of the scheduler's
/// parent/child + exit-status bookkeeping (`AGENTS.md` §17.4) and the
/// syscall handler owns the capability posture and argument validation,
/// never the producer.
///
/// Implementations must be [`Sync`]: the single installed producer is
/// shared by the per-CPU syscall handlers, exactly like the console device,
/// the spawn producer, and the anonymous-memory producer.
pub trait ProcessWait: Sync {
    /// Block `parent` until the child selected by `pid` exits, reap it, and
    /// return the reaped child's PID and exit code.
    ///
    /// `pid` is either a specific child's PID or [`rustos_abi::WAIT_ANY`]
    /// to wait for whichever of `parent`'s children exits next. The handler
    /// has already validated that the caller passed a non-null `status`
    /// pointer (the dispatcher rejects a null `UserPtr`); the implementation
    /// validates the parent/child relationship — a process may only reap its
    /// **own** children (`AGENTS.md` §4 / §5.4) — and fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::NotFound`] when `pid` does not name a child of
    /// `parent` (and `parent` has no children, for [`rustos_abi::WAIT_ANY`]).
    /// The default producer ([`NullProcessWait`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn wait(&self, parent: TaskId, pid: i32) -> Result<ReapedChild, Errno>;
}

/// The process-wait producer installed before any real one exists.
///
/// Every wait fails closed with [`Errno::NotImplemented`] — the fail-closed
/// default `AGENTS.md` §2.9 / §5.4 require, so a `wait` issued before the
/// boot path installs the scheduler-side producer (the state before
/// `plans/SPAWN.md` `SP6b`) announces an inert interface rather than
/// fabricating a reaped child or an exit code.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullProcessWait;

impl ProcessWait for NullProcessWait {
    fn wait(&self, _parent: TaskId, _pid: i32) -> Result<ReapedChild, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullProcessWait`] instance the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `process_wait` borrow here so the
/// field is always valid without an `Option` branch on the hot path; the
/// boot path replaces it with the real producer through
/// `KernelSyscallHandlers::with_process_wait` once `plans/SPAWN.md` `SP6b`
/// lands.
pub static NULL_PROCESS_WAIT: NullProcessWait = NullProcessWait;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_process_wait_fails_closed() {
        assert_eq!(
            NULL_PROCESS_WAIT.wait(TaskId(7), 9),
            Err(Errno::NotImplemented)
        );
        // A WAIT_ANY request announces the inert interface too, rather than
        // pretending a child was reaped.
        assert_eq!(
            NullProcessWait.wait(TaskId(1), rustos_abi::WAIT_ANY),
            Err(Errno::NotImplemented)
        );
    }
}
