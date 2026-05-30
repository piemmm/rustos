//! Freestanding (`x86_64-unknown-none`) half of the virtio-net-pci
//! integration test.
//!
//! The device-agnostic bring-up *and* the virtio-net ARP + ICMP-echo tail
//! both live in the shared `rustos-test-virtio-qemu-support` crate
//! (`AGENTS.md` §2.2). This module supplies only what is unique to this
//! vertical: the modern virtio-net PCI device id, the resolver binding
//! the loaded image to the virtio-net `register`, and the boot harness.
//! The device tail ([`virtio_net_ping`]) is the same code the riscv64
//! MMIO vertical runs.

use rustos_drv_network_virtio_net::register as virtio_net_register;
use rustos_test_virtio_qemu_support::{
    define_boot_harness, run_virtio_pci_scenario, virtio_net_ping, FixedResolver, ScenarioConfig,
    ScenarioTransport,
};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Modern virtio-net PCI device id (`0x1040 + virtio-net`).
const VIRTIO_NET_DEVICE_ID: u16 = 0x1041;

/// Resolver binding every verified manifest to the virtio-net driver's
/// `register` entry point.
static RESOLVER: FixedResolver = FixedResolver::new(virtio_net_register);

/// Drive the full virtio-net-pci ARP + ICMP-echo round-trip and exit
/// through QEMU's debug-exit device. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        resolver: &RESOLVER,
        start_msg: "virtio-net-pci: scenario start",
    };
    run_virtio_pci_scenario(
        VIRTIO_NET_DEVICE_ID,
        &cfg,
        virtio_net_ping::<ScenarioTransport>,
    )
}

define_boot_harness!(run_scenario);
