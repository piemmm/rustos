//! `rustos-dma-barrier` — DMA memory-ordering barriers for user-space drivers.
//!
//! A user-space driver shares a block of memory with a device that is a
//! separate bus master (an xHCI controller, a virtio device, …). On a
//! platform whose device DMA is **not** I/O-coherent — the Raspberry Pi 4's
//! PCIe root complex is the standing example — that shared block is mapped
//! Normal **Non-Cacheable** (`PageFlags::DMA_COHERENT`, see `kernel/arch`).
//! Non-cacheable removes the *cache*-coherency problem, but it does **not**
//! make the CPU's accesses *ordered* with respect to the device: without an
//! explicit barrier the CPU's write that rings a doorbell can be observed by
//! the device before the descriptor writes the doorbell announces, and a read
//! of a device-written ring entry can observe a freshly-set ownership/cycle
//! flag together with the *previous* entry's stale payload (a torn read).
//!
//! This crate is the single home of the architecture-specific barrier
//! instruction (`AGENTS.md` §2.2), the user-space analogue of the syscall-trap
//! carve-out in `rustos-abi-trap` and the §1 assembly carve-out it belongs to:
//! the barrier is something the silicon strictly requires and that no
//! target-neutral Rust can express to the right shareability domain. (Rust's
//! own [`core::sync::atomic::fence`] lowers to an *inner*-shareable `dmb ish`
//! on AArch64, which does not order accesses with respect to a non-coherent
//! outer/system-domain DMA master, so it is **not** a correct substitute here.)
//!
//! Two barriers, matching the two ordering hazards above:
//!
//! * [`dma_wmb`] — a **write** barrier. Call it after writing device-visible
//!   data (descriptors/TRBs/buffers) and **before** the MMIO store that hands
//!   that data to the device (a doorbell / queue-notify). It guarantees the
//!   data writes are observable by the device before the doorbell write is.
//! * [`dma_rmb`] — a **read** barrier. Call it after reading a device-written
//!   ownership/cycle flag and finding the entry owned, and **before** reading
//!   the rest of that entry. It guarantees the payload reads observe the
//!   device's writes that the flag announced, never stale bytes.
//!
//! # Targets
//!
//! The barrier instruction is compiled in only for the three native Tier-1
//! targets (`x86_64`, `aarch64`, `riscv64`); each per-arch block is gated on a
//! build-script-emitted `dma_barrier_<arch>` cfg (`build.rs`) rather than a
//! target-architecture predicate, so the instruction choice stays out of the
//! source tree the §17.2 `cfg-check` guards. On the host (unit tests,
//! `cargo xtask ci`) and on `wasm32` — a single-threaded sandbox with no
//! separate DMA master — the barriers are a no-op.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

/// Order writes to device-shared memory before a following MMIO store.
///
/// Issue this **after** writing the descriptors/ring entries/buffers a device
/// will read and **before** the MMIO write (doorbell / queue-notify) that
/// tells the device to read them. Without it, the device — a separate,
/// non-coherent bus master — may observe the doorbell write before the data
/// writes and act on stale memory.
///
/// AArch64: `dmb oshst` — an outer-shareable store-store barrier, the domain
/// that covers a non-coherent DMA master (the same choice Linux's `dma_wmb()`
/// makes). It orders the Normal-Non-Cacheable data stores ahead of the
/// Device-memory doorbell store.
#[cfg(dma_barrier_aarch64)]
#[inline(always)]
pub fn dma_wmb() {
    // SAFETY: a barrier-only instruction. It performs no memory access, takes
    // no operands, and clobbers no registers; it only constrains the
    // observable order of stores this PE has already issued.
    unsafe {
        core::arch::asm!("dmb oshst", options(nostack, preserves_flags));
    }
}

/// Order reads of device-written memory after observing its ownership flag.
///
/// Issue this **after** reading a device-written ownership/cycle flag and
/// finding the entry owned by software, and **before** reading the rest of
/// that entry. Without it the payload reads may observe bytes from before the
/// device's write that the flag announced (a torn read of a ring entry).
///
/// AArch64: `dmb oshld` — an outer-shareable load barrier (the same choice
/// Linux's `dma_rmb()` makes), ordering the flag load ahead of the payload
/// loads with respect to the non-coherent DMA master's writes.
#[cfg(dma_barrier_aarch64)]
#[inline(always)]
pub fn dma_rmb() {
    // SAFETY: a barrier-only instruction (see `dma_wmb`).
    unsafe {
        core::arch::asm!("dmb oshld", options(nostack, preserves_flags));
    }
}

/// Order writes to device-shared memory before a following MMIO store
/// (x86_64).
///
/// x86_64 DMA is I/O-coherent and its memory model is strongly ordered
/// (writes are not reordered with other writes), so a plain `sfence` —
/// which also drains any write-combining buffers a mapping might use —
/// suffices to keep the data stores ahead of the doorbell store.
#[cfg(dma_barrier_x86_64)]
#[inline(always)]
pub fn dma_wmb() {
    // SAFETY: a barrier-only instruction; it constrains store ordering only.
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
}

/// Order reads of device-written memory after observing its ownership flag
/// (x86_64).
///
/// `lfence` serialises load ordering, pairing with [`dma_wmb`]'s `sfence`.
#[cfg(dma_barrier_x86_64)]
#[inline(always)]
pub fn dma_rmb() {
    // SAFETY: a barrier-only instruction; it constrains load ordering only.
    unsafe {
        core::arch::asm!("lfence", options(nostack, preserves_flags));
    }
}

/// Order writes to device-shared memory before a following MMIO store
/// (riscv64).
///
/// RISC-V orders normal memory and device I/O in separate predecessor/
/// successor sets, so a full `fence iorw, iorw` is used: it unambiguously
/// orders every prior memory and device access before everything that
/// follows, which is correct on any RISC-V implementation. (A narrower
/// `fence w, o` would suffice on a coherent part; the full fence is chosen
/// for correctness over a micro-optimisation on a target not yet exercised on
/// this DMA path — `AGENTS.md` §2.16.)
#[cfg(dma_barrier_riscv64)]
#[inline(always)]
pub fn dma_wmb() {
    // SAFETY: a barrier-only instruction; it constrains access ordering only.
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags));
    }
}

/// Order reads of device-written memory after observing its ownership flag
/// (riscv64).
///
/// A full `fence iorw, iorw` is used, for the same reason as [`dma_wmb`]:
/// unambiguously correct on any RISC-V implementation.
#[cfg(dma_barrier_riscv64)]
#[inline(always)]
pub fn dma_rmb() {
    // SAFETY: a barrier-only instruction; it constrains access ordering only.
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags));
    }
}

/// Order writes to device-shared memory before a following MMIO store
/// (host / `wasm32` fallback — no separate DMA master, so a no-op).
#[cfg(not(dma_barrier_native))]
#[inline(always)]
pub fn dma_wmb() {}

/// Order reads of device-written memory after observing its ownership flag
/// (host / `wasm32` fallback — no separate DMA master, so a no-op).
#[cfg(not(dma_barrier_native))]
#[inline(always)]
pub fn dma_rmb() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the host the barriers are a no-op (no separate DMA master); the
    /// test exists so the crate carries its own unit test (`AGENTS.md` §6)
    /// and so the public symbols are exercised. The real `dmb`/`fence`
    /// instructions are only emitted on the native targets and are verified
    /// on metal (`AGENTS.md` §0.4).
    #[test]
    fn host_barriers_are_callable_noops() {
        dma_wmb();
        dma_rmb();
    }
}
