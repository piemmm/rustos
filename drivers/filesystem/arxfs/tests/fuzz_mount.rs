//! Deterministic fuzz harness for the arxfs mount / metadata-decode path.
//!
//! [`ARXFS::open`] decodes a device's superblock ring, transaction root,
//! inode-tree nodes, per-file extent-tree nodes, and — since Stage 7 — the
//! chunk/refcount tree and the reverse-reference tree (mount rebuilds the
//! dedupe index from them, decoding every `ChunkRecord` and reverse-reference
//! record). All are self-identifying metadata (`header` / `superblock` /
//! `transaction` / `btree` / `dedupe`) read from a backing store that, on a
//! real system, may have been written by anything. The base image is
//! populated with several files, a multi-extent file, **duplicate-content
//! files, a reflink, and a symbolic link** so the sweep spends its time near
//! real inode-tree, extent-tree, chunk-tree, reverse-reference, and link
//! nodes, not just the superblock ring. Per that decode path is driven by a fuzz harness
//! whose single invariant is:
//!
//! * `open` never panics for any device contents — it returns `Ok` for a
//!   genuinely valid volume and `Err` (fail closed) for everything else.
//!
//! A mounted volume is then driven through the remaining decode paths the
//! "fuzz targets" list enumerates: the **directory-block decode** path
//! (`read_dir`/`lookup` decrypt and parse the encrypted dirent payload that
//! the mount-time free-space walk never reads), the **symbolic-link decode**
//! path (`read_link` over an inode of on-disk kind `3`, whose target is held
//! as node data and reached through the same integrity pipeline as file
//! bytes), the scrub-progress and health-baseline record decoders
//! (`scrub`/`health`), the offline `check` re-walk, and the read-only
//! `rescue` root scan. Each shares the same invariant: it returns a
//! `Result`, never panics, and fails closed.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded
//! LCG draws pseudo-random images, and a structured sweep flips bytes of a real
//! formatted image to hammer the block-identity checks (magic, type,
//! address, keyed authenticator). Stage 3 added the keyed metadata
//! authenticator and a redundant mirror copy of every metadata block, so the
//! single-byte sweep also exercises the authenticate-then-fall-back-to-the-
//! mirror path, and a dedicated **duplicated-copy sweep** corrupts *both*
//! copies of each block pair to hammer the both-copies-bad fail-closed path.
//!
//! Each `exercise` mounts and fully re-checks an encrypted volume (open, a
//! directory walk, scrub, the offline `check`, health, a reopen, and a raw
//! `rescue` scan), so it is orders of magnitude heavier than a byte decoder.
//! A plain `cargo test` — a developer machine and the per-PR `ci` gate, with no
//! budget — therefore runs a single quick smoke pass: a small, seed-driven
//! [`SMOKE_FLIP_SAMPLES`] sample of the single-byte sweep plus
//! [`SMOKE_ITERATIONS`] PRNG images, all from a fresh, logged seed. The
//! time-limited GitHub soak (`cargo xtask fuzz`) exports
//! `TAIRIX_FUZZ_BUDGET_SECS`, which switches the harness to its budgeted
//! coverage — byte positions are swept in a seeded full-coverage order until
//! the wall-clock budget elapses (the nightly budget flips every byte of the
//! image) and the PRNG loop runs to the same budget. The cheap, deterministic
//! both-copies-bad sweep runs in either mode.

use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, HealthSnapshot};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError};
use tairix_drv_fs_arxfs::{
    EntropySource, RescueSink, ScrubBudget, VolumeKey, ARXFS, VOLUME_KEY_LEN,
};
use tairix_log::{Event, Sink};

/// Capability set granting the scrub gate (`CAP_FS_MOUNT`).
struct AllCaps;
impl CapabilityQuery for AllCaps {
    fn holds(&self, _cap: CapabilityId) -> bool {
        true
    }
}

/// Log sink that discards the scrub's findings.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// Rescue sink that discards every recovered block: the harness only cares
/// that the rescue scan/extract decode path never panics for any device
/// contents.
struct NullRescueSink;
impl RescueSink for NullRescueSink {
    fn emit_block(&mut self, _inode: u32, _logical_block: u64, _size: u64, _data: &[u8]) {}
}

/// Volume key the fuzz image is formatted and reopened with. `ARXFS` is
/// encrypted-by-default (`docs/src/filesystem/arxfs-spec.md` §5); the fuzz
/// sweep exercises the encrypted-volume open path under this fixed key.
const FUZZ_KEY: VolumeKey = [0x5a; VOLUME_KEY_LEN];

