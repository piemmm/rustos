//! The kernel-side demand-paged file-mapping seam the `file_map` (`abi-v1`
//! number 75) and `file_unmap` (number 76) syscalls and the user-fault
//! resolver use.
//!
//! [`FileMap`] is the file-mapping sibling of [`crate::memmap::MemMap`]:
//! the one object-safe boundary between the arch-neutral handler/resolver
//! in `kernel/core` and the architecture-specific producer in `kernel/mem`
//! that mutates a *live* user address space. The three operations mirror a
//! demand-paged region's life: reserve pure address space at map time, make
//! one faulting page resident with the file bytes the caller supplies, and
//! sparsely release the region — the handler owns every policy decision
//! (descriptor resolution, identity snapshot, rlimit charge, region
//! bookkeeping), the producer only the page-table mechanism.
//!
//! Until a producer is installed the handler holds [`NULL_FILE_MAP`], which
//! fails closed with [`Errno::NotImplemented`] — an uninstalled interface
//! announces itself rather than pretending a mapping succeeded, exactly as
//! [`NULL_MEM_MAP`](crate::memmap::NULL_MEM_MAP) does.

use tairix_abi::Errno;

/// The kernel-side producer of demand-paged, read-only file-backed user
/// memory.
///
/// Implemented by the architecture-port-installed producer over the
/// **caller's own** hardware-isolated live address space (the same
/// current-CPU exclusivity as [`crate::memmap::MemMap`]; the fault
/// resolver runs on the CPU executing the faulting task, so the same
/// access rule holds there). Implementations must be [`Sync`]: the single
/// installed producer is shared by the per-CPU syscall handlers.
pub trait FileMap: Sync {
    /// Reserve `len` bytes (rounded up to whole pages) of address space
    /// for a file mapping at a kernel-chosen base, returning that base.
    /// No page is backed: the region costs nothing until a fault lands in
    /// it and [`FileMap::map_page`] makes that page resident.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when the caller's file-mapping window cannot
    /// hold the reservation (deterministic, fail-closed refusal);
    /// [`Errno::NotImplemented`] from the default producer
    /// ([`NullFileMap`]).
    fn reserve(&self, len: u64) -> Result<u64, Errno>;

    /// Make the single page at the page-aligned `va` — inside a region
    /// previously returned by [`FileMap::reserve`] — resident, carrying
    /// `contents` (at most one page; a short slice is the page straddling
    /// end-of-file, zero-filled past it). The page is mapped read-only and
    /// never executable.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when `va` lies outside every live reserved file
    /// region (fail closed — the fault path never backs address space the
    /// task did not map); [`Errno::OutOfMemory`] on frame exhaustion;
    /// [`Errno::BadAddress`] when the page is already resident (a benign
    /// fault race — the caller simply resumes the access).
    fn map_page(&self, va: u64, contents: &[u8]) -> Result<(), Errno>;

    /// Release the whole file region of `len` bytes based at `base`
    /// previously returned by [`FileMap::reserve`], sparsely unmapping the
    /// pages fault history made resident (zeroing each frame on free) and
    /// returning how many pages were resident.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when `(base, len)` does not name a live file
    /// region of the caller's (fail closed: nothing is torn down);
    /// [`Errno::NotImplemented`] from the default producer.
    fn release(&self, base: u64, len: u64) -> Result<u64, Errno>;
}

/// The file-mapping producer installed before any real one exists.
///
/// Every operation fails closed with [`Errno::NotImplemented`], so a
/// `file_map`/`file_unmap` (or a fault) issued before the boot path
/// installs the `kernel/mem` producer announces an inert interface rather
/// than pretending a region was reserved, backed, or freed.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullFileMap;

impl FileMap for NullFileMap {
    fn reserve(&self, _len: u64) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }

    fn map_page(&self, _va: u64, _contents: &[u8]) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn release(&self, _base: u64, _len: u64) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullFileMap`] instance the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `file_map` borrow here so the
/// field is always valid without an `Option` branch on the hot path; the
/// boot path replaces it with the real producer through
/// `KernelSyscallHandlers::with_file_map`.
pub static NULL_FILE_MAP: NullFileMap = NullFileMap;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_file_map_fails_closed_on_every_operation() {
        assert_eq!(NULL_FILE_MAP.reserve(0x1000), Err(Errno::NotImplemented));
        assert_eq!(
            NULL_FILE_MAP.map_page(0x10_0000, &[1]),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            NULL_FILE_MAP.release(0x10_0000, 0x1000),
            Err(Errno::NotImplemented)
        );
    }
}
