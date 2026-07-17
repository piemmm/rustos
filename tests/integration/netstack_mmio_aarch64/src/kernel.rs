//! Freestanding (`aarch64-unknown-none`) half of the netstack-mmio
//! integration test.
//!
//! The device-agnostic bring-up *and* the netstack ring-pump ping tail
//! both live in the shared `tairix-test-virtio-qemu-support` crate. This module supplies only what is unique to this
//! vertical: the bare virtio-net MMIO device id, the spawner registering the
//! loaded image through the virtio-net `register`, and the boot harness. The
//! device tail ([`netstack_ping`]) is the same code the riscv64 MMIO and
//! x86_64 PCI verticals run.

use tairix_drv_network_virtio_net::register as virtio_net_register;
use tairix_test_virtio_qemu_support::{
    define_mmio_boot_harness_aarch64, netstack_ping, run_virtio_mmio_scenario, FixedSpawner,
    ScenarioConfig, ScenarioTransport,
};

use crate::fixture::{DTB_BLOB, RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Bare virtio-net MMIO device id (the `DeviceID` register value; over
/// MMIO this is the bare virtio device type, not the PCI `0x1040 + type`
/// encoding).
const VIRTIO_NET_DEVICE_ID: u32 = 1;

/// Spawner registering every verified manifest through the virtio-net driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_net_register);

/// Drive the full netstack-over-virtio-net-mmio ping round-trip and report
/// the result through the ARM semihosting finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "netstack-mmio: scenario start",
    };
    run_virtio_mmio_scenario(
        VIRTIO_NET_DEVICE_ID,
        DTB_BLOB,
        &cfg,
        netstack_ping::<ScenarioTransport>,
    )
}

define_mmio_boot_harness_aarch64!(run_scenario);
