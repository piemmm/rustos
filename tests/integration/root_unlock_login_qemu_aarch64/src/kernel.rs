//! Freestanding (`aarch64-unknown-none`) half of the `plans/PI.md` P11
//! Chunk B-2 root-mount->login integration test.
//!
//! The device-agnostic bring-up (boot harness, DTB MMIO walk, GICv2 + EL1
//! IRQ wiring, static DMA pool, signed-`.rxe` load) *and* the unlock tail
//! itself both live in the shared `tairix-test-virtio-qemu-support` crate:
//! the tail ([`root_unlock_login`]) is generic over the transport, so the
//! aarch64 virtio-MMIO and x86_64 virtio-PCI verticals drive one
//! definition of the unlock policy proof rather than two sibling copies
//! (`AGENTS.md` §2.2). This module supplies only what is unique to this
//! vertical: the bare virtio-blk device id, the spawner registering the
//! loaded image through the virtio-blk `register`, and the boot harness.

use tairix_drv_storage_virtio_blk::register as virtio_blk_register;
use tairix_test_virtio_qemu_support::{
    define_mmio_boot_harness_aarch64, root_unlock_login, run_virtio_mmio_scenario, FixedSpawner,
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

/// Drive the full virtio-blk-mmio bring-up, then the shared interactive
/// root unlock + login proof, reporting the result through the ARM
/// semihosting finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "root-unlock: scenario start",
    };
    run_virtio_mmio_scenario(
        VIRTIO_BLK_DEVICE_ID,
        DTB_BLOB,
        &cfg,
        root_unlock_login::<ScenarioTransport>,
    )
}

define_mmio_boot_harness_aarch64!(run_scenario);