/// Deterministic stand-in for the platform RNG seam used to format the fuzz
/// baseline image. Test scaffolding only, never a production entropy source.
struct FuzzEntropy {
    next: u8,
}

impl EntropySource for FuzzEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.next;
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

const BLOCK_SIZE: u32 = 512;
const BLOCK_COUNT: u64 = 64;
/// Device size in bytes. `64` is `BLOCK_COUNT`, kept as a `usize` literal so
/// the const needs no `u64`-to-`usize` cast.
const IMAGE_LEN: usize = BLOCK_SIZE as usize * 64;

/// PRNG-image count for the quick smoke pass a plain `cargo test` runs (no
/// budget set). Small on purpose: each iteration mounts and fully re-checks an
/// encrypted volume, so the exhaustive coverage belongs to the time-limited
/// soak, not the per-PR run.
const SMOKE_ITERATIONS: u64 = 512;

/// Number of seed-driven single-byte-flip positions the smoke pass samples from
/// the structured sweep. The soak sweeps positions to its wall-clock budget
/// instead.
const SMOKE_FLIP_SAMPLES: u64 = 256;

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
        if buf.is_empty() || !buf.len().is_multiple_of(bs) {
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
        if buf.is_empty() || !buf.len().is_multiple_of(bs) {
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

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        // Report telemetry so the persisted health baseline carries a real
        // snapshot and the sweep exercises the `HealthBaseline` snapshot
        // decode path (Stage 11), not just the `Unavailable` variant.
        Ok(DeviceHealth::Available(HealthSnapshot {
            power_on_hours: 42,
            unsafe_shutdowns: 1,
            media_errors: 1,
            reallocated_sectors: 0,
            pending_sectors: 0,
            uncorrectable_sectors: 0,
            crc_errors: 0,
            percentage_used: 10,
            available_spare: 100,
            temperature_kelvin: 300,
            critical_warning: false,
        }))
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

/// Drive the directory-block decode path on a mounted volume: walk every
/// reachable directory (bounded), decoding each block's encrypted dirent
/// payload through `read_dir` and resolving each decoded name through
/// `lookup`. Every call must return a `Result`, never panic, for any device
/// contents. The traversal is bounded by a hard
/// visit budget and a depth cap so a fuzzed image cannot drive it forever.
fn walk_directories(fs: &mut ARXFS<MemBlock>) {
    let mut name = [0u8; 256];
    let mut stack = vec![(fs.root(), 0u32)];
    let mut visits = 0u32;
    while let Some((dir, depth)) = stack.pop() {
        visits += 1;
        if visits > 4096 {
            break;
        }
        let mut cursor = 0u64;
        let mut steps = 0u32;
        let mut target = [0u8; 4096];
        while let Ok(Some(entry)) = fs.read_dir(dir, cursor, &mut name) {
            let len = entry.name_len.min(name.len());
            let _ = fs.lookup(dir, &name[..len]);
            match entry.info.kind {
                NodeKind::Directory if depth < 8 => stack.push((entry.node, depth + 1)),
                // Drive the link decode path: a fuzzed inode may claim any
                // kind and any target length, and `read_link` must answer
                // with a `Result` for every one of them.
                NodeKind::Symlink => {
                    let _ = fs.read_link(entry.node, &mut target);
                }
                NodeKind::Directory | NodeKind::RegularFile => {}
            }
            // A fuzzed image may hand back any cursor; a non-advancing one
            // would loop forever, and the step budget bounds the rest.
            if entry.next_cursor == cursor {
                break;
            }
            cursor = entry.next_cursor;
            steps += 1;
            if steps > 65_536 {
                break;
            }
        }
    }
}

/// The single invariant: opening an arbitrary image must return a `Result`,
/// never panic. A successful mount must additionally survive being reopened.
fn exercise(image: &[u8]) {
    let mut store = image.to_vec();
    store.resize(IMAGE_LEN, 0);
    let dev = MemBlock { store };
    if let Ok(mut fs) = ARXFS::open(dev, &FUZZ_KEY) {
        // Drive the directory-block decode path ('s "directory decode"
        // target): list the root directory and resolve every decoded name.
        // `read_dir`/`lookup` decrypt and parse the directory block's dirent
        // payload (the encrypted directory record), which `open`'s
        // free-space walk never reads. Like `open` it must return a `Result`,
        // never panic, for any device contents.
        walk_directories(&mut fs);
        // Drive the Stage-8 scrub-progress decode path: a bounded scrub reads
        // and decodes any persisted scrub-progress record (`load_scrub_progress`)
        // before resuming. Like `open` it must return a `Result`, never panic,
        // for any device contents.
        let _ = fs.scrub(&AllCaps, &NullSink, ScrubBudget::Inodes(1));
        // Drive the Stage-9 offline check on a mounted handle: it re-walks
        // every tree, rebuilds the derived state, and reconciles refcounts. It
        // too must return a `Result`, never panic.
        let _ = fs.check(&AllCaps, &NullSink);
        // Drive the Stage-11 health pass: it decodes any persisted
        // `HealthBaseline` record before classifying and (possibly) triggering
        // a scrub. Like `open` it must return a `Result`, never panic, for any
        // device contents.
        let _ = fs.health(&AllCaps, &NullSink);
        // A volume that mounts must mount again from its own bytes.
        let bytes = fs.into_block().store;
        let _ = ARXFS::open(MemBlock { store: bytes }, &FUZZ_KEY);
    }
    // Drive the Stage-9 rescue decode path on the raw image, which does not
    // require a mountable volume: it scans every block for a self-identifying
    // transaction root (`TxnRoot::decode_any`) and runs the recovered
    // inode/extent metadata through the integrity pipeline. Like `open` it must
    // return a `Result`, never panic, for any device contents.
    let mut rescue_store = image.to_vec();
    rescue_store.resize(IMAGE_LEN, 0);
    let _ = ARXFS::rescue(
        MemBlock {
            store: rescue_store,
        },
        &FUZZ_KEY,
        &AllCaps,
        &NullSink,
        &mut NullRescueSink,
    );
}

/// A real formatted image, populated with several inodes (so the inode tree
/// splits past a single node) and a file with two non-adjacent blocks (so an
/// extent-tree node exists). Both the PRNG and the structured sweep then spend
/// most of their time near genuinely valid tree metadata rather than pure
/// noise. Populating stops at the first `NoSpace` on the tiny fuzz device.
fn formatted_image() -> Vec<u8> {
    let mut fs = ARXFS::format(
        MemBlock {
            store: vec![0u8; IMAGE_LEN],
        },
        16,
        &FUZZ_KEY,
        &mut FuzzEntropy { next: 1 },
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
    // A symbolic link puts an inode of the new on-disk kind — and the data
    // blocks holding its target — in the sweep's path, and makes the base
    // image declare the symlink incompatible-feature bit so the sweep also
    // hammers the superblock feature-word validation. Best-effort on the
    // tiny fuzz device.
    let _ = fs.create_link(root, b"alink", b"/f0");
    // Leave a scrub paused mid-pass so the base image carries a persisted
    // scrub-progress record (Stage 8): the sweep then hammers that on-disk
    // decode path too. Best-effort on the tiny fuzz device.
    let _ = fs.scrub(&AllCaps, &NullSink, ScrubBudget::Inodes(1));
    fs.into_block().store
}

#[test]
fn open_never_panics_on_arbitrary_images() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let base = formatted_image();

    // Draw and log the seed up front so every sampled byte position and every
    // PRNG image below replays exactly from the logged value:
    // fresh per run, fresh per soak run under `cargo xtask fuzz`.
    let mut state: u64 = tairix_fuzzseed::start(
        "open_never_panics_on_arbitrary_images",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    // Structured single-byte sweep over a valid image, probing the
    // identity/checksum rejection on a near-valid image. The soak visits
    // positions in a seeded full-coverage order and stops at the wall-clock
    // deadline — the nightly budget flips every byte, a short budget probes a
    // reproducible spread on time. A plain `cargo test` (no budget) samples a
    // small, seed-driven subset so the per-PR run stays quick — each
    // `exercise` is a full encrypted mount + re-check, far heavier than a
    // byte decoder.
    if let Some(deadline) = deadline {
        tairix_fuzzseed::budgeted_sweep(base.len(), next(), deadline, |i| {
            let mut image = base.clone();
            image[i] ^= 0xff;
            exercise(&image);
        });
    } else {
        for _ in 0..SMOKE_FLIP_SAMPLES {
            let mut image = base.clone();
            let i = index(next(), base.len());
            image[i] ^= 0xff;
            exercise(&image);
        }
    }

    // Duplicated-copy sweep (Stage 3): every metadata block is mirrored at the
    // adjacent block, and `open` falls back to — and repairs from — the mirror
    // when one copy fails the keyed authenticator. Corrupt the keyed-tag byte
    // of BOTH a block and its companion so neither copy authenticates,
    // exercising the both-copies-bad path. `open` must still return a
    // `Result`, never panic. One image per block, so
    // it is cheap enough to run in either mode.
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

    // PRNG sweep: an LCG mutates the valid image at random offsets. The smoke
    // pass does SMOKE_ITERATIONS images; the soak loops the continuing stream
    // until the wall-clock budget elapses.
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
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
