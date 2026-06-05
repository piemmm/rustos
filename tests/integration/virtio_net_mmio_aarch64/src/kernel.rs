//! Freestanding (`aarch64-unknown-none`) half of the virtio-net-mmio
//! integration test.
//!
//! The device-agnostic bring-up *and* the virtio-net ARP + ICMP-echo tail
//! both live in the shared `rustos-test-virtio-qemu-support` crate
//! (`AGENTS.md` §2.2). This module supplies only what is unique to this
//! vertical: the bare virtio-net MMIO device id, the resolver binding the
//! loaded image to the virtio-net `register`, and the boot harness. The
//! device tail ([`virtio_net_ping`]) is the same code the riscv64 MMIO and
//! x86_64 PCI verticals run.

use rustos_drv_network_virtio_net::register as virtio_net_register;
use rustos_test_virtio_qemu_support::{
    define_mmio_boot_harness_aarch64, run_virtio_mmio_scenario, virtio_net_ping, FixedResolver,
    ScenarioConfig, ScenarioTransport,
};

use crate::fixture::{DTB_BLOB, RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Bare virtio-net MMIO device id (the `DeviceID` register value; over
/// MMIO this is the bare virtio device type, not the PCI `0x1040 + type`
/// encoding).
const VIRTIO_NET_DEVICE_ID: u32 = 1;

/// Resolver binding every verified manifest to the virtio-net driver's
/// `register` entry point.
static RESOLVER: FixedResolver = FixedResolver::new(virtio_net_register);

/// Drive the full virtio-net-mmio ARP + ICMP-echo round-trip and report
/// the result through the ARM semihosting finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        resolver: &RESOLVER,
        start_msg: "virtio-net-mmio: scenario start",
    };
    run_virtio_mmio_scenario(
        VIRTIO_NET_DEVICE_ID,
        DTB_BLOB,
        &cfg,
        virtio_net_ping::<ScenarioTransport>,
    )
}

define_mmio_boot_harness_aarch64!(run_scenario);
