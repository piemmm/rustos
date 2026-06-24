//! Arch-neutral boot-keyboard driver-process orchestration (`plans/PI.md`
//! P10 chunk 5d-2-ii).
//!
//! This is the composition a **user-space** USB boot-keyboard driver runs at
//! start-up: it brings the keyboard up over the device-resource grants the
//! kernel minted for it (`AGENTS.md` §18.3) and hands back a [`BootKeyboard`]
//! the driver's service loop drives with [`crate::pump_once`], injecting each
//! decoded key edge through the `key_inject` syscall.
//!
//! # What lives here, and what does not
//!
//! The driver process is **arch-neutral**: the board `PCIe` root-complex
//! bring-up and BAR assignment stay in the separate board bus driver
//! (`drivers/bus/pcie_brcm` + `drivers/bus/usb`), which assigns the
//! controller's BAR inside the bridge's outbound window and emits the
//! enumerated device into the hardware tree. The keyboard driver is then
//! autoloaded against the HID node, granted *only* the resources its matched
//! node requested — its already-assigned xHCI register BAR and a DMA
//! constraint — and reaches them through the [`DriverHost`] its runtime
//! ([`rustos_drvrt`](https://docs.rs/rustos-drvrt)-style host) builds over
//! those grants. So this orchestration knows nothing of PCI, the BCM2711, or
//! any board: it maps a register window by address, carves a DMA region, and
//! speaks the bus-agnostic xHCI protocol in [`rustos_usb`] (`AGENTS.md`
//! §2.20 / §17.4).
//!
//! # Composition
//!
//! [`bring_up_boot_keyboard`] carves the device-shared DMA region first and
//! checks it lies wholly below the discovered inbound-DMA aperture *before*
//! any register is touched (fail closed, `AGENTS.md` §5.4), maps the granted
//! register BAR, brings the controller up ([`Xhci::open`] +
//! [`UsbDevice::start`]), and runs the arch-neutral
//! root→hub→downstream-HID enumeration ([`UsbDevice::enumerate_boot_keyboard`])
//! that descends the Pi 4's onboard hub when present. On success the returned
//! [`BootKeyboard`] wraps the device pointed at the keyboard's slot.
//!
//! # Testing boundary
//!
//! QEMU models no Pi USB timing (`AGENTS.md` §0.4 / §2.1), so the host tests
//! prove the composition and its fail-closed paths up to the controller
//! hand-off; over an inert mock register window [`Xhci::open`] fails closed
//! with [`DriverError::DeviceFault`], which is exactly the on-metal boundary
//! (mirroring `drivers/bus/usb`'s `wiring` tests, `AGENTS.md` §2.2). The live
//! controller bring-up and the report pump are the on-metal acceptance item.

use rustos_abi::driver::dma::DmaSlab;
use rustos_abi::hwtree::{HwResource, HwResourceKind};
use rustos_abi::{CapabilityId, Delay, DriverError, DriverHost, MmioMapper, RegisterWindow};
use rustos_usb::device::{EnumStage, UsbDevice};
use rustos_usb::{Xhci, XhciOpenStage, DEFAULT_POLL_BUDGET, XHCI_DMA_BYTES};

use crate::BootKeyboard;

/// The concrete bring-up inputs a USB boot-keyboard driver process derives
/// from its kernel-issued device-resource grants to drive
/// [`bring_up_boot_keyboard`].
///
/// A `devmgr`-autoloaded driver is granted exactly the resources its matched
/// hardware-tree node requested — its already-assigned xHCI register BAR and
/// a DMA constraint — and no more (`AGENTS.md` §4 / §18.3). The driver does
/// not know those addresses at build time (they depend on the board's bus
/// layout, `AGENTS.md` §2.20); it reads them from the grants the kernel
/// delivered through `resource_grants` and turns them into these values with
/// [`derive_keyboard_resources`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KeyboardResources {
    /// Device-visible base address of the controller's assigned register BAR
    /// window — the address the driver names the BAR by when mapping it
    /// through its [`DriverHost`] (the host resolves the covering grant and
    /// performs the bus→CPU translation, `AGENTS.md` §18.1).
    pub bar_base: u64,
    /// Length of that register window, in bytes.
    pub bar_len: usize,
    /// Exclusive upper bound, in the **device-visible** address space, of the
    /// inbound DMA aperture the controller may reach — the bound
    /// [`bring_up_boot_keyboard`] checks the carved DMA region against.
    pub dma_aperture_top: u64,
}

