//! aarch64 control-group scan over Advanced SIMD (NEON).
//!
//! Compiled only when the target is aarch64 *and* NEON is already in the
//! target's own feature set (the build script's `swiss_neon` name). The EL1
//! exception path saves and restores the whole FP/SIMD register file, so a
//! vector scan is legal in kernel code on this port.
//!
//! NEON has no single lane-mask extract, so each comparison's all-ones lanes
//! are weighted by their bit position and horizontally summed: `AND` with the
//! weight vector, then one `ADDV` per half. Three instructions where x86 needs
//! one, still a fraction of the portable baseline's scalar work.

use core::arch::aarch64::{
    uint8x16_t, vaddv_u8, vandq_u8, vceqq_u8, vdupq_n_u8, vget_high_u8, vget_low_u8, vld1q_u8,
    vtstq_u8,
};

use super::{Group, GroupMatch, EMPTY};

/// Bit weight of each lane within its half of the group.
const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

/// Scan one control group using NEON.
///
/// Reached only after `lib/cpuops` has confirmed the Advanced SIMD feature bit
/// and self-verified the output against the portable reference.
#[must_use]
pub fn scan_neon(group: &Group, tag: u8) -> GroupMatch {
    // SAFETY: `scan_unchecked` requires Advanced SIMD. This candidate is
    // registered with `requires: &[CpuFeature::Asimd]`, so the selector only
    // hands its function pointer out after confirming the bit is set in the
    // delivered feature set; a core without it runs the portable baseline.
    unsafe { scan_unchecked(group, tag) }
}

/// Collapse a vector whose lanes are all-ones or all-zero into one bit per
/// lane, lane `n` in bit `n`.
///
/// # Safety
///
/// The caller must ensure the CPU implements Advanced SIMD.
#[target_feature(enable = "neon")]
unsafe fn lane_mask(lanes: uint8x16_t) -> u16 {
    // SAFETY: `WEIGHTS` is sixteen readable bytes and `vld1q_u8` is an
    // unaligned load, so no alignment invariant applies.
    let weights = unsafe { vld1q_u8(WEIGHTS.as_ptr()) };
    let weighted = vandq_u8(lanes, weights);
    // Each half sums to at most 255, so neither `ADDV` can overflow its byte.
    u16::from(vaddv_u8(vget_low_u8(weighted))) | (u16::from(vaddv_u8(vget_high_u8(weighted))) << 8)
}

/// The `#[target_feature]` core.
///
/// # Safety
///
/// The caller must ensure the CPU implements Advanced SIMD; executing these
/// instructions on a core that does not would raise an illegal-instruction
/// fault. The `lib/cpuops` capability gate is the sole caller and enforces it.
#[target_feature(enable = "neon")]
unsafe fn scan_unchecked(group: &Group, tag: u8) -> GroupMatch {
    // SAFETY: `group` is sixteen readable bytes and `vld1q_u8` is an unaligned
    // load, so no alignment invariant applies.
    let ctrl = unsafe { vld1q_u8(group.as_ptr()) };
    // SAFETY: this function carries the same `neon` target feature, so the
    // helper's requirement is discharged by this function's own caller.
    unsafe {
        GroupMatch {
            tag: lane_mask(vceqq_u8(ctrl, vdupq_n_u8(tag))),
            empty: lane_mask(vceqq_u8(ctrl, vdupq_n_u8(EMPTY))),
            // A free lane is one whose control byte has its top bit set;
            // `vtstq_u8` yields the all-ones lanes the mask extract needs,
            // where a plain `AND` would leave `0x80`.
            free: lane_mask(vtstq_u8(ctrl, vdupq_n_u8(0x80))),
        }
    }
}
