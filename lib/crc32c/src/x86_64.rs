//! x86_64 hardware CRC-32C candidate, over the SSE4.2 `crc32` instruction.
//!
//! Compiled only when the target architecture is x86_64 (the build-script
//! `crc32c_x86_64` cfg). The `crc32` instruction (`CPUID.1:ECX.SSE4_2`)
//! computes exactly the Castagnoli CRC-32C the portable baseline does, on
//! general-purpose registers — no XMM/vector state — so it carries none of the
//! freestanding-SIMD codegen risk the wide-vector extensions do, and needs no
//! FPU/SIMD save across the call.
//!
//! The function is only ever reached after `lib/cpuops` has confirmed the
//! [`CpuFeature::Sse42`](tairix_abi::cpufeatures::CpuFeature::Sse42) bit is
//! present and self-verified the output against the portable reference, so the
//! instruction can never trap on a core that lacks it and a decode bug can
//! never be selected.

use core::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};

/// Compute the CRC-32C of `data` using the SSE4.2 `crc32` instruction.
///
/// Safe wrapper: the unsafe intrinsic call is sound because
/// `crc32c_sse42_unchecked` is only invoked here, and this whole candidate
/// is compiled and selected only when SSE4.2 is present (the caller in
/// `lib/cpuops` gates on the feature bit).
#[must_use]
pub fn crc32c_sse42(data: &[u8]) -> u32 {
    // SAFETY: `crc32c_sse42_unchecked` requires the SSE4.2 feature. This
    // candidate is registered with `requires: &[CpuFeature::Sse42]`, so the
    // `lib/cpuops` selector only ever hands its function pointer to a consumer
    // after confirming the bit is set in the delivered `CpuFeatureSet`; a core
    // without SSE4.2 filters it out and runs the portable baseline instead.
    unsafe { crc32c_sse42_unchecked(data) }
}

/// The `#[target_feature]` core.
///
/// # Safety
///
/// The caller must ensure the CPU implements SSE4.2 (the `crc32` instruction);
/// executing it on a core that does not would raise an illegal-instruction
/// fault. The `lib/cpuops` capability gate is the sole caller and enforces
/// this.
// `crc as u32` narrows the running `_mm_crc32_u64` accumulator: the `crc32`
// instruction defines the result's upper 32 bits as zero, so the narrow is
// exact, not a lossy truncation.
#[allow(clippy::cast_possible_truncation)]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42_unchecked(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu64;
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        // The bytes are folded low-address-first to match the reflected
        // CRC-32C the portable reference computes; `from_le_bytes` fixes the
        // order independently of the host's endianness (both x86_64 and the
        // reference agree). A decode mistake here is caught by the mandatory
        // self-verify before this candidate can be selected.
        let word = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        // The intrinsic is safe to call here: this fn carries the matching
        // `#[target_feature(enable = "sse4.2")]`, so the compiler proves the
        // instruction is legal in this body.
        crc = _mm_crc32_u64(crc, word);
    }
    let mut crc = crc as u32;
    for &byte in chunks.remainder() {
        crc = _mm_crc32_u8(crc, byte);
    }
    !crc
}