/// Derive the [`KeyboardResources`] from the [`HwResource`] grants the kernel
/// minted for this keyboard driver process (`AGENTS.md` §18.3).
///
/// Exactly one mappable register window — an [`HwResourceKind::Mmio`] window
/// (CPU/identity space) or an [`HwResourceKind::BusWindow`] (outbound
/// PCIe-bus space, addressed by its far-side [`translated_base`]) — supplies
/// the BAR `bar_base`/`bar_len`. Exactly one [`HwResourceKind::Dma`]
/// constraint supplies the aperture bound: its device-visible exclusive top
/// is the far-side base plus extent for a translated inbound viewport
/// ([`HwResource::dma_translated`]), or its `addr_limit` for an untranslated
/// constraint ([`HwResource::dma`]). Any other resource (an IRQ line, a port
/// range) is ignored — this driver maps only the BAR and carves only the DMA
/// region.
///
/// # Errors
///
/// Fails closed (`AGENTS.md` §2.9 / §5.4), never guessing a missing address:
///
/// * [`DriverError::NotFound`] if no register-window grant or no DMA grant is
///   present.
/// * [`DriverError::Unsupported`] if more than one register-window grant or
///   more than one DMA grant is present (an ambiguous delivery — a packaging
///   defect the driver refuses rather than picking one).
/// * [`DriverError::OutOfRange`] for a zero-length BAR, a BAR length past
///   `usize`, or a translated DMA aperture whose far-side top overflows.
///
/// [`translated_base`]: HwResource::translated_base
pub fn derive_keyboard_resources<'a, I>(resources: I) -> Result<KeyboardResources, DriverError>
where
    I: IntoIterator<Item = &'a HwResource>,
{
    let mut bar: Option<(u64, u64)> = None;
    let mut aperture: Option<u64> = None;
    for resource in resources {
        // The register-window base (CPU `base` for an `Mmio` window, far-side
        // `translated_base` for a `BusWindow`) is the one definition in
        // `HwResource::register_window_base` (`AGENTS.md` §2.2 — shared with
        // `sole_register_window`), so this driver does not re-decide it.
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
            // `[translated_base, translated_base + len)`, so its exclusive
            // top is the far-side base plus extent; an untranslated
            // constraint's `addr_limit` (stored as `base`) is already the
            // device-visible exclusive top (`AGENTS.md` §18.1).
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
        // An IRQ line, a port range, or an unknown kind is not part of this
        // driver's bring-up (`AGENTS.md` §5.4 — validate the kind).
    }
    let (bar_base, bar_len) = bar.ok_or(DriverError::NotFound)?;
    let bar_len = usize::try_from(bar_len).map_err(|_| DriverError::OutOfRange)?;
    if bar_len == 0 {
        return Err(DriverError::OutOfRange);
    }
    let dma_aperture_top = aperture.ok_or(DriverError::NotFound)?;
    Ok(KeyboardResources {
        bar_base,
        bar_len,
        dma_aperture_top,
    })
}

/// The user-space keyboard driver's brought-up boot keyboard: a
/// [`BootKeyboard`] over the controller's single-device enumeration engine,
/// owning the mapped register BAR and the carved DMA region.
pub type KeyboardSource = BootKeyboard<UsbDevice<RegisterWindow, DmaSlab>>;

/// Bring up a HID boot keyboard reachable through the granted xHCI
/// controller and return a [`BootKeyboard`] the driver's service loop drives
/// with [`crate::pump_once`].
///
/// `host` is the driver process's [`DriverHost`] over its kernel-issued
/// device-resource grants (its register BAR and DMA constraint). `bar_base`
/// and `bar_len` name the controller's already-assigned register BAR window
/// — the board bus driver assigned it inside the bridge's outbound window and
/// the keyboard node was granted it, so the driver maps it by address through
/// the host (which resolves the grant and performs the bus→CPU translation,
/// `AGENTS.md` §18.1). `dma_aperture_top` is the *exclusive* upper bound, in
/// the **device-visible** address space, of the inbound window the bridge
/// lets the controller reach (`inbound_pcie_base + inbound_size`, the
/// discovered `dma-ranges` aperture): the carved region's device-visible
/// address must lie wholly below it or the controller could not reach its own
/// rings (`AGENTS.md` §5.4 — the bound must match the address space it
/// guards).
///
/// The DMA region is carved and aperture-checked before any register is
/// touched, so a region the controller could not reach is refused fail-closed
/// with nothing half-configured (`AGENTS.md` §5.4 / §2.9). `delay` supplies
/// the hardware-dictated hub settle windows; the caller owns the clock.
///
/// # Errors
///
/// Fails closed (`AGENTS.md` §2.9), leaving nothing half-configured:
///
/// * [`DriverError::PermissionDenied`] if `host` did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * [`DriverError::Unsupported`] if `host` exposes no [`MmioMapper`] or no
///   DMA facility.
/// * [`DriverError::OutOfRange`] if the carved DMA region's device-visible
///   end does not lie below `dma_aperture_top`.
/// * Any error of the DMA carve, the BAR map, [`Xhci::open`],
///   [`UsbDevice::start`], or [`UsbDevice::enumerate_boot_keyboard`] (e.g.
///   [`DriverError::NotFound`] for an empty root hub or a hub with no
///   connected downstream port).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (to map the register BAR); the DMA
/// carve is gated on the host's own DMA capability (`CAP_MEM_DMA`) at
/// allocation time. Both are re-checked kernel-side at each map/allocation
/// (`AGENTS.md` §5.4).
pub fn bring_up_boot_keyboard(
    host: &dyn DriverHost,
    delay: &dyn Delay,
    bar_base: u64,
    bar_len: usize,
    dma_aperture_top: u64,
) -> Result<KeyboardSource, DriverError> {
    bring_up_boot_keyboard_diagnostic(host, delay, bar_base, bar_len, dma_aperture_top)
        .map_err(|err| err.error)
}

