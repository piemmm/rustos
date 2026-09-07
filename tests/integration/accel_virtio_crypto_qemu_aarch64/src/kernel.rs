//! Freestanding (`aarch64-unknown-none`) half of the virtio-crypto
//! accelerator integration test.
//!
//! The device-agnostic bring-up *and* the AES-CBC known-answer tail both live
//! in the shared `tairix-test-virtio-qemu-support` crate. This module supplies
//! only what is unique to this vertical: the bare virtio-crypto MMIO device
//! id, the spawner registering the loaded image through the virtio-crypto
//! `register`, and the boot harness. The device tail
//! ([`virtio_crypto_aes_cbc`]) is shared with any future PCI sibling.

use tairix_drv_accelerator_virtio_crypto::{
    register as virtio_crypto_register, VIRTIO_CRYPTO_DEVICE_ID,
};
use tairix_test_virtio_qemu_support::{
    define_mmio_boot_harness_aarch64, run_virtio_mmio_scenario, virtio_crypto_aes_cbc,
    FixedSpawner, ScenarioConfig, ScenarioTransport,
};

use crate::fixture::{DTB_BLOB, RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Spawner registering every verified manifest through the accelerator
/// driver's `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_crypto_register);

/// Drive the AES-CBC round trip on the real device and report the result
/// through the ARM semihosting finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "virtio-crypto-mmio: scenario start",
    };
    run_virtio_mmio_scenario(
        VIRTIO_CRYPTO_DEVICE_ID,
        DTB_BLOB,
        &cfg,
        virtio_crypto_aes_cbc::<ScenarioTransport>,
    )
}

define_mmio_boot_harness_aarch64!(run_scenario);
