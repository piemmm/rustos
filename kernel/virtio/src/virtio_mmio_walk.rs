//! Ring-0 virtio-MMIO provisioning walk (Stage 4.D Item 4).
//!
//! On a `virt`-style platform (QEMU `aarch64 -M virt` / `riscv64 -M
//! virt`) a virtio device is driven through a single kernel-mapped
//! register block discovered from the flat device tree. Turning a slot
//! on the bus into that window is a boot-time job: enumerate the bus,
//! pick the slot whose virtio device ID matches, and ask the kernel
//! MMIO-map facility for a window over its register block.
//!
//! This module is that walk — the MMIO counterpart of
//! [`provision_virtio_pci`](crate::virtio_pci_walk::provision_virtio_pci).
//! It stays driver-agnostic: it reaches the MMIO bus driver only
//! through the frozen [`VirtioMmioBus`] ABI seam and the kernel mapping
//! facility only through [`MmioMapper`], so ring 0 never names a
//! concrete `drivers/bus/*` type and never synthesises
//! a pointer of its own. The capability check that
//! authorises the window lives inside the mapper.

use tairix_abi::driver::bus::BusDevice;
use tairix_abi::driver::virtio_mmio::VirtioMmioBus;
use tairix_abi::{DriverError, MmioMapper, RegisterWindow};
use tairix_virtio::VirtioError;

/// Upper bound on the number of bus slots the walk will record while
/// searching for the virtio device.
///
/// A QEMU `virt` machine exposes up to 32 `virtio-mmio` transport
/// slots; the bound is set above that so a normal topology fits in one
/// enumeration pass, while a pathologically large slot list fails
/// closed ([`VirtioMmioWalkError::SlotTableOverflow`]) rather than
/// spilling onto an unbounded heap allocation during boot.
pub const MAX_SLOTS: usize = 64;

/// Why a [`provision_virtio_mmio`] walk could not produce a transport.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VirtioMmioWalkError {
    /// The bus enumeration call failed (propagated verbatim).
    Enumerate(DriverError),
    /// More than [`MAX_SLOTS`] slots responded, so the walk cannot
    /// guarantee it inspected the whole bus.
    SlotTableOverflow,
    /// No populated slot matched the requested virtio device ID.
    NoVirtioSlot,
    /// Mapping the device's register window failed (propagated
    /// verbatim; e.g. the caller lacks `CAP_MMIO_MAP`).
    MapWindow(DriverError),
    /// The mapped window did not form a valid transport (e.g. the
    /// magic/version/device-id registers did not identify a modern
    /// virtio-MMIO device).
    Transport(VirtioError),
}

/// A provisioned virtio-MMIO device: the transport `T` the caller's
/// builder produced over the kernel-mapped register window, plus the
/// physical base of the register block it was built from.
#[derive(Debug)]
pub struct VirtioMmioProvision<T> {
    /// Transport the builder constructed over the kernel-mapped
    /// virtio register window.
    pub transport: T,
    /// Physical base address of the provisioned slot's register block.
    pub base: u64,
}

/// Enumerate `bus`, locate the first populated slot whose virtio device
/// ID equals `device_id`, map its register window through `mapper`, and
/// hand the window to `build` to construct the caller's transport.
///
/// `device_id` is the virtio device type reported in the slot's
/// `DeviceID` register (e.g. `2` for block, `1` for network) — over
/// MMIO this is the bare device type, not the PCI `0x1040 + type`
/// encoding.
///
/// The walk maps the window but never names a concrete transport type:
/// the caller passes `build` (in production
/// `tairix_drv_bus_virtio::MmioTransport::new`), so ring 0 depends only
/// on `lib/*` and never on a `drivers/bus/*` crate (`kernel/* → lib/*`, never a driver).
///
/// # Errors
///
/// See [`VirtioMmioWalkError`]; every failure mode is reported rather
/// than panicking. The walk touches no device state
/// itself; any identity-register validation the transport performs
/// happens inside `build`, whose [`VirtioError`] is surfaced as
/// [`VirtioMmioWalkError::Transport`].
///
/// # Capabilities
///
/// The `mapper` enforces
/// [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP) on the
/// window; this walk holds no ambient authority of its own.
pub fn provision_virtio_mmio<T, B>(
    bus: &dyn VirtioMmioBus,
    device_id: u32,
    mapper: &dyn MmioMapper,
    build: B,
) -> Result<VirtioMmioProvision<T>, VirtioMmioWalkError>
where
    B: FnOnce(RegisterWindow) -> Result<T, VirtioError>,
{
    let base = find_virtio_slot(bus, device_id)?;
    let window = bus
        .map_slot_window(base, mapper)
        .map_err(VirtioMmioWalkError::MapWindow)?;
    let transport = build(window).map_err(VirtioMmioWalkError::Transport)?;
    Ok(VirtioMmioProvision { transport, base })
}

