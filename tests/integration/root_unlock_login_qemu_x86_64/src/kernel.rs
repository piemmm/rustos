//! Freestanding (`x86_64-unknown-none`) half of the `plans/ARCHSUPPORT.md`
//! A2 root-mount->login integration test.
//!
//! The device-agnostic virtio-PCI bring-up (boot harness, PCI walk, MSI-X
//! routing, per-device DMA pool, signed-`.rxe` load) *and* the unlock tail
//! ([`root_unlock_login`]) both live in the shared
//! `tairix-test-virtio-qemu-support` crate — the tail is generic over the
//! transport, so this x86_64 vertical and the aarch64 MMIO vertical drive one
//! definition of the unlock-policy proof. This module supplies only what is
//! unique to this vertical: the modern virtio-blk PCI device id, the spawner
//! registering the loaded image through the virtio-blk `register`, and the boot
//! harness.

use tairix_drv_storage_virtio_blk::register as virtio_blk_register;
use tairix_test_virtio_qemu_support::{
    define_boot_harness, root_unlock_login, run_virtio_pci_scenario, FixedSpawner, ScenarioConfig,
    ScenarioTransport,
};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Modern virtio-blk PCI device id (`0x1040 + virtio-blk` = `0x1042`).
const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;

/// Spawner registering every verified manifest through the virtio-blk driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_blk_register);

/// Drive the full virtio-blk-pci bring-up, then the shared interactive root
/// unlock + login proof, and exit through QEMU's debug-exit device. Never
/// returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "root-unlock: scenario start",
    };
    run_virtio_pci_scenario(
        VIRTIO_BLK_DEVICE_ID,
        &cfg,
        root_unlock_login::<ScenarioTransport>,
    )
}

define_boot_harness!(run_scenario);
