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
//! Per the production driver crate exposes only
//! `register`; the test module reaches into the `pub(crate)`
//! enumeration core directly.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, MmioMapError, MmioMapper, RegisterWindow,
};
use rustos_fdt::Fdt;

use crate::enumerate::{Mmio, VIRTIO_MMIO_DEFAULT_VENDOR, VIRTIO_MMIO_MAGIC};
use crate::transport::MmioRead;

// ---- Mock MMIO mapper ----------------------------------------------------

/// Stand-in for the kernel's MMIO-map facility (mirrors the PCI
/// crate's mock). Backs every minted [`RegisterWindow`] with a fixed
/// heap buffer that outlives the mapper and every window; `granted`
/// models the `CAP_MMIO_MAP` check.
struct MockMapper {
    /// `u32`-element backing so its base is ≥ 4-byte aligned, matching
    /// the page-aligned mapping the real kernel mapper produces.
    backing: RefCell<Vec<u32>>,
    granted: bool,
}

impl MockMapper {
    fn new(granted: bool) -> Self {
        Self {
            // 0x400 words = 4 KiB.
            backing: RefCell::new(vec![0u32; 0x400]),
            granted,
        }
    }
}

impl MmioMapper for MockMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        if !self.granted {
            return Err(MmioMapError::CapabilityMissing);
        }
        if len == 0 {
            return Err(MmioMapError::InvalidRegion);
        }
        let mut backing = self.backing.borrow_mut();
        if len > backing.len() * 4 {
            return Err(MmioMapError::Unsupported);
        }
        let base = NonNull::new(backing.as_mut_ptr().cast::<u8>()).expect("non-null heap buffer");
        // SAFETY: `base` covers `backing.len() * 4 >= len` bytes and is
        // 4-byte aligned (the `Vec<u32>` allocation guarantee); the
        // backing lives for the mapper's lifetime, which outlives every
        // window minted here, and no other reference aliases it while
        // the window is live.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

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
    let dtb = Fdt::new(&blob).expect("DTB parses");
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
    let dtb = Fdt::new(&blob).expect("DTB parses");
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
    let dtb = Fdt::new(&blob).expect("DTB parses");
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
    let dtb = Fdt::new(&blob).expect("DTB parses");
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

#[test]
fn map_slot_window_hands_off_to_kernel_mapper() {
    let blob = build_virt_dtb(4);
    let dtb = Fdt::new(&blob).expect("DTB parses");
    let regs: Vec<(u64, u32)> = slot(0x0A00_0200, 2, 0x554D_4551, 2).to_vec();
    let bus = Mmio::new(dtb, FakeMmio { regs });
    let mapper = MockMapper::new(true);
    // Slot 1 sits at 0x0A00_0200 with a 0x200-byte window.
    let window = bus
        .map_slot_window(0x0A00_0200, &mapper)
        .expect("slot maps");
    assert_eq!(window.phys_base(), 0x0A00_0200);
    assert_eq!(window.len(), 0x200);
    window.write_u32(0, VIRTIO_MMIO_MAGIC).expect("in bounds");
    assert_eq!(window.read_u32(0).expect("in bounds"), VIRTIO_MMIO_MAGIC);
}

#[test]
fn map_slot_window_reports_not_found_for_unknown_base() {
    let blob = build_virt_dtb(2);
    let dtb = Fdt::new(&blob).expect("DTB parses");
    let bus = Mmio::new(dtb, FakeMmio { regs: vec![] });
    let mapper = MockMapper::new(true);
    // No slot is laid out at this address.
    assert_eq!(
        bus.map_slot_window(0x0B00_0000, &mapper).unwrap_err(),
        DriverError::NotFound
    );
}

#[test]
fn map_slot_window_propagates_capability_denial() {
    let blob = build_virt_dtb(2);
    let dtb = Fdt::new(&blob).expect("DTB parses");
    let bus = Mmio::new(dtb, FakeMmio { regs: vec![] });
    // Mapper without CAP_MMIO_MAP: the hand-off surfaces the refusal.
    let mapper = MockMapper::new(false);
    assert_eq!(
        bus.map_slot_window(0x0A00_0000, &mapper).unwrap_err(),
        DriverError::PermissionDenied
    );
}

#[test]
fn slot_window_len_reports_the_dtb_declared_extent() {
    let blob = build_virt_dtb(4);
    let dtb = Fdt::new(&blob).expect("DTB parses");
    let bus = Mmio::new(dtb, FakeMmio { regs: vec![] });
    // The `virt` layout advertises a 0x200-byte window per slot; the
    // unmapped extent lookup reads it straight from the `reg` pair and
    // touches no device register (no mapper involved).
    assert_eq!(bus.slot_window_len(0x0A00_0200), Ok(0x200));
}

#[test]
fn slot_window_len_reports_not_found_for_unknown_base() {
    let blob = build_virt_dtb(2);
    let dtb = Fdt::new(&blob).expect("DTB parses");
    let bus = Mmio::new(dtb, FakeMmio { regs: vec![] });
    assert_eq!(
        bus.slot_window_len(0x0B00_0000).unwrap_err(),
        DriverError::NotFound
    );
}

// ---- `virtio_mmio_bus_from_dtb` construction seam ------------------------

