//! Arch-neutral xHCI controller bring-up orchestration for the HCD process
//! (`plans/USB.md` U3b).
//!
//! This is the composition the **host-controller driver** runs at start-up: it
//! brings the controller up over the device-resource grants the kernel minted
//! for it (its already-assigned register BAR and a DMA constraint) and returns
//! the [`UsbDevice`] enumeration engine. The engine is pointed at the attached
//! device's slot when one is present, or left serving with its first-connect
//! watch armed when none is (a keyboard absent at boot is a first-class state,
//! not a failure).
//! The HCD then serves that device's transfers over the URB transport seam
//! (`rustos_usb::transport`) to the autoloaded class driver — it does **not**
//! decode HID reports itself; that is the class driver's job.
//!
//! # What lives here, and what does not
//!
//! The HCD is **arch-neutral**: the board PCIe root-complex bring-up and BAR
//! assignment stay in the separate board bus drivers (`drivers/bus/pcie_brcm` +
//! `drivers/bus/usb/vl805`), which assign the controller's BAR and emit the
//! enumerated controller as the `usb,xhci` node carrying the BAR + DMA + IRQ
//! grants. This HCD is autoloaded against that node, granted *only* the
//! resources it requested, and reaches them through the [`DriverHost`] its
//! runtime builds over those grants. So this orchestration knows nothing of
//! PCI, the BCM2711, or any board: it maps a register window by address,
//! carves a DMA region, and speaks the bus-agnostic xHCI protocol in
//! [`rustos_usb`].
//!
//! # Testing boundary
//!
//! QEMU models no Pi USB timing, so the host tests prove the composition and
//! its fail-closed paths up to the controller hand-off; over an inert mock
//! register window [`Xhci::open`] fails closed with
//! [`DriverError::DeviceFault`], which is exactly the on-metal boundary. The
//! live controller bring-up is the on-metal acceptance item.

use rustos_abi::driver::dma::DmaSlab;
use rustos_abi::hwtree::{HwResource, HwResourceKind};
use rustos_abi::{CapabilityId, Delay, DriverError, DriverHost, MmioMapper, RegisterWindow};
use rustos_usb::device::{EnumStage, UsbDevice};
use rustos_usb::{Xhci, XhciOpenStage, DEFAULT_POLL_BUDGET, XHCI_DMA_BYTES};

/// The brought-up controller engine the HCD serves: a [`UsbDevice`] over the
/// mapped register BAR and the carved DMA region. It is enumerated and pointed
/// at the attached device's slot when a device is present, or left serving
/// with its first-connect watch armed when none is yet attached.
pub type ControllerDevice = UsbDevice<RegisterWindow, DmaSlab>;

/// The concrete bring-up inputs the HCD derives from its kernel-issued
/// device-resource grants to drive [`bring_up_controller`].
///
/// A `devmgr`-autoloaded HCD is granted exactly the resources its matched
/// `usb,xhci` node requested — its already-assigned register BAR and a DMA
/// constraint — and no more. It does not know those addresses at build time
/// (they depend on the board's bus layout); it reads them from the grants the
/// kernel delivered through `resource_grants` and turns them into these values
/// with [`derive_controller_resources`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ControllerResources {
    /// Device-visible base address of the controller's assigned register BAR
    /// window — the address the HCD names the BAR by when mapping it through
    /// its [`DriverHost`] (the host resolves the covering grant and performs
    /// the bus→CPU translation).
    pub bar_base: u64,
    /// Length of that register window, in bytes.
    pub bar_len: usize,
    /// Exclusive upper bound, in the **device-visible** address space, of the
    /// inbound DMA aperture the controller may reach — the bound
    /// [`bring_up_controller`] checks the carved DMA region against.
    pub dma_aperture_top: u64,
}

