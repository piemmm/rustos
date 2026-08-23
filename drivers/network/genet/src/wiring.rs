//! Driver-host wiring: discovered GENET node → a live [`Genet`].
//!
//! The aarch64 `FdtDiscovery` emits the `brcm,bcm2711-genet-v5` node as a
//! Network-class device carrying four grant requests: its register window
//! (base and length read from the device tree's `reg`, translated through the
//! ancestor buses' `ranges` — never a compiled-in constant), the two
//! interrupt lines it raises, and the DMA addressing constraint its bus
//! declares. The node also carries the firmware-published link-layer address
//! (a [`LinkAddress`](tairix_abi::HwResourceKind::LinkAddress) resource),
//! which the Pi's GENET does not hold in its own registers.
//!
//! [`open_discovered`] is the only seam that maps memory or carves DMA: it
//! checks its capabilities, resolves the window from the grant set, maps it
//! through the host's [`MmioMapper`](tairix_abi::MmioMapper), carves the one
//! frame-buffer region through the host's
//! [`DmaHost`](tairix_abi::driver::dma::DmaHost), and hands both to
//! [`Genet::open`].
//! Everything below — the bring-up sequence, the MDIO/PHY link, and the
//! frame path — is the host-provable engine driven over the register seam.

use tairix_abi::driver::mmio::MmioMapError;
use tairix_abi::driver::net::{MacAddress, MAC_ADDRESS_LEN};
use tairix_abi::driver::timing::Delay;
use tairix_abi::{CapabilityId, DriverError, DriverHost, HwResource, RegisterWindow};

use crate::{Genet, DMA_REGION_BYTES};

/// Map the discovered GENET register window, carve its frame buffers, and
/// bring the controller online.
///
/// `resources` is the driver's kernel-issued grant set and `link_address`
/// the firmware-published MAC from its matched node.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host granted neither
///   [`CapabilityId::MMIO_MAP`] nor [`CapabilityId::MEM_DMA`].
/// * [`DriverError::Unsupported`] if the host exposes no MMIO mapper or DMA
///   facility, or the controller is not a GENET v5.
/// * [`DriverError::NotFound`] if the grant set names no register window, or
///   the node published no link-layer address — a NIC brought up on an
///   invented address would answer to the wrong DHCP reservation and form
///   the wrong IPv6 link-local, so bring-up refuses instead.
/// * Any [`Genet::open`] error (an unmappable window, a short DMA carve, or
///   a PHY that never answers on MDIO).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] and [`CapabilityId::MEM_DMA`] in
/// addition to the load-time [`CapabilityId::DRV_LOAD`]
/// [`register`](crate::register) checked.
pub fn open_discovered<'a, I, D>(
    host: &dyn DriverHost,
    resources: I,
    link_address: Option<[u8; MAC_ADDRESS_LEN]>,
    delay: D,
) -> Result<Genet<RegisterWindow, D>, DriverError>
where
    I: IntoIterator<Item = &'a HwResource>,
    D: Delay,
{
    if !host.has_capability(CapabilityId::MMIO_MAP) || !host.has_capability(CapabilityId::MEM_DMA) {
        return Err(DriverError::PermissionDenied);
    }
    let mac = MacAddress::new(link_address.ok_or(DriverError::NotFound)?);
    let (base, len) = tairix_abi::driver::sole_register_window(resources)?;
    let window = host
        .mmio_mapper()
        .ok_or(DriverError::Unsupported)?
        .map_window(base, len)
        .map_err(MmioMapError::as_driver_error)?;
    let frames = host
        .dma_host()
        .ok_or(DriverError::Unsupported)?
        .alloc_dma_zeroed(DMA_REGION_BYTES)?;
    Genet::open(window, delay, frames, mac)
}
