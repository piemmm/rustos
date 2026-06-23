//! Host tests for the VL805 driver-host wiring.
//!
//! QEMU models no Pi USB timing (`AGENTS.md` §0.4 / §2.1), so these
//! tests prove the [`open_discovered`] composition and its fail-closed
//! paths against in-process mocks (a recording [`PciBus`], an MMIO
//! mapper backing real heap memory, and a DMA host minting leaked
//! [`DmaSlab`]s). The live controller bring-up — the first 32-bit read
//! off a real BAR returning a plausible `CAPLENGTH` — is the on-metal
//! acceptance item; over the inert zeroed window here [`Xhci::open`]
//! fails closed with [`DriverError::DeviceFault`], which is exactly
//! the boundary the "reaches the controller" test asserts.

extern crate alloc;

use alloc::boxed::Box;
use core::cell::Cell;
use core::ptr::NonNull;

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::dma::{DmaHost, DmaSlab, PoolId};
use rustos_abi::driver::mmio::MmioMapError;
use rustos_abi::driver::virtio::VirtioHost;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwDeviceClass, HwMatchKey, HwNode,
    MmioMapper, PciBus, RegisterWindow,
};

use super::{bring_up_boot_input, map_controller, open_discovered};
use rustos_pci::USB_CONTROLLER_CLASS;

/// A no-op [`Delay`] for the host tests (QEMU models no controller
/// timing; the live waits are the metal-acceptance item).
struct NoDelay;

impl rustos_abi::Delay for NoDelay {
    fn delay_us(&self, _us: u32) {}
    fn now_us(&self) -> u64 {
        0
    }
}

/// Device-visible base the DMA host hands out for an in-aperture carve.
const DMA_PHYS_IN_APERTURE: u64 = 0x1000_0000;
/// Inbound-DMA aperture top comfortably above the in-aperture carve.
const APERTURE_TOP: u64 = 0xC000_0000;
/// Outbound (CPU→PCIe) window the controller's BAR is assigned from,
/// in the PCIe-bus address space — `(pcie_base, size)`.
const OUTBOUND_WINDOW: (u64, u64) = (0x6000_0000, 0x1000_0000);

/// Leak a `len`-byte, 4-byte-aligned buffer and return a pointer to it.
///
/// The leak is deliberate: a window/slab minted here lives for the
/// whole test process, satisfying the lifetime contracts of
/// [`RegisterWindow::from_mapping`] and [`DmaSlab::from_leaked`]
/// without bookkeeping (these are the mock-host's `'static` storage
/// strategy, mirroring `lib/abi`'s own `from_leaked` doc).
fn leak_aligned(len: usize) -> NonNull<u8> {
    let words = len.div_ceil(4).max(1);
    let buf: Box<[u32]> = alloc::vec![0u32; words].into_boxed_slice();
    NonNull::new(Box::leak(buf).as_mut_ptr().cast::<u8>()).expect("non-null leaked buffer")
}

/// MMIO mapper backing every window with leaked, 4-byte-aligned heap.
struct MockMapper {
    grant: bool,
}

impl MmioMapper for MockMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        if !self.grant {
            return Err(MmioMapError::CapabilityMissing);
        }
        if len == 0 {
            return Err(MmioMapError::InvalidRegion);
        }
        let base = leak_aligned(len);
        // SAFETY: `base` covers `len` bytes, is 4-byte aligned, lives
        // for the whole test process (leaked), and no other reference
        // aliases it.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

/// DMA host minting one leaked slab at a fixed device-visible base.
struct MockDmaHost {
    phys: u64,
    fail: bool,
}

impl DmaHost for MockDmaHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        if self.fail {
            return Err(DriverError::LengthOutOfRange);
        }
        let ptr = leak_aligned(size);
        // SAFETY: `ptr` covers `size` zeroed bytes and lives for the
        // whole test process; `phys` is the test's device-visible base
        // for `ptr[0]`. Drop is a no-op (the `from_leaked` contract).
        Ok(unsafe { DmaSlab::from_leaked(self.phys, ptr, size, PoolId::MOCK, 0) })
    }
}

impl VirtioHost for MockDmaHost {
    fn notify_wait(&self, _queue_index: u16) {}
}

/// Recording PCI bus: enumerates one function of `class` and tracks
/// whether bus mastering was enabled.
struct MockPciBus {
    class: u16,
    address: u64,
    master_enabled: Cell<bool>,
    /// The `(window_base, window_size)` the last `assign_bar` was asked
    /// to place the BAR inside, so a test can assert the bring-up routed
    /// the outbound window through to BAR assignment.
    assigned: Cell<Option<(u64, u64)>>,
}

impl MockPciBus {
    fn usb() -> Self {
        Self {
            class: USB_CONTROLLER_CLASS,
            address: 0x0001_0000,
            master_enabled: Cell::new(false),
            assigned: Cell::new(None),
        }
    }
}

