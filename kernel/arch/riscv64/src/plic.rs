//! riscv64 Platform-Level Interrupt Controller (PLIC) register driver.
//!
//! Stage 4.D Item 4 (riscv64 external-IRQ controller). On the QEMU
//! `virt` board — and on every SiFive-derived platform RustOS targets
//! — external interrupts from `virtio-mmio` transports (and any other
//! wired peripheral) are routed through a single PLIC. The PLIC is the
//! riscv64 analogue of the x86_64 IO-APIC: it owns per-source priority,
//! per-context enable bitmaps, a per-context priority threshold, and a
//! per-context claim/complete register pair.
//!
//! This module abstracts the PLIC's memory-mapped 32-bit registers
//! behind the [`PlicMmio`] trait so the whole control flow is exercised
//! by host-side unit tests against an in-memory mock; the bare-metal
//! `VolatilePlicMmio` is compiled only on `target_os = "none"` (so it
//! is plain text on host doc builds).
//!
//! # Mask-before-wake
//!
//! The architecture-neutral IRQ table (`kernel/irq`) requires a
//! controller's `mask` to complete (and be globally observable)
//! *before* a wait handle's `ready` flag flips
//! (`docs/src/security/irq.md`). [`PlicController::mask`] honours the
//! contract by writing the masked source's **priority register to
//! zero** — a single 32-bit MMIO write, after which a source can never
//! out-prioritise the context threshold and so cannot re-fire — and
//! then emitting a [`core::sync::atomic::fence`] with
//! [`Ordering::SeqCst`]. The priority-write masking strategy is
//! deliberately lock-free: it is a single store to a per-source
//! register, so it needs no read-modify-write and races neither the
//! trap handler's claim/complete nor a concurrent arm/unmask on a
//! different source (no hacks,
//! interrupt-reentrancy-safe by design).
//!
//! # Arch HAL boundary
//!
//! This crate is a pure Arch HAL implementation and does **not** name
//! `kernel/irq`. [`PlicController`] exposes an inherent
//! [`PlicController::mask`]; the downstream boot consumer wraps it in a
//! local newtype that implements `kernel/irq`'s `IrqController` (orphan
//! rules), keeping the `kernel/irq` dependency out of the arch port.
//!
//! # Hart contexts
//!
//! The PLIC exposes one interrupt *context* per (hart, privilege)
//! pair. On the `virt` board the contexts are laid out
//! `M(hart0), S(hart0), M(hart1), S(hart1), …`, so hart `h`'s
//! supervisor context is `2 * h + 1` ([`s_mode_context`]). The
//! boot-to-`BootCompleted` slice is single-hart, so the controller is
//! built for the boot hart's S-mode context; SMP bring-up extends the
//! context set with the rest of the harts.

use core::sync::atomic::{fence, Ordering};

/// PLIC register-offset arithmetic.
///
/// Offsets follow the SiFive PLIC layout used verbatim by QEMU's
/// `virt` machine (`hw/intc/sifive_plic.c`). Every helper is a pure
/// function so the host unit tests pin the arithmetic without an MMIO
/// window.
pub mod regs {
    /// Base of the per-source priority registers (`4 * source`).
    pub const PRIORITY_BASE: usize = 0x0000;
    /// Base of the per-context interrupt-enable bitmaps.
    pub const ENABLE_BASE: usize = 0x2000;
    /// Stride between successive contexts' enable bitmaps.
    pub const ENABLE_CONTEXT_STRIDE: usize = 0x80;
    /// Base of the per-context threshold/claim register blocks.
    pub const CONTEXT_BASE: usize = 0x0020_0000;
    /// Stride between successive contexts' threshold/claim blocks.
    pub const CONTEXT_STRIDE: usize = 0x1000;
    /// Offset of the priority-threshold register within a context block.
    pub const THRESHOLD_OFFSET: usize = 0x0;
    /// Offset of the claim/complete register within a context block.
    pub const CLAIM_OFFSET: usize = 0x4;

    /// Byte offset of `source`'s priority register.
    #[must_use]
    pub const fn source_priority(source: u32) -> usize {
        PRIORITY_BASE + 4 * source as usize
    }

