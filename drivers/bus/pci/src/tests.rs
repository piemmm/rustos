//! In-crate tests for the PCI driver.
//!
//! The fixture below models QEMU's `q35` PCI tree as enumerated by
//! `qemu-system-x86_64 -machine q35 -netdev user,id=n -device
//! virtio-net-pci,netdev=n,bus=pcie.0,addr=0x3`. The IDs and class
//! codes are pulled from QEMU's `hw/i386/pc_q35.c` /
//! `hw/pci-host/q35.c` so a future live-on-QEMU integration test
//! can assert the same exact list this host-side test does, with no
//! divergence between mock and real hardware.
//!
//! Per `AGENTS.md` §8 the production driver crate exposes only
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

use crate::config::{BarKind, Capability, ConfigAddress, ConfigSpace};
use crate::enumerate::Pci;

// ---- Mock MMIO mapper ----------------------------------------------------

/// Stand-in for the kernel's MMIO-map facility. Backs every minted
/// [`RegisterWindow`] with a fixed heap buffer that outlives the
/// mapper (and therefore every window). `granted` models the
/// `CAP_MMIO_MAP` check the real kernel mapper performs.
struct MockMapper {
    /// `u32`-element backing so its base is \u2265 4-byte aligned, matching
    /// the page-aligned mapping the real kernel mapper produces.
    backing: RefCell<Vec<u32>>,
    granted: bool,
}

