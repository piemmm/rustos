//! The closed registry of soak filesystems and the per-target runner.

use std::time::{Duration, Instant};

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite};
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, VOLUME_KEY_LEN};
use tairix_drv_fs_ext4::Ext4;
use tairix_drv_fs_fat32::Fat32;
use tairix_fuzzseed::FSSOAK_SEED_ENV;

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

/// Run the named filesystem's soak on a `device_bytes` RAM volume until
/// `deadline` passes. `None` — a plain `cargo test` — runs exactly one pass
/// (the smoke iteration); otherwise passes repeat with fresh seeds.
///
/// # Errors
/// Returns a descriptive error (including the failing seed) when the
/// filesystem is unknown or the exerciser finds an inconsistency.
pub fn run_target(name: &str, device_bytes: u64, deadline: Option<Instant>) -> Result<(), String> {
    match name {
        "arxfs" => soak(name, device_bytes, deadline, exercise::<ARXFS<RamBlock>>),
        "ext4" => soak(name, device_bytes, deadline, exercise::<Ext4<RamBlock>>),
        "fat32" => soak(name, device_bytes, deadline, exercise::<Fat32<RamBlock>>),
        "arxfs-random" => soak(
            name,
            device_bytes,
            deadline,
            random_exercise::<ARXFS<RamBlock>>,
        ),
        other => Err(format!(
            "fssoak: unknown filesystem `{other}`; known: {}",
            TARGETS.join(", ")
        )),
    }
}

/// Drive one `pass` — a whole [`exercise()`] or [`random_exercise`] round over
/// a freshly formatted volume — repeatedly until the budget runs out, from a
/// fresh, logged start seed.
///
/// The start seed is fresh from host entropy by default, so the run takes a
/// different path on every launch, and each pass advances it, so a long soak
/// never replays one stream. Pinning [`FSSOAK_SEED_ENV`] replays a logged
/// failure exactly.
///
/// The exerciser is a function pointer rather than a second copy of this loop
/// per target: the budget arithmetic is the easy thing to get subtly different
/// in two places.
fn soak(
    target: &str,
    device_bytes: u64,
    deadline: Option<Instant>,
    pass: fn(u64, u64) -> Result<(), String>,
) -> Result<(), String> {
    let mut seed = tairix_fuzzseed::start(target, FSSOAK_SEED_ENV);
    loop {
        let began = Instant::now();
        pass(device_bytes, seed)?;
        // SplitMix64-style advance: deterministic, full-period.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        if !room_for_another_pass(deadline, began.elapsed()) {
            return Ok(());
        }
    }
}

/// Whether `deadline` still leaves room for another pass expected to take
/// about `pass`; always `false` without a deadline, so a plain `cargo test`
/// runs a single pass.
///
/// A pass formats and fills a whole volume, so it is far too coarse to start
/// one that cannot finish in time: checking only that the deadline has not yet
/// passed overruns the budget by a full pass every run, and it is then the
/// orchestrator's hung-child deadline — sized for building and starting the
/// binary, not for another pass — that ends the soak.
fn room_for_another_pass(deadline: Option<Instant>, pass: Duration) -> bool {
    matches!(deadline, Some(end) if end.saturating_duration_since(Instant::now()) > pass)
}

#[cfg(test)]
mod tests {
    use super::{room_for_another_pass, run_target, TARGETS};
    use std::time::{Duration, Instant};

    #[test]
    fn no_deadline_runs_a_single_pass() {
        assert!(!room_for_another_pass(None, Duration::ZERO));
    }

    #[test]
    fn a_pass_that_would_not_finish_in_time_is_not_started() {
        let deadline = Instant::now() + Duration::from_secs(10);
        assert!(
            !room_for_another_pass(Some(deadline), Duration::from_secs(30)),
            "a 30 s pass must not start with 10 s of budget left"
        );
        assert!(room_for_another_pass(
            Some(deadline),
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn an_elapsed_deadline_stops_the_soak() {
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("an instant one second in the past exists");
        assert!(!room_for_another_pass(Some(past), Duration::ZERO));
    }

    #[test]
    fn an_unknown_filesystem_fails_closed() {
        let err = run_target("zfs", 1024, None).expect_err("unknown target must not run");
        for known in TARGETS {
            assert!(err.contains(known), "the error should list {known}: {err}");
        }
    }
}
