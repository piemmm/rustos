//! Production aarch64 device-IRQ wiring (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (1)).
//!
//! Brings the kernel-wide [`rustos_kernel_irq::IrqTable`] to life on the
//! aarch64 boot path so a discovered device's shared-peripheral interrupt
//! (SPI) can be bound and a task parked on it is woken when the GIC
//! delivers the line. It is the aarch64 analogue of the x86_64
//! `IoApicController` + `production_external_irq_dispatch` wiring in
//! [`crate::x86_64::arch_wrapper`]; before it, the aarch64 port kept the
//! conservative fail-closed [`rustos_kernel_core::IrqRouting::unsupported`]
//! default and delivered no device interrupts at all.
//!
//! Three pieces compose the path the kernel core (`Phase::Irq`) drives
//! through [`rustos_kernel_core::KernelArch::irq_routing`] /
//! [`rustos_kernel_core::KernelArch::install_irq_dispatch`]:
//!
//! 1. [`GicIrqController`] — a kernel-side [`IrqController`] over the
//!    arch port's validated [`GicController`]. The arch crate cannot
//!    depend on `kernel/irq` (`AGENTS.md` §17.4), so the bridge from the
//!    arch HAL [`rustos_arch_api::IrqController`] to the
//!    [`rustos_kernel_irq::IrqController`] [`IrqTable::fire`] consumes
//!    lives here, in the kernel binary, exactly like the x86_64
//!    `IoApicController` does. It adds **no** masking policy of its own —
//!    it delegates to the range-checked, fence-ordered [`GicController`]
//!    (`AGENTS.md` §2.2).
//! 2. [`gic_irq_routing`] — the [`IrqRouting`] the boot path hands
//!    [`crate::aarch64::arch_wrapper::Aarch64BinArch`], naming the
//!    `'static` [`GIC_IRQ_CONTROLLER`] and the GICv2 maximum INTID as the
//!    bind ceiling.
//! 3. [`install_device_irq_dispatch`] — publishes the live `IrqTable`
//!    into a set-once slot and registers [`production_device_irq_dispatch`]
//!    with the arch crate's EL1 IRQ-vector seam
//!    ([`rustos_arch_aarch64::exceptions::set_device_irq_dispatch`]). The
//!    EL1 IRQ handler acknowledges the GIC, forwards every non-timer INTID
//!    here, and issues the end-of-interrupt itself; this dispatcher only
//!    translates the acknowledged INTID into an [`IrqTable::fire`] (which
//!    masks the line before a waiter observes the wake —
//!    `docs/src/security/irq.md`).
//!
//! The wiring is **additive and non-regressing** (`AGENTS.md` §2.17): no
//! device SPI is bound or routed until INCREMENT (2)'s unlock kthread does
//! so, and [`production_device_irq_dispatch`] is only ever reached for a
//! non-timer INTID the GIC delivers — which cannot occur until a line is
//! routed — so the metal-confirmed boot is unaffected.

use rustos_arch_aarch64::gic::{GicController, GicMmio};
use rustos_kernel_core::IrqRouting;
use rustos_kernel_irq::{IrqController, IrqTable, MaskError};
use rustos_sync::once::OnceCell;

/// Preemption-quantum rate, in hertz (a ~10 ms time slice).
///
/// The scheduler arms the generic-timer one-shot to one quantum at this
/// rate while a CPU is contended; a tick taken while EL0 was running
/// preempts the current user task (round-robin time-slicing over the
/// EEVDF virtual-deadline order, `kernel/sched`). RustOS is tickless
/// (`AGENTS.md` §17.1): a CPU running a sole task disarms and takes no
/// ticks. The rate is the shared
/// [`DEFAULT_PREEMPT_QUANTUM_HZ`](rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ)
/// the riscv64 port also uses — defined once so the two ports cannot
/// diverge (`AGENTS.md` §2.2).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
const PREEMPT_TICK_HZ: u64 = rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ;

