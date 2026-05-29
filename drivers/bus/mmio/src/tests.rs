//! In-crate tests for the MMIO bus driver.
//!
//! The DTB fixture below mirrors QEMU's `virt` machine: a fixed
//! grid of virtio-MMIO transport slots starting at `0x0A00_0000`,
//! each 0x200 bytes wide (`hw/arm/virt.c`,
//! `hw/riscv/virt.c`). The fixture exposes four slots; the MMIO
//! register fake populates the first two with attached devices
//! (virtio-net, virtio-blk) and the other two with the "empty"
//! `DeviceID == 0` sentinel, so the walker must enumerate exactly
//! two devices in document order — matching what real QEMU
//! presents when started with `-device virtio-net-device -device
//! virtio-blk-device`.
//!
//! Per `AGENTS.md` §8 the production driver crate exposes only
//! `register`; the test module reaches into the `pub(crate)`
//! enumeration core directly.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::{CapabilityId, DriverError, DriverHost, DriverKind};
use rustos_util::dtb::Dtb;

use crate::enumerate::{Mmio, VIRTIO_MMIO_DEFAULT_VENDOR, VIRTIO_MMIO_MAGIC};
use crate::transport::MmioRead;

// ---- Mock host -----------------------------------------------------------

struct MockHost {
    granted: bool,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.granted && cap == CapabilityId::DRV_LOAD
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

// ---- MMIO fake -----------------------------------------------------------

struct FakeMmio {
    regs: Vec<(u64, u32)>,
}

impl MmioRead for FakeMmio {
    fn read32(&self, physical_address: u64) -> u32 {
        self.regs
            .iter()
            .find(|(a, _)| *a == physical_address)
            .map_or(0, |(_, v)| *v)
    }
}

// ---- DTB fixture ---------------------------------------------------------

const FDT_BEGIN_NODE: u32 = 0x0000_0001;
const FDT_END_NODE: u32 = 0x0000_0002;
const FDT_PROP: u32 = 0x0000_0003;
const FDT_END: u32 = 0x0000_0009;
const FDT_MAGIC: u32 = 0xd00d_feed;
const HEADER_LEN: u32 = 40;

/// Build a minimal `virt`-style DTB with `slot_count` `virtio_mmio`
/// nodes laid out at the canonical `0x0A00_0000 + i * 0x200` grid.
fn build_virt_dtb(slot_count: u32) -> Vec<u8> {
    // ---- strings block ---------------------------------------------------
    let mut strings: Vec<u8> = Vec::new();
    let off_compatible = u32::try_from(strings.len()).unwrap();
    strings.extend_from_slice(b"compatible\0");
    let off_reg = u32::try_from(strings.len()).unwrap();
    strings.extend_from_slice(b"reg\0");

    // ---- struct block ----------------------------------------------------
    let mut structs: Vec<u8> = Vec::new();
    // Root.
    structs.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
    structs.extend_from_slice(&[0, 0, 0, 0]); // empty name + padding.
    for i in 0..slot_count {
        let base = 0x0A00_0000u64 + (u64::from(i) * 0x200);
        // Each node name is e.g. "virtio_mmio@a000000\0"; pad to 4.
        structs.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        let mut name = alloc::format!("virtio_mmio@{base:x}\0").into_bytes();
        while name.len() % 4 != 0 {
            name.push(0);
        }
        structs.extend_from_slice(&name);
        // compatible.
        let compat = b"virtio,mmio\0";
        structs.extend_from_slice(&FDT_PROP.to_be_bytes());
        structs.extend_from_slice(&u32::try_from(compat.len()).unwrap().to_be_bytes());
        structs.extend_from_slice(&off_compatible.to_be_bytes());
        structs.extend_from_slice(compat);
        while structs.len() % 4 != 0 {
            structs.push(0);
        }
        // reg = <base_hi base_lo length_hi length_lo> (16 bytes).
        structs.extend_from_slice(&FDT_PROP.to_be_bytes());
        structs.extend_from_slice(&16u32.to_be_bytes());
        structs.extend_from_slice(&off_reg.to_be_bytes());
        structs.extend_from_slice(&base.to_be_bytes());
        structs.extend_from_slice(&0x0000_0000_0000_0200u64.to_be_bytes());
        structs.extend_from_slice(&FDT_END_NODE.to_be_bytes());
    }
    structs.extend_from_slice(&FDT_END_NODE.to_be_bytes()); // close root
    structs.extend_from_slice(&FDT_END.to_be_bytes());

    // ---- assemble --------------------------------------------------------
    let off_struct = HEADER_LEN;
    let size_struct = u32::try_from(structs.len()).unwrap();
    let off_strings = off_struct + size_struct;
    let size_strings = u32::try_from(strings.len()).unwrap();
    let total = off_strings + size_strings;
    let mut blob = Vec::new();
    blob.extend_from_slice(&FDT_MAGIC.to_be_bytes());
    blob.extend_from_slice(&total.to_be_bytes());
    blob.extend_from_slice(&off_struct.to_be_bytes());
    blob.extend_from_slice(&off_strings.to_be_bytes());
    blob.extend_from_slice(&0u32.to_be_bytes()); // off_mem_rsvmap
    blob.extend_from_slice(&17u32.to_be_bytes()); // version
    blob.extend_from_slice(&16u32.to_be_bytes()); // last_comp_version
    blob.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid_phys
    blob.extend_from_slice(&size_strings.to_be_bytes());
    blob.extend_from_slice(&size_struct.to_be_bytes());
    blob.extend_from_slice(&structs);
    blob.extend_from_slice(&strings);
    blob
}

fn slot(base: u64, device_id: u32, vendor: u32, version: u32) -> [(u64, u32); 4] {
    [
        (base, VIRTIO_MMIO_MAGIC),
        (base + 0x004, version),
        (base + 0x008, device_id),
        (base + 0x00C, vendor),
    ]
}

// ---- Tests ---------------------------------------------------------------

#[test]
fn register_requires_drv_load_capability() {
    let denied = MockHost { granted: false };
    assert_eq!(crate::register(&denied), Err(DriverError::PermissionDenied));
    let allowed = MockHost { granted: true };
    let h = crate::register(&allowed).expect("granted host registers cleanly");
    assert_ne!(h.as_u64(), 0);
}

#[test]
fn virt_enumeration_matches_exact_device_list() {
    let blob = build_virt_dtb(4);
    let dtb = Dtb::parse(&blob).expect("DTB parses");
    // Slot 0: virtio-net (DeviceID = 1).
    // Slot 1: virtio-blk (DeviceID = 2).
    // Slots 2..3: empty (DeviceID == 0).
    let mut regs: Vec<(u64, u32)> = Vec::new();
    regs.extend_from_slice(&slot(0x0A00_0000, 1, 0x554D_4551, 2));
    regs.extend_from_slice(&slot(0x0A00_0200, 2, 0x554D_4551, 2));
    // Slots 2 and 3: MagicValue present but DeviceID = 0 (= empty).
    regs.extend_from_slice(&slot(0x0A00_0400, 0, 0x554D_4551, 2));
    regs.extend_from_slice(&slot(0x0A00_0600, 0, 0x554D_4551, 2));

    let bus = Mmio::new(dtb, FakeMmio { regs });
    let mut out = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 8];
    let n = (&bus as &dyn Bus).enumerate(&mut out).expect("enum ok");
    let got: Vec<_> = out[..n].to_vec();
    let want = vec![
        BusDevice {
            vendor: 0x554D_4551,
            device: 1,
            class: 2,
            reserved0: 0,
            address: 0x0A00_0000,
        },
        BusDevice {
            vendor: 0x554D_4551,
            device: 2,
            class: 2,
            reserved0: 0,
            address: 0x0A00_0200,
        },
    ];
    assert_eq!(got, want);
}

