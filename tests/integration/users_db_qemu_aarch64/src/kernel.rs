//! Freestanding (`aarch64-unknown-none`) half of the `plans/PI.md` P11
//! users-database integration test.
//!
//! The device-agnostic bring-up *and* the users-database device tail
//! both live in the shared `rustos-test-virtio-qemu-support` crate
//! (`AGENTS.md` §2.2). This module supplies only what is unique to this
//! vertical: the bare virtio-blk MMIO device id, the spawner registering
//! the loaded image through the virtio-blk `register`, and the boot
//! harness. The device tail ([`users_db_load`]) mounts the planted
//! users-root rustfs volume and drives
//! `rustos_kernel_core::load_users_db` — the boot-time root-volume read
//! path for `/System/Security/Users` — then proves the parsed database
//! authenticates the planted account.

use rustos_drv_storage_virtio_blk::register as virtio_blk_register;
use rustos_test_virtio_qemu_support::{
    define_mmio_boot_harness_aarch64, run_virtio_mmio_scenario, users_db_load, FixedSpawner,
    ScenarioConfig, ScenarioTransport,
};

use crate::fixture::{DTB_BLOB, RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Bare virtio-blk MMIO device id (the `DeviceID` register value; over
/// MMIO this is the bare virtio device type, not the PCI `0x1040 + type`
/// encoding).
const VIRTIO_BLK_DEVICE_ID: u32 = 2;

/// Spawner registering every verified manifest through the virtio-blk driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_blk_register);

/// Drive the full virtio-blk-mmio bring-up, mount the planted users-root
/// volume, load and authenticate the users database, then report the
/// result through the ARM semihosting finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "users-db: scenario start",
    };
    run_virtio_mmio_scenario(
        VIRTIO_BLK_DEVICE_ID,
        DTB_BLOB,
        &cfg,
        users_db_load::<ScenarioTransport>,
    )
}

define_mmio_boot_harness_aarch64!(run_scenario);