    /// Byte offset of the enable-bitmap word that holds `source`'s bit
    /// for `context`.
    #[must_use]
    pub const fn enable_word(context: usize, source: u32) -> usize {
        ENABLE_BASE + context * ENABLE_CONTEXT_STRIDE + 4 * (source as usize / 32)
    }

    /// Mask selecting `source`'s bit within its enable-bitmap word.
    #[must_use]
    pub const fn enable_bit(source: u32) -> u32 {
        1u32 << (source % 32)
    }

    /// Byte offset of `context`'s priority-threshold register.
    #[must_use]
    pub const fn threshold(context: usize) -> usize {
        CONTEXT_BASE + context * CONTEXT_STRIDE + THRESHOLD_OFFSET
    }

    /// Byte offset of `context`'s claim/complete register.
    #[must_use]
    pub const fn claim(context: usize) -> usize {
        CONTEXT_BASE + context * CONTEXT_STRIDE + CLAIM_OFFSET
    }
}

/// PLIC supervisor-context index for hart `hartid` on a `virt`-style
/// layout (`M, S, M, S, …` interleaving — supervisor context is the
/// odd member of each hart's pair).
///
/// `hartid` is taken as `usize` because it indexes the PLIC's context
/// table (a `usize`-domain quantity); the boot pipeline converts the
/// SBI-provided hart id once at the call site.
#[must_use]
pub const fn s_mode_context(hartid: usize) -> usize {
    2 * hartid + 1
}

/// Source priority installed for an armed line. Any non-zero value
/// above the context threshold (which the controller pins at zero)
/// delivers the interrupt; `1` is the lowest such value.
const ACTIVE_PRIORITY: u32 = 1;

/// Priority that masks a source: a source at priority zero never
/// out-prioritises the threshold, so the PLIC never raises it.
const MASKED_PRIORITY: u32 = 0;

/// Volatile 32-bit MMIO access to a PLIC register window.
///
/// The production implementation is `VolatilePlicMmio` (freestanding
/// only); host tests substitute an in-memory mock. All PLIC registers
/// are independent 32-bit words, so the seam takes `&self` — every
/// controller operation is a single read or single write to a distinct
/// register and needs no external locking.
pub trait PlicMmio {
    /// Read the 32-bit register at byte offset `offset` from the PLIC
    /// base.
    fn read32(&self, offset: usize) -> u32;

    /// Write `value` to the 32-bit register at byte offset `offset`
    /// from the PLIC base.
    fn write32(&self, offset: usize, value: u32);
}

/// Low-level PLIC register driver bound to one interrupt context.
///
/// Stays free of policy: it exposes the raw priority / enable /
/// threshold / claim operations and leaves the mask-before-wake and
/// capability concerns to [`PlicController`].
pub struct Plic<M: PlicMmio> {
    mmio: M,
    context: usize,
}

impl<M: PlicMmio> Plic<M> {
    /// Bind a driver to `mmio` for interrupt `context`.
    pub const fn new(mmio: M, context: usize) -> Self {
        Self { mmio, context }
    }

    /// Write `source`'s priority register.
    pub fn set_source_priority(&self, source: u32, priority: u32) {
        self.mmio.write32(regs::source_priority(source), priority);
    }

    /// Read `source`'s priority register.
    #[must_use]
    pub fn source_priority(&self, source: u32) -> u32 {
        self.mmio.read32(regs::source_priority(source))
    }

    /// Set `source`'s enable bit in this context's enable bitmap.
    ///
    /// This is a read-modify-write of the shared enable word. The
    /// external-interrupt trap handler never writes the enable bitmap
    /// (it only claims/completes and drops the *priority* register to
    /// mask), so this RMW never races a taken interrupt; concurrent
    /// writers to the same 32-source word are serialised by the caller
    /// (line-arm and the owner's re-arm are the only writers).
    pub fn enable_source(&self, source: u32) {
        let off = regs::enable_word(self.context, source);
        let word = self.mmio.read32(off) | regs::enable_bit(source);
        self.mmio.write32(off, word);
    }

    /// Clear `source`'s enable bit in this context's enable bitmap.
    pub fn disable_source(&self, source: u32) {
        let off = regs::enable_word(self.context, source);
        let word = self.mmio.read32(off) & !regs::enable_bit(source);
        self.mmio.write32(off, word);
    }