/// The phase of [`bring_up_boot_keyboard_diagnostic`] that failed, so a
/// driver's one-shot diagnostic can name *where* a coarse [`DriverError`]
/// came from when the controller does not come up (`AGENTS.md` §15.7). QEMU
/// models no Pi USB, so the live bring-up is metal-only (`AGENTS.md` §0.4):
/// this is the breadcrumb the on-metal capture reports.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BringupPhase {
    /// Acquiring the host's capability / mapper / DMA seams (a missing
    /// `CAP_MMIO_MAP`, `MmioMapper`, or DMA host).
    Setup,
    /// Carving the device-shared DMA region (`dma_alloc`).
    DmaCarve,
    /// The carved region's device-visible end exceeds the inbound DMA
    /// aperture the controller can reach.
    DmaAperture,
    /// Mapping the controller's register BAR (`mmio_map`).
    BarMap,
    /// Bringing the controller to the halted/reset state ([`Xhci::open`]).
    ControllerOpen,
    /// Programming the DMA structures and starting the controller
    /// ([`UsbDevice::start`]).
    ControllerStart,
    /// Enumerating the boot keyboard ([`UsbDevice::enumerate_boot_keyboard`]).
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

/// A structured boot-keyboard bring-up failure: the coarse [`DriverError`]
/// plus the breadcrumb a one-shot on-metal diagnostic needs to pin which
/// controller step stalled (`AGENTS.md` §15.7). The engine itself holds no
/// logging dependency (`AGENTS.md` §17.4 / §2.2); the driver wraps the engine
/// with its own diagnostics and emits this through `lib/log`.
///
/// Which fields are populated depends on [`Self::phase`]:
///
/// * [`BringupPhase::ControllerOpen`] sets [`Self::open_stage`] and the
///   [`Self::usbcmd`]/[`Self::usbsts`] snapshot the reset stage observed.
/// * [`BringupPhase::Enumerate`] sets [`Self::enum_stage`],
///   [`Self::last_completion`], [`Self::last_event_type`],
///   [`Self::last_reject`] (the engine's per-transfer breadcrumbs) and the
///   [`Self::port1_portsc`] snapshot — the `stage=N completion=M` signature
///   that historically localised every enumeration stall.
/// * The earlier phases carry only [`Self::phase`] and [`Self::error`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KeyboardBringupError {
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
    /// [`BringupPhase::Enumerate`] (the connect/power/enable/speed bits).
    pub port1_portsc: Option<u32>,
}

