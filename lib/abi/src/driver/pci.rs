//! Generic PCI/PCIe transport seam (`abi-v1`).
//!
//! [`VirtioPciBus`](super::virtio_pci::VirtioPciBus) provisions the
//! *virtio*-specific register windows a virtio transport needs. A
//! non-virtio PCI device — an xHCI USB host controller, say — needs a
//! different, smaller surface: the physical window of one of its base
//! address registers (BARs), and the function's bus-mastering bit set
//! so it may issue the upstream DMA its rings live in.
//!
//! [`PciBus`] is that surface. The PCI bus driver (`drivers/bus/pci`)
//! implements it; a device-class driver (`drivers/bus/usb`, …) reaches
//! the bus through a `&dyn PciBus` rather than depending on the
//! concrete bus crate (`AGENTS.md` §8 / §17.4 — a driver crate's only
//! public surface is `register`, and one driver never names another).
//! [`Bus`] is a supertrait so a single trait object can both enumerate
//! the bus (to pick the function) and provision it.
//!
//! Like every other `lib/abi` item the trait is held to the §9 ABI
//! discipline; while `abi-v1` is unfrozen it may still evolve in place
//! (`AGENTS.md` §2.13), every caller updated in the same change.

use super::bus::Bus;
use super::{DriverError, MmioMapper, RegisterWindow};

/// A PCI bus that can provision a non-virtio function's resources.
///
/// # Capabilities
///
/// [`map_bar_window`](Self::map_bar_window) routes through the supplied
/// [`MmioMapper`], which enforces
/// [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP); the
/// implementation synthesises no pointer itself (`AGENTS.md` §4 — no
/// ambient authority). [`enable_bus_master`](Self::enable_bus_master)
/// touches only the function's own configuration space, which the bus
/// driver already reaches by holding its [`DriverHandle`](crate::driver::DriverHandle).
pub trait PciBus: Bus {
    /// Resolve the memory BAR at `bar_index` on function `bdf` and ask
    /// `mapper` to map it, returning the resulting [`RegisterWindow`].
    ///
    /// This is the hand-off a memory-mapped device driver consumes:
    /// the bus driver reads the BAR's physical base and probed size
    /// from configuration space and asks the kernel's MMIO-map facility
    /// for a window over exactly that region. The driver never
    /// synthesises a pointer — the kernel allocates and validates the
    /// mapping (`AGENTS.md` §4).
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — no BAR with `bar_index` exists, or
    ///   the BAR is unused (probed size zero).
    /// * [`DriverError::Unsupported`] — the BAR is an I/O-port BAR
    ///   (reached through port I/O, not a mapped window), or the
    ///   function is not a type-0 header.
    /// * [`DriverError::LengthOutOfRange`] — the BAR size does not fit
    ///   in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](crate::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    fn map_bar_window(
        &self,
        bdf: u64,
        bar_index: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError>;

    /// Enable memory-space decoding and bus-mastering on function
    /// `bdf` (PCI Local Bus 3.0 §6.2.2).
    ///
    /// Firmware leaves the Bus Master Enable bit clear, so a function
    /// whose BAR is mapped but whose bus-master bit is clear can never
    /// issue the upstream memory transactions its DMA rings depend on.
    /// A driver that programs a device for DMA calls this once before
    /// it expects the controller to touch host memory.
    ///
    /// The status half of the command/status register is RW1C, so the
    /// implementation must preserve the low command bits, write the
    /// high status bits as zero, and OR in the two enable bits.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the configuration write cannot
    ///   be completed by the bus transport.
    fn enable_bus_master(&self, bdf: u64) -> Result<(), DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::bus::BusDevice;
    use crate::driver::mmio::MmioMapError;
    use core::cell::Cell;
    use core::ptr::NonNull;

    /// 4-byte-aligned backing so a window base satisfies
    /// `RegisterWindow::from_mapping`'s alignment contract.
    static mut BACKING: [u32; 16] = [0u32; 16];

    struct FakeMapper {
        grant: bool,
        last: Cell<Option<(u64, usize)>>,
    }

    impl MmioMapper for FakeMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.grant {
                return Err(MmioMapError::CapabilityMissing);
            }
            self.last.set(Some((phys_base, len)));
            let base = NonNull::new(core::ptr::addr_of_mut!(BACKING).cast::<u8>())
                .expect("static is non-null");
            // SAFETY: single-threaded test; the static outlives the
            // window and the window only touches `len <= 64` bytes.
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len.min(64)) })
        }
    }

    struct FakeBus {
        bar_base: u64,
        bar_size: u64,
        master_enabled: Cell<bool>,
    }

    impl Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.is_empty() {
                return Err(DriverError::BufferTooSmall);
            }
            out[0] = BusDevice {
                vendor: 0x1106,
                device: 0x3483,
                class: 0x0C03,
                reserved0: 0,
                address: 0x0001_0000,
            };
            Ok(1)
        }
    }

    impl PciBus for FakeBus {
        fn map_bar_window(
            &self,
            _bdf: u64,
            bar_index: u8,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            if bar_index != 0 {
                return Err(DriverError::NotFound);
            }
            if self.bar_size == 0 {
                return Err(DriverError::NotFound);
            }
            let len = usize::try_from(self.bar_size).map_err(|_| DriverError::LengthOutOfRange)?;
            mapper
                .map_window(self.bar_base, len)
                .map_err(MmioMapError::as_driver_error)
        }

        fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
            self.master_enabled.set(true);
            Ok(())
        }
    }

    fn bus() -> FakeBus {
        FakeBus {
            bar_base: 0x6000_0000,
            bar_size: 0x40,
            master_enabled: Cell::new(false),
        }
    }

    #[test]
    fn trait_object_maps_the_bar_and_enables_mastering() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        let mapper = FakeMapper {
            grant: true,
            last: Cell::new(None),
        };
        dyn_bus
            .enable_bus_master(0x0001_0000)
            .expect("bus master enable");
        let window = dyn_bus
            .map_bar_window(0x0001_0000, 0, &mapper)
            .expect("bar window");
        assert_eq!(window.len(), 0x40);
        assert_eq!(mapper.last.get(), Some((0x6000_0000, 0x40)));
        assert!(bus.master_enabled.get());
    }

    #[test]
    fn missing_bar_is_not_found() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        let mapper = FakeMapper {
            grant: true,
            last: Cell::new(None),
        };
        assert!(matches!(
            dyn_bus.map_bar_window(0x0001_0000, 2, &mapper),
            Err(DriverError::NotFound)
        ));
    }

    #[test]
    fn missing_capability_propagates_as_permission_denied() {
        let bus = bus();
        let dyn_bus: &dyn PciBus = &bus;
        let mapper = FakeMapper {
            grant: false,
            last: Cell::new(None),
        };
        assert!(matches!(
            dyn_bus.map_bar_window(0x0001_0000, 0, &mapper),
            Err(DriverError::PermissionDenied)
        ));
    }
}