/// A kernel-side [`IrqController`] over the arch port's [`GicController`].
///
/// Wraps the validated GICv2 controller and re-exposes its line masking
/// through the [`rustos_kernel_irq::IrqController`] trait
/// [`IrqTable::fire`] requires. The wrapper exists only to satisfy the
/// orphan rule (both the trait and `GicController` are foreign to the arch
/// crate's dependency island) and adds no policy: every `mask` is the
/// arch controller's range-checked, `SeqCst`-fenced
/// [`rustos_arch_api::IrqController::mask`] (`AGENTS.md` §2.2 — the
/// mask-before-wake fence lives once, in the arch port).
pub struct GicIrqController<M: GicMmio + Send + Sync> {
    inner: GicController<M>,
}

impl<M: GicMmio + Send + Sync> GicIrqController<M> {
    /// Wrap an arch-port [`GicController`] as a kernel-side controller.
    #[must_use]
    pub const fn new(inner: GicController<M>) -> Self {
        Self { inner }
    }

    /// Re-arm (unmask) `line` at the GIC distributor after a completion.
    ///
    /// [`IrqTable::fire`] masks the line before a waiter observes the wake
    /// (mask-before-wake, `docs/src/security/irq.md`), so a level- or
    /// edge-triggered device cannot re-fire while the driver drains its
    /// completion queue. Once the driver has handled the completion the
    /// line must be re-enabled for the *next* one, and that re-enable is an
    /// *arch* operation ([`rustos_arch_api::IrqController::unmask`]) the
    /// kernel-side [`rustos_kernel_irq::IrqController`] trait deliberately
    /// does not expose (it carries only `mask`, the one operation
    /// [`IrqTable::fire`] needs). The re-arm therefore lives here, in the
    /// bin layer that owns the GIC (`AGENTS.md` §17.4), exactly as the
    /// `-M virt` IRQ vertical re-arms through its `GicBridge`. It adds no
    /// policy of its own — it delegates to the range-checked
    /// [`GicController`] (`AGENTS.md` §2.2).
    ///
    /// # Errors
    ///
    /// Surfaces [`rustos_arch_api::IrqControlError`] verbatim — an
    /// out-of-range line fails closed without touching the distributor
    /// (`AGENTS.md` §5.4.5).
    pub fn rearm(&self, line: u32) -> Result<(), rustos_arch_api::IrqControlError> {
        use rustos_arch_api::IrqController as ArchIrqController;
        ArchIrqController::unmask(&self.inner, line)
    }
}

/// The `'static` [`IrqTable`] the kernel core published in
/// [`crate::Phase::Irq`](rustos_kernel_core::Phase::Irq) through
/// [`install_device_irq_dispatch`], or [`None`] before it is published.
///
/// An in-kernel service kthread (the INCREMENT (2) root-unlock kthread)
/// that must bind and block on a device SPI binds on **this** table — the
/// one [`production_device_irq_dispatch`] fires into — never a fresh table
/// the EL1 vector would never reach. Reading the set-once slot is the only
/// way to reach the live table from the kthread, since the core owns its
/// allocation inside the leaked `KernelState` (`AGENTS.md` §2.2 — one
/// table definition, not two that could diverge).
///
/// Freestanding-only: the in-kernel unlock kthread that consumes it is
/// itself bare-metal aarch64 ([`crate::unlock_service`]); a host build has
/// no kthread to bind a line, so the accessor is not compiled there.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
#[must_use]
pub fn published_irq_table() -> Option<&'static IrqTable> {
    IRQ_TABLE_SLOT.get().ok().flatten().copied()
}

impl<M: GicMmio + Send + Sync> IrqController for GicIrqController<M> {
    /// Mask `line` by delegating to the arch controller, mapping its
    /// [`rustos_arch_api::IrqControlError`] onto the
    /// [`rustos_kernel_irq::MaskError`] [`IrqTable::fire`] expects.
    ///
    /// An out-of-range line maps to [`MaskError::OutOfRange`]; any other
    /// arch-side refusal maps to [`MaskError::Unsupported`] so the table
    /// surfaces it as the standard architecture-unsupported outcome
    /// (`AGENTS.md` §5.4.5 — fail closed).
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        use rustos_arch_api::{IrqControlError, IrqController as ArchIrqController};
        match ArchIrqController::mask(&self.inner, line) {
            Ok(()) => Ok(()),
            Err(IrqControlError::OutOfRange) => Err(MaskError::OutOfRange),
        }
    }
}

