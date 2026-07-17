//! Driver-host wiring: reload the VL805 firmware, then publish the
//! controller as a bindable xHCI hardware-tree node.
//!
//! This is the user-space VL805 bus driver's whole job (`plans/PI.md` P10
//! D5c). The device manager autoloads the driver against the VL805 PCI node
//! (`node A`) the PCIe root-complex driver enumerated and published
//! ([`crate::BIND_KEYS`]), and mints it one grant per resource that node
//! requested — the controller's already-assigned register BAR (resolved to
//! its CPU-physical address) and the inbound-DMA constraint, and no more. This driver holds **only** `CAP_MAILBOX` and
//! `CAP_HW_EMIT`: it cannot map the forwarded BAR/DMA itself (it lacks
//! `CAP_MMIO_MAP`/`CAP_MEM_DMA`), which is the point — its job is to reload
//! the device firmware and hand the grants on, not to touch the registers.
//!
//! It does two things, in order:
//!
//! 1. reloads the VL805's firmware over the board-neutral
//!    [`MailboxChannel`](tairix_abi::driver::mailbox::MailboxChannel) the
//!    host exposes ([`DriverHost::mailbox`]) — the link bring-up's `PERST#`
//!    drops the `VideoCore`-loaded firmware on EEPROM-less Pi 4 boards
//!    ([`crate::reload_firmware`]); and
//! 2. publishes the controller as `node B`, an
//!    [`XHCI_COMPATIBLE`]
//!    [`HwNode`] **forwarding** the BAR + DMA grants it received, so
//!    `devmgr` autoloads the xHCI controller's own driver against it
//!    (`drivers/input/usb_kbd`).
//!
//! Firmware-before-bring-up holds **by construction**: node B does not exist
//! until this driver has run the reload, so the controller driver that binds
//! node B can never bring the controller up before its firmware is loaded
//! (`plans/PI.md` P10 D5c). The reload itself is best-effort — the
//! authoritative liveness gate is the controller's capability block at
//! `Xhci::open` in the bound driver — so a refused reload
//! does not block the publish; the node is emitted regardless and the
//! outcome is returned for the caller to log.
//!
//! No QEMU vertical exists — QEMU models no `VideoCore` mailbox or Pi USB
//! timing — so the host tests prove the composition and
//! its fail-closed paths; the live firmware reload is the on-metal
//! acceptance item.

use tairix_abi::hwtree::HW_NODE_ROOT;
use tairix_abi::{
    DriverError, DriverHost, HwDeviceClass, HwMatchKey, HwNode, HwResource, HwResourceKind,
};
use tairix_usb::XHCI_COMPATIBLE;

use crate::{reload_firmware, FirmwareResetOutcome};

