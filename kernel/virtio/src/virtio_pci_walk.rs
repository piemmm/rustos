//! Ring-0 virtio-PCI provisioning walk (Stage 4.D Item 4).
//!
//! A modern virtio PCI device is driven through four kernel-mapped
//! register windows (common / notify / ISR / device configuration)
//! plus the notification capability's `notify_off_multiplier`
//! ([`PciTransportWindows`]). Turning a device on the bus into those
//! windows is a boot-time job: enumerate the bus, pick the virtio
//! function, and ask the kernel MMIO-map facility for a window over
//! each of its virtio capabilities.
//!
//! This module is that walk. It stays driver-agnostic — it reaches
//! the PCI bus driver only through the frozen [`VirtioPciBus`] ABI
//! seam and the kernel mapping facility only through [`MmioMapper`],
//! so ring 0 never names a concrete `drivers/bus/*` type and never synthesises a pointer of its own
//! . The capability check that authorises every
//! window lives inside the mapper.

use rustos_abi::driver::bus::BusDevice;
use rustos_abi::driver::virtio_pci::{
    VirtioPciBus, VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE, VIRTIO_PCI_CFG_ISR,
    VIRTIO_PCI_CFG_NOTIFY, VIRTIO_PCI_VENDOR_ID,
};
use rustos_abi::{DriverError, MmioMapper};
use rustos_virtio::{PciTransportWindows, VirtioError};

/// Upper bound on the number of bus functions the walk will record
/// while searching for the virtio device.
///
/// A QEMU `q35` machine exposes a handful of functions; the bound is
/// set well above that so a normal topology fits in one enumeration
/// pass, while a pathologically large device list fails closed
/// ([`VirtioPciWalkError::DeviceTableOverflow`]) rather than spilling
/// onto an unbounded heap allocation during boot.
pub const MAX_FUNCTIONS: usize = 64;

/// Why a [`provision_virtio_pci`] walk could not produce a transport.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VirtioPciWalkError {
    /// The bus enumeration call failed (propagated verbatim).
    Enumerate(DriverError),
    /// More than [`MAX_FUNCTIONS`] functions responded, so the walk
    /// cannot guarantee it inspected the whole bus.
    DeviceTableOverflow,
    /// No responding function matched the requested virtio device ID.
    NoVirtioFunction,
    /// Mapping one of the device's virtio register windows failed
    /// (propagated verbatim; e.g. the caller lacks `CAP_MMIO_MAP`).
    MapWindow(DriverError),
    /// The mapped windows did not form a valid transport (e.g. a
    /// malformed common-configuration capability).
    Transport(VirtioError),
    /// Routing the device's MSI-X interrupt failed (propagated
    /// verbatim from
    /// [`route_msix`](rustos_abi::driver::msix::MsixBus::route_msix);
    /// e.g. the caller lacks `CAP_MMIO_MAP` or the function
    /// advertises no MSI-X capability).
    RouteMsix(DriverError),
}

/// A provisioned virtio-PCI device: the transport `T` the caller's
/// builder produced over the four kernel-mapped register windows, plus
/// the bus-local address of the function it was built from.
///
/// The boot wiring needs the `bdf` after the walk to route the
/// device's MSI-X interrupt
/// ([`route_msix`](rustos_abi::driver::msix::MsixBus::route_msix)),
/// which is keyed by function — the walk already located it, so it is
/// returned here rather than re-enumerated.
#[derive(Debug)]
pub struct VirtioProvision<T> {
    /// Transport the builder constructed over the kernel-mapped
    /// virtio register windows.
    pub transport: T,
    /// Bus-local address of the provisioned virtio function.
    pub bdf: u64,
}

