//! virtio-1.x MMIO transport-provisioning seam (`abi-v1`).
//!
//! On a `virt`-style platform (QEMU `aarch64 -M virt` / `riscv64 -M
//! virt`, `SiFive` boards) a virtio device is presented as a single
//! memory-mapped register block discovered from the flat device tree
//! (virtio 1.1 §4.2.2). Unlike the PCI transport — which collects four
//! capability-selected windows ([`VirtioPciBus`](super::virtio_pci::VirtioPciBus))
//! — the MMIO transport is driven through exactly one kernel-mapped
//! [`RegisterWindow`] over that block.
//!
//! The boot-time device-tree walk that turns a slot into a window
//! lives in ring 0, but ring 0 must stay driver-agnostic: it may not
//! name the concrete `drivers/bus/mmio` types (`AGENTS.md` §8 — a
//! driver crate's only public surface is `register`). This module is
//! the versioned ABI seam that breaks the tension: the MMIO bus driver
//! implements [`VirtioMmioBus`] and the kernel calls it through a
//! `&dyn VirtioMmioBus`, exactly as it reaches a PCI bus through
//! [`VirtioPciBus`](super::virtio_pci::VirtioPciBus), any bus through
//! [`Bus`], and the MMIO-map facility through [`MmioMapper`].
//!
//! Like every other item in `lib/abi`, this trait is frozen for the
//! lifetime of `abi-v1`: new behaviour ships in `abi-v2` rather than
//! mutating this surface (`AGENTS.md` §9).

use super::bus::Bus;
use super::{DriverError, MmioMapper, RegisterWindow};

/// An MMIO bus that can provision a virtio device's register window
/// for a transport.
///
/// [`Bus`] is a supertrait so the kernel walk can enumerate the bus
/// (to pick the slot) and provision its window through a single
/// `&dyn VirtioMmioBus`, without depending on the concrete
/// `drivers/bus/mmio` crate (`AGENTS.md` §8).
///
/// # Capabilities
///
/// The window-mapping method routes through the supplied
/// [`MmioMapper`], which enforces
/// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP); the
/// implementation performs no mapping itself (`AGENTS.md` §4 — no
/// ambient authority).
pub trait VirtioMmioBus: Bus {
    /// Resolve the virtio-MMIO transport slot whose register block
    /// begins at physical `base` and ask `mapper` to map it,
    /// returning the resulting [`RegisterWindow`].
    ///
    /// `base` is the slot address reported in [`BusDevice::address`]
    /// by [`Bus::enumerate`], so a caller enumerates the bus, picks
    /// the slot it wants, and provisions its window in two calls
    /// through the same trait object. The returned window is exactly
    /// what a virtio MMIO transport consumes.
    ///
    /// [`BusDevice::address`]: super::bus::BusDevice::address
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — no virtio-MMIO slot whose
    ///   register block begins at `base` exists on the bus.
    /// * [`DriverError::DeviceFault`] — the slot descriptor is
    ///   malformed (fails closed, never under-maps).
    /// * [`DriverError::LengthOutOfRange`] — the slot length does not
    ///   fit in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    fn map_slot_window(
        &self,
        base: u64,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::bus::BusDevice;
    use crate::driver::mmio::MmioMapError;
    use core::ptr::NonNull;

    /// Physical base the fake bus reports for its single slot.
    const SLOT_BASE: u64 = 0x1000_1000;
    /// Length the fake bus maps for the slot.
    const SLOT_LEN: usize = 0x200;

    /// 4-byte-aligned (by element type) backing store so a window's
    /// base satisfies `RegisterWindow::from_mapping`'s ≥ 4-byte
    /// alignment contract; 64 × `u32` is 256 bytes.
    static mut BACKING: [u32; 64] = [0u32; 64];

    /// Mapper that hands out a window over the shared static backing
    /// store, recording the last `(phys, len)` it was asked for.
    struct FakeMapper {
        last: core::cell::Cell<Option<(u64, usize)>>,
        grant: bool,
    }

    impl MmioMapper for FakeMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.grant {
                return Err(MmioMapError::CapabilityMissing);
            }
            self.last.set(Some((phys_base, len)));
            // SAFETY: single-threaded test; the static lives for the
            // whole process and the returned window only performs
            // volatile accesses within `len <= 256` bytes.
            let base = NonNull::new(core::ptr::addr_of_mut!(BACKING).cast::<u8>())
                .expect("static is non-null");
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len.min(256)) })
        }
    }

    /// Minimal [`VirtioMmioBus`] exposing one slot at [`SLOT_BASE`],
    /// proving the kernel walk only needs the trait object.
    struct FakeBus;

    impl Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.is_empty() {
                return Err(DriverError::BufferTooSmall);
            }
            out[0] = BusDevice {
                vendor: 0x554D_4551, // "QEMU"
                device: 2,           // virtio-blk device id over MMIO
                class: 2,            // modern transport version
                reserved0: 0,
                address: SLOT_BASE,
            };
            Ok(1)
        }
    }

    impl VirtioMmioBus for FakeBus {
        fn map_slot_window(
            &self,
            base: u64,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            if base != SLOT_BASE {
                return Err(DriverError::NotFound);
            }
            mapper
                .map_window(base, SLOT_LEN)
                .map_err(MmioMapError::as_driver_error)
        }
    }

    #[test]
    fn trait_object_provisions_the_slot_window() {
        let bus: &dyn VirtioMmioBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: true,
        };
        let window = bus
            .map_slot_window(SLOT_BASE, &mapper)
            .expect("slot window");
        assert_eq!(window.len(), SLOT_LEN.min(256));
        assert_eq!(mapper.last.get(), Some((SLOT_BASE, SLOT_LEN)));
    }

    #[test]
    fn unknown_base_is_not_found() {
        let bus: &dyn VirtioMmioBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: true,
        };
        assert!(matches!(
            bus.map_slot_window(SLOT_BASE + 0x1000, &mapper),
            Err(DriverError::NotFound)
        ));
        assert_eq!(mapper.last.get(), None);
    }

    #[test]
    fn missing_capability_propagates_as_permission_denied() {
        let bus: &dyn VirtioMmioBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: false,
        };
        assert!(matches!(
            bus.map_slot_window(SLOT_BASE, &mapper),
            Err(DriverError::PermissionDenied)
        ));
    }

    #[test]
    fn enumerate_surfaces_the_slot() {
        let bus: &dyn VirtioMmioBus = &FakeBus;
        let mut out = [BusDevice {
            vendor: 0,
            device: 0,
            class: 0,
            reserved0: 0,
            address: 0,
        }; 4];
        let n = bus.enumerate(&mut out).expect("enumerate");
        assert_eq!(n, 1);
        assert_eq!(out[0].address, SLOT_BASE);
        assert_eq!(out[0].device, 2);
    }
}
