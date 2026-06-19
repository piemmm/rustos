//! Host tests for the arch-neutral boot-keyboard driver-process
//! orchestration ([`super::bring_up_boot_keyboard`]).
//!
//! QEMU models no Pi USB timing (`AGENTS.md` §0.4 / §2.1), so these tests
//! prove the composition and its fail-closed paths against in-process mocks
//! (an MMIO mapper backing real heap, a DMA host minting leaked
//! [`DmaSlab`]s, a no-op [`Delay`]). The live controller bring-up — the
//! first 32-bit read off a real BAR returning a plausible `CAPLENGTH` — is
//! the on-metal acceptance item; over the inert zeroed window here
//! [`Xhci::open`](rustos_usb::Xhci::open) fails closed with
//! [`DriverError::DeviceFault`], which is exactly the boundary the "reaches
//! the controller" test asserts (mirroring `drivers/bus/usb`'s `wiring`
//! tests, `AGENTS.md` §2.2).

extern crate alloc;

use alloc::boxed::Box;
use core::ptr::NonNull;

use rustos_abi::driver::dma::{DmaHost, DmaSlab, PoolId};
use rustos_abi::hwtree::HwResource;
use rustos_abi::{
    CapabilityId, Delay, DriverError, DriverHost, DriverKind, MmioMapError, MmioMapper,
    RegisterWindow,
};

use super::{bring_up_boot_keyboard, derive_keyboard_resources, KeyboardResources};

/// The controller's register BAR base/len the keyboard driver maps (the
/// metal VL805 BAR0 is 4 KiB).
const BAR_BASE: u64 = 0x6000_0000;
const BAR_LEN: usize = 0x1000;
/// Device-visible base the DMA host hands out for an in-aperture carve.
const DMA_PHYS_IN_APERTURE: u64 = 0x1000_0000;
/// Inbound-DMA aperture top comfortably above the in-aperture carve.
const APERTURE_TOP: u64 = 0xC000_0000;

/// Leak a `len`-byte, 4-byte-aligned, zeroed buffer and return a pointer to
/// it.
///
/// The leak is deliberate: a window/slab minted here lives for the whole
/// test process, satisfying the lifetime contracts of
/// [`RegisterWindow::from_mapping`] and [`DmaSlab::from_leaked`] without
/// bookkeeping (the mock-host `'static` storage strategy, as
/// `drivers/bus/usb`'s `wiring` tests).
fn leak_aligned(len: usize) -> NonNull<u8> {
    let words = len.div_ceil(4).max(1);
    let buf: Box<[u32]> = alloc::vec![0u32; words].into_boxed_slice();
    NonNull::new(Box::leak(buf).as_mut_ptr().cast::<u8>()).expect("non-null leaked buffer")
}

/// MMIO mapper backing every window with leaked, 4-byte-aligned heap.
struct MockMapper;

impl MmioMapper for MockMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        if len == 0 {
            return Err(MmioMapError::InvalidRegion);
        }
        let base = leak_aligned(len);
        // SAFETY: `base` covers `len` zeroed bytes, is 4-byte aligned, lives
        // for the whole test process (leaked), and no other reference aliases
        // it.
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
        // SAFETY: `ptr` covers `size` zeroed bytes and lives for the whole
        // test process; `phys` is the test's device-visible base for `ptr[0]`.
        // Drop is a no-op (the `from_leaked` contract).
        Ok(unsafe { DmaSlab::from_leaked(self.phys, ptr, size, PoolId::MOCK, 0) })
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

    fn dma_host(&self) -> Option<&dyn DmaHost> {
        self.dma.as_ref().map(|d| d as &dyn DmaHost)
    }
}

/// A no-op delay: the fail-closed and controller-hand-off paths all fail
/// before any settle window, so the clock is never read here.
struct NoopDelay;

impl Delay for NoopDelay {
    fn delay_us(&self, _us: u32) {}

    fn now_us(&self) -> u64 {
        0
    }
}

fn host_with(phys: u64, mmio_map: bool, mapper: bool, dma: bool) -> MockHost {
    MockHost {
        mmio_map,
        mapper: mapper.then_some(MockMapper),
        dma: dma.then_some(MockDmaHost { phys, fail: false }),
    }
}

fn bring_up(host: &MockHost, dma_aperture_top: u64) -> Result<super::KeyboardSource, DriverError> {
    bring_up_boot_keyboard(host, &NoopDelay, BAR_BASE, BAR_LEN, dma_aperture_top)
}

