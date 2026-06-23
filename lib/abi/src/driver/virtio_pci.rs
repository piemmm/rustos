//! virtio-1.x PCI transport-provisioning seam (`abi-v1`).
//!
//! A modern virtio PCI device publishes its register blocks through a
//! set of vendor-specific PCI capabilities (virtio 1.1 §4.1.4): a
//! *common configuration* structure, a *notification* area, an
//! *ISR-status* byte, and a *device-specific configuration* area. To
//! drive the device the kernel needs all four as kernel-mapped
//! [`RegisterWindow`]s plus the notification capability's
//! `notify_off_multiplier`.
//!
//! The boot-time PCI walk that turns those capabilities into windows
//! lives in ring 0, but ring 0 must stay driver-agnostic: it may not
//! name the concrete `lib/pci` types (`AGENTS.md` §17.4 — a
//! driver crate's only public surface is `register`). This module is
//! the versioned ABI seam that breaks the tension: the PCI bus driver
//! implements [`VirtioPciBus`] and the kernel calls it through a
//! `&dyn VirtioPciBus`, exactly as it already reaches a bus through
//! [`Bus`] and the MMIO-map facility through [`MmioMapper`].
//!
//! Like every other item in `lib/abi`, the trait and its `cfg_type`
//! constants are frozen for the lifetime of `abi-v1`: new behaviour
//! ships in `abi-v2` rather than mutating this surface (`AGENTS.md`
//! §9).

use super::bus::Bus;
use super::{DriverError, MmioMapper, RegisterWindow};

/// PCI vendor ID assigned to virtio devices (virtio 1.1 §4.1.2).
///
/// A modern virtio PCI function reports this vendor ID; the device ID
/// is `0x1040 + virtio_device_type` (e.g. `0x1042` for block,
/// `0x1041` for network).
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1AF4;

/// `cfg_type` for the common configuration structure (virtio 1.1
/// §4.1.4.3).
pub const VIRTIO_PCI_CFG_COMMON: u8 = 1;
/// `cfg_type` for the notification structure (virtio 1.1 §4.1.4.4).
pub const VIRTIO_PCI_CFG_NOTIFY: u8 = 2;
/// `cfg_type` for the ISR-status structure (virtio 1.1 §4.1.4.5).
pub const VIRTIO_PCI_CFG_ISR: u8 = 3;
/// `cfg_type` for the device-specific structure (virtio 1.1 §4.1.4.6).
pub const VIRTIO_PCI_CFG_DEVICE: u8 = 4;
/// `cfg_type` for the PCI configuration-access window (virtio 1.1
/// §4.1.4.7).
pub const VIRTIO_PCI_CFG_PCI: u8 = 5;

/// A PCI bus that can provision a modern virtio function's register
/// windows for a transport.
///
/// [`Bus`] is a supertrait so the kernel walk can enumerate the bus
/// (to pick the virtio function) and provision its windows through a
/// single `&dyn VirtioPciBus`, without depending on the concrete
/// `lib/pci` crate (`AGENTS.md` §17.4).
///
/// # Capabilities
///
/// The window-mapping method routes through the supplied
/// [`MmioMapper`], which enforces
/// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP); the
/// implementation performs no mapping itself (`AGENTS.md` §4 — no
/// ambient authority).
pub trait VirtioPciBus: Bus {
    /// Resolve the virtio configuration structure of kind `cfg_type`
    /// (one of the `VIRTIO_PCI_CFG_*` constants) on function `bdf` and
    /// ask `mapper` to map it, returning the resulting
    /// [`RegisterWindow`].
    ///
    /// The four windows the kernel collects from
    /// [`VIRTIO_PCI_CFG_COMMON`], [`VIRTIO_PCI_CFG_NOTIFY`],
    /// [`VIRTIO_PCI_CFG_ISR`], and [`VIRTIO_PCI_CFG_DEVICE`] are
    /// exactly what a virtio PCI transport consumes.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no virtio
    ///   capability of `cfg_type`, or the underlying BAR is unused.
    /// * [`DriverError::Unsupported`] — the structure lives in an
    ///   I/O-port BAR, or the function is not a type-0 header.
    /// * [`DriverError::OutOfRange`] — the structure's
    ///   `bar_offset + length` exceeds the resolved BAR size.
    /// * [`DriverError::LengthOutOfRange`] — the region length does
    ///   not fit in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    fn map_virtio_window(
        &self,
        bdf: u64,
        cfg_type: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError>;

