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
use rustos_abi::driver::dma::{DmaSlab, PoolId};
use rustos_abi::driver::mmio::MmioMapError;
use rustos_abi::driver::virtio::VirtioHost;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, MmioMapper, PciBus, RegisterWindow,
};

use super::{open_discovered, USB_CONTROLLER_CLASS};

/// Device-visible base the DMA host hands out for an in-aperture carve.
const DMA_PHYS_IN_APERTURE: u64 = 0x1000_0000;
/// Inbound-DMA aperture top comfortably above the in-aperture carve.
const APERTURE_TOP: u64 = 0xC000_0000;

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

impl VirtioHost for MockDmaHost {
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

    fn notify_wait(&self, _queue_index: u16) {}
}

/// Recording PCI bus: enumerates one function of `class` and tracks
/// whether bus mastering was enabled.
struct MockPciBus {
    class: u16,
    address: u64,
    master_enabled: Cell<bool>,
}

impl MockPciBus {
    fn usb() -> Self {
        Self {
            class: USB_CONTROLLER_CLASS,
            address: 0x0001_0000,
            master_enabled: Cell::new(false),
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
}

/// A host granting `MMIO_MAP`, with an optional mapper and DMA host.
struct MockHost {
    mmio_map: bool,
    mapper: Option<MockMapper>,
    dma: Option<MockDmaHost>,
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

    fn virtio_host(&self) -> Option<&dyn VirtioHost> {
        self.dma.as_ref().map(|d| d as &dyn VirtioHost)
    }
}

fn host_with(phys: u64) -> MockHost {
    MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
        dma: Some(MockDmaHost { phys, fail: false }),
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
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP).err(),
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
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_discovered_requires_a_dma_host() {
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
        dma: None,
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP).err(),
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
    };
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP).err(),
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
        open_discovered(&host, &bus, APERTURE_TOP).err(),
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
    };
    let bus = MockPciBus::usb();
    assert_eq!(
        open_discovered(&host, &bus, APERTURE_TOP).err(),
        Some(DriverError::LengthOutOfRange)
    );
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
    let result = open_discovered(&host, &bus, APERTURE_TOP);
    assert_eq!(result.err(), Some(DriverError::DeviceFault));
    assert!(
        bus.master_enabled.get(),
        "bus mastering must be enabled before the controller runs"
    );
}
