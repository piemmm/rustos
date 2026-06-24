//! In-RAM filesystem soak harness (`.junie/filesystems.md`).
//!
//! Formats a RAM-backed [`RamBlock`] device with each first-party
//! formatter (`rustfs`/`ext4`/`fat32`) and drives one generic,
//! filesystem-agnostic [`exercise()`] body over the frozen
//! `FilesystemRead`/`FilesystemWrite` ABI: create/write/read-back/
//! truncate/remove round-trips across nested directories with a remount
//! re-verification, plus the fail-closed extremes — `NoSpace`
//! on a full data region, `Busy` on a duplicate create or a
//! non-empty `rmdir`, and `LengthOutOfRange` on an empty or
//! oversize name. There is no `mkfs` shell-out and no parallel
//! re-implementation of any filesystem semantics: the
//! harness consumes the OS's own formatters and read/write paths and
//! asserts the OS reports the extremes cleanly.
//!
//! The exerciser drives a small LCG from a per-launch start seed (fresh from
//! host entropy by default, or pinned by `RUSTOS_FSSOAK_SEED`). The start seed
//! is logged at the start of each run, so every launch exercises different
//! content yet any failure reproduces from its logged seed.
//!
//! Alongside that fixed-sequence body, [`random_exercise`] drives a
//! genuinely *randomized*, model-checked op mix (create/move/delete/
//! extend/truncate in a different order every run) against a byte-exact
//! oracle, registered as the `rustfs-random` soak target so it runs in
//! parallel with the others (`tools/ci/soak.sh`).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod exercise;
mod ramblock;
mod random;
mod registry;

pub use exercise::exercise;
pub use ramblock::RamBlock;
pub use random::random_exercise;
pub use registry::{run_target, SoakFs, TARGETS};

/// Minimum soak device size, in bytes: 1 GiB (`.junie/filesystems.md`).
pub const MIN_DEVICE_BYTES: u64 = 1024 * 1024 * 1024;

/// Logical-block (sector) size of the RAM device, in bytes. 4096 keeps
/// the three filesystems on their large-block paths, so a 1 GiB fill is
/// a tractable number of blocks.
pub const SECTOR_BYTES: u32 = 4096;