impl Bus for MockPciBus {
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        if out.is_empty() {
            return Err(DriverError::BufferTooSmall);
        }
        out[0] = BusDevice {
            vendor: 0x1106,
            device: 0x3483,
            class: self.class,
            reserved0: 0,
            address: self.address,
        };
        Ok(1)
    }
}

impl PciBus for MockPciBus {
    fn map_bar_window(
        &self,
        _bdf: u64,
        bar_index: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        if bar_index != 0 {
            return Err(DriverError::NotFound);
        }
        mapper
            .map_window(0x6000_0000, 0x1000)
            .map_err(MmioMapError::as_driver_error)
    }

    fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
        self.master_enabled.set(true);
        Ok(())
    }

    fn assign_bar(
        &self,
        _bdf: u64,
        bar_index: u8,
        window_base: u64,
        window_size: u64,
    ) -> Result<u64, DriverError> {
        if bar_index != 0 {
            return Err(DriverError::NotFound);
        }
        self.assigned.set(Some((window_base, window_size)));
        Ok(window_base)
    }

    fn read_config(&self, _bdf: u64, _offset: u16) -> Result<u32, DriverError> {
        Ok(0)
    }

    fn describe_function(&self, _bdf: u64) -> Result<HwNode, DriverError> {
        // The mock carries only the 16-bit base+sub-class; promote it
        // to the 24-bit code (prog-if 0) for the emitted key. Identity
        // (id/parent) is unassigned: the kernel assigns it on publish
        // (`AGENTS.md` §4 / §18.1).
        let class24 = u32::from(self.class) << 8;
        let mut node = HwNode::new(0, rustos_abi::hwtree::HW_NODE_ROOT, HwDeviceClass::Bus);
        node.push_match_key(HwMatchKey::pci(0x1106, 0x3483, class24))
            .map_err(|_| DriverError::DeviceFault)?;
        Ok(node)
    }
}

/// A host granting `MMIO_MAP`, with an optional mapper and DMA host, that
/// records the last node handed to [`DriverHost::emit_node`].
struct MockHost {
    mmio_map: bool,
    mapper: Option<MockMapper>,
    dma: Option<MockDmaHost>,
    emitted: Cell<Option<HwNode>>,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => true,
            CapabilityId::MMIO_MAP => self.mmio_map,
            _ => false,
        }
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        self.mapper.as_ref().map(|m| m as &dyn MmioMapper)
    }

    fn dma_host(&self) -> Option<&dyn DmaHost> {
        self.dma.as_ref().map(|d| d as &dyn DmaHost)
    }

    fn virtio_host(&self) -> Option<&dyn VirtioHost> {
        self.dma.as_ref().map(|d| d as &dyn VirtioHost)
    }

    fn emit_node(&self, node: HwNode) -> Result<(), DriverError> {
        self.emitted.set(Some(node));
        Ok(())
    }
}

fn host_with(phys: u64) -> MockHost {
    MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
        dma: Some(MockDmaHost { phys, fail: false }),
        emitted: Cell::new(None),
    }
}

#[test]
fn open_discovered_requires_the_mmio_capability() {
    let host = MockHost {
        mmio_map: false,
        mapper: Some(MockMapper { grant: true }),
        dma: Some(MockDmaHost {
            phys: DMA_PHYS_IN_APERTURE,
            fail: false,
        }),
        emitted: Cell::new(None),
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW).err(),
        Some(DriverError::PermissionDenied)
    );
    assert!(!bus.master_enabled.get());
}

#[test]
fn open_discovered_requires_a_mapper() {
    let host = MockHost {
        mmio_map: true,
        mapper: None,
        dma: Some(MockDmaHost {
            phys: DMA_PHYS_IN_APERTURE,
            fail: false,
        }),
        emitted: Cell::new(None),
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_discovered_requires_a_dma_host() {
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
        dma: None,
        emitted: Cell::new(None),
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_discovered_rejects_a_bus_without_a_usb_controller() {
    let host = host_with(DMA_PHYS_IN_APERTURE);
    let bus = MockPciBus {
        class: 0x0200, // Ethernet, not a USB controller.
        address: 0x0001_0000,
        master_enabled: Cell::new(false),
        assigned: Cell::new(None),
    };
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW).err(),
        Some(DriverError::NotFound)
    );
    // The bus carried no USB function, so no device was activated.
    assert!(!bus.master_enabled.get());
}

#[test]
fn open_discovered_rejects_a_dma_carve_above_the_aperture() {
    // The carve sits at the aperture top: its end overruns the window
    // the bridge lets the controller reach, so the wiring fails closed
    // before any hardware is touched (`AGENTS.md` §5.4).
    let host = host_with(APERTURE_TOP);
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW).err(),
        Some(DriverError::OutOfRange)
    );
    assert!(!bus.master_enabled.get());
}

