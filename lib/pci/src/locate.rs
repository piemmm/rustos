//! Locating a target function on a PCI(e) bus and bringing its register BAR
//! online.
//!
//! These are the shared bus-driver primitives a DMA-driving host-controller
//! driver (the generic xHCI driver) and a root-complex bus driver (the
//! BCM2711 PCIe driver) both need: find the function to bind, and assign /
//! enable / map its register BAR. They are defined here once, beside the
//! configuration-access mechanism they drive, so the two callers share one
//! definition rather than each carrying its own copy (`AGENTS.md` §2.2 /
//! §17.4). Each operates over the `abi-v1` [`PciBus`] seam, so a caller never
//! names this concrete crate's internals (`AGENTS.md` §8).

use rustos_abi::driver::bus::BusDevice;
use rustos_abi::driver::pci::PciBus;
use rustos_abi::{DriverError, MmioMapper, RegisterWindow};

/// PCI base-class + sub-class identifying a USB host controller (PCI Local
/// Bus 3.0 Appendix D: base `0x0C` Serial Bus Controller, sub-class `0x03`
/// USB).
///
/// A standard PCI *class* code, not a vendor or product identity
/// (`AGENTS.md` §8): a bus driver locates *a* USB host controller behind a
/// bridge by this class without naming any specific part. It lives here,
/// beside the configuration-access mechanism that reads a function's class,
/// as the one definition both the generic xHCI driver and a root-complex bus
/// driver share (`AGENTS.md` §2.2).
pub const USB_CONTROLLER_CLASS: u16 = 0x0C03;

/// Upper bound on functions scanned while locating a target by class.
///
/// A defence bound (`AGENTS.md` §24.4), not a capacity: a bounded scan stops
/// a malfunctioning or hostile bus from driving an unbounded enumeration. A
/// handful of functions covers every real single-controller bridge; the
/// populated prefix is searched even when the bus reports more.
const MAX_BUS_SCAN: usize = 32;

/// Locate the bus-local address (BDF) of the first function on `bus` whose
/// 16-bit `class:subclass` equals `class`.
///
/// Enumerates into a bounded buffer (`MAX_BUS_SCAN`) and matches `class`.
/// A [`DriverError::BufferTooSmall`] from the bus still fills the buffer, so
/// the populated prefix is searched either way rather than failing a bring-up
/// on an oversized bus.
///
/// # Errors
///
/// * [`DriverError::NotFound`] — no function on the bus carries `class`
///   (fail closed, never a fabricated target, `AGENTS.md` §2.9 / §18.5).
/// * any non-[`BufferTooSmall`](DriverError::BufferTooSmall) error of
///   [`Bus::enumerate`](rustos_abi::driver::bus::Bus::enumerate).
pub fn find_function_by_class(bus: &dyn PciBus, class: u16) -> Result<u64, DriverError> {
    let mut devices = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; MAX_BUS_SCAN];
    let found = match bus.enumerate(&mut devices) {
        Ok(n) => n,
        // The bus filled the buffer before reporting the overflow; search the
        // populated prefix rather than failing the whole bring-up.
        Err(DriverError::BufferTooSmall) => devices.len(),
        Err(other) => return Err(other),
    };
    devices[..found]
        .iter()
        .find(|d| d.class == class)
        .map(|d| d.address)
        .ok_or(DriverError::NotFound)
}

