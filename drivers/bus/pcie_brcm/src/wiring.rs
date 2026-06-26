//! Driver-host wiring: discovered BCM2711 PCIe controller → trained link.
//!
//! The aarch64 `FdtDiscovery` emits the `brcm,bcm2711-pcie` node into
//! `rustos_abi::hwtree` as a `Bus` node whose controller/ECAM-access
//! `reg` window and inbound-DMA aperture (`dma-ranges`) are
//! device-tree-discovered, never compiled-in (
//! `plans/PI.md` P10). A `devmgr`/host composition maps that window and
//! calls [`open_discovered`] to reset the root complex and train its
//! link; once it returns, the same register window is handed to
//! `rustos_pci::mechanism_brcm` to enumerate the VL805 xHCI
//! controller behind the bridge.
//!
//! This is the only seam that maps memory: [`open_discovered`] checks
//! [`CapabilityId::MMIO_MAP`], maps the controller window through the
//! host's [`MmioMapper`] (never a pointer the driver synthesises), and brings the link up over it. Everything below —
//! the reset/SerDes/window/link state machine — is the host-provable
//! [`BrcmPcieRc`] engine driven over the register seam.
//!
//! QEMU models no Pi PCIe link timing, so the
//! host tests prove the composition and its fail-closed paths up to the
//! root-port / link-up check; the live link training is the on-metal
//! acceptance item.

use rustos_abi::driver::mmio::MmioMapError;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, HwNode, HwResource, HwResourceKind, MmioMapper,
    MsiMessage, PciBus, RegisterWindow,
};
use rustos_pci::{
    assign_and_map_bar, bus_to_cpu_phys, find_function_by_class, mechanism_brcm,
    USB_CONTROLLER_CLASS,
};
use rustos_usb::XHCI_BAR_INDEX;

use crate::{regs, BrcmPcieRc, Delay, PcieWindows};

/// The discovered inputs the PCIe root-complex bring-up needs, all read
/// from the `brcm,bcm2711-pcie` [`HwNode`] — never
/// compiled-in constants.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PcieBringup {
    /// CPU-physical base of the PCIe controller register block (the
    /// translated `reg` MMIO window).
    pub regs_phys: u64,
    /// The inbound (`dma-ranges`) and outbound (`ranges`) address windows
    /// the root complex is programmed with.
    pub windows: PcieWindows,
}

/// Why a `brcm,bcm2711-pcie` [`HwNode`] could not be turned into a
/// [`PcieBringup`]: a required discovered resource is absent. Each is a
/// fail-closed refusal — the bring-up never invents a window.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BringupError {
    /// The node carries no controller register (`Mmio`) window.
    NoControllerWindow,
    /// The node carries no inbound-DMA aperture (`Dma`) resource.
    NoInboundAperture,
    /// The node carries no outbound (`BusWindow`) resource.
    NoOutboundWindow,
}

impl BringupError {
    /// Map an incomplete-node refusal onto the bus-neutral
    /// [`DriverError`] the autonomous entry reports. A required
    /// discovered resource is missing, so the device cannot be reached:
    /// [`DriverError::NotFound`], failing closed.
    #[must_use]
    pub const fn as_driver_error(self) -> DriverError {
        DriverError::NotFound
    }
}

/// Assemble the bring-up inputs from a discovered `brcm,bcm2711-pcie`
/// [`HwNode`].
///
/// The node carries three resources the bring-up needs (all discovered by
/// the architecture port):
///
/// * the controller register window — the first [`Mmio`](HwResourceKind::Mmio)
///   resource, whose base is [`PcieBringup::regs_phys`];
/// * the inbound viewport — the [`Dma`](HwResourceKind::Dma) resource,
///   whose `length` is the viewport size and `translated_base` the
///   PCIe-space base the inbound BAR is programmed at; and
/// * the outbound window — the [`BusWindow`](HwResourceKind::BusWindow)
///   resource (`base` CPU aperture, `length` size, `translated_base` the
///   PCIe-space base it maps to).
///
/// # Errors
///
/// A [`BringupError`] naming the first missing resource; the inputs are
/// never partially assembled.
pub fn pcie_bringup_from_node(node: &HwNode) -> Result<PcieBringup, BringupError> {
    pcie_bringup_from_resources(node.resources())
}

