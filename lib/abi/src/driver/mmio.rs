//! Capability-checked memory-mapped register windows (`abi-v1`).
//!
//! A [`RegisterWindow`] is the host↔driver ABI seam through which a
//! bus driver (`lib/pci`, `drivers/bus/mmio`) reaches a
//! device's register block. It lives in `lib/abi` for the same
//! reason [`DmaSlab`](super::DmaSlab) does: the [`DriverHost`] trait
//! and the bus-class drivers all have to be able to name it without
//! pulling in `drivers/bus/*`, which would invert the dependency
//! direction and violate `AGENTS.md` §3.
//!
//! # The minting rule (`AGENTS.md` §4 — no ambient authority)
//!
//! The only way to obtain a [`RegisterWindow`] in safe code is to ask
//! a [`MmioMapper`] (the kernel-side MMIO-map facility) for one. The
//! sole constructor, [`RegisterWindow::from_mapping`], is `unsafe`:
//! the *kernel* calls it after it has (1) verified the caller holds
//! [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP),
//! (2) validated the requested physical region, and (3) installed a
//! caching-disabled mapping for it. A bus driver therefore cannot
//! synthesise a pointer to arbitrary physical memory — it can only
//! ever hold a window the kernel chose to hand it.
//!
//! # Bounds and alignment
//!
//! Every accessor validates its `offset` against the window length
//! and the natural alignment of the access width *before* it touches
//! memory, returning [`WindowError`] on a violation rather than
//! reading or writing out of bounds (`AGENTS.md` §5.4.3 — validate
//! every input; §2.9 — no panics on the production path).
//!
//! [`DriverHost`]: super::DriverHost

use core::ptr::NonNull;

use super::DriverError;

/// Failure modes of a [`RegisterWindow`] accessor.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum WindowError {
    /// The access (`offset .. offset + width`) would fall outside the
    /// window.
    OutOfBounds,
    /// The `offset` is not a multiple of the access width.
    Misaligned,
}

impl WindowError {
    /// Map to the closest [`DriverError`] for callers that surface
    /// register access failures across the driver ABI.
    ///
    /// Both variants name a malformed access at the call site, so
    /// both collapse to [`DriverError::OutOfRange`].
    #[must_use]
    pub const fn as_driver_error(self) -> DriverError {
        DriverError::OutOfRange
    }
}

/// A capability-checked, kernel-mapped memory-mapped register window.
///
/// # Invariants (established by [`RegisterWindow::from_mapping`])
///
/// * `base` is a non-null pointer to a caching-disabled mapping of
///   exactly `len` bytes that remains valid for the lifetime of the
///   window.
/// * `phys` is the device-visible physical base of `base[0]`, kept
///   so a driver can program the address back into a sibling
///   register (for example a virtio queue's descriptor-table
///   address) without a second round-trip to the mapper.
/// * The window has unique access to its byte range: the kernel maps
///   one device's register block into exactly one window.
#[derive(Debug)]
pub struct RegisterWindow {
    base: NonNull<u8>,
    len: usize,
    phys: u64,
}

// SAFETY: a `RegisterWindow` is a tagged pointer to a device register
// block that the kernel mapped into the owning process's address
// space. `NonNull<u8>` is `!Send` by default because the compiler
// cannot know whether the pointee permits cross-thread access. We
// assert `Send` because the kernel maps the block into the process
// (not a single thread) and hands the window to the driver task that
// owns the device. We deliberately do *not* assert `Sync`: two
// threads issuing concurrent volatile writes to the same register
// would race, and the device-driver model gives each window a single
// owning task.
unsafe impl Send for RegisterWindow {}

impl RegisterWindow {
    /// Construct a window over a kernel-installed mapping.
    ///
    /// # Safety
    ///
    /// The caller (the kernel's MMIO-map facility, after a
    /// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    /// check) must guarantee:
    ///
    /// * `base` points at a readable + writable, caching-disabled
    ///   mapping of exactly `len` bytes that stays valid for the
    ///   whole lifetime of the returned window;
    /// * `base` is aligned to at least 4 bytes (a page-aligned MMIO
    ///   mapping satisfies this), so a naturally-aligned `offset`
    ///   yields a naturally-aligned access;
    /// * no other live reference aliases that byte range;
    /// * `phys` is the device-visible physical base of `base[0]`.
    #[must_use]
    pub unsafe fn from_mapping(phys: u64, base: NonNull<u8>, len: usize) -> Self {
        Self { base, len, phys }
    }

