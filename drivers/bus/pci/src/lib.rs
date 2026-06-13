//! RustOS PCI/PCIe bus driver.
//!
//! Implements the bus enumeration class trait
//! ([`rustos_abi::driver::bus::Bus`]) on top of the x86_64
//! configuration-access **mechanism #1** (PCI Local Bus 3.0 §3.2.2.3.2):
//! the 32-bit configuration address word at I/O port `0xCF8` selects
//! a `(bus, device, function, register)` tuple and the 32-bit data
//! word at I/O port `0xCFC` reads or writes the corresponding
//! configuration dword.
//!
//! Per `AGENTS.md` §8 the only public surface of a driver crate is
//! `pub fn register(host) -> Result<DriverHandle, DriverError>`.
//! Everything below is intentionally `pub(crate)` and tested through
//! the in-crate `#[cfg(test)]` module against a mock
//! `ConfigSpace` fixture that mirrors QEMU's `q35` default PCI
//! topology (LPC bridge, `SMBus` controller, plus the virtio-net
//! function the driver-host integration test will attach later).
//!
//! Per the Stage 4 sub-bullet on bus drivers in `PLAN.md`, MSI / MSI-X
//! capabilities are *discovered* but never enabled here — actual
//! interrupt routing is the responsibility of the `virtio_blk` /
//! `virtio_net` drivers in Stage 4.D. The BAR walker likewise
//! produces `BarDescriptor` records but never invokes the kernel
//! memory capability: callers route the mapping request through the
//! driver host once 4.D wires up the host-side memory facility.
//!
//! # Safety
//!
//! This crate contains no `unsafe`: the `in`/`out` instructions that
//! reach I/O ports `0xCF8`/`0xCFC` live in the architecture port behind
//! the [`rustos_abi::PortIo`] seam (`AGENTS.md` §17.2). The driver only
//! ever drives that seam through `mech_one::PortIoConfigSpace`, which
//! the unit tests exercise against a recording mock.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::msix::MsixBus;
use rustos_abi::driver::pci::PciBus;
use rustos_abi::driver::virtio_pci::VirtioPciBus;
use rustos_abi::{
    CapabilityId, DriverError, DriverHandle, DriverHost, HwNode, MmioMapper, MsiMessage, PortIo,
    RegisterWindow,
};

pub(crate) mod config;
pub(crate) mod enumerate;
pub(crate) mod mech_brcm;
pub(crate) mod mech_ecam;
pub(crate) mod mech_one;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The driver host re-issues a host-local handle when binding this
/// driver into its load table; this constant is the on-the-wire
/// signal that `register` cleared every gate (`AGENTS.md` §8).
const REGISTER_HANDLE_MARKER: u64 = 0x5043_4900_0000_0001;

/// Driver entry point (`AGENTS.md` §8).
///
/// Verifies the host already granted [`CapabilityId::DRV_LOAD`] and
/// returns the registration marker handle. No hardware probe runs
/// here; enumeration is driven by the host once it dispatches into
/// [`Bus::enumerate`] on the per-driver [`Bus`] trait object.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
/// * [`DriverError::OutOfRange`] is impossible by construction: the
///   marker is non-zero.
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

// --- Real-hardware construction seam --------------------------------------