/// Assemble the bring-up inputs from the discovered resources of a
/// `brcm,bcm2711-pcie` node — the same three resources
/// [`pcie_bringup_from_node`] reads, but over any iterator of
/// [`HwResource`]s.
///
/// This is the form the **user-space** BCM2711 PCIe bus driver uses
/// (`plans/PI.md` D5b.2b): an autoloaded driver does not hold the matched
/// [`HwNode`] itself — the kernel mints it one device-resource grant per
/// resource the node requested and it learns them through the
/// `resource_grants` syscall, exposed by its
/// `rustos_drvrt::RtDriverHost::resources`. The node-taking form delegates
/// here, so the two callers parse the same three resources through one
/// definition.
///
/// The first [`Mmio`](HwResourceKind::Mmio), [`Dma`](HwResourceKind::Dma),
/// and [`BusWindow`](HwResourceKind::BusWindow) resources supply the
/// controller register window, the inbound viewport, and the outbound
/// window respectively (see [`pcie_bringup_from_node`] for the field
/// meanings).
///
/// # Errors
///
/// A [`BringupError`] naming the first missing resource; the inputs are
/// never partially assembled.
pub fn pcie_bringup_from_resources<'a, I>(resources: I) -> Result<PcieBringup, BringupError>
where
    I: IntoIterator<Item = &'a HwResource>,
{
    let mut regs: Option<&HwResource> = None;
    let mut inbound: Option<&HwResource> = None;
    let mut outbound: Option<&HwResource> = None;
    // One pass: a grant iterator is consumed once, and a node's resource
    // ordering is not guaranteed, so latch the first of each kind.
    for resource in resources {
        match resource.kind() {
            Some(HwResourceKind::Mmio) if regs.is_none() => regs = Some(resource),
            Some(HwResourceKind::Dma) if inbound.is_none() => inbound = Some(resource),
            Some(HwResourceKind::BusWindow) if outbound.is_none() => outbound = Some(resource),
            _ => {}
        }
    }

    let regs = regs.ok_or(BringupError::NoControllerWindow)?;
    let inbound = inbound.ok_or(BringupError::NoInboundAperture)?;
    let outbound = outbound.ok_or(BringupError::NoOutboundWindow)?;

    Ok(PcieBringup {
        regs_phys: regs.base(),
        windows: PcieWindows {
            inbound_pcie_base: inbound.translated_base(),
            inbound_size: inbound.length(),
            inbound_cpu_top: inbound.base(),
            outbound_cpu_base: outbound.base(),
            outbound_pcie_base: outbound.translated_base(),
            outbound_size: outbound.length(),
        },
    })
}

/// Autonomous bootstrap-floor entry: bring the discovered BCM2711 PCIe
/// root complex up straight from its hardware-tree [`HwNode`].
///
/// This is the floor-driver autonomous bring-up the kernel's
/// bootstrap-floor catalogue drives directly, talking to the kernel
/// solely through the [`DriverHost`] contract (no `kernel/*` dependency). It reads the controller window + address windows
/// off the discovered node ([`pcie_bringup_from_node`]) — never a
/// compiled-in board constant — then maps the
/// window and trains the link over it ([`open_discovered`]). On success
/// the returned [`BrcmPcieRc`] owns the mapped window with the link up,
/// ready for `rustos_pci::mechanism_brcm` to enumerate behind the
/// bridge.
///
/// # Errors
///
/// * [`DriverError::NotFound`] if the discovered node is missing a
///   required resource (via [`BringupError::as_driver_error`]).
/// * every error of [`open_discovered`] (no [`CapabilityId::MMIO_MAP`],
///   no [`MmioMapper`], a window that cannot be mapped, or a link that
///   never trains).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
/// [`CapabilityId::DRV_LOAD`] [`crate::register`] checked.
pub fn bring_up_from_node(
    host: &dyn DriverHost,
    node: &HwNode,
    delay: &dyn Delay,
) -> Result<BrcmPcieRc<RegisterWindow>, DriverError> {
    let bringup = pcie_bringup_from_node(node).map_err(BringupError::as_driver_error)?;
    open_discovered(host, bringup.regs_phys, &bringup.windows, delay)
}

