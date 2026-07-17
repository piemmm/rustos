//! Freestanding (`riscv64gc-unknown-none-elf`) half of the
//! virtio-input-mmio integration test.
//!
//! The device-agnostic bring-up *and* the virtio-input key-decode tail
//! both live in the shared `tairix-test-virtio-qemu-support` crate. This module supplies only what is unique to this
//! vertical: the bare virtio-input MMIO device id, the spawner registering
//! the loaded image through the virtio-input `register`, and the boot
//! harness. The device tail ([`virtio_input_keypress`]) is the same code
//! the aarch64 virtio-input vertical runs.

use tairix_drv_input_virtio_input::register as virtio_input_register;
use tairix_test_virtio_qemu_support::{
    define_mmio_boot_harness, run_virtio_mmio_scenario, virtio_input_keypress, FixedSpawner,
    ScenarioConfig, ScenarioTransport,
};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Bare virtio-input MMIO device id (the `DeviceID` register value; over
/// MMIO this is the bare virtio device type, not the PCI `0x1040 + type`
/// encoding). virtio-input is device type 18 (virtio 1.1 §5.8).
const VIRTIO_INPUT_DEVICE_ID: u32 = 18;

/// Spawner registering every verified manifest through the virtio-input driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_input_register);

/// Drive the full virtio-input key round-trip and report the result
/// through the `SiFive` Test finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "virtio-input-mmio: scenario start",
    };
    run_virtio_mmio_scenario(
        VIRTIO_INPUT_DEVICE_ID,
        &cfg,
        virtio_input_keypress::<ScenarioTransport>,
    )
}

define_mmio_boot_harness!(run_scenario);