/// Enumerate `bus`, locate the first virtio function whose PCI device
/// ID equals `device_id`, map its four virtio register windows through
/// `mapper`, and hand the assembled [`PciTransportWindows`] to `build`
/// to construct the caller's transport.
///
/// `device_id` is the modern virtio PCI device ID, `0x1040 +
/// virtio_device_type` (e.g. `0x1042` for block, `0x1041` for
/// network). Only functions reporting the virtio vendor ID
/// ([`VIRTIO_PCI_VENDOR_ID`]) are considered.
///
/// The walk maps the windows but never names a concrete transport
/// type: the caller passes `build` (in production
/// `rustos_drv_bus_virtio::PciTransport::new`), so ring 0 depends only
/// on `lib/*` and never on a `drivers/bus/*` crate (`kernel/* → lib/*`, never a driver).
///
/// # Errors
///
/// See [`VirtioPciWalkError`]; every failure mode is reported rather
/// than panicking. The walk touches no device state
/// itself; any device init the transport drives happens inside `build`,
/// whose [`VirtioError`] is surfaced as
/// [`VirtioPciWalkError::Transport`].
///
/// # Capabilities
///
/// The `mapper` enforces
/// [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP) on
/// every window; this walk holds no ambient authority of its own.
pub fn provision_virtio_pci<T, B>(
    bus: &dyn VirtioPciBus,
    device_id: u16,
    mapper: &dyn MmioMapper,
    build: B,
) -> Result<VirtioProvision<T>, VirtioPciWalkError>
where
    B: FnOnce(PciTransportWindows) -> Result<T, VirtioError>,
{
    let bdf = find_virtio_function(bus, device_id)?;
    let windows = PciTransportWindows {
        common: map(bus, bdf, VIRTIO_PCI_CFG_COMMON, mapper)?,
        notify: map(bus, bdf, VIRTIO_PCI_CFG_NOTIFY, mapper)?,
        isr: map(bus, bdf, VIRTIO_PCI_CFG_ISR, mapper)?,
        device: map(bus, bdf, VIRTIO_PCI_CFG_DEVICE, mapper)?,
        notify_off_multiplier: bus
            .notify_off_multiplier(bdf)
            .map_err(VirtioPciWalkError::MapWindow)?,
    };
    let transport = build(windows).map_err(VirtioPciWalkError::Transport)?;
    Ok(VirtioProvision { transport, bdf })
}

/// Map one virtio configuration window, tagging a failure as a
/// window-mapping error.
fn map(
    bus: &dyn VirtioPciBus,
    bdf: u64,
    cfg_type: u8,
    mapper: &dyn MmioMapper,
) -> Result<rustos_abi::RegisterWindow, VirtioPciWalkError> {
    bus.map_virtio_window(bdf, cfg_type, mapper)
        .map_err(VirtioPciWalkError::MapWindow)
}

