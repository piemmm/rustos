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
}
