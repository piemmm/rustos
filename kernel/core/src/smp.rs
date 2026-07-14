//! Secondary-CPU hand-off: the seam a freshly started core joins the
//! live kernel through.
//!
//! [`crate::kernel_main`] brings secondary CPUs online only after every
//! init phase has succeeded, so a started core arrives to a fully-built
//! kernel: scheduler, IRQ dispatch, and syscall hook all live. The
//! hand-off is a set-once published handle over the boot-leaked
//! `KernelState` (published *before* any core is started, so a secondary
//! can never observe an empty slot in a correct boot); the arch port's
//! secondary entry performs its per-CPU hardware init and then calls
//! [`run_secondary`], which audits the core's arrival and runs the same
//! dispatch loop the boot CPU drives — per-CPU run queue, work stealing,
//! and the tickless idle park.
//!
//! A secondary CPU never decides the system is finished: when it has
//! nothing to run it parks on its idle instruction and is woken by the
//! scheduler's placement IPI or a device interrupt. Only the boot CPU
//! owns the "all tasks exited" halt. [`run_secondary`] therefore returns
//! only on a hand-off refusal (fail closed: unpublished slot, an id out
//! of range) or a scheduler error, and the port's entry parks the core.

use rustos_kernel_sched_api::CpuId;
use rustos_log::{Field, FieldValue, Level, Sink};
use rustos_sync::OnceCell;
use rustos_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};

/// What a started secondary core needs from the live kernel, type-erased
/// so the arch port's `extern "C"` entry can reach the generic
/// `Scheduler<A>` / arch pair without naming either concrete type.
///
/// Implemented over the boot-leaked `KernelState` and published through
/// [`publish_secondary_dispatch`] exactly once per boot.
pub(crate) trait SecondaryDispatch: Sync {
    /// Dense id of the boot CPU (which must never enter [`run_secondary`]).
    fn boot_cpu(&self) -> CpuId;

    /// Total logical CPUs the handover declared; ids at or beyond it are
    /// refused.
    fn cpu_count(&self) -> u32;

    /// The audit sink the arrival record is emitted through.
    fn audit_sink(&self) -> &'static (dyn Sink + Sync);

    /// Record that dense id `cpu` has completed its per-CPU bring-up and
    /// is about to join the dispatch loop.
    ///
    /// This is the acknowledgement the boot CPU's bring-up barrier waits
    /// on: it is set once, on `cpu` itself, immediately before
    /// [`run`](Self::run) is entered — so a boot CPU that observes it
    /// knows the core has adopted the kernel translation regime, armed
    /// its per-CPU interrupt state, and is live. A core that never sets
    /// it never checked in, and the boot CPU's bounded wait fails loud
    /// rather than proceeding over a half-brought-up core.
    fn mark_online(&self, cpu: CpuId);

    /// Run the kernel dispatch loop on `cpu`. Returns only on a
    /// scheduler error; the caller then parks the core.
    fn run(&self, cpu: CpuId);
}

/// The published hand-off. Set exactly once by `kernel_main` before any
/// secondary is started; a secondary reading an empty slot is a boot
/// ordering defect and fails closed through
/// [`SecondaryExit::NotPublished`].
static SECONDARY_DISPATCH: OnceCell<&'static (dyn SecondaryDispatch + Sync)> = OnceCell::new();

/// Why [`run_secondary`] handed the CPU back to the arch port. Every
/// variant is terminal for the core: the port parks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondaryExit {
    /// No dispatch hand-off was published — the core was started before
    /// (or without) `kernel_main`'s publish, so it refuses to run
    /// anything (fail closed).
    NotPublished,
    /// The id names the boot CPU or lies outside the declared CPU count;
    /// scheduling on it would corrupt per-CPU state (fail closed).
    InvalidCpu,
    /// The dispatch loop stopped on a scheduler error.
    Stopped,
}

impl SecondaryExit {
    /// Stable cause string for the port's diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotPublished => "secondary_dispatch_not_published",
            Self::InvalidCpu => "secondary_cpu_id_invalid",
            Self::Stopped => "secondary_dispatch_stopped",
        }
    }
}

/// Error from [`publish_secondary_dispatch`]: the slot is set-once per
/// boot, so a second publish is refused rather than re-pointing live
/// secondaries at a different kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchAlreadyPublished;