/// Map the discovered PCIe controller window and train the root-complex
/// link.
///
/// `regs_phys` is the CPU-physical base of the PCIe controller register
/// block as reported by the hardware-tree `brcm,bcm2711-pcie` node;
/// `windows` carries the discovered inbound (`dma-ranges`) and outbound
/// (`ranges`) address windows the root complex is programmed with;
/// `delay` supplies the link bring-up's microsecond waits. The window is
/// mapped read/write under [`CapabilityId::MMIO_MAP`] and handed to
/// [`BrcmPcieRc::open`].
///
/// On success the returned [`BrcmPcieRc`] owns the mapped window with the
/// link up; the caller recovers the window with
/// [`BrcmPcieRc::into_regs`] and builds the windowed configuration
/// accessor (`rustos_pci::mechanism_brcm`) over it.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * [`DriverError::Unsupported`] if the host exposes no [`MmioMapper`].
/// * [`DriverError::LengthOutOfRange`] / [`DriverError::DeviceFault`] if
///   the platform cannot map the window, plus any [`BrcmPcieRc::open`]
///   error (the controller is not a root port, or the link never
///   trains).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
/// [`CapabilityId::DRV_LOAD`] [`crate::register`] checked.
pub fn open_discovered(
    host: &dyn DriverHost,
    regs_phys: u64,
    windows: &PcieWindows,
    delay: &dyn Delay,
) -> Result<BrcmPcieRc<RegisterWindow>, DriverError> {
    if !host.has_capability(CapabilityId::MMIO_MAP) {
        return Err(DriverError::PermissionDenied);
    }
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;
    let window = mapper
        .map_window(regs_phys, regs::REGS_LEN_BYTES)
        .map_err(MmioMapError::as_driver_error)?;
    BrcmPcieRc::open(window, delay, windows)
}

/// Train the BCM2711 root-complex link, enumerate the USB host controller
/// behind the bridge, assign and enable its register BAR, and publish it
/// into the live hardware tree as a bindable child [`HwNode`].
///
/// This is the user-space BCM2711 PCIe bus driver's whole job
/// (`plans/PI.md` D5b.2b): the device manager autoloads the driver against
/// the discovered `brcm,bcm2711-pcie` node, mints it one grant per resource
/// that node requested (the controller register window, the inbound-DMA
/// aperture, and the outbound bus window), and this
/// composition turns those grants — supplied as `bringup`
/// ([`pcie_bringup_from_resources`]) — into a published USB-host node the
/// manager then autoloads the next driver against. It talks to the kernel
/// solely through the [`DriverHost`] contract; the
/// concrete driver bin (`drivers/bus/pcie_brcm`) supplies an
/// `rustos_drvrt::RtDriverHost` and an `rustos_rt::ClockDelay`.
///
/// On success the returned [`HwNode`] is the node already published through
/// [`DriverHost::emit_node`] — its kernel-assigned id/parent are still the
/// placeholders the emitter built it with (the kernel owns identity on
/// publish; D5b.2a), so the value is returned only
/// for the caller to log/inspect, never to re-address the tree.
///
/// # Errors
///
/// Every failure is fail-closed; nothing is left
/// half-published:
///
/// * every error of [`open_discovered`] (no [`CapabilityId::MMIO_MAP`], no
///   [`MmioMapper`], an unmappable window, or a link that never trains —
///   the live link training is the on-metal acceptance item); and
/// * every error of [`publish_usb_function`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (the controller register window and
/// the BAR probe) and `CAP_HW_EMIT` (the node publish, enforced kernel-side
/// by [`DriverHost::emit_node`]).
pub fn emit_vl805_node(
    host: &dyn DriverHost,
    bringup: &PcieBringup,
    delay: &dyn Delay,
) -> Result<HwNode, DriverError> {
    let rc = open_discovered(host, bringup.regs_phys, &bringup.windows, delay)?;
    // The windowed configuration accessor over the trained register window:
    // it forwards config only to the single device on the secondary bus, so a
    // scan never TLPs an absent target (which would CPU-abort on the SoC bus).
    let bus = mechanism_brcm(rc.into_regs(), regs::RC_SECONDARY_BUS);
    publish_usb_function(host, &bus, &bringup.windows)
}

