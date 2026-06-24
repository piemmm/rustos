//! [`PlicIrqController`] — the `kernel/irq` `IrqController` bridge over
//! the arch port's PLIC register driver.
//!
//! The riscv64 arch port (`kernel/arch/riscv64`) is a pure Arch HAL
//! implementation and owns no `kernel/irq` dependency: its [`rustos_arch_riscv64::plic::PlicController`] exposes an
//! inherent `mask`/`arm`/`unmask`/`claim`/`complete` surface but does
//! not implement the architecture-neutral
//! [`rustos_kernel_irq::IrqController`] trait. Rust's orphan rules
//! forbid a third crate from implementing that foreign trait for the
//! foreign `PlicController` directly, so this newtype is the smallest
//! local type that bridges the two — exactly mirroring how the x86_64
//! `IoApicController` lives downstream in `kernel/rustos-kernel`.
//!
//! The wrapper [`core::ops::Deref`]s to the inner controller, so a
//! holder keeps the full inherent surface and additionally satisfies
//! `&dyn IrqController` for [`rustos_kernel_irq::IrqTable::fire`].

use core::ops::Deref;

use rustos_arch_riscv64::plic::{PlicController, PlicMmio};
use rustos_kernel_irq::{IrqController, MaskError};

/// `IrqController` bridge wrapping a [`PlicController`].
///
/// `mask` forwards to the inherent [`PlicController::mask`] (which drops
/// the source priority to zero and emits a `SeqCst` fence — the
/// mask-before-wake contract) and maps its [`rustos_arch_riscv64::plic::PlicError`]
/// onto [`MaskError::OutOfRange`].
pub struct PlicIrqController<M: PlicMmio>(PlicController<M>);

impl<M: PlicMmio> PlicIrqController<M> {
    /// Wrap `controller` so it can be driven as an `IrqController`.
    #[must_use]
    pub const fn new(controller: PlicController<M>) -> Self {
        Self(controller)
    }
}

impl<M: PlicMmio> Deref for PlicIrqController<M> {
    type Target = PlicController<M>;

    fn deref(&self) -> &PlicController<M> {
        &self.0
    }
}

impl<M: PlicMmio> IrqController for PlicIrqController<M> {
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        // The PLIC controller validates `line` against its configured
        // source range and fails closed; the only failure mode is an
        // out-of-range source.
        self.0.mask(line).map_err(|_| MaskError::OutOfRange)
    }
}

#[cfg(test)]
#[path = "plic_irq_tests.rs"]
mod tests;
