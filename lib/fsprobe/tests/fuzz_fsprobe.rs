//! Deterministic fuzz harness for the `lib/fsprobe` signature probe (a
//! parser of untrusted on-disk bytes).
//!
//! The probe head is read straight off removable media outside RustOS's
//! trust boundary: a hostile stick can carry any bytes at the signature,
//! label, geometry, and identity offsets. Whatever the head holds, the
//! probe must either recognise a supported filesystem or return `None` —
//! it must never panic, read out of bounds, or fabricate a match. The run
//! aborting *is* the failure.
//!
//! RustOS pulls in no external fuzz runner: a per-run-seeded LCG mutates
//! valid seed heads (a structurally sound FAT32 boot sector, an ext4
//! superblock, and a `ARXFS` superblock slot), truncates them, and feeds
//! pure noise. A plain `cargo test` runs the fixed [`SMOKE_ITERATIONS`]
//! sweep; `cargo xtask fuzz` extends the loop to a wall-clock budget.

use rustos_fsprobe::{
    fingerprint, probe, ARXFS_HEADER_MAGIC, EXT4_SUPERBLOCK_MAGIC, PROBE_HEAD_LEN,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// A structurally valid FAT32 boot-sector head.
fn fat32_head() -> Vec<u8> {
    let mut head = vec![0u8; PROBE_HEAD_LEN];
    head[11..13].copy_from_slice(&512u16.to_le_bytes());
    head[13] = 8;
    head[14..16].copy_from_slice(&32u16.to_le_bytes());
    head[16] = 2;
    head[36..40].copy_from_slice(&1024u32.to_le_bytes());
    head[44..48].copy_from_slice(&2u32.to_le_bytes());
    head[66] = 0x29;
    head[67..71].copy_from_slice(b"SRLN");
    head[71..82].copy_from_slice(b"HOLIDAY PIX");
    head[82..90].copy_from_slice(b"FAT32   ");
    head[510] = 0x55;
    head[511] = 0xAA;
    head
}

/// A structurally valid ext4 superblock head.
fn ext4_head() -> Vec<u8> {
    let mut head = vec![0u8; PROBE_HEAD_LEN];
    let sb = 1024;
    head[sb..sb + 4].copy_from_slice(&8192u32.to_le_bytes());
    head[sb + 4..sb + 8].copy_from_slice(&32768u32.to_le_bytes());
    head[sb + 0x18..sb + 0x1C].copy_from_slice(&2u32.to_le_bytes());
    head[sb + 0x38..sb + 0x3A].copy_from_slice(&EXT4_SUPERBLOCK_MAGIC.to_le_bytes());
    head[sb + 0x68..sb + 0x78].copy_from_slice(&[7u8; 16]);
    head[sb + 0x78..sb + 0x7E].copy_from_slice(b"backup");
    head
}

/// A `ARXFS` superblock-slot head.
fn arxfs_head() -> Vec<u8> {
    let mut head = vec![0u8; PROBE_HEAD_LEN];
    head[..8].copy_from_slice(&ARXFS_HEADER_MAGIC.to_le_bytes());
    head[8..12].copy_from_slice(&1u32.to_le_bytes());
    head[16..32].copy_from_slice(&[9u8; 16]);
    head
}

/// `x` reduced into `0..=max`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// Probe `bytes` and drain every accessor a caller would: must never
/// panic, whatever the head holds.
fn exercise_never_panics(bytes: &[u8]) {
    if let Some(probed) = probe(bytes) {
        let _ = probed.fstype;
        let _ = probed.label();
        // The fingerprint renderer must accept any identity the probe
        // produced.
        let _ = fingerprint(&probed.identity);
    }
}

#[test]
fn probing_any_head_never_panics() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    let corpus = [fat32_head(), ext4_head(), arxfs_head()];

    let mut state: u64 = rustos_fuzzseed::start(
        "probing_any_head_never_panics",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. A real head with a handful of bytes flipped at random,
        //    hammering the signatures, geometry fields, labels, and
        //    identities.
        let template = &corpus[bounded(next(), corpus.len() - 1)];
        let mut mutated = template.clone();
        let flips = bounded(next(), 24);
        for _ in 0..flips {
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        exercise_never_panics(&mutated);

        // 2. A truncation of a real head, driving the bounds checks.
        let keep = bounded(next(), template.len());
        exercise_never_panics(&template[..keep]);

        // 3. Pure noise of an arbitrary length (including oversize heads).
        let nlen = bounded(next(), PROBE_HEAD_LEN * 2 + 17);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 29)).collect();
        exercise_never_panics(&noise);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
