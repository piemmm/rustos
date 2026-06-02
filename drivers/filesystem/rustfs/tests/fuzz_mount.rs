//! Deterministic fuzz harness for the rustfs mount / metadata-decode path
//! (`AGENTS.md` §19.5 / §19.6).
//!
//! [`RustFs::open`] decodes a device's superblock ring, transaction root,
//! inode-tree nodes, per-file extent-tree nodes, and — since Stage 7 — the
//! chunk/refcount tree and the reverse-reference tree (mount rebuilds the
//! dedupe index from them, decoding every `ChunkRecord` and reverse-reference
//! record). All are self-identifying metadata (`header` / `superblock` /
//! `transaction` / `btree` / `dedupe`) read from a backing store that, on a
//! real system, may have been written by anything. The base image is
//! populated with several files, a multi-extent file, **duplicate-content
//! files, and a reflink** so the sweep spends its time near real inode-tree,
//! extent-tree, chunk-tree, and reverse-reference nodes, not just the
//! superblock ring. Per §19.6 that decode path is driven by a fuzz harness
//! whose single invariant is:
//!
//! * `open` never panics for any device contents — it returns `Ok` for a
//!   genuinely valid volume and `Err` (fail closed) for everything else.
//!
//! RustOS pulls in no external fuzz runner (`AGENTS.md` §2.12): a fixed-seed
//! LCG draws pseudo-random images, and a structured sweep flips every byte of
//! a real formatted image to hammer the §8 block-identity checks (magic, type,
//! address, keyed authenticator). Stage 3 added the keyed metadata
//! authenticator and a redundant mirror copy of every metadata block, so the
//! single-byte sweep also exercises the authenticate-then-fall-back-to-the-
//! mirror path, and a dedicated **duplicated-copy sweep** corrupts *both*
//! copies of each block pair to hammer the both-copies-bad fail-closed path. A
//! plain `cargo test` runs the fixed [`SMOKE_ITERATIONS`] sweep; `cargo xtask
//! fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS` to extend the PRNG loop to a
//! wall-clock budget.

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use rustos_abi::DriverError;
use rustos_drv_fs_rustfs::{RustFs, VolumeKey, VOLUME_KEY_LEN};

/// Volume key the fuzz image is formatted and reopened with. `RustFS` is
/// encrypted-by-default (`docs/src/filesystem/rustfs-spec.md` §5); the fuzz
/// sweep exercises the encrypted-volume open path under this fixed key.
const FUZZ_KEY: VolumeKey = [0x5a; VOLUME_KEY_LEN];

const BLOCK_SIZE: u32 = 512;
const BLOCK_COUNT: u64 = 64;
/// Device size in bytes. `64` is `BLOCK_COUNT`, kept as a `usize` literal so
/// the const needs no `u64`-to-`usize` cast.
const IMAGE_LEN: usize = BLOCK_SIZE as usize * 64;

/// Fixed-iteration sweep run by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

/// Byte offset inside the 32-byte keyed-tag slot (72..104) of the 128-byte
/// block header; flipping it always breaks a block's authenticator.
const TAG_OFFSET: usize = 80;

/// In-RAM device over a fixed-size byte image.
struct MemBlock {
    store: Vec<u8>,
}

impl Block for MemBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BLOCK_SIZE,
            block_count: BLOCK_COUNT,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = BLOCK_SIZE as usize;
        if buf.is_empty() || buf.len() % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(bs))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let bs = BLOCK_SIZE as usize;
        if buf.is_empty() || buf.len() % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(bs))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        self.store[start..end].copy_from_slice(buf);
        Ok(())
    }
}

