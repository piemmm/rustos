//! Freestanding (`aarch64-unknown-none`) half of the virtio-blk-mmio
//! integration test.
//!
//! The device-agnostic bring-up *and* the virtio-blk round-trip tail both
//! live in the shared `tairix-test-virtio-qemu-support` crate. This module supplies only what is unique to this
//! vertical: the bare virtio-blk MMIO device id, the spawner registering the
//! loaded image through the virtio-blk `register`, and the boot harness. The
//! device tail ([`virtio_blk_round_trip`]) is the same code the riscv64
//! MMIO and x86_64 PCI verticals run.

use tairix_drv_storage_virtio_blk::register as virtio_blk_register;
use tairix_test_virtio_qemu_support::{
    define_mmio_boot_harness_aarch64, run_virtio_mmio_scenario, virtio_blk_round_trip,
    FixedSpawner, ScenarioConfig, ScenarioTransport,
};

use crate::fixture::{DTB_BLOB, RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Bare virtio-blk MMIO device id (the `DeviceID` register value; over
/// MMIO this is the bare virtio device type, not the PCI `0x1040 + type`
/// encoding).
const VIRTIO_BLK_DEVICE_ID: u32 = 2;

/// Spawner registering every verified manifest through the virtio-blk driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_blk_register);

/// Drive the full virtio-blk-mmio round-trip and report the result
/// through the ARM semihosting finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "virtio-blk-mmio: scenario start",
    };
    run_virtio_mmio_scenario(
        VIRTIO_BLK_DEVICE_ID,
        DTB_BLOB,
        &cfg,
        virtio_blk_round_trip::<ScenarioTransport>,
    )
}

define_mmio_boot_harness_aarch64!(run_scenario);