/// Set-once slot for the `'static` [`IrqTable`] the kernel core builds in
/// `Phase::Irq` and publishes through
/// [`install_device_irq_dispatch`].
///
/// [`production_device_irq_dispatch`] reads it from interrupt context to
/// translate an acknowledged GIC INTID into an [`IrqTable::fire`]. The
/// [`OnceCell`] enforces the one-shot-publish invariant (`AGENTS.md`
/// §2.1 — no global mutable state; this is a publish-once pointer).
static IRQ_TABLE_SLOT: OnceCell<&'static IrqTable> = OnceCell::new();

/// The `'static` GICv2-backed controller every [`IrqTable::fire`] masks
/// through.
///
/// Built over the arch port's zero-sized [`VolatileGicMmio`] handle, which
/// reads the **discovered** GICv2 distributor/CPU-interface bases on every
/// access, so the controller carries no board constant (`AGENTS.md`
/// §2.20). The bind ceiling is the GICv2 maximum INTID
/// ([`rustos_arch_aarch64::gic::MAX_INTID`]); a device SPI is bound below
/// it and the table refuses any line above it.
///
/// Freestanding-only: [`VolatileGicMmio`] performs real MMIO and exists
/// only on the bare-metal target. Host builds return
/// [`IrqRouting::unsupported`] from [`Aarch64BinArch::irq_routing`]
/// instead.
///
/// [`VolatileGicMmio`]: rustos_arch_aarch64::gic::VolatileGicMmio
/// [`Aarch64BinArch::irq_routing`]: crate::aarch64::arch_wrapper::Aarch64BinArch
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub static GIC_IRQ_CONTROLLER: GicIrqController<rustos_arch_aarch64::gic::VolatileGicMmio> =
    GicIrqController::new(GicController::new(
        rustos_arch_aarch64::gic::Gicv2::new(rustos_arch_aarch64::gic::VolatileGicMmio),
        rustos_arch_aarch64::gic::MAX_INTID,
    ));

/// The [`IrqRouting`] the aarch64 boot path installs: the GICv2 controller
/// plus the GICv2 maximum INTID as the inclusive bind ceiling.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
#[must_use]
pub fn gic_irq_routing() -> IrqRouting {
    IrqRouting {
        max_line: rustos_arch_aarch64::gic::MAX_INTID,
        controller: &GIC_IRQ_CONTROLLER,
    }
}

/// The production device-IRQ dispatcher the arch crate's EL1 IRQ-vector
/// path invokes with each acknowledged non-timer GIC INTID.
///
/// Looks up the published [`IrqTable`] and forwards to
/// [`IrqTable::fire`], which masks the line through [`GIC_IRQ_CONTROLLER`]
/// before setting the per-handle ready flag a parked waiter observes
/// (mask-before-wake, `docs/src/security/irq.md`). The GIC
/// end-of-interrupt handshake is the arch handler's job and happens after
/// this returns. The `fire` outcome is intentionally ignored: a stray INTID
/// (no binding) or an out-of-range line surfaces to the next waiter through
/// the table's own [`rustos_kernel_irq::WaitStep`] taxonomy, and the line is
/// already masked.
///
/// Safe to invoke from interrupt context: every operation is wait-free and
/// allocation-free (`AGENTS.md` §2.16). A delivery before the table is
/// published (impossible in production — the core installs the table in
/// `Phase::Irq`, strictly before any SPI is routed) returns silently.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub extern "C" fn production_device_irq_dispatch(intid: u32) {
    let Ok(Some(table)) = IRQ_TABLE_SLOT.get() else {
        return;
    };
    let _ = table.fire(intid, &GIC_IRQ_CONTROLLER);
}