/// Derive the [`ControllerResources`] from the [`HwResource`] grants the
/// kernel minted for this HCD process.
///
/// Exactly one mappable register window — an [`HwResourceKind::Mmio`] window
/// (CPU/identity space) or an [`HwResourceKind::BusWindow`] (outbound
/// PCIe-bus space, addressed by its far-side `translated_base`) — supplies the
/// BAR `bar_base`/`bar_len`. Exactly one [`HwResourceKind::Dma`] constraint
/// supplies the aperture bound: its device-visible exclusive top is the
/// far-side base plus extent for a translated inbound viewport, or its
/// `addr_limit` for an untranslated constraint. Any other resource (an IRQ
/// line, the URB endpoint/shared-region grants the HCD itself mints later) is
/// ignored here — this derive maps only the BAR and carves only the DMA
/// region.
///
/// # Errors
///
/// Fails closed, never guessing a missing address:
///
/// * [`DriverError::NotFound`] if no register-window grant or no DMA grant is
///   present.
/// * [`DriverError::Unsupported`] if more than one register-window grant or
///   more than one DMA grant is present (an ambiguous delivery — a packaging
///   defect the HCD refuses rather than picking one).
/// * [`DriverError::OutOfRange`] for a zero-length BAR, a BAR length past
///   `usize`, or a translated DMA aperture whose far-side top overflows.
pub fn derive_controller_resources<'a, I>(resources: I) -> Result<ControllerResources, DriverError>
where
    I: IntoIterator<Item = &'a HwResource>,
{
    let mut bar: Option<(u64, u64)> = None;
    let mut aperture: Option<u64> = None;
    for resource in resources {
        // The register-window base (CPU `base` for an `Mmio` window, far-side
        // `translated_base` for a `BusWindow`) is the one definition in
        // `HwResource::register_window_base`, so this driver does not
        // re-decide it.
        if let Some(base) = resource.register_window_base() {
            if bar.is_some() {
                return Err(DriverError::Unsupported);
            }
            bar = Some((base, resource.length()));
            continue;
        }
        if resource.kind() == Some(HwResourceKind::Dma) {
            if aperture.is_some() {
                return Err(DriverError::Unsupported);
            }
            // A translated inbound viewport's device-visible window is
            // `[translated_base, translated_base + len)`, so its exclusive top
            // is the far-side base plus extent; an untranslated constraint's
            // `addr_limit` (stored as `base`) is already the device-visible
            // exclusive top.
            let top = if resource.translated_base() != 0 {
                resource
                    .translated_base()
                    .checked_add(resource.length())
                    .ok_or(DriverError::OutOfRange)?
            } else {
                resource.base()
            };
            aperture = Some(top);
        }
        // An IRQ line, an endpoint/shared grant, or an unknown kind is not part
        // of this derive (validate the kind).
    }
    let (bar_base, bar_len) = bar.ok_or(DriverError::NotFound)?;
    let bar_len = usize::try_from(bar_len).map_err(|_| DriverError::OutOfRange)?;
    if bar_len == 0 {
        return Err(DriverError::OutOfRange);
    }
    let dma_aperture_top = aperture.ok_or(DriverError::NotFound)?;
    Ok(ControllerResources {
        bar_base,
        bar_len,
        dma_aperture_top,
    })
}

/// Bring up the granted xHCI controller and enumerate the attached device,
/// returning the [`ControllerDevice`] engine pointed at its slot.
///
/// `host` is the HCD process's [`DriverHost`] over its kernel-issued
/// device-resource grants. `bar_base`/`bar_len` name the controller's
/// already-assigned register BAR window; `dma_aperture_top` is the exclusive
/// upper bound, in the device-visible address space, of the inbound window the
/// bridge lets the controller reach. The carved region's device-visible end
/// must lie wholly below it or the controller could not reach its own rings.
/// `delay` supplies the hardware-dictated hub settle windows; the caller owns
/// the clock.
///
/// The DMA region is carved and aperture-checked before any register is
/// touched, so a region the controller could not reach is refused fail-closed
/// with nothing half-configured.
///
/// # Errors
///
/// As [`bring_up_controller_diagnostic`], but with the coarse
/// [`DriverError`] unwrapped from the diagnostic breadcrumb.
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (to map the register BAR); the DMA
/// carve is gated on the host's own DMA capability (`CAP_MEM_DMA`) at
/// allocation time. Both are re-checked kernel-side at each map/allocation.
pub fn bring_up_controller(
    host: &dyn DriverHost,
    delay: &dyn Delay,
    bar_base: u64,
    bar_len: usize,
    dma_aperture_top: u64,
) -> Result<ControllerDevice, DriverError> {
    bring_up_controller_diagnostic(host, delay, bar_base, bar_len, dma_aperture_top)
        .map_err(|err| err.error)
}