/// Enumerate the bus into a bounded buffer and return the register-block
/// base of the first slot matching `device_id`.
fn find_virtio_slot(bus: &dyn VirtioMmioBus, device_id: u32) -> Result<u64, VirtioMmioWalkError> {
    let blank = BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    };
    let mut table = [blank; MAX_SLOTS];
    let count = match bus.enumerate(&mut table) {
        Ok(n) => n,
        Err(DriverError::BufferTooSmall) => return Err(VirtioMmioWalkError::SlotTableOverflow),
        Err(e) => return Err(VirtioMmioWalkError::Enumerate(e)),
    };
    table[..count]
        .iter()
        .find(|d| d.device == device_id)
        .map(|d| d.address)
        .ok_or(VirtioMmioWalkError::NoVirtioSlot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use tairix_abi::driver::mmio::MmioMapError;
    use tairix_abi::RegisterWindow;

    const VIRTIO_BLK_DEVICE_ID: u32 = 2;
    const SLOT_BASE: u64 = 0x1000_4000;
    /// Length the fake bus advertises for the slot. The walk only maps
    /// the window; identity-register validation is the builder's job,
    /// so any non-zero length exercises it.
    const SLOT_LEN: usize = 0x200;

    /// Identity builder: keeps the mapped window so the test can assert
    /// on it directly, standing in for a real transport constructor
    /// without depending on a `drivers/bus/*` crate.
    fn keep_window() -> impl FnOnce(RegisterWindow) -> Result<RegisterWindow, VirtioError> {
        |window| Ok(window)
    }

    /// Mapper that hands out a window over freshly-leaked, aligned
    /// backing storage, recording the `(phys, len)` it was asked for.
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
            let window = unsafe { RegisterWindow::from_mapping(phys_base, base, len) };
            Ok(window)
        }
    }

    /// Fake MMIO bus enumerating `slots` and resolving a window for the
    /// slot whose base matches.
    struct FakeBus {
        slots: alloc::vec::Vec<BusDevice>,
    }

    impl tairix_abi::driver::bus::Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.len() < self.slots.len() {
                return Err(DriverError::BufferTooSmall);
            }
            out[..self.slots.len()].copy_from_slice(&self.slots);
            Ok(self.slots.len())
        }
    }

    impl VirtioMmioBus for FakeBus {
        fn map_slot_window(
            &self,
            base: u64,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            if !self.slots.iter().any(|s| s.address == base) {
                return Err(DriverError::NotFound);
            }
            mapper
                .map_window(base, SLOT_LEN)
                .map_err(MmioMapError::as_driver_error)
        }

        fn slot_window(&self, base: u64) -> Result<u64, DriverError> {
            if !self.slots.iter().any(|s| s.address == base) {
                return Err(DriverError::NotFound);
            }
            Ok(SLOT_LEN as u64)
        }
    }

    fn slot(device: u32, address: u64) -> BusDevice {
        BusDevice {
            vendor: 0x554D_4551,
            device,
            class: 2,
            reserved0: 0,
            address,
        }
    }

    #[test]
    fn provisions_transport_for_matching_slot() {
        let bus = FakeBus {
            slots: alloc::vec![slot(1, 0x1000_3000), slot(VIRTIO_BLK_DEVICE_ID, SLOT_BASE)],
        };
        let mapper = RecordingMapper::new(true);
        let provision = provision_virtio_mmio(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_window())
            .expect("provisioned");
        assert_eq!(provision.base, SLOT_BASE);
        assert_eq!(provision.transport.len(), SLOT_LEN);
        assert_eq!(
            mapper.requests.borrow().as_slice(),
            &[(SLOT_BASE, SLOT_LEN)]
        );
    }

    #[test]
    fn errors_when_no_matching_slot() {
        let bus = FakeBus {
            slots: alloc::vec![slot(1, 0x1000_3000)],
        };
        let mapper = RecordingMapper::new(true);
        assert_eq!(
            provision_virtio_mmio(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_window())
                .expect_err("no slot"),
            VirtioMmioWalkError::NoVirtioSlot
        );
        assert!(mapper.requests.borrow().is_empty());
    }

    #[test]
    fn propagates_map_failure_as_permission_denied() {
        let bus = FakeBus {
            slots: alloc::vec![slot(VIRTIO_BLK_DEVICE_ID, SLOT_BASE)],
        };
        let mapper = RecordingMapper::new(false);
        assert_eq!(
            provision_virtio_mmio(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_window())
                .expect_err("map refused"),
            VirtioMmioWalkError::MapWindow(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn slot_enumeration_overflow_fails_closed() {
        let mut slots = alloc::vec::Vec::new();
        for i in 0..=(MAX_SLOTS as u64) {
            slots.push(slot(VIRTIO_BLK_DEVICE_ID, 0x1000_0000 + i * 0x1000));
        }
        let bus = FakeBus { slots };
        let mapper = RecordingMapper::new(true);
        assert_eq!(
            provision_virtio_mmio(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, keep_window())
                .expect_err("overflow"),
            VirtioMmioWalkError::SlotTableOverflow
        );
    }
}
