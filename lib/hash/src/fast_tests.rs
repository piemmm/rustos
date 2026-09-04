//! Unit tests for [`FastHash`].
//!
//! The oracle is the XXH64 reference implementation's published outputs: the
//! values below are the ones the C reference produces for these inputs, which
//! is what pins this implementation to the algorithm rather than to itself.

extern crate std;

use core::hash::{Hash, Hasher};
use std::vec::Vec;

use super::FastHash;

/// The seed the reference's seeded cases use.
const SEED: u64 = 0xae05_4331_1b70_2d91;

/// `(input, seed, expected)` from the XXH64 reference implementation.
fn reference_cases() -> Vec<(Vec<u8>, u64, u64)> {
    let hundred: Vec<u8> = (0..100u8).collect();
    std::vec![
        (Vec::new(), 0, 0xef46_db37_51d8_e999),
        (std::vec![42], 0, 0x0a9e_dece_beb0_3ae4),
        (b"Hello, world!\0".to_vec(), 0, 0x7b06_c531_ea43_e89f),
        (hundred.clone(), 0, 0x6ac1_e580_3216_6597),
        (Vec::new(), SEED, 0x4b6a_04fc_df7a_4672),
        (hundred, SEED, 0x567e_355e_0682_e1f1),
        (
            b"x".to_vec(),
            u64::MAX - super::PRIME5,
            0xf953_d52c_12a9_f5fb
        ),
    ]
}

#[test]
fn matches_the_reference_implementation() {
    for (input, seed, expected) in reference_cases() {
        assert_eq!(
            FastHash::hash_bytes(seed, &input),
            expected,
            "one-shot over {} bytes, seed {seed:#x}",
            input.len()
        );
    }
}

#[test]
fn streaming_matches_the_reference_implementation() {
    for (input, seed, expected) in reference_cases() {
        let mut hasher = FastHash::with_seed(seed);
        for byte in &input {
            hasher.write(&[*byte]);
        }
        assert_eq!(
            hasher.finish(),
            expected,
            "byte-at-a-time over {} bytes, seed {seed:#x}",
            input.len()
        );
    }
}

/// Every way of splitting an input into writes must produce one hash — the
/// stripe buffer's whole job.
#[test]
fn any_chunking_hashes_alike() {
    let input: Vec<u8> = pattern(200);
    let whole = FastHash::hash_bytes(SEED, &input);
    for split in 0..=input.len() {
        let mut hasher = FastHash::with_seed(SEED);
        hasher.write(&input[..split]);
        hasher.write(&input[split..]);
        assert_eq!(hasher.finish(), whole, "split at {split}");
    }
    for chunk in [1usize, 3, 7, 8, 16, 31, 32, 33, 64] {
        let mut hasher = FastHash::with_seed(SEED);
        for piece in input.chunks(chunk) {
            hasher.write(piece);
        }
        assert_eq!(hasher.finish(), whole, "chunks of {chunk}");
    }
}

#[test]
fn the_unseeded_form_is_the_zero_seed() {
    assert_eq!(
        FastHash::new().finish(),
        FastHash::hash_bytes(0, &[]),
        "an unseeded hasher digests the empty input like the zero seed"
    );
}

#[test]
fn integer_writes_are_width_and_endian_independent() {
    let mut by_int = FastHash::new();
    by_int.write_u32(0x1234_5678);
    let mut by_bytes = FastHash::new();
    by_bytes.write(&[0x78, 0x56, 0x34, 0x12]);
    assert_eq!(by_int.finish(), by_bytes.finish());

    let mut pointer_sized = FastHash::new();
    pointer_sized.write_usize(0x1234_5678);
    let mut widened = FastHash::new();
    widened.write_u64(0x1234_5678);
    assert_eq!(pointer_sized.finish(), widened.finish());
}

/// The distribution gate, as a deterministic counter rather than a timing:
/// consecutive kernel-assigned identifiers must spread evenly over the low
/// bits a table indexes with.
#[test]
fn low_bits_spread_evenly_over_consecutive_keys() {
    const KEYS: usize = 4096;
    const BUCKETS: usize = 256;
    let mut counts = [0usize; BUCKETS];
    for key in 0..KEYS as u64 {
        let mut hasher = FastHash::new();
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

#[test]
fn one_input_bit_flips_about_half_the_output() {
    for bit in 0..64 {
        let base = FastHash::hash_bytes(0, &0u64.to_le_bytes());
        let flipped = FastHash::hash_bytes(0, &(1u64 << bit).to_le_bytes());
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
    let mut byte = 7u8;
    for _ in 0..len {
        out.push(byte);
        byte = byte.wrapping_mul(31).wrapping_add(7);
    }
    out
}
