//! PCI MSI-X interrupt-routing seam (`abi-v1`).
//!
//! A modern PCI function delivers interrupts by writing an
//! architecture-defined *message* (an address/data pair) to the
//! platform interrupt controller. MSI-X (PCI Local Bus 3.0 §6.8.2)
//! holds one such message per table entry in a memory BAR and a
//! per-function enable bit in configuration space. Routing a device's
//! interrupt therefore means: program a table entry with the message
//! the kernel's interrupt controller minted, unmask that entry, and
//! enable MSI-X on the function.
//!
//! As with [`VirtioPciBus`](super::virtio_pci::VirtioPciBus), the boot
//! walk that performs this lives in ring 0, which must stay
//! driver-agnostic and may not name the concrete `lib/pci`
//! types (`AGENTS.md` §8). This module is the versioned seam that
//! breaks the tension: the PCI bus driver implements [`MsixBus`] and
//! the kernel calls it through a `&dyn MsixBus`, handing it the
//! [`MsiMessage`] the architecture layer built and the
//! [`MmioMapper`] that gates the table write on
//! [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP).
//!
//! The [`MsiMessage`] is opaque to the bus driver: only the
//! architecture layer knows how to address its interrupt controller
//! (x86 writes the local-APIC message format, a GIC or PLIC port
//! would build a different pair). The bus driver copies the two words
//! into the table entry verbatim.
//!
//! Like every other item in `lib/abi`, this surface is frozen for the
//! lifetime of `abi-v1`: new behaviour ships in `abi-v2` rather than
//! mutating it (`AGENTS.md` §9).

use super::bus::Bus;
use super::{DriverError, MmioMapper};

/// An architecture-built MSI message: the address/data pair a PCI
/// function writes to deliver an interrupt.
///
/// The bus driver treats both fields as opaque and copies them
/// verbatim into the MSI-X table entry; only the architecture layer
/// (for example `rustos_arch_x86_64::irq::msi_message`) knows how to
/// encode a vector and destination into them.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct MsiMessage {
    /// Message address. On x86 this is the local-APIC message-address
    /// register format (`0xFEE0_0000`-based); other architectures use
    /// their own interrupt-controller addressing.
    pub address: u64,
    /// Message data. On x86 the low byte is the delivered interrupt
    /// vector; other architectures carry their own encoding.
    pub data: u32,
}

/// A PCI bus that can route a function's interrupt through MSI-X.
///
/// [`Bus`] is a supertrait so the kernel walk can enumerate the bus
/// (to pick the device function) and route its interrupt through a
/// single `&dyn MsixBus`, without depending on the concrete
/// `lib/pci` crate (`AGENTS.md` §17.4).
///
/// # Capabilities
///
/// The table write routes through the supplied [`MmioMapper`], which
/// enforces [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP);
/// the implementation performs no mapping itself (`AGENTS.md` §4 — no
/// ambient authority).
pub trait MsixBus: Bus {
    /// Program MSI-X table `entry` of function `bdf` with `message`,
    /// unmask the entry, and enable MSI-X on the function.
    ///
    /// The implementation locates the function's MSI-X capability,
    /// maps the addressed table entry through `mapper`, writes
    /// `message` and clears the entry's per-vector mask, then sets the
    /// MSI-X Enable bit and clears the function-wide mask in the
    /// capability's Message Control register.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no MSI-X
    ///   capability (or no capability list at all).
    /// * [`DriverError::OutOfRange`] — `entry` is beyond the function's
    ///   MSI-X table, or the table overruns its BAR.
    /// * [`DriverError::Unsupported`] — the table lives in an I/O-port
    ///   BAR, or the function is not a type-0 header.
    /// * [`DriverError::LengthOutOfRange`] — the table region length
    ///   does not fit in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    /// * [`DriverError::BufferTooSmall`] / [`DriverError::DeviceFault`]
    ///   — propagated from the capability-list walk.
    fn route_msix(
        &self,
        bdf: u64,
        entry: u16,
        message: MsiMessage,
        mapper: &dyn MmioMapper,
    ) -> Result<(), DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::bus::BusDevice;
    use crate::driver::mmio::{MmioMapError, RegisterWindow, WindowError};
    use core::cell::Cell;
    use core::ptr::NonNull;