    /// Read the `notify_off_multiplier` from function `bdf`'s virtio
    /// notification capability (virtio 1.1 §4.1.4.4).
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no virtio
    ///   notification capability, or no capability list at all.
    /// * [`DriverError::BufferTooSmall`] / [`DriverError::DeviceFault`]
    ///   — propagated from the capability-list walk.
    fn notify_off_multiplier(&self, bdf: u64) -> Result<u32, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::bus::BusDevice;
    use crate::driver::mmio::MmioMapError;
    use core::ptr::NonNull;

    /// 4-byte-aligned (by element type) backing store so a window's
    /// base satisfies `RegisterWindow::from_mapping`'s ≥ 4-byte
    /// alignment contract; 16 × `u32` is 64 bytes.
    static mut BACKING: [u32; 16] = [0u32; 16];

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
            // volatile accesses within `len <= 64` bytes.
            let base = NonNull::new(core::ptr::addr_of_mut!(BACKING).cast::<u8>())
                .expect("static is non-null");
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len.min(64)) })
        }
    }

    /// Minimal [`VirtioPciBus`] whose windows are fixed `(phys, len)`
    /// pairs per `cfg_type`, proving the kernel walk only needs the
    /// trait object.
    struct FakeBus;

    impl Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.is_empty() {
                return Err(DriverError::BufferTooSmall);
            }
            out[0] = BusDevice {
                vendor: u32::from(VIRTIO_PCI_VENDOR_ID),
                device: 0x1042,
                class: 0x0100,
                reserved0: 0,
                address: 0x0000_0800,
            };
            Ok(1)
        }
    }

    impl VirtioPciBus for FakeBus {
        fn map_virtio_window(
            &self,
            _bdf: u64,
            cfg_type: u8,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            let len = match cfg_type {
                VIRTIO_PCI_CFG_COMMON => 0x38,
                VIRTIO_PCI_CFG_NOTIFY => 0x10,
                VIRTIO_PCI_CFG_ISR => 0x4,
                VIRTIO_PCI_CFG_DEVICE => 0x8,
                _ => return Err(DriverError::NotFound),
            };
            mapper
                .map_window(0xC000_0000 + u64::from(cfg_type), len)
                .map_err(MmioMapError::as_driver_error)
        }

        fn notify_off_multiplier(&self, _bdf: u64) -> Result<u32, DriverError> {
            Ok(4)
        }
    }

    #[test]
    fn trait_object_provisions_each_cfg_type() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: true,
        };
        let common = bus
            .map_virtio_window(0x0800, VIRTIO_PCI_CFG_COMMON, &mapper)
            .expect("common window");
        assert_eq!(common.len(), 0x38);
        assert_eq!(mapper.last.get(), Some((0xC000_0001, 0x38)));
        assert_eq!(bus.notify_off_multiplier(0x0800), Ok(4));
    }

    #[test]
    fn unknown_cfg_type_is_not_found() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: true,
        };
        assert!(matches!(
            bus.map_virtio_window(0x0800, VIRTIO_PCI_CFG_PCI, &mapper),
            Err(DriverError::NotFound)
        ));
    }

    #[test]
    fn missing_capability_propagates_as_permission_denied() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mapper = FakeMapper {
            last: core::cell::Cell::new(None),
            grant: false,
        };
        assert!(matches!(
            bus.map_virtio_window(0x0800, VIRTIO_PCI_CFG_COMMON, &mapper),
            Err(DriverError::PermissionDenied)
        ));
    }

    #[test]
    fn enumerate_surfaces_the_virtio_function() {
        let bus: &dyn VirtioPciBus = &FakeBus;
        let mut out = [BusDevice {
            vendor: 0,
            device: 0,
            class: 0,
            reserved0: 0,
            address: 0,
        }; 4];
        let n = bus.enumerate(&mut out).expect("enumerate");
        assert_eq!(n, 1);
        assert_eq!(out[0].vendor, u32::from(VIRTIO_PCI_VENDOR_ID));
        assert_eq!(out[0].device, 0x1042);
    }
}
