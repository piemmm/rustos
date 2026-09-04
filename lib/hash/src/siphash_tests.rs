//! Unit tests for [`SipHash13`].
//!
//! The oracle is the published `SipHash` reference vector set: the incrementing
//! byte prefixes `[]`, `[0x00]`, `[0x00, 0x01]`, … `[0x00 … 0x3e]` under the
//! reference key `00 01 02 … 0f`. The table below is the SipHash-1-3
//! expectation for that set, cross-checked against two independent
//! implementations (the Rust standard library's `SipHasher13` and `CPython`'s
//! `siphash13`) before it was written down.

extern crate std;

use core::hash::{Hash, Hasher};
use std::vec::Vec;

use super::SipHash13;
use crate::seed::HashSeed;

/// The published reference key: bytes `00 01 … 0f` as two little-endian
/// words.
const REFERENCE_KEY: HashSeed = HashSeed::from_words(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);

/// SipHash-1-3 of the incrementing prefix of length `i` under
/// [`REFERENCE_KEY`], for `i` in `0..64`.
const VECTORS: [u64; 64] = [
    0xabac_0158_050f_c4dc,
    0xc9f4_9bf3_7d57_ca93,
    0x82cb_9b02_4dc7_d44d,
    0x8bf8_0ab8_e7dd_f7fb,
    0xcf75_5760_88d3_8328,
    0xdef9_d52f_4953_3b67,
    0xc50d_2b50_c59f_22a7,
    0xd392_7d98_9bb1_1140,
    0x3690_9511_8d29_9a8e,
    0x25a4_8eb3_6c06_3de4,
    0x79de_85ee_92ff_097f,
    0x70c1_18c1_f94d_c352,
    0x78a3_84b1_57b4_d9a2,
    0x306f_760c_1229_ffa7,
    0x605a_a111_c0f9_5d34,
    0xd320_d86d_2a51_9956,
    0xcc4f_dd1a_7d90_8b66,
    0x9cf2_6890_63db_d80c,
    0x8ffc_389c_b473_e63e,
    0xf21f_9de5_8d29_7d1c,
    0xc0dc_2f46_a6cc_e040,
    0xb992_abfe_2b45_f844,
    0x7ffe_7b9b_a320_872e,
    0x525a_0e7f_dae6_c123,
    0xf464_aeb2_6734_9c8c,
    0x45cd_5928_705b_0979,
    0x3a3e_35e3_ca99_13a5,
    0xa91d_c74e_4ade_3b35,
    0xfb0b_ed02_ef6c_d00d,
    0x88d9_3cb4_4ab1_e1f4,
    0x540f_11d6_43c5_e663,
    0x2370_dd1f_8c21_d1bc,
    0x8115_7b6c_16a7_b60d,
    0x4d54_b9e5_7a8f_f9bf,
    0x759f_1278_1f2a_753e,
    0xcea1_a3be_bf18_6b91,
    0x2cf5_08d3_ada2_6206,
    0xb610_1c2d_a3c3_3057,
    0xb3f4_7496_ae3a_36a1,
    0x626b_5754_7b10_8392,
    0xc1d2_3632_99e4_1531,
    0x667c_c192_3f1a_d944,
    0x6570_4ffe_c813_8825,
    0x24f2_80d1_c289_49a6,
    0xc2ca_1ced_faf8_876b,
    0xc216_4bfc_9f04_2196,
    0xa16e_9c93_68b1_d623,
    0x49fb_169c_8b51_14fd,
    0x9f31_43f8_df07_4c46,
    0xc6fd_af24_12cc_86b3,
    0x7eaf_49d1_0a52_098f,
    0x1cf3_1355_9d29_2f9a,
    0xc44a_30dd_a2f4_1f12,
    0x36fa_e989_43a7_1ed0,
    0x318f_b34c_73f0_bce6,
    0xa27a_bf36_70a7_e980,
    0xb4bc_c0db_243c_6d75,
    0x23f8_d852_fdb7_1513,
    0x8f03_5f4d_a67d_8a08,
    0xd89c_d0e5_b7e8_f148,
    0xf6f4_e6bc_f7a6_44ee,
    0xaec5_9ad8_0f18_37f2,
    0xc3b2_f615_4b66_94e0,
    0x9d19_9062_b7bb_b3a8,
];

#[test]
fn matches_the_published_reference_vectors() {
    let bytes: Vec<u8> = (0..64u8).collect();
    for (len, &expected) in VECTORS.iter().enumerate() {
        assert_eq!(
            SipHash13::hash_bytes(REFERENCE_KEY, &bytes[..len]),
            expected,
            "one-shot vector {len}"
        );
    }
}