/// Build `node B`: the bindable xHCI controller node that **forwards** the
/// register BAR and inbound-DMA grants this driver received on `node A`.
///
/// `resources` is the driver's own granted resource set (its
/// `tairix_drvrt::RtDriverHost::resources`, the grants the kernel minted for
/// the matched VL805 PCI node). The first
/// [`Mmio`](HwResourceKind::Mmio) grant is the controller's already-assigned
/// register BAR at its CPU-physical address, and the
/// [`Dma`](HwResourceKind::Dma) grant the inbound-DMA constraint; both are
/// copied verbatim onto the published node as grant *requests*. The kernel's
/// `hw_emit_node` coverage check then admits node B exactly because every
/// resource it requests is covered by one of this driver's own grants — no
/// ambient authority is created: the controller driver that
/// binds node B receives the same BAR + DMA it would have, only now with the
/// firmware reloaded first.
///
/// The node is built with placeholder identity ([`HW_NODE_ROOT`] parent,
/// id `0`): the kernel assigns a fresh, collision-free id and the emitter's
/// own node as parent on publish (D5b.2a). It is
/// classed [`HwDeviceClass::Bus`] (an xHCI host controller is a bus to
/// further USB devices) and matched by the
/// [`XHCI_COMPATIBLE`] `compatible` string the
/// controller driver binds — the single definition shared with that driver.
///
/// # Errors
///
/// Fail-closed: [`DriverError::NotFound`] if the grant
/// set is missing the register BAR or the DMA constraint (an unbound or
/// mis-provisioned node, never a fabricated resource); [`DriverError::DeviceFault`]
/// if the `compatible` match key cannot be pushed; [`DriverError::NoSpace`]
/// if the node cannot carry both forwarded grants.
pub fn build_xhci_node<'a, I>(resources: I) -> Result<HwNode, DriverError>
where
    I: IntoIterator<Item = &'a HwResource>,
{
    let mut bar: Option<&HwResource> = None;
    let mut dma: Option<&HwResource> = None;
    let mut irq: Option<&HwResource> = None;
    // One pass: a grant iterator is consumed once, and the grant order is not
    // guaranteed, so latch the first of each kind.
    for resource in resources {
        match resource.kind() {
            Some(HwResourceKind::Mmio) if bar.is_none() => bar = Some(resource),
            Some(HwResourceKind::Dma) if dma.is_none() => dma = Some(resource),
            Some(HwResourceKind::Irq) if irq.is_none() => irq = Some(resource),
            _ => {}
        }
    }
    let bar = bar.ok_or(DriverError::NotFound)?;
    let dma = dma.ok_or(DriverError::NotFound)?;

    let key = HwMatchKey::compatible(XHCI_COMPATIBLE).map_err(|_| DriverError::DeviceFault)?;
    let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Bus);
    node.push_match_key(key)
        .map_err(|_| DriverError::DeviceFault)?;
    node.push_resource(*bar).map_err(|_| DriverError::NoSpace)?;
    node.push_resource(*dma).map_err(|_| DriverError::NoSpace)?;
    // Forward the MSI interrupt line the PCIe bus driver allocated and routed
    // for the controller (when present), so the matched xHCI driver receives
    // an `irq_bind`-able grant and services completions on its interrupt
    // rather than busy-polling (`plans/PI.md` U-MSI). The kernel's
    // `hw_emit_node` coverage check admits it exactly because this driver
    // holds the same forwarded IRQ grant on node A. A boot shape with no MSI
    // (no such grant) simply omits it and the matched driver falls back to its
    // poll path.
    if let Some(irq) = irq {
        node.push_resource(*irq).map_err(|_| DriverError::NoSpace)?;
    }
    Ok(node)
}

/// Reload the VL805 firmware over `host`'s mailbox, then publish `node` (the
/// xHCI controller node from [`build_xhci_node`]) into the hardware tree.
///
/// The reload runs **first** so firmware-before-bring-up holds by
/// construction (the bound controller driver cannot exist until node B is
/// published). It is best-effort: a missing mailbox reports
/// [`FirmwareResetOutcome::NotAvailable`] and a refused tag
/// [`FirmwareResetOutcome::Failed`], but neither blocks the publish — the
/// authoritative liveness gate is `Xhci::open` in the bound driver. The outcome is returned so the composing bin can log
/// it.
///
/// `node` is published through [`DriverHost::emit_node`]; the kernel gates
/// that call by `CAP_HW_EMIT` and admits the node only when every resource
/// it requests is covered by one of this driver's own grants.
///
/// # Errors
///
/// [`DriverError`] from [`DriverHost::emit_node`] if the publish is refused
/// (the driver lacks `CAP_HW_EMIT`, or a forwarded resource is not covered
/// by a grant) — fail-closed, nothing is left half-published. A refused
/// firmware reload is **not** an error: it is returned as the
/// [`FirmwareResetOutcome`].
pub fn reload_firmware_and_publish(
    host: &dyn DriverHost,
    node: HwNode,
) -> Result<FirmwareResetOutcome, DriverError> {
    // Reload first (best-effort) so the firmware is loaded before node B —
    // and so the controller driver that binds it — exists (`plans/PI.md`
    // D5c). A boot shape with no VideoCore mailbox reports `NotAvailable`.
    let outcome = match host.mailbox() {
        Some(channel) => reload_firmware(channel),
        None => FirmwareResetOutcome::NotAvailable,
    };
    host.emit_node(node)?;
    Ok(outcome)
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