    /// Set this context's priority threshold (sources must exceed it).
    pub fn set_threshold(&self, threshold: u32) {
        self.mmio.write32(regs::threshold(self.context), threshold);
    }

    /// Claim the highest-priority pending interrupt for this context,
    /// returning its source id (`0` means "no interrupt pending").
    #[must_use]
    pub fn claim(&self) -> u32 {
        self.mmio.read32(regs::claim(self.context))
    }

    /// Signal completion of `source` to this context's claim register.
    pub fn complete(&self, source: u32) {
        self.mmio.write32(regs::claim(self.context), source);
    }

    /// The interrupt context this driver targets.
    #[must_use]
    pub const fn context(&self) -> usize {
        self.context
    }
}

/// Failure modes of the [`PlicController`] arm/unmask surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PlicError {
    /// `source` is `0` (PLIC source 0 is the reserved "no interrupt"
    /// id) or above the controller's highest configured source.
    SourceOutOfRange,
}

/// Single-context PLIC controller: the policy layer over [`Plic`].
///
/// The controller validates every source against `max_source` and
/// fails closed before touching a register. It
/// exposes an inherent [`Self::mask`] the downstream `IrqController`
/// bridge forwards to (the arch port owns no
/// `kernel/irq` dependency).
pub struct PlicController<M: PlicMmio> {
    plic: Plic<M>,
    max_source: u32,
}

impl<M: PlicMmio> PlicController<M> {
    /// Build a controller over `plic` whose highest valid source id is
    /// `max_source` (inclusive). Sources `1..=max_source` are
    /// addressable; source `0` is always rejected.
    #[must_use]
    pub const fn new(plic: Plic<M>, max_source: u32) -> Self {
        Self { plic, max_source }
    }

    /// Inclusive upper bound on accepted source ids.
    #[must_use]
    pub const fn max_source(&self) -> u32 {
        self.max_source
    }

    /// `true` iff `source` is in the addressable range `1..=max_source`.
    const fn in_range(&self, source: u32) -> bool {
        source != 0 && source <= self.max_source
    }

    /// Arm `source`: enable it in this context's bitmap, drop the
    /// context threshold to zero, and set the source priority so it can
    /// deliver. Idempotent.
    ///
    /// Called both from boot/line-setup and from the `irq_wait` park
    /// path's re-arm (the `kernel/irq` bridge forwards `rearm` here so a
    /// user-space driver's line — which no in-kernel code ever `arm`ed —
    /// is made deliverable on its behalf). The enable-bitmap
    /// read-modify-write is safe against a taken interrupt because the
    /// trap handler never writes the enable bitmap (see
    /// [`Plic::enable_source`]).
    ///
    /// # Errors
    ///
    /// [`PlicError::SourceOutOfRange`] if `source` is `0` or exceeds
    /// [`Self::max_source`].
    pub fn arm(&self, source: u32) -> Result<(), PlicError> {
        if !self.in_range(source) {
            return Err(PlicError::SourceOutOfRange);
        }
        self.plic.set_threshold(0);
        self.plic.enable_source(source);
        self.plic.set_source_priority(source, ACTIVE_PRIORITY);
        Ok(())
    }

    /// Clear the mask on `source` by restoring its delivering priority.
    ///
    /// Symmetric counterpart of [`Self::mask`]: that method drops the
    /// priority to zero, this one restores it. The enable bit is
    /// untouched (arm sets it once).
    ///
    /// # Errors
    ///
    /// [`PlicError::SourceOutOfRange`] if `source` is `0` or exceeds
    /// [`Self::max_source`].
    pub fn unmask(&self, source: u32) -> Result<(), PlicError> {
        if !self.in_range(source) {
            return Err(PlicError::SourceOutOfRange);
        }
        self.plic.set_source_priority(source, ACTIVE_PRIORITY);
        Ok(())
    }

    /// Claim the highest-priority pending interrupt for the controller's
    /// context (`0` means none pending).
    #[must_use]
    pub fn claim(&self) -> u32 {
        self.plic.claim()
    }

    /// Signal completion of `source`.
    pub fn complete(&self, source: u32) {
        self.plic.complete(source);
    }

    /// Read `source`'s current priority register. Test/diagnostic
    /// observer of the mask state (`0` == masked).
    #[must_use]
    pub fn source_priority(&self, source: u32) -> u32 {
        self.plic.source_priority(source)
    }