impl KeyboardBringupError {
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

/// [`bring_up_boot_keyboard`] with a structured [`KeyboardBringupError`] on
/// failure, naming the phase that stalled and the controller state observed
/// there.
///
/// The driver process calls this (rather than the plain
/// [`bring_up_boot_keyboard`]) so it can emit a one-shot diagnostic through
/// `lib/log` when the controller does not come up — the user-space
/// replacement for the in-kernel scaffold's per-stage logging, now that the
/// keyboard is brought up in user space (`AGENTS.md` §17.4 / §15.7). On a
/// non-I/O-coherent platform a stall at [`BringupPhase::Enumerate`] with
/// `enum_stage = EnableSlot` and `last_completion = 0` is the classic
/// DMA-not-visible signature; a [`BringupPhase::ControllerOpen`] stall names
/// the reset sub-stage and its `USBCMD`/`USBSTS`.
///
/// # Errors
///
/// As [`bring_up_boot_keyboard`], but wrapped in [`KeyboardBringupError`].
///
/// # Capabilities
///
/// As [`bring_up_boot_keyboard`].
pub fn bring_up_boot_keyboard_diagnostic(
    host: &dyn DriverHost,
    delay: &dyn Delay,
    bar_base: u64,
    bar_len: usize,
    dma_aperture_top: u64,
) -> Result<KeyboardSource, KeyboardBringupError> {
    // Capability before state (`AGENTS.md` §5.4); the kernel re-checks at the
    // map/carve traps regardless.
    if !host.has_capability(CapabilityId::MMIO_MAP) {
        return Err(KeyboardBringupError::bare(
            BringupPhase::Setup,
            DriverError::PermissionDenied,
        ));
    }
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(KeyboardBringupError::bare(
        BringupPhase::Setup,
        DriverError::Unsupported,
    ))?;
    // A USB keyboard is not a virtio device: it allocates its xHCI DMA through
    // the bus-neutral DMA seam, not the virtio host (`AGENTS.md` §2.2).
    let dma_host = host.dma_host().ok_or(KeyboardBringupError::bare(
        BringupPhase::Setup,
        DriverError::Unsupported,
    ))?;

    // Carve the device-shared DMA region and verify it lies wholly below the
    // discovered inbound-DMA aperture before any register is touched: a region
    // the controller cannot reach is a fail-closed refusal, never a silent
    // truncation (`AGENTS.md` §5.4). The slab is reclaimed on the early
    // return. The kernel maps it coherent (Normal Non-Cacheable on a
    // non-I/O-coherent platform), so the controller sees the rings the driver
    // writes with no cache maintenance (`AGENTS.md` §4 / §2.20).
    let dma = dma_host
        .alloc_dma_zeroed(XHCI_DMA_BYTES)
        .map_err(|e| KeyboardBringupError::bare(BringupPhase::DmaCarve, e))?;
    let end = dma
        .phys()
        .checked_add(dma.len() as u64)
        .ok_or(KeyboardBringupError::bare(
            BringupPhase::DmaAperture,
            DriverError::OutOfRange,
        ))?;
    if end > dma_aperture_top {
        return Err(KeyboardBringupError::bare(
            BringupPhase::DmaAperture,
            DriverError::OutOfRange,
        ));
    }

    // Map the controller's already-assigned register BAR. The host resolves
    // the grant covering `[bar_base, bar_base + bar_len)` and maps that window
    // once (`AGENTS.md` §2.16); a window no grant covers is refused.
    let window = mapper
        .map_window(bar_base, bar_len)
        .map_err(|e| KeyboardBringupError::bare(BringupPhase::BarMap, e.as_driver_error()))?;

    // Bring the controller to the halted/reset state. `open_diagnostic`
    // names the failing reset sub-stage and the `USBCMD`/`USBSTS` it saw.
    let xhci = Xhci::open_diagnostic_with_budget(window, DEFAULT_POLL_BUDGET).map_err(|err| {
        let mut e = KeyboardBringupError::bare(BringupPhase::ControllerOpen, err.error);
        e.open_stage = Some(err.stage);
        e.usbcmd = err.registers.usbcmd;
        e.usbsts = err.registers.usbsts;
        e
    })?;

    // Lay out and program the device-shared structures out of the carved
    // region and start the controller.
    let mut device = UsbDevice::start(xhci, dma, DEFAULT_POLL_BUDGET)
        .map_err(|e| KeyboardBringupError::bare(BringupPhase::ControllerStart, e))?;

    // Enumerate the boot keyboard, transparently descending one tier through
    // an onboard hub. The arch-neutral root→hub→downstream orchestration lives
    // once in `rustos_usb` (`AGENTS.md` §2.2 / §18), so the keyboard is
    // discovered, never a guessed port; on success `device` is left pointed at
    // the keyboard's slot so `BootKeyboard` drains its reports. On failure the
    // engine's per-transfer breadcrumbs localise the stall (`AGENTS.md`
    // §15.7).
    if let Err(error) = device.enumerate_boot_keyboard(delay) {
        let mut e = KeyboardBringupError::bare(BringupPhase::Enumerate, error);
        e.enum_stage = Some(device.enum_stage());
        e.last_completion = device.last_completion_code();
        e.last_event_type = device.last_event_type();
        e.last_reject = device.last_reject_reason();
        e.port1_portsc = device.port_status_raw(1);
        return Err(e);
    }
    Ok(BootKeyboard::new(device))
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