/// Construct a real-hardware PCI root bus over configuration
/// **mechanism #1** (PCI Local Bus 3.0 §3.2.2.3.2): the address word
/// at I/O port `0xCF8` selects a `(bus, device, function, register)`
/// tuple and the data word at `0xCFC` reads/writes the corresponding
/// configuration dword. The `pio` backend issues the port accesses
/// (the x86_64 architecture port supplies [`rustos_abi::PortIo`]).
///
/// The returned value is the bus the ring-0 boot pipeline drives
/// through the three frozen `abi-v1` seams — [`Bus`] (enumeration),
/// [`VirtioPciBus`] (virtio register-window provisioning), and
/// [`MsixBus`] (MSI-X interrupt routing). [`VirtioPciBus`] and
/// [`MsixBus`] both have [`Bus`] as a supertrait, so the return bound
/// names only the two leaf seams; the opaque type still implements
/// [`Bus`] and coerces to `&dyn Bus`. The concrete `Pci` type stays
/// crate-private (`AGENTS.md` §8): callers borrow the result as
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
/// architecture-neutral and carries no target-conditional `cfg` gate
/// (`AGENTS.md` §17.2 / §17.4); architectures without an I/O port
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
/// after a [`CapabilityId::MMIO_MAP`] check (`AGENTS.md` §4). Its base
/// is the physical base of `(bus 0, device 0, function 0, register 0)`
/// and its length bounds the buses the enumeration can reach: an
/// access past the window resolves to the PCI "no device" sentinel, so
/// the walk fails closed rather than reading out of bounds.
///
/// The returned value is the bus the ring-0 boot pipeline drives
/// through the [`Bus`], [`VirtioPciBus`], and [`MsixBus`] seams,
/// identically to [`mechanism_one`]; the concrete `Pci` type stays
/// crate-private (`AGENTS.md` §8).
///
/// Construction performs **no** I/O — it only stores the supplied
/// window. Configuration access happens lazily on the trait methods.
///
/// # Platform
///
/// ECAM is architecture-neutral: the window is just mapped memory, so
/// this constructor carries no target-conditional `cfg` gate
/// (`AGENTS.md` §17.2 / §17.4). It is the path the Raspberry Pi 4
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
/// [`CapabilityId::MMIO_MAP`] check (`AGENTS.md` §4). The link must be
/// up before any downstream configuration access, or the controller
/// raises a CPU abort; the bring-up driver guarantees this before
/// handing the window here.
///
/// The returned value is driven through the [`Bus`], [`VirtioPciBus`],
/// [`MsixBus`], and [`PciBus`] seams identically to [`mechanism_ecam`];
/// the concrete `Pci` type stays crate-private (`AGENTS.md` §8).
/// Construction performs **no** I/O.
///
/// # Platform
///
/// The index/data windowing is BCM2711-specific, but it is expressed
/// entirely as offsets within the supplied memory window, so this
/// constructor carries no target-conditional `cfg` gate (`AGENTS.md`
/// §17.2 / §17.4): an architecture without a BCM2711 root complex
/// simply never calls it.
#[must_use]
pub fn mechanism_brcm(window: RegisterWindow) -> impl VirtioPciBus + MsixBus + PciBus {
    Pci::new(mech_brcm::BrcmConfigSpace::new(window))
}

// --- Public re-exports through the `Bus` trait ----------------------------
//
// The trait impl below is the only post-`register` surface a host may
// reach; it is reached through `&dyn Bus`, never through the concrete
// type, satisfying `AGENTS.md` §8.

use config::ConfigSpace;
use enumerate::Pci;

impl<C: ConfigSpace> Bus for Pci<C> {
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        self.enumerate_into(out)
    }
}

// The frozen `abi-v1` virtio-PCI transport-provisioning seam
// (`AGENTS.md` §9). The ring-0 boot walk reaches the concrete `Pci`
// through `&dyn VirtioPciBus`, so the bus driver never leaks its
// concrete type across the crate boundary (`AGENTS.md` §8). Both
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

// The frozen `abi-v1` MSI-X interrupt-routing seam (`AGENTS.md` §9).
// Ring 0 reaches the concrete `Pci` through `&dyn MsixBus` to route a
// device's interrupt, never naming the driver's concrete type
// (`AGENTS.md` §8). The inherent `Pci::route_msix` wins method
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

// The `abi-v1` generic-PCI transport seam (`AGENTS.md` §9): the surface
// a non-virtio, DMA-driving device driver (xHCI) consumes to map one of
// the controller's BARs and enable bus mastering, reached through
// `&dyn PciBus` so the device driver never names this concrete crate
// (`AGENTS.md` §8 / §17.4). Both methods forward to the inherent
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

    fn describe_function(
        &self,
        bdf: u64,
        parent_id: u32,
        node_id: u32,
    ) -> Result<HwNode, DriverError> {
        Pci::describe_function(self, bdf, parent_id, node_id)
    }
}
