//! Canonical anonymous-memory `mem_map` / `mem_unmap` ABI
//! (`plans/SPAWN.md` SP5).
//!
//! A spawned process boots with exactly its fixed spawn-time image
//! (code/data/bss plus a fixed user stack, `plans/SPAWN.md` SP2/SP3). The
//! `mem_map` / `mem_unmap` pair is the one `abi-v1` mechanism by which a
//! process obtains and releases **additional** memory at runtime — the
//! foundation a `lib/rt` heap allocator layers its `malloc`/`free` over.
//! This module fixes the *contract*; the kernel-side producer that mutates
//! a live user address space lives in `kernel/mem` and is reached through a
//! `kernel/core` seam.
//!
//! # The contract
//!
//! * `mem_map(len, flags, addr_hint)` maps `len` bytes (rounded up to whole
//!   pages) of fresh **anonymous** memory into the **caller's own**
//!   hardware-isolated address space and returns the base address of the
//!   new region, or an [`Errno`].
//! * `mem_unmap(base, len)` releases a region previously returned by
//!   `mem_map`.
//!
//! The binding invariants (`plans/SPAWN.md` SP5, settled in the SP5-0 design
//! note):
//!
//! * **W^X, `RW` only.** A mapping is always readable
//!   and writable and **never** executable. An executable (JIT) mapping is a
//!   separate, later `CAP_JIT_MAP_EXEC`-gated `RW`→`RX` flip; `mem_map` never
//!   produces `RWX`.
//! * **Per-process, never global.** A region is mapped only
//!   into the caller's own address space; there is no global user heap and no
//!   cross-process mapping (shared memory stays the capability-checked IPC
//!   object).
//! * **Unprivileged (precedent).** Growing one's *own*
//!   address space with anonymous `RW` memory requires no capability, exactly
//!   as "list my own processes" does. The kernel still validates every
//!   argument and fails closed.
//! * **Zero on map and on free (secret hygiene).** Pages are
//!   zeroed before the mapping is visible (no stale kernel / other-process
//!   bytes), and the frames `mem_unmap` reclaims are zeroed on free.
//! * **Deterministic OOM.** A frame- or
//!   page-table-allocation failure returns [`Errno::OutOfMemory`], never a
//!   panic. There is no per-process quota; a process is bounded only by the
//!   physical frames available.
//!
//! The numeric flag bits are part of the frozen mapping ABI: new behaviour is
//! added by allocating an unused bit, never by repurposing an existing one.

use crate::Errno;

/// The system page granule, in bytes: the unit `mem_map` rounds a length up
/// to, every Tier-1 target's smallest translation granule, and the quantum
/// the physical frame allocator and both heaps work in.
///
/// It lives here because it is user-visible — a program sizes a mapping
/// against it — so it is one value across the whole system rather than a
/// per-port constant each layer re-states. A port that needed a different
/// granule would be changing the mapping ABI, not a private detail.
pub const PAGE_SIZE: usize = 4096;

/// Bit-shift such that `1 << PAGE_SHIFT == PAGE_SIZE`.
pub const PAGE_SHIFT: u32 = PAGE_SIZE.trailing_zeros();

const _: () = assert!(PAGE_SIZE.is_power_of_two());

/// Flags accepted by [`mem_map`](crate::SyscallNumber::MEM_MAP).
///
/// A `#[repr(transparent)]` newtype over the `u32` flags register so the wire
/// representation is exactly the integer the syscall trampoline passes. Only
/// the bits named here are defined; every other bit is reserved and must be
/// zero. [`MapFlags::from_bits`] rejects a value with any reserved bit set, so
/// a future flag cannot be silently ignored by an older kernel
/// (validate every input, fail closed).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct MapFlags(u32);

impl MapFlags {
    /// Treat `addr_hint` as a mandatory placement rather than advice.
    ///
    /// When set, the kernel maps the region at exactly `addr_hint` (which must
    /// be page-aligned and name a free range) or fails closed; it never picks
    /// a different address. When clear, `addr_hint` is advisory — the kernel
    /// places the region where it sees fit, and a `0` hint means "kernel
    /// chooses" outright.
    pub const FIXED: Self = Self(1 << 0);

    /// The set of all defined flag bits.
    ///
    /// Any bit outside this mask is reserved and rejected by
    /// [`MapFlags::from_bits`].
    const DEFINED_BITS: u32 = Self::FIXED.0;

    /// An empty flag set (advisory `addr_hint`, no options).
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw flag bits, as carried on the ABI.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build a flag set from raw bits, rejecting any reserved bit.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if `bits` sets any reserved
    /// (currently-undefined) bit.
    pub const fn from_bits(bits: u32) -> Result<Self, Errno> {
        if bits & !Self::DEFINED_BITS != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(bits))
    }

    /// Whether every bit set in `other` is also set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the caller demanded the `addr_hint` be honoured exactly.
    #[must_use]
    pub const fn is_fixed(self) -> bool {
        self.contains(Self::FIXED)
    }
}

#[cfg(test)]
mod tests {
    use super::MapFlags;
    use crate::Errno;

    #[test]
    fn empty_is_advisory() {
        let f = MapFlags::empty();
        assert_eq!(f.bits(), 0);
        assert!(!f.is_fixed());
    }

    #[test]
    fn fixed_round_trips() {
        let f = MapFlags::FIXED;
        assert!(f.is_fixed());
        let again = MapFlags::from_bits(f.bits()).expect("defined bit");
        assert_eq!(again, f);
    }

    #[test]
    fn reserved_bits_are_rejected() {
        // Bit 1 is reserved today.
        assert_eq!(MapFlags::from_bits(1 << 1), Err(Errno::OutOfRange));
        assert_eq!(MapFlags::from_bits(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn contains_is_subset_relation() {
        assert!(MapFlags::FIXED.contains(MapFlags::empty()));
        assert!(!MapFlags::empty().contains(MapFlags::FIXED));
    }
}