#[test]
fn short_buffer_yields_buffer_too_small() {
    let blob = build_virt_dtb(4);
    let dtb = Dtb::parse(&blob).expect("DTB parses");
    let mut regs: Vec<(u64, u32)> = Vec::new();
    regs.extend_from_slice(&slot(0x0A00_0000, 1, 0x554D_4551, 2));
    regs.extend_from_slice(&slot(0x0A00_0200, 2, 0x554D_4551, 2));
    let bus = Mmio::new(dtb, FakeMmio { regs });
    let mut out = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 1];
    assert_eq!(
        (&bus as &dyn Bus).enumerate(&mut out),
        Err(DriverError::BufferTooSmall),
    );
    assert_eq!(out[0].device, 1);
}

#[test]
fn slot_without_magic_is_skipped() {
    let blob = build_virt_dtb(2);
    let dtb = Dtb::parse(&blob).expect("DTB parses");
    let regs: Vec<(u64, u32)> = vec![
        // Wrong magic on slot 0: skipped silently.
        (0x0A00_0000, 0xDEAD_BEEF),
        // Slot 1 fully populated.
        (0x0A00_0200, VIRTIO_MMIO_MAGIC),
        (0x0A00_0200 + 0x004, 1),
        (0x0A00_0200 + 0x008, 3), // DeviceID 3 = console.
        (0x0A00_0200 + 0x00C, 0), // VendorID = 0 -> driver substitutes fallback.
    ];
    let bus = Mmio::new(dtb, FakeMmio { regs });
    let mut out = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 4];
    let n = (&bus as &dyn Bus).enumerate(&mut out).expect("enum ok");
    assert_eq!(n, 1);
    assert_eq!(out[0].vendor, VIRTIO_MMIO_DEFAULT_VENDOR);
    assert_eq!(out[0].device, 3);
    assert_eq!(out[0].address, 0x0A00_0200);
}

#[test]
fn empty_dtb_enumerates_to_zero() {
    let blob = build_virt_dtb(0);
    let dtb = Dtb::parse(&blob).expect("DTB parses");
    let bus = Mmio::new(dtb, FakeMmio { regs: vec![] });
    let mut out = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 4];
    assert_eq!((&bus as &dyn Bus).enumerate(&mut out), Ok(0));
}