/// Assign (when firmware left it unassigned), enable bus-mastering on, and
/// map the memory BAR `bar_index` of function `bdf`, returning the mapped
/// register window.
///
/// This is the prefix a DMA-driving PCI device driver runs before it can
/// touch the controller, composed from three [`PciBus`] seam calls so the
/// callers share one definition (`AGENTS.md` §2.2):
///
/// * [`PciBus::assign_bar`] places the BAR at a size-aligned address inside
///   the bridge's outbound window `outbound_window` (`(pcie_base, size)`,
///   PCIe-bus space) when firmware left it unassigned — a no-op that returns
///   the existing base otherwise;
/// * [`PciBus::enable_bus_master`] sets the bus-master enable bit the
///   controller's upstream DMA depends on (firmware leaves it clear, PCI
///   Local Bus 3.0 §6.2.2); and
/// * [`PciBus::map_bar_window`] maps the BAR through `mapper`, which enforces
///   [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP)
///   kernel-side (`AGENTS.md` §4 — no ambient authority).
///
/// The returned window's [`phys_base`](RegisterWindow::phys_base) is the
/// BAR's assigned base in the address space `mapper` maps (PCIe-bus space
/// when the bridge's mapper translates), and its
/// [`len`](RegisterWindow::len) the probed BAR size.
///
/// # Errors
///
/// Every step fails closed (`AGENTS.md` §5.4): any error of
/// [`PciBus::assign_bar`] (the BAR cannot be placed inside `outbound_window`),
/// [`PciBus::enable_bus_master`], or [`PciBus::map_bar_window`] (no such BAR,
/// an I/O-port BAR, a size past `usize`, or a missing
/// [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP)).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP)
/// (the BAR map, enforced by `mapper` kernel-side).
pub fn assign_and_map_bar(
    bus: &dyn PciBus,
    bdf: u64,
    bar_index: u8,
    outbound_window: (u64, u64),
    mapper: &dyn MmioMapper,
) -> Result<RegisterWindow, DriverError> {
    let (outbound_base, outbound_size) = outbound_window;
    bus.assign_bar(bdf, bar_index, outbound_base, outbound_size)?;
    bus.enable_bus_master(bdf)?;
    bus.map_bar_window(bdf, bar_index, mapper)
}