/// Enumerate the bus into a bounded buffer and return the bus-local
/// address of the first virtio function matching `device_id`.
fn find_virtio_function(bus: &dyn VirtioPciBus, device_id: u16) -> Result<u64, VirtioPciWalkError> {
    let blank = BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    };
    let mut table = [blank; MAX_FUNCTIONS];
    let count = match bus.enumerate(&mut table) {
        Ok(n) => n,
        Err(DriverError::BufferTooSmall) => return Err(VirtioPciWalkError::DeviceTableOverflow),
        Err(e) => return Err(VirtioPciWalkError::Enumerate(e)),
    };
    table[..count]
        .iter()
        .find(|d| d.vendor == u32::from(VIRTIO_PCI_VENDOR_ID) && d.device == u32::from(device_id))
        .map(|d| d.address)
        .ok_or(VirtioPciWalkError::NoVirtioFunction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use rustos_abi::driver::mmio::MmioMapError;
    use rustos_abi::RegisterWindow;

    const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;
    const TARGET_BDF: u64 = 0x0000_0800;

    /// Length the fake device advertises for each virtio config
    /// structure, keyed by `cfg_type`. The walk only maps the windows
    /// and assembles them; it does not inspect their contents (that is
    /// the builder's job), so any non-zero length exercises it.
    fn cfg_len(cfg_type: u8) -> usize {
        match cfg_type {
            VIRTIO_PCI_CFG_COMMON => 0x38,
            VIRTIO_PCI_CFG_NOTIFY => 0x10,
            VIRTIO_PCI_CFG_ISR => 0x4,
            VIRTIO_PCI_CFG_DEVICE => 0x8,
            _ => 0,
        }
    }

    /// Identity builder: keeps the assembled windows so the test can
    /// assert on them directly, standing in for a real transport
    /// constructor without depending on a `drivers/bus/*` crate.
    fn keep_windows() -> impl FnOnce(PciTransportWindows) -> Result<PciTransportWindows, VirtioError>
    {
        |windows| Ok(windows)
    }

    /// Mapper that hands out windows over freshly-leaked, aligned
    /// backing storage and records the `(phys, len)` of each request.
    struct RecordingMapper {
        grant: bool,
        requests: RefCell<alloc::vec::Vec<(u64, usize)>>,
    }

    impl RecordingMapper {
        fn new(grant: bool) -> Self {
            Self {
                grant,
                requests: RefCell::new(alloc::vec::Vec::new()),
            }
        }
    }

    impl MmioMapper for RecordingMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.grant {
                return Err(MmioMapError::CapabilityMissing);
            }
            self.requests.borrow_mut().push((phys_base, len));
            let words = len.div_ceil(8).max(1);
            let boxed = alloc::vec![0u64; words].into_boxed_slice();
            let raw = alloc::boxed::Box::leak(boxed);
            let base = core::ptr::NonNull::new(raw.as_mut_ptr().cast::<u8>()).expect("non-null");
            // SAFETY: `base` covers `len` bytes of leaked storage that
            // lives for the rest of the test process; nothing else
            // aliases it and the window only performs volatile access.
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
        }
    }

    /// Fake bus that enumerates `devices` and resolves virtio windows
    /// by asking the mapper for a fixed length per `cfg_type`. A
    /// synthetic physical base encodes the `cfg_type` so a test can
    /// confirm the right window was mapped for the right structure.
    struct FakeBus {
        devices: alloc::vec::Vec<BusDevice>,
    }

    impl rustos_abi::driver::bus::Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.len() < self.devices.len() {
                return Err(DriverError::BufferTooSmall);
            }
            out[..self.devices.len()].copy_from_slice(&self.devices);
            Ok(self.devices.len())
        }
    }

    impl VirtioPciBus for FakeBus {
        fn map_virtio_window(
            &self,
            _bdf: u64,
            cfg_type: u8,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            let len = cfg_len(cfg_type);
            if len == 0 {
                return Err(DriverError::NotFound);
            }
            mapper
                .map_window(0xC000_0000 + u64::from(cfg_type), len)
                .map_err(MmioMapError::as_driver_error)
        }

        fn notify_off_multiplier(&self, _bdf: u64) -> Result<u32, DriverError> {
            Ok(4)
        }
    }

    fn dev(vendor: u16, device: u16, address: u64) -> BusDevice {
        BusDevice {
            vendor: u32::from(vendor),
            device: u32::from(device),
            class: 0x0100,
            reserved0: 0,
            address,
        }
    }

    #[test]
    fn provisions_transport_for_matching_device() {
        let bus = FakeBus {
            devices: alloc::vec![
                dev(0x8086, 0x29C0, 0x0000_0000), // q35 host bridge
                dev(VIRTIO_PCI_VENDOR_ID, VIRTIO_BLK_DEVICE_ID, TARGET_BDF),
            ],
        };
        let mapper = RecordingMapper::new(true);
        let provision = provision_virtio_pci(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_windows())
            .expect("transport");

        // The located function and all four windows plus the
        // multiplier were provisioned.
        assert_eq!(provision.bdf, TARGET_BDF);
        assert_eq!(provision.transport.notify_off_multiplier, 4);
        let reqs = mapper.requests.borrow();
        assert_eq!(reqs.len(), 4);
        // Each cfg_type was mapped exactly once at its synthetic base.
        for cfg in [
            VIRTIO_PCI_CFG_COMMON,
            VIRTIO_PCI_CFG_NOTIFY,
            VIRTIO_PCI_CFG_ISR,
            VIRTIO_PCI_CFG_DEVICE,
        ] {
            let want = (0xC000_0000 + u64::from(cfg), cfg_len(cfg));
            assert!(reqs.contains(&want), "missing window for cfg {cfg}");
        }
    }

    #[test]
    fn errors_when_no_matching_device() {
        let bus = FakeBus {
            devices: alloc::vec![dev(0x8086, 0x29C0, 0)],
        };
        let mapper = RecordingMapper::new(true);
        assert_eq!(
            provision_virtio_pci(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_windows()).unwrap_err(),
            VirtioPciWalkError::NoVirtioFunction
        );
        // No device matched, so no window was mapped.
        assert!(mapper.requests.borrow().is_empty());
    }

    #[test]
    fn matches_only_the_requested_device_id() {
        // A virtio-net function is present but the walk wants block.
        let bus = FakeBus {
            devices: alloc::vec![dev(VIRTIO_PCI_VENDOR_ID, 0x1041, TARGET_BDF)],
        };
        let mapper = RecordingMapper::new(true);
        assert_eq!(
            provision_virtio_pci(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_windows()).unwrap_err(),
            VirtioPciWalkError::NoVirtioFunction
        );
    }

    #[test]
    fn propagates_map_failure_as_permission_denied() {
        let bus = FakeBus {
            devices: alloc::vec![dev(VIRTIO_PCI_VENDOR_ID, VIRTIO_BLK_DEVICE_ID, TARGET_BDF)],
        };
        let mapper = RecordingMapper::new(false);
        assert_eq!(
            provision_virtio_pci(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_windows()).unwrap_err(),
            VirtioPciWalkError::MapWindow(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn enumeration_overflow_fails_closed() {
        // More functions than fit the bounded table.
        let mut devices = alloc::vec::Vec::new();
        for i in 0..=MAX_FUNCTIONS {
            devices.push(dev(0x8086, 0x0001, i as u64));
        }
        let bus = FakeBus { devices };
        let mapper = RecordingMapper::new(true);
        assert_eq!(
            provision_virtio_pci(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_windows()).unwrap_err(),
            VirtioPciWalkError::DeviceTableOverflow
        );
    }
}