/// Publish the hand-off. Called by `kernel_main` exactly once, before
/// any secondary CPU is started.
pub(crate) fn publish_secondary_dispatch(
    handle: &'static (dyn SecondaryDispatch + Sync),
) -> Result<(), DispatchAlreadyPublished> {
    SECONDARY_DISPATCH
        .set(handle)
        .map_err(|_| DispatchAlreadyPublished)
}

/// Join the calling secondary CPU to the live kernel: audit its arrival
/// (`SecondaryCpuOnline`) and run the kernel dispatch loop on `cpu`.
///
/// Called by the arch port's secondary entry after that core's own
/// hardware init (MMU adoption, vectors, interrupt controller, per-CPU
/// preemption timer) is complete — the loop enables device IRQs on this
/// core as its first act, so that init must already be in place.
///
/// Returns only when the core must stop (see [`SecondaryExit`]); the
/// port then parks it forever. It never returns "success".
pub fn run_secondary(cpu: CpuId) -> SecondaryExit {
    let handle = match SECONDARY_DISPATCH.get() {
        Ok(Some(handle)) => *handle,
        _ => return SecondaryExit::NotPublished,
    };
    if cpu == handle.boot_cpu() || cpu >= handle.cpu_count() {
        return SecondaryExit::InvalidCpu;
    }
    let mut cpu_buf = [0u8; 12];
    emit(
        handle.audit_sink(),
        Level::Info,
        AuditEvent::SecondaryCpuOnline,
        &[Field {
            key: "cpu",
            value: FieldValue::Str(format_usize(cpu as usize, &mut cpu_buf)),
        }],
    );
    // Acknowledge arrival *before* entering the (never-returning) dispatch
    // loop, so the boot CPU's bring-up barrier can observe that this core
    // is fully live and release the next secondary / proceed to spawn PID
    // 1. The order is load-bearing: the ack is published only after the
    // arch port's secondary entry has adopted the kernel translation
    // regime and armed this core's interrupt state, so a boot CPU that
    // sees it can safely mutate shared kernel state (the ack is the
    // "bring-up complete" edge).
    handle.mark_online(cpu);
    handle.run(cpu);
    SecondaryExit::Stopped
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};

    struct FakeDispatch {
        ran_on: AtomicU32,
        online: AtomicU32,
        sink: &'static crate::test_sink::TestSink,
    }

    impl SecondaryDispatch for FakeDispatch {
        fn boot_cpu(&self) -> CpuId {
            0
        }
        fn cpu_count(&self) -> u32 {
            2
        }
        fn audit_sink(&self) -> &'static (dyn Sink + Sync) {
            self.sink
        }
        fn mark_online(&self, cpu: CpuId) {
            self.online.store(cpu, Ordering::SeqCst);
        }
        fn run(&self, cpu: CpuId) {
            self.ran_on.store(cpu, Ordering::SeqCst);
        }
    }

    /// One test drives the whole lifecycle because the publish slot is
    /// genuinely set-once per process: pre-publish refusal, boot-CPU and
    /// out-of-range refusals, and the audited run on a valid id.
    #[test]
    fn run_secondary_fails_closed_then_runs_a_valid_cpu() {
        assert_eq!(run_secondary(1), SecondaryExit::NotPublished);

        let sink: &'static crate::test_sink::TestSink =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(crate::test_sink::TestSink::new()));
        let handle: &'static FakeDispatch =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(FakeDispatch {
                ran_on: AtomicU32::new(u32::MAX),
                online: AtomicU32::new(u32::MAX),
                sink,
            }));
        publish_secondary_dispatch(handle).expect("first publish succeeds");
        assert!(
            publish_secondary_dispatch(handle).is_err(),
            "the slot is set-once"
        );

        assert_eq!(run_secondary(0), SecondaryExit::InvalidCpu);
        assert_eq!(run_secondary(2), SecondaryExit::InvalidCpu);
        assert_eq!(handle.ran_on.load(Ordering::SeqCst), u32::MAX);

        assert_eq!(run_secondary(1), SecondaryExit::Stopped);
        assert_eq!(handle.ran_on.load(Ordering::SeqCst), 1);
        // The arrival acknowledgement is published before the dispatch
        // loop is entered, so the boot CPU's bring-up barrier observes it.
        assert_eq!(handle.online.load(Ordering::SeqCst), 1);
        assert!(
            sink.event_ids()
                .contains(&AuditEvent::SecondaryCpuOnline.id().0),
            "arrival must be audited"
        );
    }
}