    /// 4-byte-aligned backing store for the one 16-byte table entry a
    /// `FakeBus` programs.
    static mut ENTRY: [u32; 4] = [0u32; 4];

    /// Mapper handing out a window over [`ENTRY`], recording the
    /// `(phys, len)` it was asked for and whether the grant succeeds.
    struct FakeMapper {
        last: Cell<Option<(u64, usize)>>,
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
            // volatile accesses within `len <= 16` bytes.
            let base = NonNull::new(core::ptr::addr_of_mut!(ENTRY).cast::<u8>())
                .expect("static is non-null");
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len.min(16)) })
        }
    }

    /// Minimal [`MsixBus`] that maps a fixed table-entry address and
    /// writes the supplied message, proving the kernel walk only needs
    /// the trait object.
    struct FakeBus {
        table_size: u16,
    }

    impl Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.is_empty() {
                return Err(DriverError::BufferTooSmall);
            }
            out[0] = BusDevice {
                vendor: 0x1AF4,
                device: 0x1042,
                class: 0x0100,
                reserved0: 0,
                address: 0x0000_0800,
            };
            Ok(1)
        }
    }

    impl MsixBus for FakeBus {
        fn route_msix(
            &self,
            _bdf: u64,
            entry: u16,
            message: MsiMessage,
            mapper: &dyn MmioMapper,
        ) -> Result<(), DriverError> {
            if entry >= self.table_size {
                return Err(DriverError::OutOfRange);
            }
            let phys = 0xC100_0000 + u64::from(entry) * 16;
            let window = mapper
                .map_window(phys, 16)
                .map_err(MmioMapError::as_driver_error)?;
            window
                .write_u32(0, (message.address & 0xFFFF_FFFF) as u32)
                .map_err(WindowError::as_driver_error)?;
            window
                .write_u32(4, (message.address >> 32) as u32)
                .map_err(WindowError::as_driver_error)?;
            window
                .write_u32(8, message.data)
                .map_err(WindowError::as_driver_error)?;
            window
                .write_u32(12, 0)
                .map_err(WindowError::as_driver_error)?;
            Ok(())
        }
    }

    #[test]
    fn trait_object_routes_a_message() {
        let bus: &dyn MsixBus = &FakeBus { table_size: 4 };
        let mapper = FakeMapper {
            last: Cell::new(None),
            grant: true,
        };
        let message = MsiMessage {
            address: 0xFEE0_1000,
            data: 0x0000_0041,
        };
        bus.route_msix(0x0800, 1, message, &mapper)
            .expect("routes the entry");
        assert_eq!(mapper.last.get(), Some((0xC100_0010, 16)));
        // SAFETY: single-threaded test reading the static the mapper
        // window wrote through.
        unsafe {
            assert_eq!(ENTRY[0], 0xFEE0_1000);
            assert_eq!(ENTRY[1], 0);
            assert_eq!(ENTRY[2], 0x0000_0041);
            assert_eq!(ENTRY[3], 0);
        }
    }

    #[test]
    fn entry_beyond_table_is_out_of_range() {
        let bus: &dyn MsixBus = &FakeBus { table_size: 2 };
        let mapper = FakeMapper {
            last: Cell::new(None),
            grant: true,
        };
        let message = MsiMessage {
            address: 0xFEE0_0000,
            data: 0x30,
        };
        assert_eq!(
            bus.route_msix(0x0800, 2, message, &mapper),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn missing_capability_propagates_as_permission_denied() {
        let bus: &dyn MsixBus = &FakeBus { table_size: 4 };
        let mapper = FakeMapper {
            last: Cell::new(None),
            grant: false,
        };
        let message = MsiMessage {
            address: 0xFEE0_0000,
            data: 0x30,
        };
        assert_eq!(
            bus.route_msix(0x0800, 0, message, &mapper),
            Err(DriverError::PermissionDenied)
        );
    }
}