/// Enumerate the USB host controller on the trained `bus`, assign/enable/map
/// its register BAR, and publish it as a bindable child [`HwNode`] carrying
/// exactly the two device-resource grant *requests* the matched downstream
/// xHCI driver needs and no more.
///
/// Split out from [`emit_vl805_node`] so the post-link logic — the part QEMU
/// can model (the link training itself is metal-only) — is
/// host-tested against a mock [`PciBus`]. The two emitted grants are:
///
/// * an [`HwResource::mmio`] of the controller's BAR resolved to its
///   **CPU-physical** address ([`bus_to_cpu_phys`] over the discovered
///   outbound window) — so it lies inside the bridge's outbound `BusWindow`
///   grant and the kernel's grant-coverage check admits it
///   (`HwResource::covers`, the BusWindow→Mmio case), and the matched driver
///   maps the live BAR rather than re-training the bus; and
/// * the bridge's inbound DMA aperture **forwarded verbatim** as an
///   [`HwResource::dma_translated`] — the same CPU-physical reachability
///   ceiling (`PcieWindows::inbound_cpu_top`), extent
///   (`PcieWindows::inbound_size`) and far-side translation
///   (`PcieWindows::inbound_pcie_base`) the bridge driver itself holds, so
///   the kernel's DMA-grant coverage check admits it exactly
///   (`HwResource::covers`, the `Dma`→`Dma` case requires the identical
///   translation) and the matched driver's `dma_alloc` resolves a
///   device-visible bus address through the same viewport. On the Pi 4 this aperture is `IB MEM 0x0..0x1ffffffff ->
///   0x4_0000_0000`, so a non-zero far-side base is the common case, not a
///   special one; the buffer size the matched driver carves is its own
///   concern, bounded by the aperture, never re-encoded here.
///
/// The BAR is mapped only transiently here, to learn its assigned base and
/// size; the window is dropped immediately (the matched user-space driver
/// re-maps the live BAR from its grant).
///
/// # Errors
///
/// Fail-closed: [`DriverError::NotFound`] if the bus
/// carries no USB-class function; [`DriverError::Unsupported`] if the host
/// exposes no [`MmioMapper`]; [`DriverError::OutOfRange`] if the BAR lies
/// outside the outbound window;
/// [`DriverError::NoSpace`] if the node cannot carry its grant requests;
/// plus any error of [`assign_and_map_bar`], [`PciBus::describe_function`],
/// or [`DriverHost::emit_node`].
pub fn publish_usb_function(
    host: &dyn DriverHost,
    bus: &dyn PciBus,
    windows: &PcieWindows,
) -> Result<HwNode, DriverError> {
    let bdf = find_function_by_class(bus, USB_CONTROLLER_CLASS)?;
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;

    // Assign (when firmware left it unassigned), enable bus-mastering on, and
    // map the controller's BAR — the shared `lib/pci` primitive both this
    // driver and the xHCI driver use. The map is a
    // transient probe: `phys_base` is the BAR's assigned base in the address
    // space the mapper maps (PCIe-bus space, since the bridge mapper
    // translates) and `len` its probed size.
    let outbound_window = (windows.outbound_pcie_base, windows.outbound_size);
    let window = assign_and_map_bar(bus, bdf, XHCI_BAR_INDEX, outbound_window, mapper)?;
    let bar_pcie_base = window.phys_base();
    let bar_len = window.len() as u64;
    // The map was a transient probe to learn the BAR's assigned base and
    // size; the window is not retained (it falls out of scope here). The
    // matched user-space driver re-maps the live BAR from the grant below.

    // Resolve the BAR to its CPU-physical address through the discovered
    // outbound window, so the published `Mmio` grant lies inside the bridge's
    // outbound `BusWindow` grant the kernel coverage check tests against
    // (`HwResource::covers`).
    let bar_cpu_phys = bus_to_cpu_phys(
        (
            windows.outbound_cpu_base,
            windows.outbound_pcie_base,
            windows.outbound_size,
        ),
        bar_pcie_base,
    )
    .ok_or(DriverError::OutOfRange)?;

    // The node carries the function's `vendor:device:class` match key
    // (`describe_function`) — its identity is kernel-assigned on publish
    // (D5b.2a) — plus the two grant requests.
    let mut node = bus.describe_function(bdf)?;
    node.push_resource(HwResource::mmio(bar_cpu_phys, bar_len))
        .map_err(|_| DriverError::NoSpace)?;
    node.push_resource(HwResource::dma_translated(
        windows.inbound_cpu_top,
        windows.inbound_size,
        windows.inbound_pcie_base,
    ))
    .map_err(|_| DriverError::NoSpace)?;

    // Wire the controller for message-signalled interrupts so the matched
    // xHCI driver parks on its completion interrupt rather than busy-polling
    // (`plans/PI.md` U-MSI). Allocate a vector through the host (the kernel
    // mints it, brings the platform MSI controller up, and grants this driver
    // a device resource for the resulting virtual line), program the VL805's
    // MSI capability with the returned doorbell, and forward the line as the
    // node's IRQ grant request — covered by the grant `alloc_msi` just minted,
    // so `hw_emit_node` admits it (no ambient authority). Best-effort: a
    // platform with no MSI controller (`alloc_msi` → `NotImplemented`) or a
    // function with no MSI capability (`route_msi` → `NotFound`) simply
    // publishes the node without an IRQ resource; the matched driver then
    // waits only for URB submissions and cannot complete interrupt-driven
    // transfers until hardware supplies an IRQ-capable path.
    if let Ok(allocation) = host.alloc_msi() {
        let message = MsiMessage {
            address: allocation.address,
            data: allocation.data,
        };
        if bus.route_msi(bdf, message).is_ok() {
            node.push_resource(HwResource::irq(u64::from(allocation.line), 1))
                .map_err(|_| DriverError::NoSpace)?;
        }
    }

    host.emit_node(node)?;
    Ok(node)
}
