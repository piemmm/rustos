//! Driver-host wiring: discovered mailbox → [`HvsConfig`] → [`RpiHvs`].
//!
//! This is the `plans/PI.md` P7 metal-wiring seam. The driver host
//! discovers the `VideoCore` firmware mailbox through the hardware tree
//! (the aarch64 `FdtDiscovery` emits a `brcm,bcm2835-mbox` node whose
//! resources are the doorbell MMIO window and a DMA property-buffer
//! carve request bounded by the 30-bit `VideoCore` aperture,
//! `AGENTS.md` §18.1), carves the buffer, and hands both to
//! [`open_discovered`]: it maps them under
//! [`CapabilityId::MMIO_MAP`], rings the firmware for the scan-out
//! surface over [`MmioMailbox`], assembles the full [`HvsConfig`]
//! (firmware scan-out + the host's HVS display-list RAM, control
//! window, and plane carves), and brings the engine online through
//! [`RpiHvs::open`].
//!
//! The mailbox exchange itself sits behind the
//! [`MailboxTransport`] seam, so [`open_with_transport`] is the
//! host-provable half: the crate's tests drive it with the shared
//! protocol-faithful mock firmware (QEMU does not model the
//! `VideoCore`, `AGENTS.md` §2.1), and the doorbell below the seam is
//! the on-metal acceptance item.

use rustos_abi::{CapabilityId, DriverError, DriverHost, MmioMapper};
use rustos_vcmailbox::{
    arm_physical_to_bus, discover_framebuffer, FramebufferRequest, MailboxError, MailboxTransport,
    MmioMailbox, MAILBOX_REGS_LEN_BYTES, PROPERTY_LEN_BYTES,
};

use crate::dlist::PlaneConfig;
use crate::{map, HvsConfig, RpiHvs, ScanoutConfig, MAX_PLANES};

/// The discovered mailbox doorbell plus the host's property-buffer
/// carve, both expressed as ARM-physical addresses the host maps under
/// [`CapabilityId::MMIO_MAP`] (`AGENTS.md` §4 — never a compiled-in
/// base).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MailboxWiring {
    /// ARM-physical base of the doorbell register block — the hardware
    /// tree mailbox node's MMIO resource (device-tree-discovered).
    pub regs_phys: u64,
    /// ARM-physical base of the DMA-visible property buffer the host
    /// carved to satisfy the mailbox node's DMA resource request; must
    /// sit inside the 30-bit `VideoCore` aperture and be 16-byte
    /// aligned.
    pub buffer_phys: u64,
    /// `VideoCore` bus alias the firmware addresses SDRAM through
    /// (e.g. [`crate::DEFAULT_BUS_ALIAS`]).
    pub bus_alias: u32,
    /// Doorbell poll budget
    /// ([`rustos_vcmailbox::DEFAULT_POLL_BUDGET`] unless tuned).
    pub poll_budget: u32,
}

/// The host-side HVS regions completing the [`HvsConfig`]: the
/// display-list RAM and display-channel control window (discovered HVS
/// MMIO), plus the per-plane source-buffer carves.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HvsRegions {
    /// Physical base of the HVS display-list RAM.
    pub dlist_phys_base: u64,
    /// Length of the display-list RAM in bytes (a multiple of four).
    pub dlist_len_bytes: usize,
    /// Physical base of the display-channel control register window.
    pub control_phys_base: u64,
    /// Per-plane source buffers; only the first `plane_count` are used.
    pub planes: [PlaneConfig; MAX_PLANES],
    /// Number of active planes (`1..=MAX_PLANES`).
    pub plane_count: usize,
}

/// Bring the HVS online from the discovered mailbox: map the doorbell
/// and property buffer, exchange the framebuffer request with the
/// firmware over [`MmioMailbox`], and delegate to
/// [`open_with_transport`].
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * [`DriverError::Unsupported`] if the host exposes no
///   [`MmioMapper`], or the platform cannot map a region.
/// * [`DriverError::LengthOutOfRange`] if the property buffer falls
///   outside the `VideoCore` aperture or is misaligned.
/// * [`DriverError::DeviceFault`] if the firmware never answers within
///   the poll budget, plus any error of [`open_with_transport`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
/// [`CapabilityId::DRV_LOAD`] [`crate::register`] checked.
pub fn open_discovered(
    host: &dyn DriverHost,
    mailbox: &MailboxWiring,
    request: &FramebufferRequest,
    regions: &HvsRegions,
) -> Result<RpiHvs, DriverError> {
    if !host.has_capability(CapabilityId::MMIO_MAP) {
        return Err(DriverError::PermissionDenied);
    }
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;
    let regs = map(mapper, mailbox.regs_phys, MAILBOX_REGS_LEN_BYTES)?;
    let buffer = map(mapper, mailbox.buffer_phys, PROPERTY_LEN_BYTES)?;
    let buffer_bus = arm_physical_to_bus(mailbox.buffer_phys, mailbox.bus_alias)
        .map_err(MailboxError::as_driver_error)?;
    let mut transport = MmioMailbox::new(regs, buffer, buffer_bus, mailbox.poll_budget)
        .map_err(MailboxError::as_driver_error)?;
    open_with_transport(host, &mut transport, request, regions)
}

/// Discover the firmware framebuffer over `transport`, assemble the
/// full [`HvsConfig`] from it and `regions`, and open the driver.
///
/// The seam below [`open_discovered`]: host tests drive it with a mock
/// firmware, metal drives it with the [`MmioMailbox`] doorbell. The
/// scan-out geometry and the `VideoCore` bus alias both come from the
/// firmware's (validated) answer, never from assumptions.
///
/// # Errors
///
/// * The mapped [`MailboxError`] of a failed or malformed exchange.
/// * Any [`RpiHvs::open`] error for a config the firmware answer
///   cannot satisfy.
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (checked by [`RpiHvs::open`]).
pub fn open_with_transport(
    host: &dyn DriverHost,
    transport: &mut dyn MailboxTransport,
    request: &FramebufferRequest,
    regions: &HvsRegions,
) -> Result<RpiHvs, DriverError> {
    let firmware =
        discover_framebuffer(transport, request).map_err(MailboxError::as_driver_error)?;
    let scanout = ScanoutConfig::from_firmware(&firmware).map_err(MailboxError::as_driver_error)?;
    let config = HvsConfig {
        scanout,
        dlist_phys_base: regions.dlist_phys_base,
        dlist_len_bytes: regions.dlist_len_bytes,
        control_phys_base: regions.control_phys_base,
        planes: regions.planes,
        plane_count: regions.plane_count,
        bus_alias: firmware.bus_alias(),
    };
    RpiHvs::open(host, config)
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
