//! Freestanding (`x86_64-unknown-none`) half of the virtio-blk-pci
//! integration test.
//!
//! The device-agnostic bring-up *and* the virtio-blk round-trip tail both
//! live in the shared `rustos-test-virtio-qemu-support` crate. This module supplies only what is unique to this
//! vertical: the modern virtio-blk PCI device id, the spawner registering
//! the loaded image through the virtio-blk `register`, and the boot harness.
//! The device tail ([`virtio_blk_round_trip`]) is the same code the
//! riscv64 MMIO vertical runs.

use rustos_drv_storage_virtio_blk::register as virtio_blk_register;
use rustos_test_virtio_qemu_support::{
    define_boot_harness, run_virtio_pci_scenario, virtio_blk_round_trip, FixedSpawner,
    ScenarioConfig, ScenarioTransport,
};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Modern virtio-blk PCI device id (`0x1040 + virtio-blk`).
const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;

/// Spawner registering every verified manifest through the virtio-blk driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_blk_register);

/// Drive the full virtio-blk-pci round-trip and exit through QEMU's
/// debug-exit device. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "virtio-blk-pci: scenario start",
    };
    run_virtio_pci_scenario(
        VIRTIO_BLK_DEVICE_ID,
        &cfg,
        virtio_blk_round_trip::<ScenarioTransport>,
    )
}

define_boot_harness!(run_scenario);