#[test]
fn streaming_one_byte_at_a_time_matches_the_vectors() {
    let bytes: Vec<u8> = (0..64u8).collect();
    for (len, &expected) in VECTORS.iter().enumerate() {
        let mut hasher = SipHash13::new(REFERENCE_KEY);
        for byte in &bytes[..len] {
            hasher.write(&[*byte]);
        }
        assert_eq!(hasher.finish(), expected, "streamed vector {len}");
    }
}

/// Every way of splitting an input into writes must produce one hash: a
/// caller that feeds a header and a body separately must agree with one that
/// feeds the concatenation, or a container's lookups would miss their
/// entries.
#[test]
fn any_chunking_hashes_alike() {
    let input: Vec<u8> = pattern(97);
    let whole = SipHash13::hash_bytes(REFERENCE_KEY, &input);
    for split in 0..=input.len() {
        let mut hasher = SipHash13::new(REFERENCE_KEY);
        hasher.write(&input[..split]);
        hasher.write(&input[split..]);
        assert_eq!(hasher.finish(), whole, "split at {split}");
    }
    for chunk in [1usize, 2, 3, 5, 7, 8, 9, 16, 31, 32] {
        let mut hasher = SipHash13::new(REFERENCE_KEY);
        for piece in input.chunks(chunk) {
            hasher.write(piece);
        }
        assert_eq!(hasher.finish(), whole, "chunks of {chunk}");
    }
}

#[test]
fn a_different_key_gives_a_different_hash() {
    let other = HashSeed::from_words(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0909);
    assert_ne!(
        SipHash13::hash_bytes(REFERENCE_KEY, b"tairix"),
        SipHash13::hash_bytes(other, b"tairix")
    );
}

/// Integer writes are little-endian and pointer-sized values are widened, so
/// the same value hashes the same on a 32-bit port as on a 64-bit one.
#[test]
fn integer_writes_are_width_and_endian_independent() {
    let mut by_int = SipHash13::new(REFERENCE_KEY);
    by_int.write_u32(0x1234_5678);
    let mut by_bytes = SipHash13::new(REFERENCE_KEY);
    by_bytes.write(&[0x78, 0x56, 0x34, 0x12]);
    assert_eq!(by_int.finish(), by_bytes.finish());

    let mut pointer_sized = SipHash13::new(REFERENCE_KEY);
    pointer_sized.write_usize(0x1234_5678);
    let mut widened = SipHash13::new(REFERENCE_KEY);
    widened.write_u64(0x1234_5678);
    assert_eq!(pointer_sized.finish(), widened.finish());

    let mut signed_pointer_sized = SipHash13::new(REFERENCE_KEY);
    signed_pointer_sized.write_isize(-3);
    let mut signed_widened = SipHash13::new(REFERENCE_KEY);
    signed_widened.write_i64(-3);
    assert_eq!(signed_pointer_sized.finish(), signed_widened.finish());
}

/// The gate a hash table depends on, stated as a deterministic counter: over
/// a run of consecutive integer keys, the low bits a table would index with
/// must land evenly. A perfect spread over 256 buckets puts 16 of 4096 keys
/// in each; anything beyond a factor of three either way is a distribution
/// defect, not load-dependent noise.
#[test]
fn low_bits_spread_evenly_over_consecutive_keys() {
    const KEYS: usize = 4096;
    const BUCKETS: usize = 256;
    let mut counts = [0usize; BUCKETS];
    for key in 0..KEYS as u64 {
        let mut hasher = SipHash13::new(REFERENCE_KEY);
        key.hash(&mut hasher);
        let bucket = usize::try_from(hasher.finish() % BUCKETS as u64).unwrap_or(0);
        counts[bucket] += 1;
    }
    let ideal = KEYS / BUCKETS;
    for (bucket, &count) in counts.iter().enumerate() {
        assert!(
            count >= ideal / 3 && count <= ideal * 3,
            "bucket {bucket} holds {count} of {KEYS} keys (ideal {ideal})"
        );
    }
}

/// Flipping one input bit must change roughly half the output bits;
/// a hash that carried a bit straight through would let an attacker
/// steer a bucket index directly.
#[test]
fn one_input_bit_flips_about_half_the_output() {
    for bit in 0..64 {
        let base = SipHash13::hash_bytes(REFERENCE_KEY, &0u64.to_le_bytes());
        let flipped = SipHash13::hash_bytes(REFERENCE_KEY, &(1u64 << bit).to_le_bytes());
        let changed = (base ^ flipped).count_ones();
        assert!(
            (16..=48).contains(&changed),
            "input bit {bit} changed {changed} output bits"
        );
    }
}

/// A deterministic byte pattern of `len` bytes, so a chunking test walks
/// varied input rather than a run of one value.
fn pattern(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut byte = 11u8;
    for _ in 0..len {
        out.push(byte);
        byte = byte.wrapping_mul(37).wrapping_add(11);
    }
    out
}