/// The phase of [`bring_up_controller_diagnostic`] that failed, so the HCD's
/// one-shot diagnostic can name *where* a coarse [`DriverError`] came from
/// when the controller does not come up. QEMU models no Pi USB, so the live
/// bring-up is metal-only: this is the breadcrumb the on-metal capture
/// reports.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BringupPhase {
    /// Acquiring the host's capability / mapper / DMA seams (a missing
    /// `CAP_MMIO_MAP`, `MmioMapper`, or DMA host).
    Setup,
    /// Carving the device-shared DMA region (`dma_alloc`).
    DmaCarve,
    /// The carved region's device-visible end exceeds the inbound DMA aperture
    /// the controller can reach.
    DmaAperture,
    /// Mapping the controller's register BAR (`mmio_map`).
    BarMap,
    /// Bringing the controller to the halted/reset state ([`Xhci::open`]).
    ControllerOpen,
    /// Programming the DMA structures and starting the controller
    /// ([`UsbDevice::start`]).
    ControllerStart,
    /// Bringing the controller up to serve the keyboard
    /// ([`UsbDevice::bring_up`]); a device absent at boot is not a
    /// failure of this phase, only a real enumeration fault is.
    Enumerate,
}

impl BringupPhase {
    /// Stable diagnostic name for the failing phase.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::DmaCarve => "dma_carve",
            Self::DmaAperture => "dma_aperture",
            Self::BarMap => "bar_map",
            Self::ControllerOpen => "controller_open",
            Self::ControllerStart => "controller_start",
            Self::Enumerate => "enumerate",
        }
    }
}

/// A structured controller bring-up failure: the coarse [`DriverError`] plus
/// the breadcrumb a one-shot on-metal diagnostic needs to pin which controller
/// step stalled. The engine itself holds no logging dependency; the HCD wraps
/// the engine with its own diagnostics and emits this through `lib/log`.
///
/// Which fields are populated depends on [`Self::phase`]:
///
/// * [`BringupPhase::ControllerOpen`] sets [`Self::open_stage`] and the
///   [`Self::usbcmd`]/[`Self::usbsts`] snapshot the reset stage observed.
/// * [`BringupPhase::Enumerate`] sets [`Self::enum_stage`],
///   [`Self::last_completion`], [`Self::last_event_type`],
///   [`Self::last_reject`] and the [`Self::port1_portsc`] snapshot.
/// * The earlier phases carry only [`Self::phase`] and [`Self::error`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ControllerBringupError {
    /// The phase that failed.
    pub phase: BringupPhase,
    /// The coarse driver error the phase returned.
    pub error: DriverError,
    /// The [`Xhci::open`] reset stage, set only for
    /// [`BringupPhase::ControllerOpen`].
    pub open_stage: Option<XhciOpenStage>,
    /// `USBCMD` observed at the failing stage, if readable.
    pub usbcmd: Option<u32>,
    /// `USBSTS` observed at the failing stage, if readable.
    pub usbsts: Option<u32>,
    /// The enumeration stage last entered, set only for
    /// [`BringupPhase::Enumerate`].
    pub enum_stage: Option<EnumStage>,
    /// Raw xHCI completion code of the last event the failing transfer saw
    /// (`0` = none/timeout), for [`BringupPhase::Enumerate`].
    pub last_completion: u8,
    /// Raw TRB-type of the last event the failing transfer saw, for
    /// [`BringupPhase::Enumerate`].
    pub last_event_type: u8,
    /// Why the last event wait was rejected (see
    /// [`UsbDevice::last_reject_reason`]), for [`BringupPhase::Enumerate`].
    pub last_reject: u8,
    /// Raw `PORTSC` of root-hub port 1, if readable, for
    /// [`BringupPhase::Enumerate`].
    pub port1_portsc: Option<u32>,
}

impl ControllerBringupError {
    /// A bare fault at `phase` with no captured controller state — the early
    /// phases (setup, DMA carve/aperture, BAR map) and a controller-start
    /// stall, which carry only the coarse error.
    const fn bare(phase: BringupPhase, error: DriverError) -> Self {
        Self {
            phase,
            error,
            open_stage: None,
            usbcmd: None,
            usbsts: None,
            enum_stage: None,
            last_completion: 0,
            last_event_type: 0,
            last_reject: 0,
            port1_portsc: None,
        }
    }
}

