//! aarch64 hardware page-zero candidate, over `DC ZVA`.
//!
//! Compiled only when the target architecture is aarch64 (the build-script
//! `pagezero_aarch64` cfg, so no target-architecture predicate appears in this
//! source), on the `lib/crc32c` precedent. `DC ZVA` (Data Cache Zero by VA)
//! zeroes one whole cache block — `4 << DCZID_EL0.BS` bytes, aligned down to
//! that size — in a single instruction, without the read-for-ownership a
//! scalar or SIMD store loop pays. It is the fastest way to clear a page on
//! ARMv8 and is a mandatory base-ISA instruction, gated only by
//! `DCZID_EL0.DZP` (whether it is permitted at the current EL), which is
//! exactly the [`CpuFeature::DcZva`](tairix_abi::cpufeatures::CpuFeature::DcZva)
//! bit the `lib/cpuops` selector gates this candidate on.
//!
//! Because `DC ZVA` operates on a whole block, an arbitrary `[ptr, ptr + len)`
//! is cleared in three parts: byte stores for the unaligned **head** up to the
//! first block boundary, `DC ZVA` for each full **block** in the middle, and
//! byte stores for the **tail** remainder. On the kernel's page frames (page
//! aligned, page sized, a multiple of the block) the head and tail are empty
//! and the whole region is cleared by `DC ZVA` alone; the head/tail paths keep
//! the routine correct for any region, which the self-verify exercises.
//!
//! It is only reached after `lib/cpuops` confirms the bit and self-verifies the
//! output against the portable reference over a fixed vector of lengths and
//! alignments. `DCZID_EL0` is readable at both EL1 and EL0, so the developer-
//! host test build on an aarch64 machine exercises the real instruction.

/// The `DC ZVA` block size in bytes, read from `DCZID_EL0.BS`
/// (`4 << BS`, always a power of two ≥ 4).
///
/// Read per call: a single side-effect-free `mrs` is negligible next to
/// clearing a page, and keeping it local avoids any shared mutable state.
fn block_bytes() -> usize {
    let dczid: u64;
    // SAFETY: `DCZID_EL0` is readable at EL1/EL0 with no architectural side
    // effect; it reports the `DC ZVA` block size in its low 4 bits.
    unsafe {
        core::arch::asm!(
            "mrs {d}, dczid_el0",
            d = out(reg) dczid,
            options(nomem, nostack, preserves_flags),
        );
    }
    4usize << (dczid & 0xF)
}

/// Zero every byte of `buf` using `DC ZVA` for the aligned interior and byte
/// stores for the unaligned head and tail.
///
/// A safe wrapper: the region comes from a live `&mut [u8]`, so the raw stores
/// are sound; the head/tail handling makes it correct for any base alignment
/// and length.
pub fn zero_dc_zva(buf: &mut [u8]) {
    let len = buf.len();
    if len == 0 {
        return;
    }
    let block = block_bytes();
    let ptr = buf.as_mut_ptr();
    // SAFETY: `ptr` is derived from a live, exclusively-borrowed `&mut [u8]`
    // valid for `len` bytes, so every write below lands inside the region and
    // nothing else aliases it. `zero_raw` writes only within `[0, len)`: the
    // head/tail loops store one byte at a time under an explicit `i < len`
    // bound, and each `DC ZVA` clears exactly the `block`-aligned block
    // starting at an offset it has proven satisfies `i + block <= len`, so no
    // `DC ZVA` touches a byte past `len`. `block` is a power of two ≥ 4 from
    // `DCZID_EL0`, and the `DcZva` feature bit that admits this candidate
    // guarantees the instruction is permitted at this EL.
    unsafe {
        zero_raw(ptr, len, block);
    }
}

/// Zero `[ptr, ptr + len)` using byte stores for the unaligned head/tail and
/// `DC ZVA` for each `block`-aligned interior block.
///
/// # Safety
///
/// `ptr` must be valid for writes of `len` bytes and exclusively borrowed for
/// the duration of the call. `block` must be the true `DC ZVA` block size (a
/// power of two ≥ 4) and `DC ZVA` must be permitted at the current EL (the
/// `lib/cpuops` `DcZva` capability gate is the sole caller and enforces both).
unsafe fn zero_raw(ptr: *mut u8, len: usize, block: usize) {
    let base = ptr as usize;
    let mask = block - 1;
    let mut i = 0usize;

    // Head: byte stores until the address is block-aligned (or the region
    // ends first).
    while i < len && (base + i) & mask != 0 {
        // SAFETY: `i < len`, so `ptr.add(i)` is in bounds of the region.
        unsafe {
            ptr.add(i).write(0);
        }
        i += 1;
    }

    // Middle: one `DC ZVA` per whole aligned block. `i` is block-aligned here,
    // and `i + block <= len` proves the whole block lies inside the region.
    while i + block <= len {
        // SAFETY: `ptr.add(i)` is in bounds and block-aligned; `DC ZVA` clears
        // exactly the `block` bytes `[ptr+i, ptr+i+block)`, all inside the
        // region. The instruction is permitted (the `DcZva` gate).
        unsafe {
            let p = ptr.add(i);
            core::arch::asm!(
                "dc zva, {p}",
                p = in(reg) p,
                options(nostack, preserves_flags),
            );
        }
        i += block;
    }

    // Tail: byte stores for the remainder past the last full block.
    while i < len {
        // SAFETY: `i < len`, so `ptr.add(i)` is in bounds of the region.
        unsafe {
            ptr.add(i).write(0);
        }
        i += 1;
    }
}