/// Publish `table` and register [`production_device_irq_dispatch`] with
/// the arch crate's EL1 IRQ-vector seam.
///
/// Called once per boot by
/// [`Aarch64BinArch::install_irq_dispatch`](crate::aarch64::arch_wrapper::Aarch64BinArch).
/// A second publish (a stray re-call) fails closed by halting the CPU
/// (`AGENTS.md` §2.1 / §5.4.5); the boot pipeline calls it exactly once,
/// so the halt branch is unreachable in production.
pub fn install_device_irq_dispatch(table: &'static IrqTable) {
    if IRQ_TABLE_SLOT.set(table).is_err() {
        rustos_arch_aarch64::halt_current_cpu();
    }
    #[cfg(all(freestanding, kernel_isa = "aarch64"))]
    {
        if rustos_arch_aarch64::exceptions::set_device_irq_dispatch(production_device_irq_dispatch)
            .is_err()
        {
            rustos_arch_aarch64::halt_current_cpu();
        }
        // Bring the GICv2 up for delivery: enable the distributor and this
        // (boot) CPU's interface so a routed device SPI can reach the EL1
        // vector once IRQs are unmasked (`crate::aarch64::init_spawn`). Reset
        // state leaves every line disabled, so no interrupt fires until a
        // driver routes + enables its own line (the root-unlock kthread does
        // so for the virtio-blk completion SPI,
        // [`crate::unlock_service`]); enabling the controller is therefore
        // additive — it changes no behaviour until the first line is armed
        // (`AGENTS.md` §2.17). It is the production counterpart of the
        // `gic::init()` the `-M virt` IRQ verticals call.
        //
        // SAFETY: the GICv2 bases were configured from the device tree
        // (`gic::configure_from_fdt`, boot discovery), the MMU is on (this
        // runs in the kernel-core `irq` phase), and this is the one-time
        // boot-CPU bring-up `gic::init` documents.
        unsafe {
            rustos_arch_aarch64::gic::init();
        }
    }
}

/// Caller-owned per-CPU preemption backing for the production boot CPU.
///
/// The production aarch64 image is single-CPU (`BootInfo::new(BOOT_CPU, 1,
/// …)`), so a `PreemptStorage<1>` covers it; secondary-core preemption is
/// sized from the discovered CPU count when SMP bring-up lands (`AGENTS.md`
/// §24.1 — the per-CPU timer bookkeeping is the discovered core count,
/// never a baked-in ceiling). Published once by [`arm_preemption`].
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static PREEMPT_STORAGE: rustos_arch_aarch64::preempt::PreemptStorage<1> =
    rustos_arch_aarch64::preempt::PreemptStorage::new();

/// The EL0-preemption callback the timer IRQ path invokes for a tick taken
/// from EL0 (installed via
/// [`rustos_arch_aarch64::preempt::set_preempt_callback`]).
///
/// It suspends the user task currently running on `cpu` back to the
/// scheduler with [`rustos_kernel_core::RescheduleAction::Yield`] — the
/// *involuntary* analogue of a `yield` syscall: the task is re-enqueued at
/// its priority and the scheduler picks the next runnable task, giving
/// EEVDF-ordered time-slicing. [`rustos_kernel_core::reschedule_current`]
/// returns `false` when no resumable user kthread is published on `cpu`
/// (it cannot be reached from EL0 with none switched in, but the
/// fail-closed return means a stray invocation is a harmless no-op rather
/// than an unsound switch — `AGENTS.md` §2.9). The call only ever runs
/// after the GIC end-of-interrupt handshake (see
/// [`rustos_arch_aarch64::exceptions::handle_irq`]), so the timer line is
/// already deactivated across the context switch.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_preempt_dispatch(cpu: rustos_arch_api::CpuId) {
    let _ =
        rustos_kernel_core::reschedule_current(cpu, rustos_kernel_core::RescheduleAction::Yield);
}