    /// Length of the window in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` iff the window is zero bytes long.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Device-visible physical base address of the window.
    #[must_use]
    pub fn phys_base(&self) -> u64 {
        self.phys
    }

    /// Validate `offset` for an access of `width` bytes, returning the
    /// in-bounds, correctly-aligned byte pointer.
    fn checked_ptr(&self, offset: usize, width: usize) -> Result<*mut u8, WindowError> {
        if offset % width != 0 {
            return Err(WindowError::Misaligned);
        }
        let end = offset.checked_add(width).ok_or(WindowError::OutOfBounds)?;
        if end > self.len {
            return Err(WindowError::OutOfBounds);
        }
        // SAFETY: `offset < len` (since `end <= len` and `width >= 1`),
        // so the pointer stays within the single mapped allocation the
        // construction invariant guarantees.
        Ok(unsafe { self.base.as_ptr().add(offset) })
    }

    /// Volatile-read a `u8` at `offset`.
    ///
    /// # Errors
    ///
    /// [`WindowError::OutOfBounds`] if `offset + 1 > len`.
    pub fn read_u8(&self, offset: usize) -> Result<u8, WindowError> {
        let ptr = self.checked_ptr(offset, 1)?;
        // SAFETY: `checked_ptr` proved `ptr` is in bounds and the
        // construction invariant guarantees the mapping is readable.
        Ok(unsafe { ptr.read_volatile() })
    }

    /// Volatile-read a little-endian `u16` at `offset`.
    ///
    /// # Errors
    ///
    /// [`WindowError::Misaligned`] if `offset` is odd, or
    /// [`WindowError::OutOfBounds`] if the access overruns the window.
    #[allow(clippy::cast_ptr_alignment)] // base is ≥ 4-byte aligned per
                                         // `from_mapping`'s contract and `offset` is 2-byte aligned per
                                         // `checked_ptr`, so the resulting pointer is naturally aligned.
    pub fn read_u16(&self, offset: usize) -> Result<u16, WindowError> {
        let ptr = self.checked_ptr(offset, 2)?;
        // SAFETY: in-bounds and 2-byte aligned per `checked_ptr`.
        Ok(unsafe { ptr.cast::<u16>().read_volatile() })
    }

    /// Volatile-read a little-endian `u32` at `offset`.
    ///
    /// # Errors
    ///
    /// [`WindowError::Misaligned`] if `offset` is not a multiple of
    /// four, or [`WindowError::OutOfBounds`] if the access overruns
    /// the window.
    #[allow(clippy::cast_ptr_alignment)] // base is ≥ 4-byte aligned per
                                         // `from_mapping`'s contract and `offset` is 4-byte aligned per
                                         // `checked_ptr`, so the resulting pointer is naturally aligned.
    pub fn read_u32(&self, offset: usize) -> Result<u32, WindowError> {
        let ptr = self.checked_ptr(offset, 4)?;
        // SAFETY: in-bounds and 4-byte aligned per `checked_ptr`.
        Ok(unsafe { ptr.cast::<u32>().read_volatile() })
    }

    /// Volatile-write a `u8` at `offset`.
    ///
    /// # Errors
    ///
    /// [`WindowError::OutOfBounds`] if `offset + 1 > len`.
    pub fn write_u8(&self, offset: usize, value: u8) -> Result<(), WindowError> {
        let ptr = self.checked_ptr(offset, 1)?;
        // SAFETY: in-bounds per `checked_ptr`; the construction
        // invariant guarantees the mapping is writable and that this
        // window is the unique owner of the byte range.
        unsafe { ptr.write_volatile(value) };
        Ok(())
    }