/// [`bring_up_controller`] with a structured [`ControllerBringupError`] on
/// failure, naming the phase that stalled and the controller state observed
/// there.
///
/// The HCD calls this (rather than the plain [`bring_up_controller`]) so it can
/// emit a one-shot diagnostic through `lib/log` when the controller does not
/// come up. On a non-I/O-coherent platform a stall at
/// [`BringupPhase::Enumerate`] with `enum_stage = EnableSlot` and
/// `last_completion = 0` is the classic DMA-not-visible signature; a
/// [`BringupPhase::ControllerOpen`] stall names the reset sub-stage and its
/// `USBCMD`/`USBSTS`.
///
/// # Errors
///
/// As [`bring_up_controller`], but wrapped in [`ControllerBringupError`].
///
/// # Capabilities
///
/// As [`bring_up_controller`].
pub fn bring_up_controller_diagnostic(
    host: &dyn DriverHost,
    delay: &dyn Delay,
    bar_base: u64,
    bar_len: usize,
    dma_aperture_top: u64,
) -> Result<ControllerDevice, ControllerBringupError> {
    // Capability before state; the kernel re-checks at the map/carve traps
    // regardless.
    if !host.has_capability(CapabilityId::MMIO_MAP) {
        return Err(ControllerBringupError::bare(
            BringupPhase::Setup,
            DriverError::PermissionDenied,
        ));
    }
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(ControllerBringupError::bare(
        BringupPhase::Setup,
        DriverError::Unsupported,
    ))?;
    // The controller allocates its xHCI DMA through the bus-neutral DMA seam,
    // not the virtio host.
    let dma_host = host.dma_host().ok_or(ControllerBringupError::bare(
        BringupPhase::Setup,
        DriverError::Unsupported,
    ))?;

    // Carve the device-shared DMA region and verify it lies wholly below the
    // discovered inbound-DMA aperture before any register is touched: a region
    // the controller cannot reach is a fail-closed refusal, never a silent
    // truncation. The kernel maps it coherent (Normal Non-Cacheable on a
    // non-I/O-coherent platform), so the controller sees the rings the driver
    // writes with no cache maintenance.
    let dma = dma_host
        .alloc_dma_zeroed(XHCI_DMA_BYTES)
        .map_err(|e| ControllerBringupError::bare(BringupPhase::DmaCarve, e))?;
    let end = dma
        .phys()
        .checked_add(dma.len() as u64)
        .ok_or(ControllerBringupError::bare(
            BringupPhase::DmaAperture,
            DriverError::OutOfRange,
        ))?;
    if end > dma_aperture_top {
        return Err(ControllerBringupError::bare(
            BringupPhase::DmaAperture,
            DriverError::OutOfRange,
        ));
    }

    // Map the controller's already-assigned register BAR. The host resolves
    // the grant covering `[bar_base, bar_base + bar_len)` and maps that window
    // once; a window no grant covers is refused.
    let window = mapper
        .map_window(bar_base, bar_len)
        .map_err(|e| ControllerBringupError::bare(BringupPhase::BarMap, e.as_driver_error()))?;

    // Bring the controller to the halted/reset state. `open_diagnostic` names
    // the failing reset sub-stage and the `USBCMD`/`USBSTS` it saw.
    let xhci = Xhci::open_diagnostic_with_budget(window, DEFAULT_POLL_BUDGET).map_err(|err| {
        let mut e = ControllerBringupError::bare(BringupPhase::ControllerOpen, err.error);
        e.open_stage = Some(err.stage);
        e.usbcmd = err.registers.usbcmd;
        e.usbsts = err.registers.usbsts;
        e
    })?;

    // Lay out and program the device-shared structures out of the carved
    // region and start the controller.
    let mut device = UsbDevice::start(xhci, dma, DEFAULT_POLL_BUDGET)
        .map_err(|e| ControllerBringupError::bare(BringupPhase::ControllerStart, e))?;

    // Bring the controller up to serve every reachable device, transparently
    // descending through hubs — including a hub plugged into a hub. The
    // arch-neutral root→hub→downstream orchestration lives once in
    // `rustos_usb`, so each device is discovered, never a guessed port. A
    // device absent at boot is
    // **not** a failure: `bring_up` leaves the controller up with the
    // first-connect watch armed (the onboard hub's status-change endpoint, or
    // the root port), and the HCD publishes interface nodes only for the
    // devices actually enumerated. On a real bring-up failure the engine's
    // per-transfer breadcrumbs localise the stall.
    match device.bring_up(delay) {
        // The served devices — possibly none yet — are the live
        // `device_live` indices; either way the controller is serving.
        Ok(()) => {}
        Err(error) => {
            let mut e = ControllerBringupError::bare(BringupPhase::Enumerate, error);
            e.enum_stage = Some(device.enum_stage());
            e.last_completion = device.last_completion_code();
            e.last_event_type = device.last_event_type();
            e.last_reject = device.last_reject_reason();
            e.port1_portsc = device.port_status_raw(1);
            return Err(e);
        }
    }
    Ok(device)
}

#[cfg(test)]
#[path = "bringup_tests.rs"]
mod tests;
