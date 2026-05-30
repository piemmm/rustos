//! Freestanding (`x86_64-unknown-none`) half of the virtio-net-pci
//! integration test.
//!
//! The device-agnostic bring-up (carve a per-device DMA region, walk PCI,
//! map the four virtio register windows, route MSI-X, mint a
//! `KernelVirtioHost`, load the signed `.rxe`) lives in the shared
//! `rustos-test-virtio-qemu-support` crate (`AGENTS.md` §2.2). This module
//! supplies only the virtio-net-specific tail: it opens [`VirtioNet`] over
//! the provisioned transport, then drives `rustos_net_icmp::Client` over
//! the real device to ARP-resolve the QEMU user-mode (SLIRP) gateway
//! `10.0.2.2` from the fixed guest address `10.0.2.15` and exchange one
//! ICMP echo. The guest must *initiate* the exchange because SLIRP never
//! pings the guest unprompted.
//!
//! Any deviation flips QEMU failure; only a confirmed echo reply reaches
//! QEMU success.

use rustos_abi::driver::net::Net;
use rustos_abi::DriverManifest;
use rustos_drv_network_virtio_net::{self as virtio_net, VirtioNet};
use rustos_drvhost::{DriverEntry, EntryResolver};
use rustos_net_icmp::{Client, Ipv4Address};
use rustos_test_virtio_qemu_support::{
    define_boot_harness, log, run_virtio_scenario, ScenarioConfig,
};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Modern virtio-net PCI device id (`0x1040 + virtio-net`).
const VIRTIO_NET_DEVICE_ID: u16 = 0x1041;

/// Fixed guest address under QEMU user-mode networking.
const GUEST_IP: Ipv4Address = Ipv4Address::new([10, 0, 2, 15]);

/// SLIRP gateway address that answers ARP and ICMP echo.
const GATEWAY_IP: Ipv4Address = Ipv4Address::new([10, 0, 2, 2]);

/// ICMP echo identifier / sequence the request carries and the reply
/// must echo back.
const ECHO_ID: u16 = 0x1234;
/// ICMP echo sequence number.
const ECHO_SEQ: u16 = 1;

/// Echo payload; the reply must mirror it byte-for-byte.
const ECHO_PAYLOAD: &[u8] = b"rustos-virtio-net";

/// Bounded poll budget for each ARP/ICMP exchange. Each poll posts one
/// receive descriptor and parks on the device's completion interrupt, so
/// the loop is bounded both by this count and by the per-test QEMU wall
/// clock.
const MAX_POLLS: usize = 64;

/// Resolver binding every verified manifest to the virtio-net driver's
/// `register` entry point.
struct ToVirtioNet;
impl EntryResolver for ToVirtioNet {
    fn resolve(&self, _manifest: &DriverManifest, _payload: &[u8]) -> Option<DriverEntry> {
        Some(virtio_net::register as DriverEntry)
    }
}

static RESOLVER: ToVirtioNet = ToVirtioNet;

/// Drive the full virtio-net-pci ARP + ICMP-echo round-trip and exit
/// through QEMU's debug-exit device. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        device_id: VIRTIO_NET_DEVICE_ID,
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        resolver: &RESOLVER,
        start_msg: "virtio-net-pci: scenario start",
    };
    run_virtio_scenario(&cfg, |transport, vhost| {
        let mut net = VirtioNet::open(transport, vhost).map_err(|_| "virtio-net open")?;
        let mac = net.mac_address().map_err(|_| "read device MAC")?;
        log("virtio-net-pci: device online");

        let client = Client::new(mac, GUEST_IP);
        let mut rx = [0u8; 2048];
        let mut tx = [0u8; 2048];

        // ARP-resolve the gateway. The guest must initiate; SLIRP answers.
        let peer = client
            .resolve(&mut net, GATEWAY_IP, &mut rx, &mut tx, MAX_POLLS)
            .map_err(|_| "ARP resolve error")?
            .ok_or("ARP: no reply from gateway")?;
        log("virtio-net-pci: gateway ARP resolved");

        // Send an ICMP echo to the gateway and confirm the reply.
        let replied = client
            .ping(
                &mut net,
                peer,
                GATEWAY_IP,
                ECHO_ID,
                ECHO_SEQ,
                ECHO_PAYLOAD,
                &mut rx,
                &mut tx,
                MAX_POLLS,
            )
            .map_err(|_| "ICMP echo error")?;
        if !replied {
            return Err("ICMP: no echo reply from gateway");
        }
        log("virtio-net-pci: ICMP echo round-trip verified");
        Ok(())
    })
}

define_boot_harness!(run_scenario);
