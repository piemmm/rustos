//! The closed registry of soak filesystems and the per-target runner.

use std::time::{Duration, Instant};

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite};
use rustos_abi::DriverError;
use rustos_drv_fs_ext4::Ext4;
use rustos_drv_fs_fat32::Fat32;
use rustos_drv_fs_rustfs::RustFs;

use crate::{exercise, RamBlock};

/// The three filesystems the soak exercises, in registry order. The
/// single source of truth for `cargo xtask fssoak --list` and the
/// `soak.sh` fan-out, so neither hard-codes the list (§2.2).
pub const TARGETS: &[&str] = &["rustfs", "ext4", "fat32"];

/// A filesystem the soak can format on a [`RamBlock`] and remount,
/// reached only through the frozen [`FilesystemRead`]/[`FilesystemWrite`]
/// ABI thereafter. Implemented for the three first-party drivers; the
/// exerciser is written once against this trait (`AGENTS.md` §2.2).
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

impl SoakFs for RustFs<RamBlock> {
    fn format_volume(block: RamBlock) -> Result<Self, DriverError> {
        let inodes = inode_budget(block.len_bytes());
        RustFs::format(block, inodes)
    }

    fn remount(self) -> Result<Self, DriverError> {
        RustFs::open(self.into_block())
    }
}

impl SoakFs for Ext4<RamBlock> {
    fn format_volume(block: RamBlock) -> Result<Self, DriverError> {
        let inodes = inode_budget(block.len_bytes());
        Ext4::format(block, inodes)
    }

    fn remount(self) -> Result<Self, DriverError> {
        Ext4::open(self.into_block())
    }
}

impl SoakFs for Fat32<RamBlock> {
    fn format_volume(block: RamBlock) -> Result<Self, DriverError> {
        // FAT has no inode table; its directory entries live in the data
        // region, so the formatter takes no inode budget.
        Fat32::format(block)
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
        "rustfs" => run::<RustFs<RamBlock>>(device_bytes, budget_secs),
        "ext4" => run::<Ext4<RamBlock>>(device_bytes, budget_secs),
        "fat32" => run::<Fat32<RamBlock>>(device_bytes, budget_secs),
        other => Err(format!(
            "fssoak: unknown filesystem `{other}`; known: {}",
            TARGETS.join(", ")
        )),
    }
}

/// Drive [`exercise()`] for one filesystem until the budget elapses.
fn run<F: SoakFs>(device_bytes: u64, budget_secs: u64) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(budget_secs);
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
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
