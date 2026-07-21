//! Freestanding (`x86_64-unknown-none`) half of the `plans/ARCHSUPPORT.md`
//! A2 users-database integration test.
//!
//! The device-agnostic virtio-PCI bring-up (boot harness, PCI walk, MSI-X
//! routing, per-device DMA pool, signed-`.rxe` load) *and* the
//! users-database device tail ([`users_db_load`]) both live in the shared
//! `tairix-test-virtio-qemu-support` crate — the tail is generic over the
//! transport, so this x86_64 vertical and the aarch64 MMIO vertical drive
//! one definition of the users-database load proof (`AGENTS.md` §2.2). This
//! module supplies only what is unique to this vertical: the modern
//! virtio-blk PCI device id, the spawner registering the loaded image
//! through the virtio-blk `register`, and the boot harness. The tail mounts
//! the planted users-root arxfs volume and drives
//! `tairix_kernel_core::load_users_db` — the boot-time root-volume read path
//! for `/System/Security/Users` — then proves the parsed database
//! authenticates the planted account and refuses a wrong password.

use tairix_drv_storage_virtio_blk::register as virtio_blk_register;
use tairix_test_virtio_qemu_support::{
    define_boot_harness, run_virtio_pci_scenario, users_db_load, FixedSpawner, ScenarioConfig,
    ScenarioTransport,
};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Modern virtio-blk PCI device id (`0x1040 + virtio-blk` = `0x1042`).
const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;

/// Spawner registering every verified manifest through the virtio-blk driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_blk_register);

/// Drive the full virtio-blk-pci bring-up, mount the planted users-root
/// volume, load and authenticate the users database, then exit through
/// QEMU's debug-exit device. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "users-db: scenario start",
    };
    run_virtio_pci_scenario(
        VIRTIO_BLK_DEVICE_ID,
        &cfg,
        users_db_load::<ScenarioTransport>,
    )
}

define_boot_harness!(run_scenario);
