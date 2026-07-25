//! x86_64 hardware page-zero candidate, over `rep stosb`.
//!
//! Compiled only when the target architecture is x86_64 (the build-script
//! `pagezero_x86_64` cfg). `rep stosb` stores `AL` to `[RDI]` `RCX` times; it
//! is a base-ISA instruction, correct and legal on *every* x86_64 CPU. The
//! Enhanced `REP MOVSB`/`STOSB` (ERMS) feature —
//! [`CpuFeature::Erms`](tairix_abi::cpufeatures::CpuFeature::Erms) — does
//! not change its meaning, only its speed: with ERMS the microcode uses a
//! wide, cache-optimised path that beats a scalar or even a hand-written SIMD
//! fill. The candidate is therefore *gated on ERMS for selection* (it is only
//! worth choosing over the portable baseline when ERMS makes it fast), even
//! though the instruction would run correctly without it.
//!
//! [`CpuFeature::Erms`](tairix_abi::cpufeatures::CpuFeature::Erms) admits this
//! candidate; it is only reached after `lib/cpuops` confirms the bit and
//! self-verifies the output against the portable reference.

/// Zero every byte of `buf` using `rep stosb`.
///
/// A safe wrapper: the region comes from a live `&mut [u8]`, so the raw fill
/// is sound.
pub fn zero_erms(buf: &mut [u8]) {
    let ptr = buf.as_mut_ptr();
    let len = buf.len();
    // SAFETY: `ptr` is derived from a live `&mut [u8]` valid for `len` bytes,
    // so it is non-null (for a non-empty slice), well-aligned for `u8`, and
    // writable for exactly `len` bytes. `rep stosb` writes `RCX` (= `len`)
    // copies of `AL` (= 0) forward from `RDI` (= `ptr`); the System V AMD64
    // ABI guarantees the direction flag is clear on entry, so the store runs
    // forward and clobbers exactly `[ptr, ptr + len)`. `len == 0` sets `RCX`
    // to 0 and stores nothing. The exclusive `&mut` borrow means nothing else
    // aliases the region for the duration of the fill.
    unsafe {
        core::arch::asm!(
            "rep stosb",
            inout("rdi") ptr => _,
            inout("rcx") len => _,
            in("al") 0u8,
            options(nostack, preserves_flags),
        );
    }
}