    /// Mask `source` by dropping its priority to zero, then emit a
    /// `SeqCst` fence so every CPU that later observes a wait handle's
    /// `ready` flag also observes the masked priority
    /// (`docs/src/security/irq.md`). Symmetric counterpart of
    /// [`Self::unmask`].
    ///
    /// This is the primitive the downstream `IrqController` bridge
    /// forwards `mask` to; keeping it inherent is what lets the arch
    /// port avoid a `kernel/irq` dependency.
    ///
    /// # Errors
    ///
    /// [`PlicError::SourceOutOfRange`] if `source` is `0` or exceeds
    /// [`Self::max_source`].
    pub fn mask(&self, source: u32) -> Result<(), PlicError> {
        if !self.in_range(source) {
            return Err(PlicError::SourceOutOfRange);
        }
        // Mask by dropping the source priority to zero: a single 32-bit
        // store, after which the source can never exceed the (zero)
        // threshold and so cannot re-fire while the driver drains its
        // completion queue.
        self.plic.set_source_priority(source, MASKED_PRIORITY);
        // SeqCst fence pairs with the SeqCst load the IRQ table performs
        // on `ready`: every CPU that observes `ready = true` also
        // observes the masked priority (`docs/src/security/irq.md`).
        fence(Ordering::SeqCst);
        Ok(())
    }
}

impl<M: PlicMmio + Send + Sync> rustos_arch_api::IrqController for PlicController<M> {
    /// Mask `line` (a PLIC source) by dropping its priority to zero,
    /// forwarding to the inherent [`PlicController::mask`].
    ///
    /// This is the HAL view of the mask-before-wake primitive the
    /// downstream `kernel/irq` bridge already forwards to; exposing it
    /// through the trait lets the architecture-neutral kernel name one
    /// controller surface across every port without
    /// the arch port acquiring a `kernel/irq` dependency.
    fn mask(&self, line: u32) -> Result<(), rustos_arch_api::IrqControlError> {
        PlicController::mask(self, line)
            .map_err(|PlicError::SourceOutOfRange| rustos_arch_api::IrqControlError::OutOfRange)
    }

    /// Unmask `line` by restoring its delivering priority, forwarding to
    /// the inherent [`PlicController::unmask`].
    fn unmask(&self, line: u32) -> Result<(), rustos_arch_api::IrqControlError> {
        PlicController::unmask(self, line)
            .map_err(|PlicError::SourceOutOfRange| rustos_arch_api::IrqControlError::OutOfRange)
    }
}

impl<M: PlicMmio + Send + Sync> rustos_arch_api::InterruptEntry for PlicController<M> {
    /// Claim the highest-priority pending source for this context.
    ///
    /// The PLIC reports source `0` ("no interrupt pending") when nothing
    /// is pending; the HAL surface maps that to [`None`].
    fn claim(&self) -> Option<u32> {
        match PlicController::claim(self) {
            0 => None,
            source => Some(source),
        }
    }

    /// Signal completion of `line` to the PLIC's claim register.
    fn complete(&self, line: u32) {
        PlicController::complete(self, line);
    }
}

/// Bare-metal [`PlicMmio`] over a fixed PLIC base address.
///
/// Compiled only for the freestanding riscv64 target; host builds use
/// the in-memory mock in the test module.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub struct VolatilePlicMmio {
    base: usize,
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
impl VolatilePlicMmio {
    /// Construct an accessor over the PLIC register window at physical
    /// (identity-mapped) address `base`.
    ///
    /// # Safety
    ///
    /// `base` must be the PLIC register-block base for the running
    /// platform (read from the device tree), identity-mapped and
    /// readable/writable for the life of the kernel. Nothing else may
    /// alias the window.
    #[must_use]
    pub const unsafe fn new(base: usize) -> Self {
        Self { base }
    }
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
impl PlicMmio for VolatilePlicMmio {
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: `base + offset` addresses a 32-bit PLIC register
        // inside the window the constructor's caller guaranteed is
        // valid; PLIC registers are 4-byte aligned by construction.
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as `read32`; the write targets a 4-byte-aligned PLIC
        // register inside the validated window.
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }
}

#[cfg(test)]
#[path = "plic_tests.rs"]
mod tests;
