//! Deterministic fuzz harness for the ADFS mount / decode path.
//!
//! [`Adfs::open`] identifies and validates whichever ADFS variant a
//! device carries — the old free-space map, a bare E-class disc record,
//! or a boot block — and the read surface then decodes directories
//! (fixed and big), the allocation map, and per-entry metadata, all
//! from a backing store that, on a real system, may have been written
//! by anything. The single invariant:
//!
//! * `open` never panics for any device contents — it returns `Ok` for
//!   a genuinely valid volume and `Err` (fail closed) for everything
//!   else — and a volume that mounts is walked (directories, lookups,
//!   attributes, stats) without panicking either.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG draws
//! pseudo-random images, and a structured sweep flips bytes of real
//! formatted images — one per map flavour (old map, new map, big
//! directories) — to hammer the checksum and structural validation.
//! A plain `cargo test` runs a quick seed-driven smoke sample; the
//! time-limited soak (`TAIRIX_FUZZ_BUDGET_SECS`) sweeps byte positions
//! in a seeded full-coverage order until the wall-clock budget elapses
//! (the nightly budget flips every byte of every base image) and runs
//! the PRNG stream to the same budget.

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::filesystem::{
    FilesystemAttrs, FilesystemRead, FilesystemStats, FilesystemWrite, NodeKind,
};
use tairix_abi::DriverError;
use tairix_drv_fs_adfs::{Adfs, AdfsVariant};

const BLOCK_SIZE: u32 = 512;

/// The smallest volume of each map flavour keeps the sweep affordable.
const BASES: [(AdfsVariant, usize); 3] = [
    (AdfsVariant::S, 160 * 1024),
    (AdfsVariant::E, 800 * 1024),
    (AdfsVariant::EPlus, 800 * 1024),
];

/// PRNG-image count for the quick smoke pass (no budget set).
const SMOKE_ITERATIONS: u64 = 128;

/// Seed-driven single-byte-flip samples per base image in the smoke
/// pass; the soak sweeps positions to its wall-clock budget instead.
const SMOKE_FLIP_SAMPLES: u64 = 128;

/// In-RAM device over a byte image.
struct MemBlock {
    store: Vec<u8>,
}

impl Block for MemBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BLOCK_SIZE,
            block_count: (self.store.len() as u64) / u64::from(BLOCK_SIZE),
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

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..len` as a `usize`, without a narrowing `as` cast.
fn index(x: u64, len: usize) -> usize {
    usize::try_from(x % len as u64).unwrap_or(0)
}

/// Walk every reachable directory (bounded), resolving each decoded
/// name and probing the metadata surfaces. Every call must return a
/// `Result`, never panic, for any device contents; the visit and depth
/// budgets stop a fuzzed image driving the walk forever.
fn walk(fs: &mut Adfs<MemBlock>) {
    let mut name = [0u8; 256];
    let mut stack = vec![(fs.root(), 0u32)];
    let mut visits = 0u32;
    while let Some((dir, depth)) = stack.pop() {
        visits += 1;
        if visits > 1024 {
            break;
        }
        let mut cursor = 0u64;
        let mut steps = 0u32;
        while let Ok(Some(entry)) = fs.read_dir(dir, cursor, &mut name) {
            let len = entry.name_len.min(name.len());
            let _ = fs.lookup(dir, &name[..len]);
            let _ = fs.node_info(entry.node);
            let _ = fs.get_attr(entry.node, b"acorn.attr", &mut [0u8; 16]);
            if matches!(entry.info.kind, NodeKind::Directory) && depth < 8 {
                stack.push((entry.node, depth + 1));
            }
            // A fuzzed image may hand back any cursor; a non-advancing
            // one would loop forever, and the step budget bounds the rest.
            if entry.next_cursor == cursor {
                break;
            }
            cursor = entry.next_cursor;
            steps += 1;
            if steps > 16_384 {
                break;
            }
        }
    }
}

/// The single invariant: opening an arbitrary image returns a
/// `Result`, never panics; a mounted volume is walked and re-opened.
fn exercise(image: &[u8]) {
    let dev = MemBlock {
        store: image.to_vec(),
    };
    if let Ok(mut fs) = Adfs::open(dev) {
        walk(&mut fs);
        let _ = fs.stats();
    }
}

/// A populated base image of `variant`: files, a nested directory, and
/// typed metadata, so the sweeps spend their time near genuinely valid
/// structures rather than pure noise.
fn formatted_image(variant: AdfsVariant, bytes: usize) -> Vec<u8> {
    let mut fs = Adfs::format(
        MemBlock {
            store: vec![0u8; bytes],
        },
        variant,
    )
    .expect("format a blank fuzz device");
    let root = fs.root();
    fs.create(root, b"Sub", NodeKind::Directory)
        .expect("create dir");
    let sub = fs.lookup(root, b"Sub").expect("dir resolves");
    for (dir, name) in [(root, &b"one"[..]), (root, b"two"), (sub, b"three")] {
        fs.create(dir, name, NodeKind::RegularFile)
            .expect("create file");
        fs.write_at(dir, name, 0, &[0xA5u8; 700]).expect("write");
    }
    let node = fs.lookup(root, b"one").expect("file resolves");
    fs.set_attr(node, b"acorn.filetype", b"fff")
        .expect("type the file");
    fs.into_device().store
}

#[test]
fn fuzz_adfs_mount() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // Draw and log the seed up front so every sampled byte position and
    // every PRNG image below replays exactly from the logged value.
    let mut state: u64 = tairix_fuzzseed::start("fuzz_adfs_mount", tairix_fuzzseed::FUZZ_SEED_ENV);
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    for (variant, bytes) in BASES {
        let base = formatted_image(variant, bytes);
        // Structured single-byte sweep over a valid image, probing the
        // checksum/structure rejection near genuinely valid data. The soak
        // visits positions in a seeded full-coverage order and stops at the
        // wall-clock deadline — the nightly budget flips every byte, a short
        // budget probes a reproducible spread on time — while the smoke pass
        // samples a fixed number of positions.
        if let Some(deadline) = deadline {
            tairix_fuzzseed::budgeted_sweep(base.len(), next(), deadline, |i| {
                let mut image = base.clone();
                image[i] ^= 0xFF;
                exercise(&image);
            });
        } else {
            for _ in 0..SMOKE_FLIP_SAMPLES {
                let mut image = base.clone();
                let i = index(next(), base.len());
                image[i] ^= 0xFF;
                exercise(&image);
            }
        }

        // PRNG sweep: multi-byte mutations of the valid image, plus the
        // occasional pure-noise image.
        let mut iteration: u64 = 0;
        loop {
            let mut image = base.clone();
            let flips = 1 + index(next(), 23);
            for _ in 0..flips {
                let pos = index(next(), image.len());
                image[pos] = low_byte(next() >> 17);
            }
            exercise(&image);

            if (next() >> 5).trailing_zeros() >= 3 {
                let noise: Vec<u8> = (0..64 * 1024).map(|_| low_byte(next() >> 23)).collect();
                exercise(&noise);
            }

            iteration += 1;
            if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
                break;
            }
        }
    }
}
