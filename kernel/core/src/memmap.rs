//! The kernel-side anonymous-memory seam the `mem_map` (`abi-v1` number
//! 14) and `mem_unmap` (number 15) syscalls use (`plans/SPAWN.md` SP5).
//!
//! [`MemMap`] is the one object-safe boundary between the arch-neutral
//! syscall handler in `kernel/core` and the architecture-specific producer
//! in `kernel/mem` that mutates a *live* user address space — mapping fresh
//! zeroed frames into it and unmapping them with the necessary TLB
//! shootdown. Naming a port's concrete page table and direct physical map is
//! irreducibly architecture-specific, so — like
//! the [`ProcessSpawn`](crate::spawn::ProcessSpawn) producer — the concrete
//! implementation is installed at boot through a `with_*` builder and the
//! handler reaches it through this trait.
//!
//! Until a producer is installed the handler holds [`NULL_MEM_MAP`], which
//! fails closed with [`Errno::NotImplemented`]. A build
//! whose `kernel/mem` live-mapping producer is not yet wired (the state
//! before `plans/SPAWN.md` `SP5b` lands a real producer) therefore announces
//! an intentionally inert interface rather than pretending a mapping
//! succeeded — exactly as [`NULL_CONSOLE`](crate::console::NULL_CONSOLE) and
//! [`NULL_PROCESS_SPAWN`](crate::spawn::NULL_PROCESS_SPAWN) do for their
//! syscalls.

use rustos_abi::{Errno, MapFlags};

/// The kernel-side producer of anonymous user memory.
///
/// Implemented by the architecture-port-installed producer that maps fresh
/// zeroed frames into, and unmaps them from, the **caller's own**
/// hardware-isolated address space (no global user heap, no
/// cross-process mapping). The trait is deliberately minimal — the two
/// already-validated user-facing operations — so `kernel/core` stays free of
/// any page-table knowledge and the syscall handler owns
/// the capability and argument validation, never the producer.
///
/// Implementations must be [`Sync`]: the single installed producer is shared
/// by the per-CPU syscall handlers, exactly like the console device and the
/// spawn producer.
pub trait MemMap: Sync {
    /// Map `len` bytes (rounded up to whole pages) of fresh anonymous `RW`
    /// memory into the caller's own address space, returning the base
    /// address of the new region.
    ///
    /// The dispatcher and handler have already validated that `len` is
    /// non-zero, that `flags` carries no reserved bit, and the capability
    /// posture (`mem_map` is unprivileged). The
    /// implementation zeroes the pages before the mapping is visible and
    /// never makes the region executable (W^X). When
    /// `flags` contains [`MapFlags::FIXED`] it maps at exactly `addr_hint`
    /// or fails closed; otherwise `addr_hint` is advisory and `0` means
    /// "producer chooses".
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfMemory`] when no backing frame (or page-table
    /// frame) is available — deterministic OOM, never a panic. The default producer ([`NullMemMap`])
    /// returns [`Errno::NotImplemented`] to mark an inert interface.
    fn map(&self, len: usize, flags: MapFlags, addr_hint: u64) -> Result<u64, Errno>;

    /// Release the region of `len` bytes based at `base` previously returned
    /// by [`MemMap::map`] from the caller's own address space.
    ///
    /// The implementation zeroes the frames it reclaims (secret hygiene) and fails closed when `(base, len)` does not name a
    /// region the caller mapped.
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] when the range cannot be unmapped. The
    /// default producer ([`NullMemMap`]) returns [`Errno::NotImplemented`].
    fn unmap(&self, base: u64, len: usize) -> Result<(), Errno>;
}

/// The anonymous-memory producer installed before any real one exists.
///
/// Every operation fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default require, so a `mem_map` or
/// `mem_unmap` issued before the boot path installs the `kernel/mem`
/// producer (the state before `plans/SPAWN.md` `SP5b`) announces an inert
/// interface rather than pretending a region was mapped or freed.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullMemMap;

impl MemMap for NullMemMap {
    fn map(&self, _len: usize, _flags: MapFlags, _addr_hint: u64) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }

    fn unmap(&self, _base: u64, _len: usize) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullMemMap`] instance the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `mem_map` borrow here so the
/// field is always valid without an `Option` branch on the hot path; the
/// boot path replaces it with the real producer through
/// `KernelSyscallHandlers::with_mem_map` once `plans/SPAWN.md` `SP5b` lands.
pub static NULL_MEM_MAP: NullMemMap = NullMemMap;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_mem_map_map_fails_closed() {
        assert_eq!(
            NULL_MEM_MAP.map(0x1000, MapFlags::empty(), 0),
            Err(Errno::NotImplemented)
        );
        // Even a FIXED request with a hint announces the inert interface
        // rather than pretending a placement succeeded.
        assert_eq!(
            NullMemMap.map(0x1000, MapFlags::FIXED, 0x10_0000),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn null_mem_map_unmap_fails_closed() {
        assert_eq!(
            NULL_MEM_MAP.unmap(0x10_0000, 0x1000),
            Err(Errno::NotImplemented)
        );
    }
}