/// Deadline for the current run, or `None` for the fixed smoke sweep.
fn budget() -> Option<std::time::Instant> {
    let secs: u64 = std::env::var("RUSTOS_FUZZ_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(std::time::Instant::now() + std::time::Duration::from_secs(secs))
}

fn within_budget(deadline: Option<std::time::Instant>) -> bool {
    matches!(deadline, Some(end) if std::time::Instant::now() < end)
}

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..len` as a `usize`, without a narrowing `as` cast.
fn index(x: u64, len: usize) -> usize {
    usize::try_from(x % len as u64).unwrap_or(0)
}

/// The single invariant: opening an arbitrary image must return a `Result`,
/// never panic. A successful mount must additionally survive being reopened.
fn exercise(image: &[u8]) {
    let mut store = image.to_vec();
    store.resize(IMAGE_LEN, 0);
    let dev = MemBlock { store };
    if let Ok(fs) = RustFs::open(dev, &FUZZ_KEY) {
        // A volume that mounts must mount again from its own bytes.
        let bytes = fs.into_block().store;
        let _ = RustFs::open(MemBlock { store: bytes }, &FUZZ_KEY);
    }
}

/// A real formatted image, populated with several inodes (so the inode tree
/// splits past a single node) and a file with two non-adjacent blocks (so an
/// extent-tree node exists). Both the PRNG and the structured sweep then spend
/// most of their time near genuinely valid tree metadata rather than pure
/// noise. Populating stops at the first `NoSpace` on the tiny fuzz device.
fn formatted_image() -> Vec<u8> {
    let mut fs = RustFs::format(
        MemBlock {
            store: vec![0u8; IMAGE_LEN],
        },
        16,
        &FUZZ_KEY,
    )
    .expect("format a blank fuzz device");
    let root = fs.root();
    for i in 0..6u32 {
        let name = format!("f{i}");
        if fs
            .create(root, name.as_bytes(), NodeKind::RegularFile)
            .is_err()
        {
            break;
        }
        // Two non-adjacent blocks build an extent record beyond the trivial.
        // The identical content across files also makes block 0 a *shared*
        // chunk, so the chunk/refcount and reverse-reference trees exist and
        // their decode paths are swept (Stage 7).
        let _ = fs.write_at(root, name.as_bytes(), 0, &[0xA5u8; 16]);
        let _ = fs.write_at(
            root,
            name.as_bytes(),
            2 * u64::from(BLOCK_SIZE),
            &[0x5Au8; 16],
        );
    }
    // A reflink guarantees a shared chunk with multiple referrers, populating
    // the reverse-reference tree even if the duplicate-content sharing above
    // is reclaimed; it is best-effort on the tiny fuzz device.
    let _ = fs.reflink(root, b"f0", b"f0clone");
    fs.into_block().store
}

#[test]
fn open_never_panics_on_arbitrary_images() {
    let deadline = budget();
    let base = formatted_image();

    // Structured sweep: flip every single byte of a valid image once. This
    // exhaustively probes the §8 identity/checksum rejection on a near-valid
    // image and runs regardless of the wall-clock budget.
    for i in 0..base.len() {
        let mut image = base.clone();
        image[i] ^= 0xff;
        exercise(&image);
    }

    // Duplicated-copy sweep (Stage 3): every metadata block is mirrored at the
    // adjacent block, and `open` falls back to — and repairs from — the mirror
    // when one copy fails the keyed authenticator. Corrupt the keyed-tag byte
    // of BOTH a block and its companion so neither copy authenticates,
    // exercising the both-copies-bad path. `open` must still return a
    // `Result`, never panic (`AGENTS.md` §2.9 / §19.6).
    let bs = BLOCK_SIZE as usize;
    let blocks = usize::try_from(BLOCK_COUNT).unwrap_or(0);
    for b in 0..blocks {
        let primary = bs * b + TAG_OFFSET;
        let companion = bs * (b + 1) + TAG_OFFSET;
        if companion >= base.len() {
            break;
        }
        let mut image = base.clone();
        image[primary] ^= 0xff;
        image[companion] ^= 0xff;
        exercise(&image);
    }

    // PRNG sweep: a fixed-seed LCG mutates the valid image at random offsets.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };
    let mut iteration: u64 = 0;
    loop {
        let mut image = base.clone();
        let flips = index(next(), 24);
        for _ in 0..flips {
            let pos = index(next(), image.len());
            image[pos] = low_byte(next() >> 17);
        }
        exercise(&image);

        // Occasionally feed pure noise too.
        if (next() >> 5).trailing_zeros() >= 3 {
            let noise: Vec<u8> = (0..IMAGE_LEN).map(|_| low_byte(next() >> 23)).collect();
            exercise(&noise);
        }

        iteration += 1;
        if !within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
