//! The closed registry of soak filesystems and the per-target runner.

use std::time::{Duration, Instant};

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite};
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, VOLUME_KEY_LEN};
use tairix_drv_fs_ext4::Ext4;
use tairix_drv_fs_fat32::Fat32;

use crate::{exercise, random_exercise, RamBlock};

/// Volume key the soak formats and remounts arxfs with. `ARXFS` is
/// encrypted-by-default (`docs/src/filesystem/arxfs-spec.md` §5), so the
/// soak exercises the encrypted-volume path under this fixed key.
const ARXFS_SOAK_KEY: VolumeKey = [0xa5; VOLUME_KEY_LEN];

/// Volume identity the soak stamps onto its ext4 volumes: deterministic
/// (the soak is reproducible), non-nil (the formatter refuses the reserved
/// all-zero identity).
const SOAK_EXT4_UUID: [u8; 16] = [0x5A; 16];

/// Deterministic stand-in for the platform RNG seam: a byte counter that gives
/// `ARXFS::format` distinct, reproducible key material and UUID. Soak
/// scaffolding only, never a production entropy source.
struct SoakEntropy {
    next: u8,
}

impl EntropySource for SoakEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.next;
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

/// The soak targets, in registry order. The single source of truth for
/// `cargo xtask fssoak --list` and the `soak.sh` fan-out, so neither
/// hard-codes the list.
///
/// `arxfs`/`ext4`/`fat32` run the fixed-sequence [`exercise()`];
/// `arxfs-random` runs the randomized, model-checked [`random_exercise`]
/// over arxfs, taking a different path on every launch. Both draw a fresh
/// start seed each launch and log it, so every run differs
/// and any failure replays from the logged seed.
pub const TARGETS: &[&str] = &["arxfs", "ext4", "fat32", "arxfs-random"];

/// A filesystem the soak can format on a [`RamBlock`] and remount,
/// reached only through the frozen [`FilesystemRead`]/[`FilesystemWrite`]
/// ABI thereafter. Implemented for the three first-party drivers; the
/// exerciser is written once against this trait.
pub trait SoakFs: FilesystemRead + FilesystemWrite + Sized {
    /// Lay a fresh, empty volume onto `block` and return it mounted.
    ///
    /// # Errors
    /// Propagates the driver's formatter error (e.g. a device too small
    /// for the filesystem).
    fn format_volume(block: RamBlock) -> Result<Self, DriverError>;

    /// Unmount and remount the same backing device, so the soak can
    /// re-verify that committed state survives a fresh `open()`.
    ///
    /// # Errors
    /// Propagates the driver's `open` error.
    fn remount(self) -> Result<Self, DriverError>;
}

/// Minimum-total inode budget for an `inode`-based filesystem on a
/// device of `bytes`: roughly one inode per 256 KiB, bounded so a tiny
/// volume still has a usable count and a huge one stays addressable.
fn inode_budget(bytes: u64) -> u32 {
    let raw = (bytes / (256 * 1024)).clamp(256, 200_000);
    u32::try_from(raw).unwrap_or(200_000)
}

impl SoakFs for ARXFS<RamBlock> {
    fn format_volume(block: RamBlock) -> Result<Self, DriverError> {
        let inodes = inode_budget(block.len_bytes());
        ARXFS::format(block, inodes, &ARXFS_SOAK_KEY, &mut SoakEntropy { next: 1 })
    }

    fn remount(self) -> Result<Self, DriverError> {
        ARXFS::open(self.into_block(), &ARXFS_SOAK_KEY)
    }
}

impl SoakFs for Ext4<RamBlock> {
    fn format_volume(block: RamBlock) -> Result<Self, DriverError> {
        let inodes = inode_budget(block.len_bytes());
        Ext4::format(block, inodes, SOAK_EXT4_UUID)
    }

    fn remount(self) -> Result<Self, DriverError> {
        Ext4::open(self.into_block())
    }
}

impl SoakFs for Fat32<RamBlock> {
    fn format_volume(block: RamBlock) -> Result<Self, DriverError> {
        // FAT has no inode table; its directory entries live in the data
        // region, so the formatter takes no inode budget. The serial is a
        // fixed test value: soak volumes never meet the volume forest.
        Fat32::format(block, 0x50A5_FA32)
    }

    fn remount(self) -> Result<Self, DriverError> {
        Fat32::open(self.into_block())
    }
}

/// Run the named filesystem's soak for `budget_secs` wall-clock seconds
/// on a `device_bytes` RAM volume. A budget of zero runs exactly one
/// iteration (the smoke pass); otherwise iterations repeat with fresh
/// seeds until the budget elapses.
///
/// # Errors
/// Returns a descriptive error (including the failing seed) when the
/// filesystem is unknown or the exerciser finds an inconsistency.
pub fn run_target(name: &str, device_bytes: u64, budget_secs: u64) -> Result<(), String> {
    match name {
        "arxfs" => run::<ARXFS<RamBlock>>(name, device_bytes, budget_secs),
        "ext4" => run::<Ext4<RamBlock>>(name, device_bytes, budget_secs),
        "fat32" => run::<Fat32<RamBlock>>(name, device_bytes, budget_secs),
        "arxfs-random" => run_random::<ARXFS<RamBlock>>(name, device_bytes, budget_secs),
        other => Err(format!(
            "fssoak: unknown filesystem `{other}`; known: {}",
            TARGETS.join(", ")
        )),
    }
}

/// Environment variable that pins the soak start seed for replay; unset
/// draws a fresh seed each launch (`tairix_fuzzseed`).
const FSSOAK_SEED_ENV: &str = "TAIRIX_FSSOAK_SEED";

/// Resolve and log this launch's *start* seed for `target`.
///
/// Fresh from host entropy by default — so the run takes a different path on
/// every launch (the issue's core requirement) — or pinned by
/// `TAIRIX_FSSOAK_SEED` to replay a failure exactly. Logged at the start so a
/// fresh-seed failure is still reproducible.
fn start_seed(target: &str) -> u64 {
    let seed = tairix_fuzzseed::resolve_seed(FSSOAK_SEED_ENV);
    println!(
        "fssoak {target}: start seed {seed} ({seed:#018x}); \
         replay with {FSSOAK_SEED_ENV}={seed}"
    );
    seed
}

/// Drive [`random_exercise`] for one filesystem until the budget elapses,
/// from a fresh, logged start seed.
fn run_random<F: SoakFs>(target: &str, device_bytes: u64, budget_secs: u64) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(budget_secs);
    let mut seed = start_seed(target);
    loop {
        random_exercise::<F>(device_bytes, seed)?;
        // SplitMix64-style advance: deterministic, full-period.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        if budget_secs == 0 || Instant::now() >= deadline {
            break;
        }
    }
    Ok(())
}

/// Drive [`exercise()`] for one filesystem until the budget elapses, from a
/// fresh, logged start seed (only the content bytes vary by seed, so each
/// launch exercises the fixed op sequence over different data).
fn run<F: SoakFs>(target: &str, device_bytes: u64, budget_secs: u64) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(budget_secs);
    let mut seed = start_seed(target);
    loop {
        exercise::<F>(device_bytes, seed)?;
        // SplitMix64-style seed advance: deterministic, full-period.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        if budget_secs == 0 || Instant::now() >= deadline {
            break;
        }
    }
    Ok(())
}
