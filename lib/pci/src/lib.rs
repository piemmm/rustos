//! TAIRiX PCI/PCIe configuration-access mechanism library.
//!
//! A `no_std` `lib/*` crate: it implements the
//! `abi-v1` PCI/PCIe bus and transport seams
//! ([`tairix_abi::driver::bus::Bus`], [`VirtioPciBus`], [`MsixBus`],
//! [`PciBus`]) over three configuration-access mechanisms, and exposes them
//! through the [`mechanism_one`] / [`mechanism_ecam`] / [`mechanism_brcm`]
//! constructors. The concrete `Pci<C>` type stays crate-private — every
//! caller borrows the result as `&dyn Bus` / `&dyn VirtioPciBus` /
//! `&dyn MsixBus` / `&dyn PciBus` and never names it.
//!
//! It lives in `lib/` (not `drivers/`) because PCI configuration access is
//! shared *bus-protocol* logic, not a single device's driver: the kernel's
//! ring-0 boot pipeline, a user-space bus driver (`drivers/bus/pcie_brcm`),
//! and the host-side integration tests all compose it through the public
//! seams above — a `drivers/*` crate may not depend on another `drivers/*`
//! crate, so a user-space driver reaches the mechanism
//! here, never through a sibling driver. This mirrors the `lib/usb` ↔
//! `drivers/bus/usb` and `lib/virtio` ↔ `drivers/bus/virtio` split.
//!
//! The three mechanisms are: x86_64 **mechanism #1** (PCI Local Bus 3.0
//! §3.2.2.3.2 — the `0xCF8`/`0xCFC` address/data port pair), **ECAM**
//! (PCI Express Base 3.0 §7.2.2 — flat MMIO configuration space), and the
//! **BCM2711 windowed** mechanism (the Raspberry Pi 4 root complex's
//! index/data window pair). MSI / MSI-X capabilities are *discovered* and
//! routed through [`MsixBus::route_msix`]; the BAR walker maps a BAR only
//! through the supplied [`MmioMapper`], which enforces
//! [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP) kernel-side
//! (no ambient authority).
//!
//! # Safety
//!
//! This crate contains no `unsafe`: the `in`/`out` instructions that reach
//! I/O ports `0xCF8`/`0xCFC` live in the architecture port behind the
//! [`tairix_abi::PortIo`] seam, driven only through
//! `mech_one::PortIoConfigSpace`, which the unit tests exercise against a
//! recording mock.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::bus::{Bus, BusDevice};
use tairix_abi::driver::msix::MsixBus;
use tairix_abi::driver::pci::PciBus;
use tairix_abi::driver::virtio_pci::VirtioPciBus;
use tairix_abi::{DriverError, HwNode, MmioMapper, MsiMessage, PortIo, RegisterWindow};

pub(crate) mod config;
pub(crate) mod enumerate;
mod locate;
pub(crate) mod mech_brcm;
pub(crate) mod mech_ecam;
pub(crate) mod mech_one;

#[cfg(test)]
mod tests;

// The shared bus-driver locate/BAR primitives (`locate.rs`): the one
// definition the generic xHCI driver and a root-complex bus driver both
// reach, re-exported at the crate root beside the
// `mechanism_*` constructors.
pub use locate::{
    assign_and_map_bar, bus_to_cpu_phys, find_function_by_class, USB_CONTROLLER_CLASS,
};

// --- Real-hardware construction seam --------------------------------------

