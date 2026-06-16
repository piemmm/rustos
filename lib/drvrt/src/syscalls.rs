//! The two `abi-v1` syscalls [`RtDriverHost`](crate::RtDriverHost) issues,
//! behind a host-testable seam.
//!
//! [`GrantSyscalls`] abstracts the `mmio_map` and `dma_alloc` syscall
//! wrappers so the host's grant-resolution and address-translation logic is
//! unit-tested without a kernel (`AGENTS.md` §7) — exactly as the bus drivers
//! mock their register windows. Production driver processes use
//! [`RtGrantSyscalls`], the zero-sized forwarder to `rustos_rt`, so the one
//! syscall trap is not duplicated (`AGENTS.md` §2.2).

/// The `mmio_map` / `dma_alloc` syscalls the user-space driver host issues.
///
/// Both methods return the kernel's raw signed `abi-v1` result register: a
/// non-negative value is the base **user virtual address** of the mapped
/// window or carved buffer, and a negative value is `-errno` (recover the
/// [`rustos_abi::Errno`] discriminant as `-ret`). The host
/// adds no authority; the kernel validates the grant handle and every bound on
/// the far side of the trap (`AGENTS.md` §5.4).
pub trait GrantSyscalls {
    /// Map the device MMIO window named by the kernel-issued grant `handle`
    /// into the calling task's own address space, returning the base user
    /// virtual address of the mapping (or `-errno`).
    ///
    /// Mirrors [`rustos_rt::mmio_map`].
    fn mmio_map(&self, handle: u64) -> i64;

    /// Carve a coherent DMA buffer of `len` bytes bounded by the constraint
    /// named by the kernel-issued grant `handle`, write the buffer's
    /// device-visible base to `device_out`, and return its base user virtual
    /// address (or `-errno`; `device_out` is left untouched on a negative
    /// result).
    ///
    /// Mirrors [`rustos_rt::dma_alloc`].
    fn dma_alloc(&self, handle: u64, len: usize, device_out: &mut u64) -> i64;
}

/// The production [`GrantSyscalls`]: forward to `rustos_rt`'s wrappers.
///
/// Zero-sized; a driver process constructs one and hands it to
/// [`RtDriverHost::new`](crate::RtDriverHost::new). It carries no state and no
/// authority — it only routes through the one shared syscall trap
/// (`AGENTS.md` §2.2).
#[derive(Debug, Default, Copy, Clone)]
pub struct RtGrantSyscalls;

impl GrantSyscalls for RtGrantSyscalls {
    #[inline]
    fn mmio_map(&self, handle: u64) -> i64 {
        rustos_rt::mmio_map(handle)
    }

    #[inline]
    fn dma_alloc(&self, handle: u64, len: usize, device_out: &mut u64) -> i64 {
        rustos_rt::dma_alloc(handle, len, device_out)
    }
}