/// Translate a PCIe-bus address `bus_addr` lying inside a host bridge's
/// outbound window into the CPU-physical address it decodes to.
///
/// `outbound` is the bridge's outbound window as `(cpu_base, pcie_base,
/// size)` — the CPU aperture base, the PCIe-bus base it maps to, and the
/// size — exactly as the discovered [`BusWindow`](rustos_abi::HwResourceKind::BusWindow)
/// resource carries it (`AGENTS.md` §18.1). A device's BAR is assigned a
/// PCIe-bus address inside this window ([`assign_and_map_bar`]); a bus driver
/// publishing that BAR as a child node's CPU-physical
/// [`Mmio`](rustos_abi::HwResourceKind::Mmio) grant translates it here, so the
/// arithmetic has one definition (`AGENTS.md` §2.2) and the result is a
/// CPU-side address the kernel's grant-coverage check accepts against the
/// bridge's outbound `BusWindow` grant (`HwResource::covers`).
///
/// Returns the CPU-physical address, or [`None`] (fail closed, `AGENTS.md`
/// §5.4) when `bus_addr` lies below the window's PCIe base, at or past its
/// top, or the CPU-side sum overflows — never a wrapped or invented address.
#[must_use]
pub fn bus_to_cpu_phys(outbound: (u64, u64, u64), bus_addr: u64) -> Option<u64> {
    let (cpu_base, pcie_base, size) = outbound;
    let offset = bus_addr.checked_sub(pcie_base)?;
    if offset >= size {
        return None;
    }
    cpu_base.checked_add(offset)
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    // `Bus` is the supertrait the `StubBus` double implements; it is not
    // needed to *call* `enumerate` on a `&dyn PciBus` (the method is in the
    // trait object's vtable), only to write the `impl` below.
    use rustos_abi::driver::bus::Bus;
    use rustos_abi::HwNode;

    /// A recording [`PciBus`] double: a fixed device list for
    /// [`find_function_by_class`], and assign/enable/map calls captured so a
    /// test asserts [`assign_and_map_bar`] drove the three steps in order.
    struct StubBus {
        devices: &'static [BusDevice],
        calls: core::cell::RefCell<alloc::vec::Vec<&'static str>>,
    }

    impl Bus for StubBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            let n = out.len().min(self.devices.len());
            out[..n].copy_from_slice(&self.devices[..n]);
            if self.devices.len() > out.len() {
                return Err(DriverError::BufferTooSmall);
            }
            Ok(n)
        }
    }

    impl PciBus for StubBus {
        fn map_bar_window(
            &self,
            _bdf: u64,
            _bar_index: u8,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            self.calls.borrow_mut().push("map");
            mapper
                .map_window(0xc000_0000, 0x1000)
                .map_err(|_| DriverError::DeviceFault)
        }

        fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
            self.calls.borrow_mut().push("enable");
            Ok(())
        }

        fn assign_bar(
            &self,
            _bdf: u64,
            _bar_index: u8,
            window_base: u64,
            _window_size: u64,
        ) -> Result<u64, DriverError> {
            self.calls.borrow_mut().push("assign");
            Ok(window_base)
        }

        fn read_config(&self, _bdf: u64, _offset: u16) -> Result<u32, DriverError> {
            Ok(0)
        }

        fn describe_function(&self, _bdf: u64) -> Result<HwNode, DriverError> {
            Err(DriverError::NotImplemented)
        }
    }

    struct OkMapper;
    impl MmioMapper for OkMapper {
        fn map_window(
            &self,
            phys_base: u64,
            len: usize,
        ) -> Result<RegisterWindow, rustos_abi::MmioMapError> {
            // A leaked, aligned, zeroed window for the test process lifetime.
            let words = len.div_ceil(4).max(1);
            let buf: alloc::boxed::Box<[u32]> = alloc::vec![0u32; words].into_boxed_slice();
            let base =
                core::ptr::NonNull::new(alloc::boxed::Box::leak(buf).as_mut_ptr().cast::<u8>())
                    .expect("non-null");
            // SAFETY: `base` covers `len` zeroed bytes, is 4-byte aligned,
            // leaked for the whole test process, and unaliased.
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
        }
    }

    #[test]
    fn finds_the_first_function_of_the_requested_class() {
        static DEVICES: [BusDevice; 3] = [
            BusDevice {
                vendor: 0x1234,
                device: 0,
                class: 0x0600,
                reserved0: 0,
                address: 0x0000,
            },
            BusDevice {
                vendor: 0x1106,
                device: 0x3483,
                class: 0x0C03,
                reserved0: 0,
                address: 0x0001_0000,
            },
            BusDevice {
                vendor: 0x1106,
                device: 0x3483,
                class: 0x0C03,
                reserved0: 0,
                address: 0x0002_0000,
            },
        ];
        let bus = StubBus {
            devices: &DEVICES,
            calls: core::cell::RefCell::new(alloc::vec::Vec::new()),
        };
        assert_eq!(
            find_function_by_class(&bus, USB_CONTROLLER_CLASS),
            Ok(0x0001_0000)
        );
    }

    #[test]
    fn missing_class_fails_closed_not_found() {
        static DEVICES: [BusDevice; 1] = [BusDevice {
            vendor: 0x1234,
            device: 0,
            class: 0x0600,
            reserved0: 0,
            address: 0,
        }];
        let bus = StubBus {
            devices: &DEVICES,
            calls: core::cell::RefCell::new(alloc::vec::Vec::new()),
        };
        assert_eq!(
            find_function_by_class(&bus, USB_CONTROLLER_CLASS),
            Err(DriverError::NotFound)
        );
    }

    #[test]
    fn assign_and_map_drives_assign_then_enable_then_map() {
        static DEVICES: [BusDevice; 1] = [BusDevice {
            vendor: 0x1106,
            device: 0x3483,
            class: 0x0C03,
            reserved0: 0,
            address: 0x0001_0000,
        }];
        let bus = StubBus {
            devices: &DEVICES,
            calls: core::cell::RefCell::new(alloc::vec::Vec::new()),
        };
        let window =
            assign_and_map_bar(&bus, 0x0001_0000, 0, (0xc000_0000, 0x4000_0000), &OkMapper)
                .expect("maps the BAR");
        assert_eq!(window.phys_base(), 0xc000_0000);
        assert_eq!(bus.calls.borrow().as_slice(), &["assign", "enable", "map"]);
    }

    #[test]
    fn bus_to_cpu_phys_translates_inside_the_window_and_fails_closed_outside() {
        // Pi 4 outbound window: CPU 0x6_0000_0000 → PCIe 0xc000_0000, 1 GiB.
        let outbound = (0x6_0000_0000, 0xc000_0000, 0x4000_0000);
        assert_eq!(bus_to_cpu_phys(outbound, 0xc000_0000), Some(0x6_0000_0000));
        assert_eq!(bus_to_cpu_phys(outbound, 0xc000_1000), Some(0x6_0000_1000));
        // Below the PCIe base, and at/over the window top: fail closed.
        assert_eq!(bus_to_cpu_phys(outbound, 0xbfff_ffff), None);
        assert_eq!(bus_to_cpu_phys(outbound, 0x1_0000_0000), None);
    }
}