/// Construct a real-hardware PCI root bus over configuration
/// **mechanism #1** (PCI Local Bus 3.0 §3.2.2.3.2): the address word
/// at I/O port `0xCF8` selects a `(bus, device, function, register)`
/// tuple and the data word at `0xCFC` reads/writes the corresponding
/// configuration dword. The `pio` backend issues the port accesses
/// (the x86_64 architecture port supplies [`tairix_abi::PortIo`]).
///
/// The returned value is the bus the ring-0 boot pipeline drives
/// through the three frozen `abi-v1` seams — [`Bus`] (enumeration),
/// [`VirtioPciBus`] (virtio register-window provisioning), and
/// [`MsixBus`] (MSI-X interrupt routing). [`VirtioPciBus`] and
/// [`MsixBus`] both have [`Bus`] as a supertrait, so the return bound
/// names only the two leaf seams; the opaque type still implements
/// [`Bus`] and coerces to `&dyn Bus`. The concrete `Pci` type stays
/// crate-private: callers borrow the result as
/// `&dyn Bus` / `&dyn VirtioPciBus` / `&dyn MsixBus` and never name it.
///
/// Construction performs **no** I/O — it only stores the supplied
/// port-I/O backend — so it is sound to call before the PCI host
/// bridge has been probed. Configuration access happens lazily on the
/// trait methods, each of which drives the [`PortIo`] backend against
/// the two legacy configuration ports.
///
/// # Platform
///
/// Mechanism #1 is x86-only, but that architecture knowledge lives
/// entirely in the [`PortIo`] backend the caller supplies — the
/// architecture port (`kernel/arch/x86_64`) provides the only
/// real implementation. This constructor is therefore
/// architecture-neutral and carries no target-conditional `cfg` gate; architectures without an I/O port
/// space simply never call it and reach `PCIe` through memory-mapped
/// ECAM, a separate seam.
#[must_use]
pub fn mechanism_one<P: PortIo>(pio: P) -> impl VirtioPciBus + MsixBus + PciBus {
    Pci::new(mech_one::PortIoConfigSpace::new(pio))
}

/// Construct a real-hardware `PCIe` root bus over the **enhanced
/// configuration access mechanism** (ECAM / MMCONFIG, PCI Express
/// Base 3.0 §7.2.2): the host bridge maps configuration space flat
/// into MMIO, one 4 KiB block per `(bus, device, function)`, and a
/// configuration dword is reached by a naturally-aligned access at the
/// computed offset within `window`.
///
/// `window` is the kernel-mapped [`RegisterWindow`] over the host
/// bridge's configuration region, obtained from the MMIO-map facility
/// after a [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP)
/// check. Its base
/// is the physical base of `(bus 0, device 0, function 0, register 0)`
/// and its length bounds the buses the enumeration can reach: an
/// access past the window resolves to the PCI "no device" sentinel, so
/// the walk fails closed rather than reading out of bounds.
///
/// The returned value is the bus the ring-0 boot pipeline drives
/// through the [`Bus`], [`VirtioPciBus`], and [`MsixBus`] seams,
/// identically to [`mechanism_one`]; the concrete `Pci` type stays
/// crate-private.
///
/// Construction performs **no** I/O — it only stores the supplied
/// window. Configuration access happens lazily on the trait methods.
///
/// # Platform
///
/// ECAM is architecture-neutral: the window is just mapped memory, so
/// this constructor carries no target-conditional `cfg` gate. It is the path the Raspberry Pi 4
/// (BCM2711) root complex uses to reach the VL805 USB host
/// controller, and the path any `PCIe` host bridge without an I/O-port
/// space uses.
#[must_use]
pub fn mechanism_ecam(window: RegisterWindow) -> impl VirtioPciBus + MsixBus + PciBus {
    Pci::new(mech_ecam::EcamConfigSpace::new(window))
}

/// Construct a real-hardware `PCIe` root bus over the BCM2711
/// **windowed** configuration access mechanism: the Raspberry Pi 4 root
/// complex does not map configuration space flat like ECAM but reaches a
/// downstream function through an index/data window pair inside the
/// controller's own register block (the BCM2711 windowed `ConfigSpace`).
///
/// `window` is the kernel-mapped [`RegisterWindow`] over the PCIe
/// controller's register block — the very window the BCM2711 PCIe
/// host-bridge bring-up driver (`drivers/bus/pcie_brcm`) trained the
/// link through — obtained from the MMIO-map facility after a
/// [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP) check. The link must be
/// up before any downstream configuration access, or the controller
/// raises a CPU abort; the bring-up driver guarantees this before
/// handing the window here.
///
/// `secondary_bus` is the bus number the bring-up driver programmed into
/// the root port's bridge bus-number register. The BCM2711 root port is
/// a single-device link, so the windowed mechanism forwards a
/// configuration transaction only to `device 0` on this one bus and
/// resolves every other downstream target to the PCI "no device"
/// sentinel without touching the controller — otherwise a flat
/// enumeration would forward unanswered config TLPs and the root complex
/// would raise a CPU abort.
///
/// The returned value is driven through the [`Bus`], [`VirtioPciBus`],
/// [`MsixBus`], and [`PciBus`] seams identically to [`mechanism_ecam`];
/// the concrete `Pci` type stays crate-private.
/// Construction performs **no** I/O.
///
/// # Platform
///
/// The index/data windowing is BCM2711-specific, but it is expressed
/// entirely as offsets within the supplied memory window, so this
/// constructor carries no target-conditional `cfg` gate: an architecture without a BCM2711 root complex
/// simply never calls it.
#[must_use]
pub fn mechanism_brcm(
    window: RegisterWindow,
    secondary_bus: u8,
) -> impl VirtioPciBus + MsixBus + PciBus {
    Pci::new(mech_brcm::BrcmConfigSpace::new(window, secondary_bus))
}