impl MockMapper {
    fn new(granted: bool) -> Self {
        Self {
            // 0x4000 words = 64 KiB.
            backing: RefCell::new(vec![0u32; 0x4000]),
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

// ---- Mock configuration space --------------------------------------------

/// A function exposed by the [`MockConfigSpace`] fixture.
///
/// `regs` is a sparse map from register dword index to its raw 32-bit
/// value. Anything not present reads as `0`. The `sizing_mask` map
/// is consulted only during BAR sizing — the mock returns the mask
/// only while the BAR is in its "probed" state.
struct MockFunction {
    bus: u8,
    device: u8,
    function: u8,
    regs: Vec<(u8, u32)>,
    /// Per-BAR sizing mask: `(register_dword_index, mask)`. The
    /// `bars()` enumerator writes `0xFFFF_FFFF` to a BAR, reads back
    /// the mask, then restores; the mock honours that protocol.
    sizing: Vec<(u8, u32)>,
}

struct MockConfigSpace {
    funcs: Vec<MockFunction>,
    state: RefCell<MockState>,
}

#[derive(Default)]
struct MockState {
    /// Active sizing probe: `(bus, dev, fn, reg)`.
    probing: Option<(u8, u8, u8, u8)>,
}

impl MockConfigSpace {
    fn new(funcs: Vec<MockFunction>) -> Self {
        Self {
            funcs,
            state: RefCell::new(MockState::default()),
        }
    }

    fn find(&self, addr: ConfigAddress) -> Option<&MockFunction> {
        self.funcs
            .iter()
            .find(|f| f.bus == addr.bus && f.device == addr.device && f.function == addr.function)
    }
}

impl ConfigSpace for MockConfigSpace {
    fn read32(&self, addr: ConfigAddress) -> u32 {
        let Some(f) = self.find(addr) else {
            return 0xFFFF_FFFF;
        };
        let st = self.state.borrow();
        if let Some(p) = st.probing {
            if p == (addr.bus, addr.device, addr.function, addr.register) {
                if let Some(&(_, mask)) = f.sizing.iter().find(|(r, _)| *r == addr.register) {
                    return mask;
                }
            }
        }
        f.regs
            .iter()
            .find(|(r, _)| *r == addr.register)
            .map_or(0, |(_, v)| *v)
    }

    fn write32(&self, addr: ConfigAddress, value: u32) {
        // Only the BAR-sizing protocol is modelled: an FFFFFFFF
        // write to a BAR enters "probing" state, any other write
        // (the restore) leaves it.
        let mut st = self.state.borrow_mut();
        if value == 0xFFFF_FFFF {
            st.probing = Some((addr.bus, addr.device, addr.function, addr.register));
        } else {
            st.probing = None;
        }
    }
}

// ---- The q35 fixture -----------------------------------------------------

/// Encode a 16-bit vendor and 16-bit device-id into the dword-0 slot.
fn id(vendor: u16, device: u16) -> (u8, u32) {
    (0, (u32::from(device) << 16) | u32::from(vendor))
}

/// Encode a class/subclass into dword 2 (upper 16 bits).
fn class(class_subclass: u16) -> (u8, u32) {
    (2, u32::from(class_subclass) << 16)
}

/// Encode the header-type / multi-function byte into dword 3 (bits 23..16).
fn header(header_type: u8) -> (u8, u32) {
    (3, u32::from(header_type) << 16)
}

/// Encode the status / command register at dword 1; we only care about
/// the capability-list bit in the status half.
fn status_with_caplist() -> (u8, u32) {
    (1, (1u32 << 4) << 16) // status bit 4 == cap list
}

/// Capability pointer at config-space byte offset 0x34 (dword 13).
fn cap_pointer(byte_offset: u8) -> (u8, u32) {
    (13, u32::from(byte_offset))
}

fn q35_fixture() -> MockConfigSpace {
    // 00:00.0 — Intel 82G33/G31/P35/P31 host bridge.
    let host_bridge = MockFunction {
        bus: 0,
        device: 0,
        function: 0,
        regs: vec![id(0x8086, 0x29C0), class(0x0600), header(0x00)],
        sizing: vec![],
    };

    // 00:1f.0 — LPC interface (multifunction).
    let lpc = MockFunction {
        bus: 0,
        device: 0x1F,
        function: 0,
        regs: vec![id(0x8086, 0x2918), class(0x0601), header(0x80)],
        sizing: vec![],
    };
    // 00:1f.2 — AHCI SATA.
    let sata = MockFunction {
        bus: 0,
        device: 0x1F,
        function: 2,
        regs: vec![id(0x8086, 0x2922), class(0x0106), header(0x00)],
        sizing: vec![],
    };
    // 00:1f.3 — SMBus.
    let smbus = MockFunction {
        bus: 0,
        device: 0x1F,
        function: 3,
        regs: vec![id(0x8086, 0x2930), class(0x0C05), header(0x00)],
        sizing: vec![],
    };

    // 00:03.0 — virtio-net-pci (modern, transitional ID 0x1041).
    //
    // BAR0: I/O at 0xC000, 32 bytes.
    // BAR1: 64-bit memory at 0xFEBF_0000, 16 KiB, non-prefetchable.
    // Cap list: PM (0x01) @ 0x40 -> MSI-X (0x11) @ 0x50 -> end.
    let virtio_net = MockFunction {
        bus: 0,
        device: 3,
        function: 0,
        regs: vec![
            id(0x1AF4, 0x1041),
            status_with_caplist(),
            class(0x0200),
            header(0x00),
            // BAR0 — IO at 0xC000, low bit = 1.
            (4, 0x0000_C001),
            // BAR1+BAR2 — 64-bit memory at 0xFEBF_0000.
            (5, 0xFEBF_0004),
            (6, 0x0000_0000),
            cap_pointer(0x40),
            // Power Management cap @ 0x40 (dword 16): id=0x01, next=0x50.
            (16, 0x0000_5001),
            // MSI-X cap @ 0x50 (dword 20):
            //   header: id=0x11, next=0x00, msg_ctrl=(table_size-1)=3 -> table_size=4.
            // `next=0x00` deliberately written as `0` for clarity
            // even though it has no effect on the encoding.
            #[allow(clippy::identity_op)]
            (20, (0x0003u32 << 16) | (0x00u32 << 8) | 0x11_u32),
            // Table off/BIR @ 0x54 (dword 21): table_bar=1, offset=0x0000_2000.
            (21, 0x0000_2001),
            // PBA off/BIR @ 0x58 (dword 22): pba_bar=1, offset=0x0000_3000.
            (22, 0x0000_3001),
        ],
        sizing: vec![
            // IO BAR0: 32-byte region -> mask=0xFFFF_FFE1 (low IO bit preserved).
            (4, 0xFFFF_FFE1),
            // 64-bit memory BAR1: 16 KiB -> mask=0xFFFF_C004.
            (5, 0xFFFF_C004),
        ],
    };

    MockConfigSpace::new(vec![host_bridge, lpc, sata, smbus, virtio_net])
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
fn q35_enumeration_matches_exact_device_list() {
    let pci = Pci::new(q35_fixture());
    let mut buf = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 16];
    let n = (&pci as &dyn Bus)
        .enumerate(&mut buf)
        .expect("enumeration succeeds");
    let got: Vec<_> = buf[..n].to_vec();
    let want = vec![
        BusDevice {
            vendor: 0x8086,
            device: 0x29C0,
            class: 0x0600,
            reserved0: 0,
            address: ConfigAddress {
                bus: 0,
                device: 0,
                function: 0,
                register: 0,
            }
            .pack_bdf(),
        },
        BusDevice {
            vendor: 0x1AF4,
            device: 0x1041,
            class: 0x0200,
            reserved0: 0,
            address: ConfigAddress {
                bus: 0,
                device: 3,
                function: 0,
                register: 0,
            }
            .pack_bdf(),
        },
        BusDevice {
            vendor: 0x8086,
            device: 0x2918,
            class: 0x0601,
            reserved0: 0,
            address: ConfigAddress {
                bus: 0,
                device: 0x1F,
                function: 0,
                register: 0,
            }
            .pack_bdf(),
        },
        BusDevice {
            vendor: 0x8086,
            device: 0x2922,
            class: 0x0106,
            reserved0: 0,
            address: ConfigAddress {
                bus: 0,
                device: 0x1F,
                function: 2,
                register: 0,
            }
            .pack_bdf(),
        },
        BusDevice {
            vendor: 0x8086,
            device: 0x2930,
            class: 0x0C05,
            reserved0: 0,
            address: ConfigAddress {
                bus: 0,
                device: 0x1F,
                function: 3,
                register: 0,
            }
            .pack_bdf(),
        },
    ];
    assert_eq!(got, want);
}

#[test]
fn short_buffer_yields_buffer_too_small_with_partial_fill() {
    let pci = Pci::new(q35_fixture());
    let mut buf = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 2];
    let res = (&pci as &dyn Bus).enumerate(&mut buf);
    assert_eq!(res, Err(DriverError::BufferTooSmall));
    // The first two slots are still populated.
    assert_eq!(buf[0].vendor, 0x8086);
    assert_eq!(buf[1].vendor, 0x1AF4);
}

#[test]
fn capabilities_walker_decodes_pm_and_msix_in_order() {
    let pci = Pci::new(q35_fixture());
    let virtio_bdf = ConfigAddress {
        bus: 0,
        device: 3,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let mut out = [Capability::Other { offset: 0, id: 0 }; 8];
    let n = pci.capabilities(virtio_bdf, &mut out).expect("cap walk ok");
    assert_eq!(n, 2);
    match out[0] {
        Capability::Other { offset, id } => {
            assert_eq!(offset, 0x40);
            assert_eq!(id, 0x01);
        }
        ref other => panic!("expected PM cap first, got {other:?}"),
    }
    match out[1] {
        Capability::MsiX {
            offset,
            table_size,
            table_bar,
            table_offset,
            pba_bar,
            pba_offset,
        } => {
            assert_eq!(offset, 0x50);
            assert_eq!(table_size, 4);
            assert_eq!(table_bar, 1);
            assert_eq!(table_offset, 0x0000_2000);
            assert_eq!(pba_bar, 1);
            assert_eq!(pba_offset, 0x0000_3000);
        }
        ref other => panic!("expected MSI-X cap second, got {other:?}"),
    }
}

#[test]
fn capabilities_walker_reports_not_found_when_status_bit_clear() {
    let pci = Pci::new(q35_fixture());
    let lpc_bdf = ConfigAddress {
        bus: 0,
        device: 0x1F,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let mut out = [Capability::Other { offset: 0, id: 0 }; 4];
    assert_eq!(
        pci.capabilities(lpc_bdf, &mut out),
        Err(DriverError::NotFound)
    );
}

#[test]
fn bar_decoder_resolves_io_and_64bit_memory_with_sizes() {
    let pci = Pci::new(q35_fixture());
    let virtio_bdf = ConfigAddress {
        bus: 0,
        device: 3,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let mut bars = [crate::config::BarDescriptor {
        index: 0,
        kind: BarKind::Memory32,
        base: 0,
        size: 0,
        prefetchable: false,
    }; 6];
    let n = pci.bars(virtio_bdf, &mut bars).expect("BAR walk ok");
    assert_eq!(n, 2);
    assert_eq!(bars[0].index, 0);
    assert_eq!(bars[0].kind, BarKind::Io);
    assert_eq!(bars[0].base, 0xC000);
    assert_eq!(bars[0].size, 32);
    assert_eq!(bars[1].index, 1);
    assert_eq!(bars[1].kind, BarKind::Memory64);
    assert_eq!(bars[1].base, 0xFEBF_0000);
    assert_eq!(bars[1].size, 16 * 1024);
    assert!(!bars[1].prefetchable);
}

#[test]
fn enumeration_skips_invalid_vendor_sentinel() {
    // Empty fixture — every slot reads 0xFFFFFFFF (no devices).
    let pci = Pci::new(MockConfigSpace::new(vec![]));
    let mut buf = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 4];
    assert_eq!((&pci as &dyn Bus).enumerate(&mut buf), Ok(0));
}

fn virtio_bdf() -> u64 {
    ConfigAddress {
        bus: 0,
        device: 3,
        function: 0,
        register: 0,
    }
    .pack_bdf()
}

#[test]
fn map_bar_window_hands_off_memory_bar_to_kernel_mapper() {
    let pci = Pci::new(q35_fixture());
    let mapper = MockMapper::new(true);
    // BAR1 is the 16 KiB 64-bit memory BAR at 0xFEBF_0000.
    let window = pci
        .map_bar_window(virtio_bdf(), 1, &mapper)
        .expect("memory BAR maps");
    assert_eq!(window.phys_base(), 0xFEBF_0000);
    assert_eq!(window.len(), 16 * 1024);
    // The window is usable: a register round-trips through it.
    window.write_u32(0, 0x1AF4_1000).expect("in bounds");
    assert_eq!(window.read_u32(0).expect("in bounds"), 0x1AF4_1000);
}

#[test]
fn map_bar_window_refuses_io_bar() {
    let pci = Pci::new(q35_fixture());
    let mapper = MockMapper::new(true);
    // BAR0 is an I/O-port BAR — not mappable as a register window.
    assert_eq!(
        pci.map_bar_window(virtio_bdf(), 0, &mapper).unwrap_err(),
        DriverError::Unsupported
    );
}

#[test]
fn map_bar_window_reports_not_found_for_absent_bar() {
    let pci = Pci::new(q35_fixture());
    let mapper = MockMapper::new(true);
    // BAR5 is unused on the virtio function.
    assert_eq!(
        pci.map_bar_window(virtio_bdf(), 5, &mapper).unwrap_err(),
        DriverError::NotFound
    );
}

#[test]
fn map_bar_window_propagates_capability_denial() {
    let pci = Pci::new(q35_fixture());
    // Mapper without CAP_MMIO_MAP: the hand-off must surface the
    // kernel's refusal, not synthesise a pointer.
    let mapper = MockMapper::new(false);
    assert_eq!(
        pci.map_bar_window(virtio_bdf(), 1, &mapper).unwrap_err(),
        DriverError::PermissionDenied
    );
}
