//! aarch64 hardware CRC-32C candidate, over the `crc32c*` instructions.
//!
//! Compiled only when the target architecture is aarch64 (the build-script
//! `crc32c_aarch64` cfg). The ARMv8 CRC32 extension
//! (`ID_AA64ISAR0_EL1.CRC32`, the [`CpuFeature::Crc32`](tairix_abi::cpufeatures::CpuFeature::Crc32) bit) provides both the
//! ISO-HDLC `crc32*` and the Castagnoli `crc32c*` instruction families; this
//! candidate uses `crc32cx`/`crc32cb`, which compute exactly the Castagnoli
//! CRC-32C the portable baseline does, on general-purpose registers (no NEON
//! state), so it carries no freestanding-SIMD codegen risk.
//!
//! [`CpuFeature::Crc32`](tairix_abi::cpufeatures::CpuFeature::Crc32) gates the
//! whole extension, so the same bit that admits this candidate guarantees the
//! `crc32c*` encodings are legal. It is only reached after `lib/cpuops`
//! confirms the bit and self-verifies the output against the reference.

use core::arch::aarch64::{__crc32cb, __crc32cd};

/// Compute the CRC-32C of `data` using the ARMv8 `crc32c*` instructions.
///
/// Safe wrapper: the unsafe intrinsic call is sound because the candidate is
/// compiled and selected only when the CRC32 extension is present (the
/// `lib/cpuops` caller gates on the feature bit).
#[must_use]
pub fn crc32c_hw(data: &[u8]) -> u32 {
    // SAFETY: `crc32c_hw_unchecked` requires the CRC32 extension. This
    // candidate is registered with `requires: &[CpuFeature::Crc32]`, so
    // `lib/cpuops` only hands out its pointer after confirming the bit is set;
    // a core without it filters the candidate out and runs the baseline.
    unsafe { crc32c_hw_unchecked(data) }
}

/// The `#[target_feature]` core.
///
/// # Safety
///
/// The caller must ensure the CPU implements the ARMv8 CRC32 extension;
/// executing `crc32c*` on a core that does not would raise an
/// undefined-instruction fault. The `lib/cpuops` capability gate is the sole
/// caller and enforces this.
#[target_feature(enable = "crc")]
unsafe fn crc32c_hw_unchecked(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    let (chunks, remainder) = data.as_chunks::<8>();
    for chunk in chunks {
        // Bytes folded low-address-first (`from_le_bytes`) to match the
        // reflected CRC-32C; a mistake is caught by the mandatory self-verify
        // before the candidate can be selected.
        let word = u64::from_le_bytes(*chunk);
        // The intrinsic is safe to call here: this fn carries the matching
        // `#[target_feature(enable = "crc")]`, so the compiler proves the
        // `crc32cd` instruction is legal in this body.
        crc = __crc32cd(crc, word);
    }
    for &byte in remainder {
        crc = __crc32cb(crc, byte);
    }
    !crc
}