// --- Trait-object seams over the concrete `Pci<C>` ------------------------
//
// The impls below are the surface a composing host reaches; each is
// reached through `&dyn Bus` / `&dyn VirtioPciBus` / `&dyn MsixBus` /
// `&dyn PciBus`, never through the crate-private concrete `Pci<C>` type.

use config::ConfigSpace;
use enumerate::Pci;

impl<C: ConfigSpace> Bus for Pci<C> {
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        self.enumerate_into(out)
    }
}

// The frozen `abi-v1` virtio-PCI transport-provisioning seam. The ring-0 boot walk reaches the concrete `Pci`
// through `&dyn VirtioPciBus`, so the bus driver never leaks its
// concrete type across the crate boundary. Both
// methods forward to the inherent enumeration core; the inherent
// `Pci::map_virtio_window` wins method resolution, so the forward is
// not recursive.
impl<C: ConfigSpace> VirtioPciBus for Pci<C> {
    fn map_virtio_window(
        &self,
        bdf: u64,
        cfg_type: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        Pci::map_virtio_window(self, bdf, cfg_type, mapper)
    }

    fn notify_off_multiplier(&self, bdf: u64) -> Result<u32, DriverError> {
        self.virtio_notify_off_multiplier(bdf)
    }
}

// The frozen `abi-v1` MSI-X interrupt-routing seam.
// Ring 0 reaches the concrete `Pci` through `&dyn MsixBus` to route a
// device's interrupt, never naming the driver's concrete type. The inherent `Pci::route_msix` wins method
// resolution, so the forward is not recursive.
impl<C: ConfigSpace> MsixBus for Pci<C> {
    fn route_msix(
        &self,
        bdf: u64,
        entry: u16,
        message: MsiMessage,
        mapper: &dyn MmioMapper,
    ) -> Result<(), DriverError> {
        Pci::route_msix(self, bdf, entry, message, mapper)
    }
}

// The `abi-v1` generic-PCI transport seam: the surface
// a non-virtio, DMA-driving device driver (xHCI) consumes to map one of
// the controller's BARs and enable bus mastering, reached through
// `&dyn PciBus` so the device driver never names this concrete crate. Both methods forward to the inherent
// enumeration core; the inherent methods win method resolution, so the
// forward is not recursive.
impl<C: ConfigSpace> PciBus for Pci<C> {
    fn map_bar_window(
        &self,
        bdf: u64,
        bar_index: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        Pci::map_bar_window(self, bdf, bar_index, mapper)
    }

    fn enable_bus_master(&self, bdf: u64) -> Result<(), DriverError> {
        Pci::enable_bus_master(self, bdf);
        Ok(())
    }

    fn assign_bar(
        &self,
        bdf: u64,
        bar_index: u8,
        window_base: u64,
        window_size: u64,
    ) -> Result<u64, DriverError> {
        Pci::assign_bar(self, bdf, bar_index, window_base, window_size)
    }

    fn read_config(&self, bdf: u64, offset: u16) -> Result<u32, DriverError> {
        Ok(Pci::read_config(self, bdf, offset))
    }

    fn describe_function(&self, bdf: u64) -> Result<HwNode, DriverError> {
        Pci::describe_function(self, bdf)
    }

    fn route_msi(&self, bdf: u64, message: MsiMessage) -> Result<(), DriverError> {
        Pci::route_msi(self, bdf, message)
    }
}