    /// Volatile-write a little-endian `u16` at `offset`.
    ///
    /// # Errors
    ///
    /// [`WindowError::Misaligned`] if `offset` is odd, or
    /// [`WindowError::OutOfBounds`] if the access overruns the window.
    #[allow(clippy::cast_ptr_alignment)] // base is ≥ 4-byte aligned per
                                         // `from_mapping`'s contract and `offset` is 2-byte aligned per
                                         // `checked_ptr`, so the resulting pointer is naturally aligned.
    pub fn write_u16(&self, offset: usize, value: u16) -> Result<(), WindowError> {
        let ptr = self.checked_ptr(offset, 2)?;
        // SAFETY: in-bounds and 2-byte aligned per `checked_ptr`.
        unsafe { ptr.cast::<u16>().write_volatile(value) };
        Ok(())
    }

    /// Volatile-write a little-endian `u32` at `offset`.
    ///
    /// # Errors
    ///
    /// [`WindowError::Misaligned`] if `offset` is not a multiple of
    /// four, or [`WindowError::OutOfBounds`] if the access overruns
    /// the window.
    #[allow(clippy::cast_ptr_alignment)] // base is ≥ 4-byte aligned per
                                         // `from_mapping`'s contract and `offset` is 4-byte aligned per
                                         // `checked_ptr`, so the resulting pointer is naturally aligned.
    pub fn write_u32(&self, offset: usize, value: u32) -> Result<(), WindowError> {
        let ptr = self.checked_ptr(offset, 4)?;
        // SAFETY: in-bounds and 4-byte aligned per `checked_ptr`.
        unsafe { ptr.cast::<u32>().write_volatile(value) };
        Ok(())
    }
}

/// Failure modes of [`MmioMapper::map_window`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum MmioMapError {
    /// The calling task does not hold
    /// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP).
    CapabilityMissing,
    /// The requested region is malformed (zero length, a length or
    /// base that cannot be mapped, or a base that is not page
    /// aligned).
    InvalidRegion,
    /// The platform cannot map the requested region (no free virtual
    /// window, or the underlying mapper reported an unrecoverable
    /// fault).
    Unsupported,
}

impl MmioMapError {
    /// Map to the closest [`DriverError`] for callers that surface
    /// mapping failures across the driver ABI.
    ///
    /// * [`Self::CapabilityMissing`] → [`DriverError::PermissionDenied`].
    /// * [`Self::InvalidRegion`] → [`DriverError::LengthOutOfRange`].
    /// * [`Self::Unsupported`] → [`DriverError::Unsupported`].
    #[must_use]
    pub const fn as_driver_error(self) -> DriverError {
        match self {
            Self::CapabilityMissing => DriverError::PermissionDenied,
            Self::InvalidRegion => DriverError::LengthOutOfRange,
            Self::Unsupported => DriverError::Unsupported,
        }
    }
}

/// Kernel-side MMIO-map facility seam.
///
/// A bus driver that has discovered a device's register block (a PCI
/// memory BAR, a virtio-MMIO transport slot) asks the host for a
/// [`MmioMapper`] and calls [`map_window`](MmioMapper::map_window) to
/// obtain a [`RegisterWindow`]. The capability check
/// ([`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)) and the
/// actual page-table mapping live inside the concrete implementation
/// (the kernel binary's mapper), keeping the bus drivers free of any
/// `kernel/*` dependency and ensuring the driver never synthesises a
/// pointer itself (`AGENTS.md` §4).
pub trait MmioMapper {
    /// Map `len` bytes of device physical memory beginning at
    /// `phys_base` and return a [`RegisterWindow`] over the mapping.
    ///
    /// # Errors
    ///
    /// * [`MmioMapError::CapabilityMissing`] — the caller does not
    ///   hold [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP).
    /// * [`MmioMapError::InvalidRegion`] — `len` is zero or the
    ///   region is otherwise unmappable.
    /// * [`MmioMapError::Unsupported`] — the platform cannot satisfy
    ///   the request.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP);
    /// the implementation enforces it.
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8-byte-aligned byte buffer so the window's base satisfies
    /// `from_mapping`'s ≥ 4-byte alignment contract (a bare `[u8; N]`
    /// on the stack is only 1-byte aligned).
    #[repr(align(8))]
    struct Aligned<const N: usize>([u8; N]);

