//! Freestanding (`x86_64-unknown-none`) half of the virtio-blk-pci
//! integration test.
//!
//! The device-agnostic bring-up (carve a per-device DMA region, walk PCI,
//! map the four virtio register windows, route MSI-X, mint a
//! `KernelVirtioHost`, load the signed `.rxe`) lives in the shared
//! `rustos-test-virtio-qemu-support` crate (`AGENTS.md` §2.2). This module
//! supplies only the virtio-blk-specific tail: it opens [`VirtioBlk`] over
//! the provisioned transport, reads sector 0 and verifies the
//! harness-planted pattern, then writes a known pattern to sector 1, reads
//! it back, and verifies it round-tripped.
//!
//! Any deviation flips QEMU failure; only the fully successful path
//! reaches QEMU success.

use rustos_abi::driver::block::Block;
use rustos_abi::DriverManifest;
use rustos_drv_storage_virtio_blk::{self as virtio_blk, VirtioBlk};
use rustos_drvhost::{DriverEntry, EntryResolver};
use rustos_test_virtio_qemu_support::{
    define_boot_harness, log, run_virtio_scenario, ScenarioConfig,
};

use crate::fixture::{RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Modern virtio-blk PCI device id (`0x1040 + virtio-blk`).
const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;

/// Logical sector size.
const SECTOR_LEN: usize = 512;

/// `true` if `sector` matches the pattern the host harness planted at
/// LBA 0 (`byte[i] == i mod 256`). Kept in sync with the `plant_raw_disk`
/// call in `tools/xtask/src/commands/qemu_tests.rs`.
fn sector0_matches(sector: &[u8; SECTOR_LEN]) -> bool {
    sector
        .iter()
        .enumerate()
        .all(|(i, b)| *b == u8::try_from(i & 0xFF).unwrap_or(0))
}

/// Fill `sector` with the pattern the test writes to LBA 1
/// (`byte[i] = (i mod 256) xor 0xA5`) — distinct from the LBA-0 pattern so
/// a stale-read regression cannot pass by accident.
fn fill_sector1(sector: &mut [u8; SECTOR_LEN]) {
    for (i, b) in sector.iter_mut().enumerate() {
        *b = u8::try_from(i & 0xFF).unwrap_or(0) ^ 0xA5;
    }
}

/// Resolver binding every verified manifest to the virtio-blk driver's
/// `register` entry point.
struct ToVirtioBlk;
impl EntryResolver for ToVirtioBlk {
    fn resolve(&self, _manifest: &DriverManifest, _payload: &[u8]) -> Option<DriverEntry> {
        Some(virtio_blk::register as DriverEntry)
    }
}

static RESOLVER: ToVirtioBlk = ToVirtioBlk;

/// Drive the full virtio-blk-pci round-trip and exit through QEMU's
/// debug-exit device. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        device_id: VIRTIO_BLK_DEVICE_ID,
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        resolver: &RESOLVER,
        start_msg: "virtio-blk-pci: scenario start",
    };
    run_virtio_scenario(&cfg, |transport, vhost| {
        let mut blk = VirtioBlk::open(transport, vhost).map_err(|_| "virtio-blk open")?;
        log("virtio-blk-pci: device online");

        // Read sector 0 and verify the harness-planted pattern.
        let mut s0 = [0u8; SECTOR_LEN];
        blk.read_blocks(0, &mut s0).map_err(|_| "read sector 0")?;
        if !sector0_matches(&s0) {
            return Err("sector 0 pattern mismatch");
        }
        log("virtio-blk-pci: sector 0 verified");

        // Write a known pattern to sector 1, read it back, verify.
        let mut s1 = [0u8; SECTOR_LEN];
        fill_sector1(&mut s1);
        blk.write_blocks(1, &s1).map_err(|_| "write sector 1")?;
        let mut rb = [0u8; SECTOR_LEN];
        blk.read_blocks(1, &mut rb)
            .map_err(|_| "read-back sector 1")?;
        if rb != s1 {
            return Err("sector 1 round-trip mismatch");
        }
        log("virtio-blk-pci: sector 1 round-trip verified");
        Ok(())
    })
}

define_boot_harness!(run_scenario);
