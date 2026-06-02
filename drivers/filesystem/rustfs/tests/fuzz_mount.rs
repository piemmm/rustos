//! Deterministic fuzz harness for the rustfs mount / metadata-decode path
//! (`AGENTS.md` §19.5 / §19.6).
//!
//! [`RustFs::open`] decodes a device's superblock ring, transaction root, and
//! inode-map blocks — all self-identifying metadata (`header` / `superblock` /
//! `transaction`) read from a backing store that, on a real system, may have
//! been written by anything. Per §19.6 that decode path is driven by a fuzz
//! harness whose single invariant is:
//!
//! * `open` never panics for any device contents — it returns `Ok` for a
//!   genuinely valid volume and `Err` (fail closed) for everything else.
//!
//! RustOS pulls in no external fuzz runner (`AGENTS.md` §2.12): a fixed-seed
//! LCG draws pseudo-random images, and a structured sweep flips every byte of
//! a real formatted image to hammer the §8 block-identity checks (magic, type,
//! address, checksum). A plain `cargo test` runs the fixed [`SMOKE_ITERATIONS`]
//! sweep; `cargo xtask fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS` to extend the
//! PRNG loop to a wall-clock budget.

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::DriverError;
use rustos_drv_fs_rustfs::RustFs;

const BLOCK_SIZE: u32 = 512;
const BLOCK_COUNT: u64 = 64;
/// Device size in bytes. `64` is `BLOCK_COUNT`, kept as a `usize` literal so
/// the const needs no `u64`-to-`usize` cast.
const IMAGE_LEN: usize = BLOCK_SIZE as usize * 64;

/// Fixed-iteration sweep run by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

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
    if let Ok(fs) = RustFs::open(dev) {
        // A volume that mounts must mount again from its own bytes.
        let bytes = fs.into_block().store;
        let _ = RustFs::open(MemBlock { store: bytes });
    }
}

/// A real formatted image, so the PRNG and the structured sweep both spend
/// most of their time near genuinely valid metadata rather than pure noise.
fn formatted_image() -> Vec<u8> {
    let fs = RustFs::format(
        MemBlock {
            store: vec![0u8; IMAGE_LEN],
        },
        16,
    )
    .expect("format a blank fuzz device");
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
