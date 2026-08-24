//! MMIO read seam.
//!
//! Splits the volatile-load behind a trait so the enumeration core
//! is testable against an in-memory fake while the production
//! wiring uses [`VolatileMmioRead`], which encapsulates the one
//! `unsafe` block in the crate.

/// Volatile reader for a 32-bit MMIO register.
///
/// Implementations promise that reads observe device-side updates
/// (no compiler reordering, no folding away of repeated reads).
/// The trait is *not* `unsafe` to implement — the in-tree
/// implementor [`VolatileMmioRead`] is the only carrier of an
/// `unsafe` block, and it gates the raw pointer dereference behind
/// the constructor.
pub trait MmioRead {
    /// Read a 32-bit dword at `physical_address`.
    ///
    /// Implementations must use a volatile load semantically
    /// equivalent to `core::ptr::read_volatile`.
    fn read32(&self, physical_address: u64) -> u32;
}

/// Real-hardware volatile reader.
///
/// The reader owns the kernel-side base address grant that mapped
/// the MMIO region — it does *not* perform the mapping itself; that
/// remains the responsibility of the driver host's memory
/// capability per the Stage 4 sub-bullet on bus drivers in
/// `PLAN.md`. The constructor accepts a `*const u32` produced by
/// such a mapping plus the byte-length of the window; reads are
/// bounds-checked against that span before any pointer arithmetic.
pub struct VolatileMmioRead {
    base: *const u32,
    /// Physical address the window covers (the value the kernel
    /// memory capability registered the mapping for).
    base_phys: u64,
    /// Byte length of the mapped window.
    len: u64,
}

// SAFETY: The pointer in `VolatileMmioRead` is treated as opaque
// metadata for bounds-checked volatile reads; it is never used to
// create a shared mutable Rust reference. Sending or sharing the
// struct across threads is therefore as safe as sending or sharing
// the underlying physical mapping, which is the host's
// responsibility to gate via the memory capability.
unsafe impl Send for VolatileMmioRead {}
unsafe impl Sync for VolatileMmioRead {}

impl VolatileMmioRead {
    /// Construct a [`VolatileMmioRead`] over a mapped window.
    ///
    /// # Safety
    ///
    /// * `base` must be a valid, non-null pointer to a 4-byte
    ///   aligned MMIO region covering at least `len` bytes
    ///   starting at physical address `base_phys`.
    /// * The region must remain mapped for the lifetime of the
    ///   returned value.
    /// * No other reference (mutable or shared) into the same
    ///   region may exist while `self` is alive.
    pub unsafe fn new(base: *const u32, base_phys: u64, len: u64) -> Self {
        Self {
            base,
            base_phys,
            len,
        }
    }
}

impl MmioRead for VolatileMmioRead {
    fn read32(&self, physical_address: u64) -> u32 {
        // Out-of-range / misaligned addresses surface as the
        // hardware "no device" sentinel; the enumeration core
        // treats this as "slot empty".
        if physical_address < self.base_phys {
            return 0;
        }
        let offset = physical_address - self.base_phys;
        if !offset.is_multiple_of(4) || offset.checked_add(4).is_none_or(|e| e > self.len) {
            return 0;
        }
        let words = offset / 4;
        // `usize::try_from` is total on the platforms the driver
        // targets (`aarch64` / `riscv64` / x86_64-host tests) where
        // `usize` is at least 32-bit; the fallback returns 0 so a
        // mis-sized window cannot crash the driver.
        let Ok(words_usize) = usize::try_from(words) else {
            return 0;
        };
        // SAFETY: `offset` is `< self.len`, dword-aligned, and the
        // constructor's safety contract guarantees the underlying
        // region remains valid for at least `self.len` bytes from
        // `self.base`. `add` therefore stays inside the allocated
        // (mapped) object, and `read_volatile` against MMIO is
        // sound by the trait contract. No Rust shared/mut
        // reference is created.
        unsafe {
            let p = self.base.add(words_usize);
            core::ptr::read_volatile(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;

    #[test]
    fn volatile_reader_round_trips_over_host_buffer() {
        // An aligned host-side buffer stands in for the MMIO window, so the
        // `unsafe` pointer arithmetic is exercised without real hardware.
        let backing: alloc::vec::Vec<u32> = alloc::vec![0x7472_6976, 2, 1, 0x554D_4551];
        let base_phys = 0x1000_u64;
        // SAFETY: `backing.as_ptr()` is non-null, 4-byte aligned
        // (allocator guarantee for `Vec<u32>`), and the slice
        // remains live for the duration of the call.
        let reader = unsafe {
            VolatileMmioRead::new(backing.as_ptr(), base_phys, (backing.len() * 4) as u64)
        };
        assert_eq!(reader.read32(base_phys), 0x7472_6976);
        assert_eq!(reader.read32(base_phys + 4), 2);
        assert_eq!(reader.read32(base_phys + 8), 1);
        assert_eq!(reader.read32(base_phys + 12), 0x554D_4551);
        // Out-of-range / misaligned / below-base reads surface as 0.
        assert_eq!(reader.read32(base_phys - 4), 0);
        assert_eq!(reader.read32(base_phys + 5), 0);
        assert_eq!(reader.read32(base_phys + 16), 0);
    }
}
