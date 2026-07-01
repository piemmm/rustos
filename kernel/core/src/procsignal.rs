//! The kernel-side process-signal seam the `signal` (`abi-v1` number 64)
//! syscall uses (`plans/SPAWN.md` SP7).
//!
//! [`ProcessSignal`] is the one object-safe boundary between the
//! arch-neutral syscall handler in `kernel/core` and the scheduler-side
//! producer that delivers a control signal to one of the sender's children.
//! Like the [`ProcessWait`](crate::procwait::ProcessWait),
//! [`ProcessSpawn`](crate::spawn::ProcessSpawn), and
//! [`MemMap`](crate::memmap::MemMap) seams, the concrete producer is
//! installed at boot through the `with_process_signal` builder and the
//! handler reaches it through this trait.
//!
//! Until a producer is installed the handler holds [`NULL_PROCESS_SIGNAL`],
//! which fails closed: every `signal` returns [`Errno::NotImplemented`],
//! never pretending a signal was delivered — exactly as
//! [`NULL_PROCESS_WAIT`](crate::procwait::NULL_PROCESS_WAIT) does for
//! `wait`. The scheduler-side producer that actually delivers the signal
//! (terminate/kill/stop/continue a child) lands in a later increment
//! (`plans/SPAWN.md` `SP7b`); this module is the fail-closed floor it slots
//! into.

use rustos_abi::{Errno, Signal};
use rustos_kernel_sec::TaskId;

/// The kernel-side producer of the `signal` syscall.
///
/// Implemented by the scheduler-side producer that authorises the target
/// against the sender's own children (a process may signal only children it
/// spawned) and delivers the control signal. Implementations must be [`Sync`]:
/// the single installed producer is shared by the per-CPU syscall handlers,
/// exactly like the process-wait producer, the spawn producer, and the
/// console device.
pub trait ProcessSignal: Sync {
    /// Deliver `signal` to the child selected by `pid` on behalf of `sender`.
    ///
    /// `sender` is the kernel-attested identity of the calling task (supplied
    /// by the dispatcher, never by the caller), and `pid` names a child the
    /// sender spawned. The implementation validates the parent/child
    /// relationship — a process may signal only its **own** children — and
    /// fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::NotFound`] when `pid` does not name a child of
    /// `sender`. The default producer ([`NullProcessSignal`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn signal(&self, sender: TaskId, pid: i32, signal: Signal) -> Result<(), Errno>;
}

/// The process-signal producer installed before any real one exists.
///
/// Every `signal` fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default, so a `signal` issued before the boot path installs
/// the scheduler-side producer announces an inert interface rather than
/// pretending a signal was delivered.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullProcessSignal;

impl ProcessSignal for NullProcessSignal {
    fn signal(&self, _sender: TaskId, _pid: i32, _signal: Signal) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullProcessSignal`] instance the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `process_signal` borrow here so
/// the field is always valid without an `Option` branch on the hot path; the
/// boot path replaces it with the concrete producer through
/// `KernelSyscallHandlers::with_process_signal`.
pub static NULL_PROCESS_SIGNAL: NullProcessSignal = NullProcessSignal;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_process_signal_fails_closed() {
        // Every variant of the closed signal set fails closed on the inert
        // default rather than pretending it was delivered.
        for signal in [Signal::Continue, Signal::Terminate, Signal::Kill] {
            assert_eq!(
                NULL_PROCESS_SIGNAL.signal(TaskId(1), 2, signal),
                Err(Errno::NotImplemented)
            );
        }
    }
}
