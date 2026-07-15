//! [`PlicIrqController`] — the `kernel/irq` [`IrqController`] bridge over
//! the riscv64 arch port's PLIC register driver.
//!
//! The riscv64 arch port (`kernel/arch/riscv64`) is a pure Arch HAL
//! implementation and owns no `kernel/irq` dependency: its
//! [`rustos_arch_riscv64::plic::PlicController`] exposes an inherent
//! `arm`/`mask`/`unmask`/`claim`/`complete` surface (and implements the Arch
//! HAL's [`rustos_arch_api::IrqController`]), but it does not implement the
//! architecture-neutral [`rustos_kernel_irq::IrqController`] trait
//! [`rustos_kernel_irq::IrqTable::fire`] and [`IrqParkWaiter`] drive. Rust's
//! orphan rules forbid a third crate from implementing that foreign trait for
//! the foreign `PlicController` directly, so this newtype is the smallest
//! local type that bridges the two — exactly mirroring how the x86_64
//! `IoApicController` lives in this same binary crate.
//!
//! [`IrqParkWaiter`]: rustos_kernel_core::IrqParkWaiter
//!
//! The wrapper [`core::ops::Deref`]s to the inner controller, so a holder
//! keeps the full inherent surface (`arm`, `claim`, `complete`,
//! `source_priority`) and additionally satisfies `&dyn IrqController` for
//! [`rustos_kernel_irq::IrqTable::fire`] and the re-arm the park path drives.
//!
//! # Why the crate root, not the `riscv64` port module
//!
//! The bridge is generic over [`rustos_arch_riscv64::plic::PlicMmio`], so it
//! is host-buildable (the mask-before-wake regression test drives an in-memory
//! mock register file). It therefore lives at the crate root — gated on the
//! riscv64 image build *or* a host `cargo test` — rather than inside the
//! freestanding-only [`crate::riscv64`] port module, so its host test keeps
//! running under `cargo test` on the CI host. The `virt`-board QEMU verticals
//! re-export it from here (one definition, no duplication).

use core::ops::Deref;

use rustos_arch_riscv64::plic::{PlicController, PlicMmio};
use rustos_kernel_irq::{IrqController, MaskError};

/// `IrqController` bridge wrapping a [`PlicController`].
///
/// `mask` forwards to the inherent [`PlicController::mask`] (which drops the
/// source priority to zero and emits a `SeqCst` fence — the mask-before-wake
/// contract) and `rearm` forwards to [`PlicController::unmask`] (restoring the
/// delivering priority so the next completion fires), each mapping the arch
/// port's [`rustos_arch_riscv64::plic::PlicError`] onto
/// [`MaskError::OutOfRange`].
pub struct PlicIrqController<M: PlicMmio>(PlicController<M>);

impl<M: PlicMmio> PlicIrqController<M> {
    /// Wrap `controller` so it can be driven as an [`IrqController`].
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

impl<M: PlicMmio + Send + Sync> IrqController for PlicIrqController<M> {
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        // The PLIC controller validates `line` against its configured source
        // range and fails closed; the only failure mode is an out-of-range
        // source.
        self.0.mask(line).map_err(|_| MaskError::OutOfRange)
    }

    fn rearm(&self, line: u32) -> Result<(), MaskError> {
        // `IrqTable::fire` masked the source (priority → 0) before the waiter
        // observed `ready` (mask-before-wake); the `irq_wait` / boot park path
        // re-arms it through here once the driver has drained the completion,
        // restoring the source's delivering priority so the next device
        // interrupt is taken. Idempotent: the enable bit is set once at `arm`,
        // so this only rewrites the priority register. An out-of-range source
        // fails closed without touching a register.
        self.0.unmask(line).map_err(|_| MaskError::OutOfRange)
    }
}

#[cfg(test)]
#[path = "riscv64_plic_irq_tests.rs"]
mod tests;