#[test]
fn requires_the_mmio_capability() {
    let host = host_with(DMA_PHYS_IN_APERTURE, false, true, true);
    assert_eq!(
        bring_up(&host, APERTURE_TOP).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn requires_a_mapper() {
    let host = host_with(DMA_PHYS_IN_APERTURE, true, false, true);
    assert_eq!(
        bring_up(&host, APERTURE_TOP).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn requires_a_dma_host() {
    let host = host_with(DMA_PHYS_IN_APERTURE, true, true, false);
    assert_eq!(
        bring_up(&host, APERTURE_TOP).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn rejects_a_dma_carve_above_the_aperture() {
    // The carve sits at the aperture top: its end overruns the window the
    // bridge lets the controller reach, so the orchestration fails closed
    // before any register is touched (`AGENTS.md` §5.4).
    let host = host_with(APERTURE_TOP, true, true, true);
    assert_eq!(
        bring_up(&host, APERTURE_TOP).err(),
        Some(DriverError::OutOfRange)
    );
}

#[test]
fn propagates_a_dma_allocation_failure() {
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper),
        dma: Some(MockDmaHost {
            phys: DMA_PHYS_IN_APERTURE,
            fail: true,
        }),
    };
    assert_eq!(
        bring_up(&host, APERTURE_TOP).err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn reaches_the_controller_hand_off() {
    // Everything valid: the carve fits and the BAR maps, so the orchestration
    // hands the (inert, zeroed) window to the engine, which fails closed on
    // the implausible capability block. That fault is the on-metal boundary;
    // the assertion proves the composition reached the controller hand-off.
    let host = host_with(DMA_PHYS_IN_APERTURE, true, true, true);
    assert_eq!(
        bring_up(&host, APERTURE_TOP).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn derives_a_bus_window_bar_and_translated_dma_aperture() {
    // The Pi 4 shape: the BAR is granted as an outbound PCIe-bus window
    // (the driver names it by its far-side bus address), and the DMA
    // constraint is a translated inbound viewport whose device-visible top is
    // the far-side base plus extent.
    let resources = [
        HwResource::bus_window(0x6_0000_0000, 0x9310, 0xC000_0000),
        HwResource::dma_translated(0xC000_0000, 0x4000_0000, 0xC000_0000),
    ];
    assert_eq!(
        derive_keyboard_resources(resources.iter()),
        Ok(KeyboardResources {
            bar_base: 0xC000_0000,
            bar_len: 0x9310,
            dma_aperture_top: 0xC000_0000 + 0x4000_0000,
        })
    );
}

#[test]
fn derives_an_mmio_bar_and_untranslated_dma_aperture() {
    // The `virt` shape: a plain identity-space register window and an
    // untranslated DMA constraint whose `addr_limit` is the device-visible
    // aperture top directly.
    let resources = [
        HwResource::mmio(0xA00_0000, 0x1000),
        HwResource::dma(0x4000_0000, 0x10_0000),
    ];
    assert_eq!(
        derive_keyboard_resources(resources.iter()),
        Ok(KeyboardResources {
            bar_base: 0xA00_0000,
            bar_len: 0x1000,
            dma_aperture_top: 0x4000_0000,
        })
    );
}

#[test]
fn ignores_an_irq_grant_when_deriving() {
    // An IRQ line the matched node also requested is not part of this
    // driver's bring-up and must not disturb the derivation.
    let resources = [
        HwResource::mmio(0xA00_0000, 0x1000),
        HwResource::irq(33, 1),
        HwResource::dma(0x4000_0000, 0x10_0000),
    ];
    assert_eq!(
        derive_keyboard_resources(resources.iter()).map(|r| r.bar_len),
        Ok(0x1000)
    );
}

#[test]
fn rejects_a_missing_register_window() {
    let resources = [HwResource::dma(0x4000_0000, 0x10_0000)];
    assert_eq!(
        derive_keyboard_resources(resources.iter()).err(),
        Some(DriverError::NotFound)
    );
}

#[test]
fn rejects_a_missing_dma_constraint() {
    let resources = [HwResource::mmio(0xA00_0000, 0x1000)];
    assert_eq!(
        derive_keyboard_resources(resources.iter()).err(),
        Some(DriverError::NotFound)
    );
}

#[test]
fn rejects_an_ambiguous_double_register_window() {
    let resources = [
        HwResource::mmio(0xA00_0000, 0x1000),
        HwResource::mmio(0xB00_0000, 0x1000),
        HwResource::dma(0x4000_0000, 0x10_0000),
    ];
    assert_eq!(
        derive_keyboard_resources(resources.iter()).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn rejects_an_ambiguous_double_dma_constraint() {
    let resources = [
        HwResource::mmio(0xA00_0000, 0x1000),
        HwResource::dma(0x4000_0000, 0x10_0000),
        HwResource::dma(0x8000_0000, 0x10_0000),
    ];
    assert_eq!(
        derive_keyboard_resources(resources.iter()).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn rejects_a_zero_length_register_window() {
    let resources = [
        HwResource::mmio(0xA00_0000, 0),
        HwResource::dma(0x4000_0000, 0x10_0000),
    ];
    assert_eq!(
        derive_keyboard_resources(resources.iter()).err(),
        Some(DriverError::OutOfRange)
    );
}
