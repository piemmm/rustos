//! Per-filesystem soak entry points.
//!
//! Each `#[test]` runs the shared exerciser for one filesystem. A plain
//! `cargo test` runs a single smoke iteration on a modest device; the
//! nightly soak (`cargo xtask fssoak`) sets `RUSTOS_FSSOAK_BUDGET_SECS`
//! and `RUSTOS_FSSOAK_BYTES` (≥ 1 GiB) to loop under a wall-clock budget
//! on a full-size RAM volume, mirroring the proptest harness's env seam.

use std::env;

use rustos_test_fs_soak::{run_target, TARGETS};

/// Wall-clock budget per target; zero (the default) runs one iteration.
fn budget_secs() -> u64 {
    env::var("RUSTOS_FSSOAK_BUDGET_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// RAM device size; defaults to a 320 MiB smoke for a plain `cargo
/// test`. That clears FAT32's ~256 MiB floor (≥ 65525 clusters at the
/// 4096-byte sector) while still giving ext4 two block groups. The soak
/// orchestrator overrides it with ≥ 1 GiB.
fn device_bytes() -> u64 {
    env::var("RUSTOS_FSSOAK_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(320 * 1024 * 1024)
}

fn run(name: &str) {
    if let Err(e) = run_target(name, device_bytes(), budget_secs()) {
        panic!("fssoak {name} failed: {e}");
    }
}

#[test]
fn soak_arxfs() {
    run("arxfs");
}

#[test]
fn soak_ext4() {
    run("ext4");
}

#[test]
fn soak_fat32() {
    run("fat32");
}

/// The randomized, model-checked arxfs soak: a different operation path
/// on every launch, run in parallel with the others under `soak.sh`.
#[test]
fn soak_arxfs_random() {
    run("arxfs-random");
}

#[test]
fn registry_lists_every_soak_target() {
    assert_eq!(TARGETS, &["arxfs", "ext4", "fat32", "arxfs-random"]);
}