    /// Build a window over a fixed-size, aligned stack buffer. The
    /// buffer is borrowed mutably for the duration of the test so the
    /// window's invariants (unique access, valid for the window's
    /// lifetime) hold without any allocation — `lib/abi` is no-alloc.
    fn window_over(buf: &mut [u8], phys: u64) -> RegisterWindow {
        let len = buf.len();
        let base = NonNull::new(buf.as_mut_ptr()).expect("stack buffer is non-null");
        // SAFETY: `base` covers exactly `len` bytes of the borrowed
        // buffer, which outlives the window inside each test, and the
        // mutable borrow guarantees no other live reference aliases
        // it. `phys` is a synthetic device address for the test.
        unsafe { RegisterWindow::from_mapping(phys, base, len) }
    }

    #[test]
    fn metadata_round_trips() {
        let mut buf = Aligned([0u8; 16]);
        let w = window_over(&mut buf.0, 0xFEE0_0000);
        assert_eq!(w.len(), 16);
        assert!(!w.is_empty());
        assert_eq!(w.phys_base(), 0xFEE0_0000);
    }

    #[test]
    fn u32_write_then_read_round_trips() {
        let mut buf = Aligned([0u8; 16]);
        let w = window_over(&mut buf.0, 0);
        w.write_u32(4, 0xDEAD_BEEF).expect("in bounds");
        assert_eq!(w.read_u32(4).expect("in bounds"), 0xDEAD_BEEF);
        // Little-endian byte order is observable through the u8 view.
        assert_eq!(w.read_u8(4).expect("in bounds"), 0xEF);
        assert_eq!(w.read_u8(7).expect("in bounds"), 0xDE);
    }

    #[test]
    fn u16_and_u8_round_trip() {
        let mut buf = Aligned([0u8; 8]);
        let w = window_over(&mut buf.0, 0);
        w.write_u16(2, 0xBEEF).expect("in bounds");
        assert_eq!(w.read_u16(2).expect("in bounds"), 0xBEEF);
        w.write_u8(0, 0x5A).expect("in bounds");
        assert_eq!(w.read_u8(0).expect("in bounds"), 0x5A);
    }

    #[test]
    fn read_rejects_out_of_bounds() {
        let mut buf = Aligned([0u8; 8]);
        let w = window_over(&mut buf.0, 0);
        assert_eq!(w.read_u32(8), Err(WindowError::OutOfBounds));
        assert_eq!(w.read_u32(5), Err(WindowError::Misaligned));
        // The last valid u32 access starts at offset 4.
        assert!(w.read_u32(4).is_ok());
    }

    #[test]
    fn write_rejects_out_of_bounds() {
        let mut buf = Aligned([0u8; 8]);
        let w = window_over(&mut buf.0, 0);
        assert_eq!(w.write_u32(8, 0), Err(WindowError::OutOfBounds));
        assert_eq!(w.write_u16(8, 0), Err(WindowError::OutOfBounds));
        assert_eq!(w.write_u16(1, 0), Err(WindowError::Misaligned));
    }

    #[test]
    fn offset_addition_overflow_is_out_of_bounds() {
        let mut buf = Aligned([0u8; 8]);
        let w = window_over(&mut buf.0, 0);
        // `usize::MAX` is a multiple of 1, so it passes the alignment
        // gate and must be caught by the checked addition.
        assert_eq!(w.read_u8(usize::MAX), Err(WindowError::OutOfBounds));
    }

    #[test]
    fn window_error_maps_to_driver_error() {
        assert_eq!(
            WindowError::OutOfBounds.as_driver_error(),
            DriverError::OutOfRange
        );
        assert_eq!(
            WindowError::Misaligned.as_driver_error(),
            DriverError::OutOfRange
        );
    }

    #[test]
    fn mmio_map_error_maps_to_driver_error() {
        assert_eq!(
            MmioMapError::CapabilityMissing.as_driver_error(),
            DriverError::PermissionDenied
        );
        assert_eq!(
            MmioMapError::InvalidRegion.as_driver_error(),
            DriverError::LengthOutOfRange
        );
        assert_eq!(
            MmioMapError::Unsupported.as_driver_error(),
            DriverError::Unsupported
        );
    }
}
