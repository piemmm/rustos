//! x86_64 control-group scan over SSE2.
//!
//! Compiled only when the target is x86_64 *and* SSE2 is already in the
//! target's own feature set (the build script's `swiss_sse2` name), so a
//! soft-float kernel target whose vector unit is off never sees these
//! intrinsics: its codegen backend cannot lower them, and a kernel that has
//! not enabled the vector unit must not touch it.
//!
//! `PCMPEQB` plus `PMOVMSKB` answer a whole sixteen-lane group in a couple of
//! instructions where the portable baseline needs tens of scalar operations.

use core::arch::x86_64::{
    __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
};

use super::{Group, GroupMatch, EMPTY};

/// Scan one control group using SSE2.
///
/// Reached only after `lib/cpuops` has confirmed the SSE2 feature bit and
/// self-verified the output against the portable reference, so the
/// instructions can never trap on a core that lacks them.
#[must_use]
pub fn scan_sse2(group: &Group, tag: u8) -> GroupMatch {
    // SAFETY: `scan_unchecked` requires SSE2. This candidate is registered
    // with `requires: &[CpuFeature::Sse2]`, so the selector only hands its
    // function pointer out after confirming the bit is set in the delivered
    // feature set; a core without SSE2 filters it out and runs the portable
    // baseline instead.
    unsafe { scan_unchecked(group, tag) }
}

/// The `#[target_feature]` core.
///
/// # Safety
///
/// The caller must ensure the CPU implements SSE2; executing these
/// instructions on a core that does not would raise an illegal-instruction
/// fault. The `lib/cpuops` capability gate is the sole caller and enforces it.
// `_mm_set1_epi8` takes a signed lane and `_mm_movemask_epi8` returns the
// sixteen lane bits in the low half of an `i32`: both are reinterpretations of
// the same bits, never value conversions.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
#[target_feature(enable = "sse2")]
unsafe fn scan_unchecked(group: &Group, tag: u8) -> GroupMatch {
    // `_mm_loadu_si128` is the *unaligned* load, so the vector alignment the
    // cast appears to promise is not one the instruction requires.
    #[allow(clippy::cast_ptr_alignment)]
    let source = group.as_ptr().cast::<__m128i>();
    // SAFETY: `group` is sixteen readable bytes, which is exactly what the
    // unaligned load reads.
    let ctrl = unsafe { _mm_loadu_si128(source) };
    let tag_hits = _mm_movemask_epi8(_mm_cmpeq_epi8(ctrl, _mm_set1_epi8(tag as i8)));
    let empty = _mm_movemask_epi8(_mm_cmpeq_epi8(ctrl, _mm_set1_epi8(EMPTY as i8)));
    // A free lane is one whose control byte has its top bit set, which
    // `PMOVMSKB` extracts directly.
    let free = _mm_movemask_epi8(ctrl);
    GroupMatch {
        tag: tag_hits as u16,
        empty: empty as u16,
        free: free as u16,
    }
}