/// Build a minimal `virt`-style DTB whose `virtio,mmio` slots sit at the
/// caller-supplied `bases`, each advertising a `length`-byte window. The
/// constructor test points the bases at a host buffer so the volatile
/// reader the constructor mints reads real memory.
fn build_virt_dtb_at(bases: &[u64], length: u64) -> Vec<u8> {
    let mut strings: Vec<u8> = Vec::new();
    let off_compatible = u32::try_from(strings.len()).unwrap();
    strings.extend_from_slice(b"compatible\0");
    let off_reg = u32::try_from(strings.len()).unwrap();
    strings.extend_from_slice(b"reg\0");

    let mut structs: Vec<u8> = Vec::new();
    structs.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
    structs.extend_from_slice(&[0, 0, 0, 0]);
    for base in bases {
        structs.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        let mut name = alloc::format!("virtio_mmio@{base:x}\0").into_bytes();
        while name.len() % 4 != 0 {
            name.push(0);
        }
        structs.extend_from_slice(&name);
        let compat = b"virtio,mmio\0";
        structs.extend_from_slice(&FDT_PROP.to_be_bytes());
        structs.extend_from_slice(&u32::try_from(compat.len()).unwrap().to_be_bytes());
        structs.extend_from_slice(&off_compatible.to_be_bytes());
        structs.extend_from_slice(compat);
        while structs.len() % 4 != 0 {
            structs.push(0);
        }
        structs.extend_from_slice(&FDT_PROP.to_be_bytes());
        structs.extend_from_slice(&16u32.to_be_bytes());
        structs.extend_from_slice(&off_reg.to_be_bytes());
        structs.extend_from_slice(&base.to_be_bytes());
        structs.extend_from_slice(&length.to_be_bytes());
        structs.extend_from_slice(&FDT_END_NODE.to_be_bytes());
    }
    structs.extend_from_slice(&FDT_END_NODE.to_be_bytes());
    structs.extend_from_slice(&FDT_END.to_be_bytes());

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
    blob.extend_from_slice(&0u32.to_be_bytes());
    blob.extend_from_slice(&17u32.to_be_bytes());
    blob.extend_from_slice(&16u32.to_be_bytes());
    blob.extend_from_slice(&0u32.to_be_bytes());
    blob.extend_from_slice(&size_strings.to_be_bytes());
    blob.extend_from_slice(&size_struct.to_be_bytes());
    blob.extend_from_slice(&structs);
    blob.extend_from_slice(&strings);
    blob
}

#[test]
fn virtio_mmio_aperture_spans_all_slots() {
    // Four slots on the canonical 0x0200 grid, each 0x200 long, span
    // [0x0A00_0000, 0x0A00_0800).
    let blob = build_virt_dtb(4);
    let dtb = Fdt::new(&blob).expect("DTB parses");
    let span = crate::virtio_mmio_aperture(&dtb)
        .expect("aperture ok")
        .expect("some slots");
    assert_eq!(span, (0x0A00_0000, 0x800));
}

#[test]
fn virtio_mmio_aperture_none_without_slots() {
    let blob = build_virt_dtb(0);
    let dtb = Fdt::new(&blob).expect("DTB parses");
    assert_eq!(
        crate::virtio_mmio_aperture(&dtb).expect("aperture ok"),
        None
    );
}

#[test]
fn virtio_mmio_bus_from_dtb_enumerates_attached_slots() {
    // Host stand-in for the MMIO aperture: a word buffer whose address
    // the DTB slot bases point at, so the volatile reader the
    // constructor mints reads the stamped identifier registers (the
    // host-buildable exercise of the constructor's `unsafe` reader).
    let mut backing = vec![0u32; 0x400];
    let base = backing.as_ptr() as u64;
    let stride = 0x200u64;
    let bases = [base, base + stride];

    // Stamp slot 0 = virtio-net (DeviceID 1), slot 1 = virtio-blk (2).
    let mut stamp = |slot_off: u64, device: u32| {
        let w = usize::try_from(slot_off / 4).unwrap();
        backing[w] = VIRTIO_MMIO_MAGIC;
        backing[w + 1] = 2; // Version (modern).
        backing[w + 2] = device;
        backing[w + 3] = VIRTIO_MMIO_DEFAULT_VENDOR;
    };
    stamp(0, 1);
    stamp(stride, 2);

    let blob = build_virt_dtb_at(&bases, stride);
    // SAFETY: `backing` outlives the bus and is exclusively owned here;
    // the DTB bases address it, so the aperture is "identity-mapped" for
    // the duration of the test.
    let bus = unsafe { crate::virtio_mmio_bus_from_dtb(&blob) }.expect("bus constructs");

    let mut out = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 8];
    let n = bus.enumerate(&mut out).expect("enumerate ok");
    assert_eq!(n, 2);
    assert_eq!(out[0].device, 1);
    assert_eq!(out[0].address, base);
    assert_eq!(out[1].device, 2);
    assert_eq!(out[1].address, base + stride);
}

#[test]
fn virtio_mmio_bus_from_dtb_reports_not_found_without_slots() {
    let blob = build_virt_dtb(0);
    // SAFETY: no `virtio,mmio` slot exists, so the constructor returns
    // before minting any reader; no memory is dereferenced.
    let err = unsafe { crate::virtio_mmio_bus_from_dtb(&blob) }
        .err()
        .expect("no slots → error");
    assert_eq!(err, DriverError::NotFound);
}