/// The per-tick callback the timer IRQ path invokes on **every** tick
/// (EL0 *or* idle EL1), installed via
/// [`rustos_arch_aarch64::preempt::set_timer_callback`].
///
/// It runs the blocking-wait timed-wake sweep (Design D P-2): any waiter
/// whose finite deadline has elapsed is unparked and the one-shot is
/// re-armed to the next pending deadline
/// ([`rustos_kernel_core::timed_wake_sweep`]). This is what makes a finite
/// `hw_tree_wait` timeout fire even when the CPU is otherwise idle (every
/// task parked) and takes no preemption tick (`AGENTS.md` §17.1). It is
/// pure accounting — it never context-switches — so it is safe on a tick
/// taken in EL1; the *preemption* of an EL0 task is the separate
/// [`production_preempt_dispatch`] EL0-only callback.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_tick_dispatch(_cpu: rustos_arch_api::CpuId) {
    rustos_kernel_core::timed_wake_sweep();
}

/// Set up tickless timer-driven preemption on the boot CPU: register the
/// per-CPU preempt storage, install the EL0-preemption callback, record
/// the per-quantum interval derived from [`PREEMPT_TICK_HZ`], and enable
/// the timer PPI — but leave the generic timer **disarmed**. RustOS is
/// tickless (`AGENTS.md` §17.1 `NO_HZ`): the scheduler arms the one-shot to
/// one quantum only when it dispatches a task onto a contended CPU (via
/// `Aarch64Arch::set_preemption`), and disarms when a CPU runs a sole
/// task, so an otherwise-quiet core takes no timer interrupts.
///
/// Called once per boot by
/// [`Aarch64BinArch::install_irq_dispatch`](crate::aarch64::arch_wrapper::Aarch64BinArch),
/// immediately after [`install_device_irq_dispatch`] has brought the GICv2
/// up — the earliest point the timer PPI can be enabled. The PE keeps IRQs
/// masked here (the kernel-core `Irq` phase runs with `DAIF.I` set), so no
/// tick is *taken* until EL0 runs with IRQs unmasked
/// (`crate::aarch64::userentry`'s preemptible `SPSR`) or the root-unlock
/// kthread unmasks at EL1 — the armed timer simply leaves PPI 30 pending
/// until then, so this is **additive and non-regressing** (`AGENTS.md`
/// §2.17): a one-shot tick taken in EL1 only disarms (it never preempts —
/// the kernel is non-preemptible), and a tick taken in EL0 drives
/// [`production_preempt_dispatch`]; the scheduler re-arms the next
/// one-shot on its following dispatch.
///
/// No *scheduler-fairness* tick callback is installed: EEVDF is tickless
/// (fairness is advanced inside `Scheduler::step`, not by a periodic
/// count). The per-tick callback that *is* installed
/// ([`production_tick_dispatch`]) runs only the blocking-wait timed-wake
/// sweep (Design D P-2): it releases any elapsed `hw_tree_wait`-style
/// waiter and re-arms the one-shot to the next deadline, so the timer is
/// armed only for a real pending event — a preemption quantum and/or the
/// nearest wakeup — never a fixed periodic tick (`AGENTS.md` §17.1).
///
/// A zero `CNTFRQ_EL0` reading (a board that does not report the counter
/// frequency) leaves the kernel cooperative rather than arming a nonsense
/// interval — fail-safe (`AGENTS.md` §2.9).
pub fn arm_preemption() {
    #[cfg(all(freestanding, kernel_isa = "aarch64"))]
    {
        use rustos_arch_aarch64::preempt;

        // Set-once per boot; a stray re-call fails closed by halting rather
        // than re-pointing the live per-CPU slices (`AGENTS.md` §2.1).
        if PREEMPT_STORAGE.register().is_err() {
            rustos_arch_aarch64::halt_current_cpu();
        }

        // Install the EL0-preemption callback *before* arming the timer, so
        // the first tick taken from EL0 already has a handler.
        preempt::set_preempt_callback(production_preempt_dispatch);

        // Install the per-tick timed-wake sweep callback (Design D P-2), so
        // every tick — including one taken on an idle EL1 CPU armed solely
        // for a blocking-wait deadline — releases any elapsed waiter and
        // re-arms the one-shot to the next deadline (`AGENTS.md` §17.1).
        preempt::set_timer_callback(production_tick_dispatch);

        // Derive the tick interval from the discovered counter frequency
        // (never a board constant, `AGENTS.md` §2.20). A zero reading is a
        // fail-safe skip.
        let counter_hz = rustos_arch_aarch64::kernel_arch::read_cntfrq();
        if counter_hz == 0 {
            return;
        }
        let interval = preempt::interval_for_hz(counter_hz, PREEMPT_TICK_HZ);

        // SAFETY: this is the boot CPU (id 0); the preempt callback is
        // installed (above), the per-CPU storage is registered (above), the
        // EL1 vector table is installed (`boot::init_vectors`), and the GIC
        // is up (`install_device_irq_dispatch` ran immediately before). It
        // records the quantum, enables the timer PPI, and leaves the timer
        // disarmed; the scheduler arms the first one-shot on its next
        // dispatch onto a contended CPU (tickless, §17.1).
        unsafe {
            preempt::init_local_preempt(0, interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_aarch64::gic::{Gicv2, MAX_INTID};

    /// A host-side [`GicMmio`] that records the last distributor word
    /// written so a test can assert the controller cleared the right
    /// enable bit when masking a line.
    #[derive(Default)]
    struct MockGicMmio {
        last_icenabler_off: core::cell::Cell<usize>,
        last_icenabler_val: core::cell::Cell<u32>,
    }

    impl GicMmio for MockGicMmio {
        fn gicd_read(&self, _off: usize) -> u32 {
            0
        }
        fn gicd_write(&self, off: usize, val: u32) {
            // ICENABLER lives at 0x180..; record the disable write.
            if (0x180..0x200).contains(&off) {
                self.last_icenabler_off.set(off);
                self.last_icenabler_val.set(val);
            }
        }
        fn gicd_write_byte(&self, _off: usize, _val: u8) {}
        fn gicc_read(&self, _off: usize) -> u32 {
            0
        }
        fn gicc_write(&self, _off: usize, _val: u32) {}
    }

    // SAFETY: the mock holds only `Cell`s and is never shared across
    // threads in these single-threaded host tests; the `Send + Sync`
    // bound `GicIrqController` requires is satisfied trivially because the
    // test constructs and drops it on one thread.
    unsafe impl Send for MockGicMmio {}
    unsafe impl Sync for MockGicMmio {}

    fn controller(max_intid: u32) -> GicIrqController<MockGicMmio> {
        GicIrqController::new(GicController::new(
            Gicv2::new(MockGicMmio::default()),
            max_intid,
        ))
    }

    #[test]
    fn mask_delegates_to_the_gic_controller_for_an_in_range_line() {
        // A device SPI (INTID 32 = SPI 0) is in range and masks cleanly.
        let c = controller(MAX_INTID);
        assert_eq!(c.mask(32), Ok(()));
    }

    #[test]
    fn mask_maps_an_out_of_range_line_to_out_of_range() {
        // A controller whose ceiling is INTID 47 refuses INTID 48,
        // surfacing the arch `OutOfRange` as the kernel `MaskError`.
        let c = controller(47);
        assert_eq!(c.mask(48), Err(MaskError::OutOfRange));
    }

    #[test]
    fn rearm_unmasks_an_in_range_line() {
        // Re-arming a device SPI delegates to the arch controller's
        // unmask and succeeds for an in-range line (`AGENTS.md` §17.4 —
        // the re-arm lives in the bin layer that owns the GIC).
        let c = controller(MAX_INTID);
        assert_eq!(c.rearm(32), Ok(()));
    }

    #[test]
    fn rearm_maps_an_out_of_range_line_to_out_of_range() {
        // A line above the controller's ceiling fails closed without
        // touching the distributor (`AGENTS.md` §5.4.5).
        let c = controller(47);
        assert_eq!(
            c.rearm(48),
            Err(rustos_arch_api::IrqControlError::OutOfRange)
        );
    }
}
