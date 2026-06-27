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
//! The tests reach into the `pub(crate)` enumeration core directly,
//! alongside the public [`crate::mechanism_one`] / [`crate::mechanism_ecam`]
//! / [`crate::mechanism_brcm`] constructors.

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::{DriverError, MmioMapError, MmioMapper, MsiMessage, RegisterWindow};

use crate::config::{
    BarKind, Capability, ConfigAddress, ConfigSpace, VIRTIO_CFG_COMMON, VIRTIO_CFG_DEVICE,
    VIRTIO_CFG_ISR, VIRTIO_CFG_NOTIFY, VIRTIO_CFG_PCI,
};
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
    state: Rc<RefCell<MockState>>,
}

#[derive(Default)]
struct MockState {
    /// Active sizing probe: `(bus, dev, fn, reg)`.
    probing: Option<(u8, u8, u8, u8)>,
    /// Log of every configuration-space write, so tests can assert the
    /// MSI-X enable hand-off without a private accessor on `Pci`.
    writes: Vec<(ConfigAddress, u32)>,
}

impl MockConfigSpace {
    fn new(funcs: Vec<MockFunction>) -> Self {
        Self {
            funcs,
            state: Rc::new(RefCell::new(MockState::default())),
        }
    }

    /// A shared handle to the mock's mutable state, cloned out before
    /// the config space is moved into a [`Pci`] so a test can inspect
    /// the write log afterwards.
    fn shared_state(&self) -> Rc<RefCell<MockState>> {
        Rc::clone(&self.state)
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
        // The BAR-sizing protocol is modelled: an FFFFFFFF write to a
        // BAR enters "probing" state, any other write (the restore)
        // leaves it. Every write is also logged so tests can assert
        // non-sizing writes such as the MSI-X enable hand-off.
        let mut st = self.state.borrow_mut();
        if value == 0xFFFF_FFFF {
            st.probing = Some((addr.bus, addr.device, addr.function, addr.register));
        } else {
            st.probing = None;
        }
        st.writes.push((addr, value));
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

// ---- BAR assignment ------------------------------------------------------

/// A single function whose 64-bit memory BAR0 is **unassigned** — its
/// address bits read zero, exactly as the VL805's BAR0 does after the
/// OS resets and re-enumerates the BCM2711 root complex. Dword 4 carries
/// only the memory-type bits, dword 5 (the high half) is zero, and the
/// 4 KiB size probe yields the writable mask.
fn vl805_like_fixture() -> MockConfigSpace {
    let func = MockFunction {
        bus: 0,
        device: 0,
        function: 0,
        regs: vec![
            id(0x1106, 0x3483),
            class(0x0C03),
            header(0x00),
            // BAR0 (dword 4): 64-bit memory (bits[2:1]=10), address zero.
            (4, 0x0000_0004),
            (5, 0x0000_0000),
        ],
        sizing: vec![
            // 4 KiB region: low-half mask, high half fully writable.
            (4, 0xFFFF_F004),
            (5, 0xFFFF_FFFF),
        ],
    };
    MockConfigSpace::new(vec![func])
}

fn vl805_bdf() -> u64 {
    ConfigAddress {
        bus: 0,
        device: 0,
        function: 0,
        register: 0,
    }
    .pack_bdf()
}

#[test]
fn assign_bar_places_an_unassigned_64bit_bar_in_the_window() {
    let cfg = vl805_like_fixture();
    let state = cfg.shared_state();
    let pci = Pci::new(cfg);
    let base = pci
        .assign_bar(vl805_bdf(), 0, 0xC000_0000, 0x4000_0000)
        .expect("the unassigned BAR is placed in the window");
    // Placed at the lowest size-aligned PCIe-bus address in the window.
    assert_eq!(base, 0xC000_0000);
    let writes = state.borrow();
    // The size-aligned base is written to the BAR's low dword with the
    // memory-type control bits preserved, and zero to its high dword.
    let last_lo = writes
        .writes
        .iter()
        .rev()
        .find(|(a, _)| a.register == 4)
        .map(|(_, v)| *v);
    let last_hi = writes
        .writes
        .iter()
        .rev()
        .find(|(a, _)| a.register == 5)
        .map(|(_, v)| *v);
    assert_eq!(last_lo, Some(0xC000_0004));
    assert_eq!(last_hi, Some(0x0000_0000));
}

#[test]
fn assign_bar_leaves_an_already_based_bar_untouched() {
    // The q35 virtio function's BAR1 is firmware-based at 0xFEBF_0000.
    let cfg = q35_fixture();
    let state = cfg.shared_state();
    let pci = Pci::new(cfg);
    let base = pci
        .assign_bar(virtio_bdf(), 1, 0xC000_0000, 0x4000_0000)
        .expect("an already-based BAR is respected");
    assert_eq!(base, 0xFEBF_0000);
    // Only the transparent size probe + restore touched the BAR: the
    // last writes to its dwords are the original values, never a new
    // base (configuration space is byte-for-byte unchanged).
    let writes = state.borrow();
    let last_lo = writes
        .writes
        .iter()
        .rev()
        .find(|(a, _)| a.register == 5)
        .map(|(_, v)| *v);
    let last_hi = writes
        .writes
        .iter()
        .rev()
        .find(|(a, _)| a.register == 6)
        .map(|(_, v)| *v);
    assert_eq!(last_lo, Some(0xFEBF_0004));
    assert_eq!(last_hi, Some(0x0000_0000));
}

#[test]
fn assign_bar_refuses_a_bar_that_does_not_fit_the_window() {
    let pci = Pci::new(vl805_like_fixture());
    // The window is smaller than the 4 KiB BAR: fail closed rather than
    // place the BAR partially outside it.
    assert_eq!(
        pci.assign_bar(vl805_bdf(), 0, 0xC000_0000, 0x800)
            .unwrap_err(),
        DriverError::OutOfRange
    );
}

#[test]
fn assign_bar_refuses_an_io_bar() {
    let pci = Pci::new(q35_fixture());
    // BAR0 of the virtio function is an I/O-port BAR.
    assert_eq!(
        pci.assign_bar(virtio_bdf(), 0, 0xC000_0000, 0x4000_0000)
            .unwrap_err(),
        DriverError::Unsupported
    );
}

#[test]
fn assign_bar_reports_not_found_for_an_absent_bar() {
    let pci = Pci::new(q35_fixture());
    // BAR5 is unused on the virtio function.
    assert_eq!(
        pci.assign_bar(virtio_bdf(), 5, 0xC000_0000, 0x4000_0000)
            .unwrap_err(),
        DriverError::NotFound
    );
}

// ---- virtio-1.x capability fixture ---------------------------------------

/// Encode one dword of a virtio vendor-specific capability header.
///
/// virtio reuses the PCI vendor-specific capability (`cap_id = 0x09`);
/// the header dword packs `cap_vndr`, `cap_next`, `cap_len`, and
/// `cfg_type` into bytes 0..=3 (virtio 1.x §4.1.4).
fn virtio_cap_header(reg: u8, next: u8, cap_len: u8, cfg_type: u8) -> (u8, u32) {
    (
        reg,
        0x09 | (u32::from(next) << 8) | (u32::from(cap_len) << 16) | (u32::from(cfg_type) << 24),
    )
}

/// A `virtio-blk-pci` (modern, device-id `0x1042`) function on
/// `00:04.0` whose four configuration structures live in a single
/// 16 KiB 64-bit memory BAR (BAR4) at `0xFE00_0000`:
///
/// * common cfg  — offset `0x0000`, length `0x38`
/// * notify cfg  — offset `0x1000`, length `0x1000`, multiplier `4`
/// * ISR cfg     — offset `0x2000`, length `0x1000`
/// * device cfg  — offset `0x3000`, length `0x1000`
///
/// The capability list is `common -> notify -> ISR -> device`, the
/// layout QEMU's `virtio-blk-pci` advertises, so a future live-on-QEMU
/// test asserts the same triples this host test does.
fn virtio_blk_fixture() -> MockConfigSpace {
    let host_bridge = MockFunction {
        bus: 0,
        device: 0,
        function: 0,
        regs: vec![id(0x8086, 0x29C0), class(0x0600), header(0x00)],
        sizing: vec![],
    };

    let virtio_blk = MockFunction {
        bus: 0,
        device: 4,
        function: 0,
        regs: vec![
            id(0x1AF4, 0x1042),
            status_with_caplist(),
            class(0x0100),
            header(0x00),
            // BAR4 — 64-bit memory at 0xFE00_0000 (low dword sets the
            // memory + 64-bit-type bits; the high dword is zero).
            (8, 0xFE00_0004),
            (9, 0x0000_0000),
            cap_pointer(0x40),
            // common cfg @ 0x40 -> next 0x50, len 0x10, cfg_type 1.
            virtio_cap_header(16, 0x50, 0x10, VIRTIO_CFG_COMMON),
            (17, 0x0000_0004), // bar = 4
            (18, 0x0000_0000), // bar_offset = 0
            (19, 0x0000_0038), // length = 0x38
            // notify cfg @ 0x50 -> next 0x68, len 0x14, cfg_type 2.
            virtio_cap_header(20, 0x68, 0x14, VIRTIO_CFG_NOTIFY),
            (21, 0x0000_0004), // bar = 4
            (22, 0x0000_1000), // bar_offset = 0x1000
            (23, 0x0000_1000), // length = 0x1000
            (24, 0x0000_0004), // notify_off_multiplier = 4
            // ISR cfg @ 0x68 -> next 0x78, len 0x10, cfg_type 3.
            virtio_cap_header(26, 0x78, 0x10, VIRTIO_CFG_ISR),
            (27, 0x0000_0004), // bar = 4
            (28, 0x0000_2000), // bar_offset = 0x2000
            (29, 0x0000_1000), // length = 0x1000
            // device cfg @ 0x78 -> next 0x00, len 0x10, cfg_type 4.
            virtio_cap_header(30, 0x00, 0x10, VIRTIO_CFG_DEVICE),
            (31, 0x0000_0004), // bar = 4
            (32, 0x0000_3000), // bar_offset = 0x3000
            (33, 0x0000_1000), // length = 0x1000
        ],
        // BAR4 is 16 KiB: it must span the device cfg's end (0x4000).
        sizing: vec![(8, 0xFFFF_C004)],
    };

    MockConfigSpace::new(vec![host_bridge, virtio_blk])
}

fn virtio_blk_bdf() -> u64 {
    ConfigAddress {
        bus: 0,
        device: 4,
        function: 0,
        register: 0,
    }
    .pack_bdf()
}

#[test]
fn capabilities_walker_decodes_virtio_structures_in_order() {
    let pci = Pci::new(virtio_blk_fixture());
    let mut out = [Capability::Other { offset: 0, id: 0 }; 8];
    let n = pci
        .capabilities(virtio_blk_bdf(), &mut out)
        .expect("cap walk ok");
    assert_eq!(n, 4);
    assert_eq!(
        out[0],
        Capability::Virtio {
            offset: 0x40,
            cfg_type: VIRTIO_CFG_COMMON,
            bar: 4,
            bar_offset: 0x0000,
            length: 0x38,
        }
    );
    assert_eq!(
        out[1],
        Capability::VirtioNotify {
            offset: 0x50,
            bar: 4,
            bar_offset: 0x1000,
            length: 0x1000,
            notify_off_multiplier: 4,
        }
    );
    assert_eq!(
        out[2],
        Capability::Virtio {
            offset: 0x68,
            cfg_type: VIRTIO_CFG_ISR,
            bar: 4,
            bar_offset: 0x2000,
            length: 0x1000,
        }
    );
    assert_eq!(
        out[3],
        Capability::Virtio {
            offset: 0x78,
            cfg_type: VIRTIO_CFG_DEVICE,
            bar: 4,
            bar_offset: 0x3000,
            length: 0x1000,
        }
    );
}

#[test]
fn map_virtio_window_hands_off_each_cfg_region() {
    let pci = Pci::new(virtio_blk_fixture());
    let mapper = MockMapper::new(true);
    let bdf = virtio_blk_bdf();

    let common = pci
        .map_virtio_window(bdf, VIRTIO_CFG_COMMON, &mapper)
        .expect("common cfg maps");
    assert_eq!(common.phys_base(), 0xFE00_0000);
    assert_eq!(common.len(), 0x38);
    // The window is usable: a register round-trips through it.
    common.write_u32(0, 0xDEAD_BEEF).expect("in bounds");
    assert_eq!(common.read_u32(0).expect("in bounds"), 0xDEAD_BEEF);

    let notify = pci
        .map_virtio_window(bdf, VIRTIO_CFG_NOTIFY, &mapper)
        .expect("notify cfg maps");
    assert_eq!(notify.phys_base(), 0xFE00_1000);
    assert_eq!(notify.len(), 0x1000);

    let isr = pci
        .map_virtio_window(bdf, VIRTIO_CFG_ISR, &mapper)
        .expect("isr cfg maps");
    assert_eq!(isr.phys_base(), 0xFE00_2000);
    assert_eq!(isr.len(), 0x1000);

    let device = pci
        .map_virtio_window(bdf, VIRTIO_CFG_DEVICE, &mapper)
        .expect("device cfg maps");
    assert_eq!(device.phys_base(), 0xFE00_3000);
    assert_eq!(device.len(), 0x1000);
}

#[test]
fn virtio_notify_off_multiplier_reads_notify_cap() {
    let pci = Pci::new(virtio_blk_fixture());
    assert_eq!(pci.virtio_notify_off_multiplier(virtio_blk_bdf()), Ok(4));
}

#[test]
fn map_virtio_window_reports_not_found_for_absent_cfg_type() {
    let pci = Pci::new(virtio_blk_fixture());
    let mapper = MockMapper::new(true);
    // The fixture advertises no PCI-window structure (cfg_type 5).
    assert_eq!(
        pci.map_virtio_window(virtio_blk_bdf(), VIRTIO_CFG_PCI, &mapper)
            .unwrap_err(),
        DriverError::NotFound
    );
}

#[test]
fn map_virtio_window_propagates_capability_denial() {
    let pci = Pci::new(virtio_blk_fixture());
    // Mapper without CAP_MMIO_MAP: the hand-off surfaces the kernel's
    // refusal rather than synthesising a pointer.
    let mapper = MockMapper::new(false);
    assert_eq!(
        pci.map_virtio_window(virtio_blk_bdf(), VIRTIO_CFG_COMMON, &mapper)
            .unwrap_err(),
        DriverError::PermissionDenied
    );
}

// ---- MSI-X interrupt routing ---------------------------------------------

/// A virtio function whose MSI-X table lives in the *I/O* BAR (BAR0).
/// Such a table is not memory-mappable; [`Pci::route_msix`] must refuse
/// it rather than pretend to map an I/O-port BAR as a register window.
fn msix_io_table_fixture() -> MockConfigSpace {
    let func = MockFunction {
        bus: 0,
        device: 5,
        function: 0,
        regs: vec![
            id(0x1AF4, 0x1041),
            status_with_caplist(),
            class(0x0200),
            header(0x00),
            // BAR0 — I/O at 0xC000.
            (4, 0x0000_C001),
            cap_pointer(0x50),
            // MSI-X cap @ 0x50 (dword 20): id=0x11, next=0, table_size=4.
            #[allow(clippy::identity_op)]
            (20, (0x0003u32 << 16) | (0x00u32 << 8) | 0x11_u32),
            // Table off/BIR @ 0x54 (dword 21): table_bar=0 (the I/O BAR).
            (21, 0x0000_0000),
            // PBA off/BIR @ 0x58 (dword 22): pba_bar=0, offset=0x80.
            (22, 0x0000_0080),
        ],
        sizing: vec![(4, 0xFFFF_FFE1)],
    };
    MockConfigSpace::new(vec![func])
}

fn msix_io_table_bdf() -> u64 {
    ConfigAddress {
        bus: 0,
        device: 5,
        function: 0,
        register: 0,
    }
    .pack_bdf()
}

#[test]
fn route_msix_programs_entry_and_enables_function() {
    let config = q35_fixture();
    let state = config.shared_state();
    let pci = Pci::new(config);
    let mapper = MockMapper::new(true);
    let message = MsiMessage {
        address: 0xFEE0_1000,
        data: 0x0000_0030,
    };

    pci.route_msix(virtio_bdf(), 0, message, &mapper)
        .expect("routes entry 0");

    // The table entry was written into the mapped window. The mock
    // mapper backs every window at the start of its buffer, so the
    // four dwords of the entry land at backing[0..4].
    let backing = mapper.backing.borrow();
    assert_eq!(backing[0], 0xFEE0_1000, "message address low");
    assert_eq!(backing[1], 0x0000_0000, "message address high");
    assert_eq!(backing[2], 0x0000_0030, "message data");
    assert_eq!(backing[3], 0x0000_0000, "vector control: entry unmasked");

    // MSI-X was enabled function-wide: the capability header dword
    // (register 20) was written with the enable bit (bit 31) set and
    // the function mask (bit 30) clear, preserving cap_id / next /
    // table-size in the low bits.
    let st = state.borrow();
    let enable = st
        .writes
        .iter()
        .rev()
        .find(|(a, _)| a.bus == 0 && a.device == 3 && a.function == 0 && a.register == 20);
    assert_eq!(enable.map(|(_, v)| *v), Some(0x8003_0011));
}

#[test]
fn route_msix_enables_memory_space_and_bus_master() {
    let config = q35_fixture();
    let state = config.shared_state();
    let pci = Pci::new(config);
    let mapper = MockMapper::new(true);
    let message = MsiMessage {
        address: 0xFEE0_1000,
        data: 0x0000_0030,
    };

    pci.route_msix(virtio_bdf(), 0, message, &mapper)
        .expect("routes entry 0");

    // The function's Command register (dword 1) was written with
    // Memory Space Enable (bit 1) and Bus Master Enable (bit 2) set:
    // without bus mastering the device could neither DMA the
    // virtqueues nor deliver the MSI-X message it was just handed.
    let st = state.borrow();
    let command = st
        .writes
        .iter()
        .rev()
        .find(|(a, _)| a.bus == 0 && a.device == 3 && a.function == 0 && a.register == 1)
        .map(|(_, v)| *v)
        .expect("command register written");
    assert_eq!(command & 0b110, 0b110, "memory-space + bus-master enabled");
    // The high-16 status bits were not re-asserted (RW1C safety).
    assert_eq!(command >> 16, 0, "status half written as zero");
}

#[test]
fn route_msix_reports_not_found_without_msix_capability() {
    let pci = Pci::new(q35_fixture());
    let mapper = MockMapper::new(true);
    // The LPC function advertises no capability list at all.
    let lpc_bdf = ConfigAddress {
        bus: 0,
        device: 0x1F,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let message = MsiMessage {
        address: 0xFEE0_0000,
        data: 0x30,
    };
    assert_eq!(
        pci.route_msix(lpc_bdf, 0, message, &mapper).unwrap_err(),
        DriverError::NotFound
    );
}

#[test]
fn route_msix_rejects_entry_beyond_table() {
    let pci = Pci::new(q35_fixture());
    let mapper = MockMapper::new(true);
    let message = MsiMessage {
        address: 0xFEE0_0000,
        data: 0x30,
    };
    // The q35 virtio function's MSI-X table holds 4 entries (0..=3).
    assert_eq!(
        pci.route_msix(virtio_bdf(), 4, message, &mapper)
            .unwrap_err(),
        DriverError::OutOfRange
    );
}

#[test]
fn route_msix_refuses_io_bar_table() {
    let pci = Pci::new(msix_io_table_fixture());
    let mapper = MockMapper::new(true);
    let message = MsiMessage {
        address: 0xFEE0_0000,
        data: 0x30,
    };
    assert_eq!(
        pci.route_msix(msix_io_table_bdf(), 0, message, &mapper)
            .unwrap_err(),
        DriverError::Unsupported
    );
}

#[test]
fn route_msix_propagates_capability_denial() {
    let pci = Pci::new(q35_fixture());
    // Mapper without CAP_MMIO_MAP: the table write must surface the
    // kernel's refusal rather than synthesise a pointer.
    let mapper = MockMapper::new(false);
    let message = MsiMessage {
        address: 0xFEE0_0000,
        data: 0x30,
    };
    assert_eq!(
        pci.route_msix(virtio_bdf(), 0, message, &mapper)
            .unwrap_err(),
        DriverError::PermissionDenied
    );
}

// ---- MSI (not MSI-X) interrupt routing -----------------------------------

/// A function advertising the legacy **MSI** capability at byte 0x50,
/// 64-bit-address capable — the shape the Pi 4's VL805 xHCI presents.
/// `addr64` selects whether the Message Control "64-bit capable" bit
/// (MC bit 7) is set, and `per_vector_masking` selects whether the mask
/// register is present, so tests can exercise both data-register placements
/// and the optional device-side MSI mask.
fn msi_fixture(addr64: bool, per_vector_masking: bool) -> MockConfigSpace {
    // Message Control occupies the high 16 bits of the header dword;
    // bit 7 advertises 64-bit addressing and bit 8 advertises per-vector
    // mask/pending registers.
    let msg_ctrl: u32 =
        (if addr64 { 0x0080 } else { 0x0000 }) | if per_vector_masking { 0x0100 } else { 0x0000 };
    let func = MockFunction {
        bus: 0,
        device: 6,
        function: 0,
        regs: vec![
            id(VL805_VENDOR, VL805_DEVICE),
            status_with_caplist(),
            class(0x0C03),
            header(0x00),
            cap_pointer(0x50),
            // MSI cap @ 0x50 (dword 20): id=0x05, next=0, Message Control.
            (20, (msg_ctrl << 16) | 0x05),
        ],
        sizing: vec![],
    };
    MockConfigSpace::new(vec![func])
}

fn msi_bdf() -> u64 {
    ConfigAddress {
        bus: 0,
        device: 6,
        function: 0,
        register: 0,
    }
    .pack_bdf()
}

#[test]
fn route_msi_programs_address_data_and_enables_single_vector() {
    let config = msi_fixture(true, false);
    let state = config.shared_state();
    let pci = Pci::new(config);
    // A BCM2711-style doorbell pair: the RC MSI controller's target
    // address and the data word selecting one vector.
    let message = MsiMessage {
        address: 0xFFFF_FFFC,
        data: 0x0000_6540,
    };

    pci.route_msi(msi_bdf(), message).expect("routes msi");

    let st = state.borrow();
    let find = |register: u8| {
        st.writes
            .iter()
            .rev()
            .find(|(a, _)| a.bus == 0 && a.device == 6 && a.function == 0 && a.register == register)
            .map(|(_, v)| *v)
    };
    // Message Address low at cap+4 (dword 21), with bits 1:0 forced 0.
    assert_eq!(find(21), Some(0xFFFF_FFFC), "message address low");
    // Message Address high at cap+8 (dword 22) — zero here.
    assert_eq!(find(22), Some(0x0000_0000), "message address high");
    // Message Data at cap+0x0C (dword 23), 16-bit value in the low half.
    assert_eq!(find(23), Some(0x0000_6540), "message data");
    // Header (dword 20): MSI Enable set, Multiple Message Enable cleared
    // (one vector), cap_id/next preserved in the low byte.
    let header = find(20).expect("header written");
    assert_eq!(header & (1 << 16), 1 << 16, "MSI Enable set");
    assert_eq!(header & (0x7 << 20), 0, "Multiple Message Enable cleared");
    assert_eq!(header & 0xFF, 0x05, "cap_id preserved");
    // Bus mastering was enabled (an MSI is an upstream memory write).
    let command = find(1).expect("command written");
    assert_eq!(command & 0b100, 0b100, "bus-master enabled");
}

#[test]
fn route_msi_unmasks_64bit_per_vector_mask_register() {
    let config = msi_fixture(true, true);
    let state = config.shared_state();
    let pci = Pci::new(config);
    let message = MsiMessage {
        address: 0xFFFF_FFFC,
        data: 0x0000_6540,
    };

    pci.route_msi(msi_bdf(), message).expect("routes msi");

    let st = state.borrow();
    assert!(
        st.writes.iter().any(|(a, v)| {
            a.bus == 0 && a.device == 6 && a.function == 0 && a.register == 24 && *v == 0
        }),
        "64-bit MSI mask register at cap+0x10 is cleared"
    );
}

#[test]
fn route_msi_unmasks_32bit_per_vector_mask_register() {
    let config = msi_fixture(false, true);
    let state = config.shared_state();
    let pci = Pci::new(config);
    let message = MsiMessage {
        address: 0xFFFF_FFFC,
        data: 0x0000_6540,
    };

    pci.route_msi(msi_bdf(), message).expect("routes msi");

    let st = state.borrow();
    assert!(
        st.writes.iter().any(|(a, v)| {
            a.bus == 0 && a.device == 6 && a.function == 0 && a.register == 23 && *v == 0
        }),
        "32-bit MSI mask register at cap+0x0c is cleared"
    );
}

#[test]
fn route_msi_reports_not_found_without_msi_capability() {
    // The q35 virtio function advertises MSI-X, not legacy MSI.
    let pci = Pci::new(q35_fixture());
    let message = MsiMessage {
        address: 0xFFFF_FFFC,
        data: 0x6540,
    };
    assert_eq!(
        pci.route_msi(virtio_bdf(), message).unwrap_err(),
        DriverError::NotFound
    );
}

#[test]
fn route_msi_rejects_a_64bit_address_on_a_32bit_capability() {
    // A 32-bit-only MSI capability cannot express a doorbell above 4 GiB:
    // writing the low half alone would deliver to the wrong address, so
    // fail closed rather than silently truncate.
    let pci = Pci::new(msi_fixture(false, false));
    let message = MsiMessage {
        address: 0x1_0000_0000,
        data: 0x6540,
    };
    assert_eq!(
        pci.route_msi(msi_bdf(), message).unwrap_err(),
        DriverError::OutOfRange
    );
}

/// The mechanism-#1 constructor yields a value usable through all
/// three frozen `abi-v1` bus seams without naming the concrete `Pci`
/// type. Construction stores the [`PortIo`] backend and issues no port
/// I/O, so it is sound to run on the host with the inert mock below
/// (no `0xCF8`/`0xCFC` access happens here); the real x86_64 backend
/// lives in `kernel/arch/x86_64::pio`.
#[test]
fn mechanism_one_exposes_the_frozen_bus_seams() {
    use rustos_abi::driver::msix::MsixBus;
    use rustos_abi::driver::virtio_pci::VirtioPciBus;
    use rustos_abi::PortIo;

    /// Inert backend: the seam-coercion assertions never read or write
    /// a port, so the methods are never reached.
    struct NoopPortIo;
    impl PortIo for NoopPortIo {
        fn read32(&self, _port: u16) -> u32 {
            0xFFFF_FFFF
        }
        fn write32(&self, _port: u16, _value: u32) {}
    }

    fn assert_seams(_: &dyn Bus, _: &dyn VirtioPciBus, _: &dyn MsixBus) {}

    let bus = crate::mechanism_one(NoopPortIo);
    assert_seams(&bus, &bus, &bus);
}

// ---- ECAM (PCIe enhanced configuration access) ---------------------------
//
// The Raspberry Pi 4 (BCM2711) reaches its VL805 USB host controller
// through ECAM, not the x86 legacy ports. The fixture below lays a
// real configuration region flat into a heap buffer — a root-port
// bridge at 00:00.0 and the VL805 xHCI at 01:00.0 — and drives the
// enumeration core over it through `EcamConfigSpace`, exactly as the
// ring-0 boot walk drives a kernel-mapped ECAM window. The
// read-only enumeration and capability-walk paths are mechanism-
// independent, so they are realistic over a plain memory backing;
// the destructive BAR *size* probe depends on the hardware's
// read-only BAR bits (already covered by the MockConfigSpace
// fixtures above) and is not asserted here.

use crate::mech_ecam::EcamConfigSpace;

/// VID/DID of the VIA VL805, the Pi 4's PCIe-attached xHCI controller.
const VL805_VENDOR: u16 = 0x1106;
const VL805_DEVICE: u16 = 0x3483;

/// Plant one configuration dword at `(bus, device, function, register)`
/// into the flat ECAM `backing`.
fn put_ecam(backing: &mut [u32], bus: u8, device: u8, function: u8, register: u8, value: u32) {
    let off = ConfigAddress {
        bus,
        device,
        function,
        register,
    }
    .ecam_offset()
    .expect("address in range");
    backing[off / 4] = value;
}

/// Build a flat ECAM region (two 1 MiB bus blocks) holding a root-port
/// bridge at 00:00.0 and the VL805 xHCI at 01:00.0, plus the heap
/// `Vec` that owns it (returned so it outlives the window).
fn vl805_ecam_region() -> (Vec<u32>, RegisterWindow) {
    // Two buses × 1 MiB = 2 MiB region. An absent function's
    // configuration space reads all-ones on real hardware (the host
    // bridge master-aborts), so the region starts filled with the
    // sentinel and the present functions are planted over it.
    let mut backing = vec![0xFFFF_FFFFu32; (2 * 0x10_0000) / 4];

    // 00:00.0 — PCIe root port (class 0x0604, type-1 header).
    put_ecam(&mut backing, 0, 0, 0, 0, 0x14E4); // Broadcom host bridge ID (device 0x0000).
    put_ecam(&mut backing, 0, 0, 0, 2, 0x0604 << 16); // class = PCI bridge.
    put_ecam(&mut backing, 0, 0, 0, 3, 0x01 << 16); // header type 1.

    // 01:00.0 — VL805 xHCI (class 0x0C03, type-0, MSI-X capable).
    put_ecam(
        &mut backing,
        1,
        0,
        0,
        0,
        (u32::from(VL805_DEVICE) << 16) | u32::from(VL805_VENDOR),
    );
    put_ecam(&mut backing, 1, 0, 0, 1, (1u32 << 4) << 16); // status: cap list present.
    put_ecam(&mut backing, 1, 0, 0, 2, 0x0C_03_30 << 8); // class = xHCI USB host (prog-if 0x30).
    put_ecam(&mut backing, 1, 0, 0, 3, 0x00 << 16); // header type 0.
    put_ecam(&mut backing, 1, 0, 0, 4, 0x6010_0000); // BAR0: 32-bit memory, base 0x6010_0000.
    put_ecam(&mut backing, 1, 0, 0, 13, 0x80); // cap pointer -> byte 0x80.
                                               // MSI-X cap at byte 0x80 (dword 32): id=0x11, next=0, table_size=8.
    put_ecam(&mut backing, 1, 0, 0, 32, (0x0007u32 << 16) | 0x11);
    put_ecam(&mut backing, 1, 0, 0, 33, 0x0000_1000); // table_bar=0, offset 0x1000.
    put_ecam(&mut backing, 1, 0, 0, 34, 0x0000_2000); // pba_bar=0, offset 0x2000.

    let base = NonNull::new(backing.as_mut_ptr().cast::<u8>()).expect("non-null heap buffer");
    let len = backing.len() * 4;
    // SAFETY: `base` is 4-byte aligned (the `Vec<u32>` allocation
    // guarantee) and covers exactly `len` bytes; the backing `Vec` is
    // returned to the caller so it outlives the window, and no other
    // reference aliases it while the window is live.
    let window = unsafe { RegisterWindow::from_mapping(0x6000_0000, base, len) };
    (backing, window)
}

#[test]
fn ecam_enumeration_finds_root_port_and_vl805() {
    let (_backing, window) = vl805_ecam_region();
    let pci = Pci::new(EcamConfigSpace::new(window));
    let mut buf = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; 8];
    let n = (&pci as &dyn Bus).enumerate(&mut buf).expect("enumerates");
    let got: Vec<_> = buf[..n].to_vec();
    let want = vec![
        BusDevice {
            vendor: 0x14E4,
            device: 0x0000,
            class: 0x0604,
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
            vendor: u32::from(VL805_VENDOR),
            device: u32::from(VL805_DEVICE),
            class: 0x0C03,
            reserved0: 0,
            address: ConfigAddress {
                bus: 1,
                device: 0,
                function: 0,
                register: 0,
            }
            .pack_bdf(),
        },
    ];
    assert_eq!(got, want);
}

#[test]
fn ecam_capability_walk_decodes_vl805_msix() {
    let (_backing, window) = vl805_ecam_region();
    let pci = Pci::new(EcamConfigSpace::new(window));
    let vl805 = ConfigAddress {
        bus: 1,
        device: 0,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let mut out = [Capability::Other { offset: 0, id: 0 }; 4];
    let n = pci.capabilities(vl805, &mut out).expect("cap walk ok");
    assert_eq!(n, 1);
    assert_eq!(
        out[0],
        Capability::MsiX {
            offset: 0x80,
            table_size: 8,
            table_bar: 0,
            table_offset: 0x1000,
            pba_bar: 0,
            pba_offset: 0x2000,
        }
    );
}

/// The ECAM constructor yields a value usable through all three frozen
/// `abi-v1` bus seams without naming the concrete `Pci` type, mirroring
/// [`mechanism_one_exposes_the_frozen_bus_seams`].
#[test]
fn mechanism_ecam_exposes_the_frozen_bus_seams() {
    use rustos_abi::driver::msix::MsixBus;
    use rustos_abi::driver::virtio_pci::VirtioPciBus;

    fn assert_seams(_: &dyn Bus, _: &dyn VirtioPciBus, _: &dyn MsixBus) {}

    let (_backing, window) = vl805_ecam_region();
    let bus = crate::mechanism_ecam(window);
    assert_seams(&bus, &bus, &bus);
}

/// The ECAM constructor's value is also reachable through the
/// generic-PCI [`PciBus`] seam: a non-virtio,
/// DMA-driving device driver maps a BAR and enables bus mastering
/// through `&dyn PciBus` without naming the concrete `Pci` type.
#[test]
fn mechanism_ecam_exposes_the_pci_bus_seam() {
    use rustos_abi::driver::pci::PciBus;

    fn assert_pci_bus(_: &dyn PciBus) {}

    let (_backing, window) = vl805_ecam_region();
    let bus = crate::mechanism_ecam(window);
    assert_pci_bus(&bus);
}

/// `enable_bus_master` sets the command register's Memory Space Enable
/// and Bus Master Enable bits on the VL805 while leaving the RW1C
/// status half untouched (the same activation
/// `route_msix` performs).
#[test]
fn pci_bus_enable_bus_master_sets_command_bits() {
    use rustos_abi::driver::pci::PciBus;

    let (backing, window) = vl805_ecam_region();
    let vl805 = ConfigAddress {
        bus: 1,
        device: 0,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let pci = crate::mechanism_ecam(window);
    (&pci as &dyn PciBus)
        .enable_bus_master(vl805)
        .expect("enable bus master");
    // Re-read the command/status dword (register 1) straight from the
    // backing: bits 1 (memory space) and 2 (bus master) set, status
    // half (high 16) still zero.
    let off = ConfigAddress {
        bus: 1,
        device: 0,
        function: 0,
        register: 1,
    }
    .ecam_offset()
    .expect("address in range");
    let command = backing[off / 4];
    assert_eq!(command & 0x6, 0x6);
    assert_eq!(command >> 16, 0);
}

/// `map_bar_window` resolves the VL805's memory BAR0 and routes the
/// mapping through the supplied [`MmioMapper`] (the kernel allocates
/// the window).
#[test]
fn pci_bus_map_bar_window_maps_vl805_bar0() {
    use rustos_abi::driver::pci::PciBus;

    let (_backing, window) = vl805_ecam_region();
    let vl805 = ConfigAddress {
        bus: 1,
        device: 0,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let pci = crate::mechanism_ecam(window);
    let mapper = MockMapper::new(true);
    let bar = (&pci as &dyn PciBus)
        .map_bar_window(vl805, 0, &mapper)
        .expect("map bar0");
    // 32-bit memory BAR planted with base 0x6010_0000; the size probe
    // over the flat backing reads a 16-byte span.
    assert_eq!(bar.len(), 0x10);
}

/// `map_bar_window` fails closed when the requested BAR slot is unused.
#[test]
fn pci_bus_map_bar_window_rejects_absent_bar() {
    use rustos_abi::driver::pci::PciBus;

    let (_backing, window) = vl805_ecam_region();
    let vl805 = ConfigAddress {
        bus: 1,
        device: 0,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let pci = crate::mechanism_ecam(window);
    let mapper = MockMapper::new(true);
    // BAR5 was never planted; it reads as the all-ones sentinel and
    // resolves to an (I/O-looking) unused slot — not a mappable
    // memory window.
    assert!((&pci as &dyn PciBus)
        .map_bar_window(vl805, 5, &mapper)
        .is_err());
}

/// `describe_function` emits the VL805 as a discovered child node whose
/// PCI match key carries its full 24-bit class, so the generic xHCI
/// driver's wildcard bind key (`0x0C_03_30`) resolves against it
/// (autoload is match *data*, not composition).
#[test]
fn describe_function_emits_the_vl805_child_node() {
    use rustos_abi::driver::pci::PciBus;
    use rustos_abi::{HwDeviceClass, HwMatchKey};

    let (_backing, window) = vl805_ecam_region();
    let vl805 = ConfigAddress {
        bus: 1,
        device: 0,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let pci = crate::mechanism_ecam(window);
    let node = (&pci as &dyn PciBus)
        .describe_function(vl805)
        .expect("describes the VL805");
    // Identity (id/parent) is unassigned here — the `hw_emit_node` publish
    // path assigns it; only the match key matters.
    // A serial-bus (USB host) controller is a bus to further devices.
    assert_eq!(node.class(), Some(HwDeviceClass::Bus));
    assert_eq!(node.match_keys().len(), 1);
    let key = node.match_keys()[0];
    assert_eq!(key.vendor(), VL805_VENDOR);
    assert_eq!(key.product(), VL805_DEVICE);
    assert_eq!(key.class(), 0x0C_03_30);
    // The generic xHCI bind key (class only, vendor/device wildcard)
    // binds; a key naming the older USB class (prog-if `0x20`, EHCI)
    // does not.
    assert!(HwMatchKey::pci(0, 0, 0x0C_03_30).matches(&key));
    assert!(!HwMatchKey::pci(0, 0, 0x0C_03_20).matches(&key));
}

/// `describe_function` fails closed on a `bdf` with no responding
/// function (the all-ones vendor sentinel), never fabricating a node.
#[test]
fn describe_function_rejects_an_absent_function() {
    use rustos_abi::driver::pci::PciBus;

    let (_backing, window) = vl805_ecam_region();
    // 00:01.0 was never planted: it reads all-ones.
    let absent = ConfigAddress {
        bus: 0,
        device: 1,
        function: 0,
        register: 0,
    }
    .pack_bdf();
    let pci = crate::mechanism_ecam(window);
    assert!(matches!(
        (&pci as &dyn PciBus).describe_function(absent),
        Err(DriverError::NotFound)
    ));
}