#[test]
fn open_discovered_propagates_a_dma_allocation_failure() {
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
        dma: Some(MockDmaHost {
            phys: DMA_PHYS_IN_APERTURE,
            fail: true,
        }),
        emitted: Cell::new(None),
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn map_controller_maps_the_bar_window_and_carves_dma() {
    // The map prefix `open_discovered` runs before the controller
    // bring-up: it must discover the USB function, route the outbound
    // window through to BAR assignment, enable bus mastering, and map
    // the BAR — returning the mapped window and the carved DMA region
    // for the caller to inspect (the geometry diagnostic) before
    // `Xhci::open`.
    let host = host_with(DMA_PHYS_IN_APERTURE);
    let bus = MockPciBus::usb();
    let mapped =
        map_controller(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW).expect("map prefix succeeds");
    // The mock BAR window is 0x1000 bytes (the metal VL805 BAR0 size).
    assert_eq!(mapped.window.len(), 0x1000);
    // The carve is the device-shared working set at the in-aperture
    // device-visible base, wholly below the aperture top.
    assert_eq!(mapped.dma.phys(), DMA_PHYS_IN_APERTURE);
    assert!(mapped.dma.phys() + mapped.dma.len() as u64 <= APERTURE_TOP);
    assert!(bus.master_enabled.get());
    assert_eq!(bus.assigned.get(), Some(OUTBOUND_WINDOW));
}

#[test]
fn open_discovered_enables_mastering_and_reaches_the_controller() {
    // Everything valid: the carve fits, so the wiring enables bus
    // mastering and maps the BAR, then hands the (inert, zeroed) window
    // to the engine, which fails closed on the implausible capability
    // block. That fault is the on-metal boundary; the assertion proves
    // the composition reached the controller hand-off.
    let host = host_with(DMA_PHYS_IN_APERTURE);
    let bus = MockPciBus::usb();
    let result = open_discovered(&host, &bus, APERTURE_TOP, OUTBOUND_WINDOW);
    assert_eq!(result.err(), Some(DriverError::DeviceFault));
    assert!(
        bus.master_enabled.get(),
        "bus mastering must be enabled before the controller runs"
    );
    // The bridge's outbound window was routed through to BAR assignment
    // before the map (the metal `length_out_of_range` fix), so a BAR the
    // firmware left unassigned gets a base inside it.
    assert_eq!(
        bus.assigned.get(),
        Some(OUTBOUND_WINDOW),
        "the outbound window must reach assign_bar before the BAR is mapped"
    );
}

/// Tree-local ids the autonomous-entry tests place the emitted child
/// under; the node↔driver bind resolves on match keys, not ids
/// (`AGENTS.md` §18.3).
const PARENT_NODE_ID: u32 = 7;
const CHILD_NODE_ID: u32 = 8;

#[test]
fn bring_up_boot_input_requires_the_mmio_capability() {
    // The autonomous floor entry shares `map_controller`'s capability
    // gate: with `MMIO_MAP` ungranted it fails closed before any
    // hardware is touched and nothing is published to the tree.
    let host = MockHost {
        mmio_map: false,
        mapper: Some(MockMapper { grant: true }),
        dma: Some(MockDmaHost {
            phys: DMA_PHYS_IN_APERTURE,
            fail: false,
        }),
        emitted: Cell::new(None),
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        bring_up_boot_input(
            &host,
            &bus,
            APERTURE_TOP,
            OUTBOUND_WINDOW,
            &NoDelay,
            PARENT_NODE_ID,
            CHILD_NODE_ID,
        )
        .err(),
        Some(DriverError::PermissionDenied)
    );
    assert!(!bus.master_enabled.get());
    assert!(
        host.emitted.get().is_none(),
        "no node may be published when the bring-up fails closed"
    );
}

#[test]
fn bring_up_boot_input_requires_a_dma_host() {
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
        dma: None,
        emitted: Cell::new(None),
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        bring_up_boot_input(
            &host,
            &bus,
            APERTURE_TOP,
            OUTBOUND_WINDOW,
            &NoDelay,
            PARENT_NODE_ID,
            CHILD_NODE_ID,
        )
        .err(),
        Some(DriverError::Unsupported)
    );
    assert!(host.emitted.get().is_none());
}

#[test]
fn bring_up_boot_input_reaches_the_controller_then_fails_closed() {
    // Everything valid up to the controller: the entry maps the BAR and
    // carves DMA, then hands the (inert, zeroed) window to the engine,
    // which fails closed on the implausible capability block — exactly
    // the metal boundary. Because enumeration never succeeds over the
    // inert window, no child node is emitted (fail closed, `AGENTS.md`
    // §5.4); the full enumerate→emit path is the on-metal acceptance item.
    let host = host_with(DMA_PHYS_IN_APERTURE);
    let bus = MockPciBus::usb();
    let result = bring_up_boot_input(
        &host,
        &bus,
        APERTURE_TOP,
        OUTBOUND_WINDOW,
        &NoDelay,
        PARENT_NODE_ID,
        CHILD_NODE_ID,
    );
    assert_eq!(result.err(), Some(DriverError::DeviceFault));
    assert!(
        bus.master_enabled.get(),
        "the controller was reached (bus mastering enabled) before the fault"
    );
    assert!(
        host.emitted.get().is_none(),
        "a node is published only after a successful enumeration"
    );
}
