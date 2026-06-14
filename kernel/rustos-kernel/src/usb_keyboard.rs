//! VL805/xHCI USB-keyboard composition (`plans/PI.md` P10).
//!
//! On the Raspberry Pi 4 (BCM2711) the USB-A ports hang off a VL805 xHCI
//! host controller behind the `SoC`'s PCIe root complex, whose link ships
//! **down** and whose config space is windowed (not flat ECAM). Bringing a
//! USB keyboard to the video-console login therefore means composing four
//! loadable driver crates into one chain:
//!
//! 1. [`rustos_drv_bus_pcie_brcm`] resets the BCM2711 root complex and
//!    trains its link, programmed with the discovered inbound/outbound
//!    address windows;
//! 2. [`rustos_drv_bus_pci::mechanism_brcm`] enumerates the VL805 over the
//!    windowed config accessor built on the same register window;
//! 3. [`rustos_drv_bus_usb`] maps the controller's BAR, carves its
//!    device-shared DMA region, and brings the xHCI controller up; and
//! 4. [`rustos_drv_input_usb_hid`] decodes the boot keyboard's reports into
//!    device-resolved [`KeyInput`] key-edge records and hands each to the
//!    kernel input-focus arbiter (via [`ArbiterConsoleSink`]), which decides
//!    by who holds focus whether to encode a press to the text console's tty
//!    bytes or deliver the whole record to the desktop (`AGENTS.md` §17.4).
//!
//! Each of those crates is a separate driver and may not name another
//! (`AGENTS.md` §17.4 — `deps-check` forbids driver→driver edges). The
//! image-assembly binary (`rustos-kernel`, `Layer::Tooling`) is the one
//! place permitted to name them all, so the composition lives here, exactly
//! as the virtio bring-up does (`crate::virtio_boot`). The engine itself is
//! architecture-neutral — it consumes only the `lib/abi` driver seams and
//! the discovered [`HwNode`] — so it compiles and is host-tested on the CI
//! host; the aarch64 boot path supplies the concrete [`DriverHost`]
//! (`KernelMmioMapper` + per-driver DMA host) and the generic-timer-backed
//! [`Delay`] that drive it on metal.
//!
//! # No QEMU vertical
//!
//! QEMU models no Pi PCIe link timing or USB (`AGENTS.md` §0.4 / §2.1), so
//! the host tests prove the composition, its window assembly, and its
//! fail-closed paths up to the controller hand-off, where the inert mock
//! register window faults — exactly the metal boundary. The live link
//! training, a real BAR answering a plausible `CAPLENGTH`, and a keyboard
//! driving the login are the on-metal acceptance items.

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::dma::DmaSlab;
use rustos_abi::driver::virtio::VirtioHost;
use rustos_abi::input::KeyInput;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwNode, HwResourceKind, MmioMapper, PciBus,
    RegisterWindow,
};
use rustos_caps::CapabilitySet;
use rustos_drv_bus_pcie_brcm::{
    self as pcie_brcm, BringUpTiming, Delay, InboundWindowReadback, OutboundWindowReadback,
    PcieWindows,
};
use rustos_drv_bus_usb::device::UsbDevice;
use rustos_drv_bus_usb::{Xhci, XhciOpenError, DEFAULT_POLL_BUDGET};
use rustos_drv_input_usb_hid::{BootKeyboard, ConsoleSink};
use rustos_kernel_core::InputFocus;
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

/// Audit event: a progress or failure milestone of the in-kernel VL805
/// USB-keyboard bring-up chain. Logged at each stage (PCIe link training,
/// xHCI controller bring-up, root-hub enumeration) so a metal capture
/// shows exactly *which* stage a silent keyboard stalls at, rather than
/// the bring-up failing silently (the issue's "what is discovered on
/// USB"). Bin-crate id alongside the boot pipeline's `4097`/`4100`; part
/// of the audit contract, not renumbered (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_BRINGUP: EventId = EventId(4101);

/// Audit event: the bring-up chain enumerated a USB device on the VL805
/// root hub. Carries the device's vendor/product id and assigned xHCI
/// slot, so a capture shows the keyboard the chain actually found (or, by
/// its absence, that none was). Bin-crate id; part of the audit contract
/// (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_DEVICE: EventId = EventId(4102);

/// Audit event: the optional `VideoCore` VL805 firmware reload fallback.
///
/// The fallback is issued only after the VL805's firmware-version register
/// (config `0x50`) stays zero, matching Linux's `rpi_firmware_init_vl805`:
/// a non-zero version skips the reload, while a zero version means the boot
/// chain did not leave firmware resident and the kernel asks the firmware
/// service once. The event is one-shot, never on the poll path
/// (`AGENTS.md` §2.16 / §19.4), and a failure is logged before the bring-up
/// stops without touching the uninitialised xHCI BAR (`AGENTS.md` §2.9).
const USB_KEYBOARD_FW_RESET: EventId = EventId(4108);

/// Audit event: a function the bring-up's one-shot PCIe configuration
/// scan saw responding on the BCM2711 root complex (and a leading
/// summary count). On the Pi 4 a healthy bus shows two: the root complex
/// itself (`14e4:2711`, class `0604`) and the VL805 USB host behind it
/// (`1106:3483`, class `0c03`). A scan that reports *no* downstream
/// function localises a silent keyboard to "the VL805 is not answering
/// configuration reads" — distinct from "enumerated but xHCI did not come
/// up" — which is the missing half of the issue's "what is discovered on
/// USB". Bin-crate id alongside the boot pipeline's `4097`/`4100`/`4101`;
/// part of the audit contract, not renumbered (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_PCI_SCAN: EventId = EventId(4104);

/// Audit event: the one-shot xHCI carve + capability-block geometry the
/// bring-up reads after mapping the VL805's register BAR, before it
/// programs the controller. It pins the device-visible DMA carve (base,
/// length, aperture bound) and the controller's own register-block
/// geometry (the mapped BAR window length and the `CAPLENGTH`/`DBOFF`/
/// `RTSOFF` offsets plus `MaxSlots`/`MaxPorts`/`AC64`/`CSZ`), so a metal
/// capture localises an `out_of_range` bring-up to a concrete value (a
/// DMA address the controller cannot reach, or a register offset past
/// the mapped window) rather than a bare error code. One-shot at
/// bring-up, never on the poll path (`AGENTS.md` §2.16 / §19.4); part of
/// the audit contract (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_GEOMETRY: EventId = EventId(4106);

/// Audit event: the raw capability-register dwords read straight off the
/// mapped VL805 register BAR, one-shot, *before* [`Xhci::open`] validates
/// them. [`Xhci::open`] failed `out_of_range` after the BAR mapped (the
/// `4105`/`4106` lines confirm discovery, the DMA carve, BAR assignment
/// and the BAR map all passed), and `out_of_range` from `open` can only
/// be a [`RegisterWindow`] bounds/alignment refusal — which, for the tiny
/// 4-byte-aligned capability offsets `open` touches, means the operational
/// base it derives from `CAPLENGTH` is itself misaligned (a `CAPLENGTH`
/// that is not a multiple of four). The geometry line ([`USB_KEYBOARD_GEOMETRY`])
/// that would show `CAPLENGTH` is logged only *after* `open` succeeds, so
/// it never prints on this failure. This event dumps the first capability
/// dwords (`CAPLENGTH`/`HCIVERSION` at `0x00`, `HCSPARAMS1` at `0x04`,
/// `HCCPARAMS1` at `0x10`, `DBOFF` at `0x14`, `RTSOFF` at `0x18`) exactly
/// as the BAR returns them, so a metal capture shows whether the BAR even
/// decodes (real values vs an all-ones bus-abort pattern) and the exact
/// `CAPLENGTH` byte that drives the refusal — measuring the cause rather
/// than guessing (`AGENTS.md` §15.7). One-shot at bring-up, never on the
/// poll path (`AGENTS.md` §2.16 / §19.4); part of the audit contract
/// (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_CAPS_RAW: EventId = EventId(4107);

/// Audit event: the bounded wait for the VL805 to present its capability
/// block after the firmware-version wait. The controller's internal xHCI
/// core boots only once firmware is present; until it does, reads of its
/// capability registers
/// return an uninitialised bus pattern (`0`, the all-ones UR sentinel, or
/// the BCM2711 `dead_dead` poison — the `4107` capture). This event
/// records how many reads the capability header took
/// (`polls_hex`), the final header dword observed (`caplength_hciversion_hex`),
/// and whether it became live (`ready_hex`). The wait is bounded by
/// elapsed wall time ([`CAPS_READY_BUDGET_US`], `AGENTS.md` §2.1 / §2.16),
/// so a slow master-aborting BAR read cannot stretch it; a controller that
/// never decodes is left to fail closed at [`Xhci::open`] (`AGENTS.md`
/// §2.9). It disambiguates "the controller just needed time after firmware
/// became present" from "it never came up", localising any remaining fault
/// (`AGENTS.md` §15.7).
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); part of the audit contract (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_CAPS_READY: EventId = EventId(4109);

/// Audit event: a read-back of configuration space after the BAR is
/// assigned and the command register enabled, but before the capability
/// block is read.
///
/// The `4107`/`4109` captures show the mapped BAR returning the BCM2711
/// `dead_dead` master-abort poison even though configuration reads
/// succeed (the `4104` scan saw the VL805). The whole controller/bridge
/// programming chain is present in code — the root port's bus-number
/// register (config forwarding), its Memory Base/Limit window (memory
/// forwarding), the CPU→PCIe outbound translation, the VL805's BAR
/// assignment, and its command register (memory-space + bus-master) — so
/// this event reads each of those registers *back* to show which write
/// actually stuck on metal: the bridge's bus numbers (`0x18`), Memory
/// Base/Limit (`0x20`), and command/status (`0x04`), and the VL805's
/// command/status (`0x04`), BAR0 (`0x10`), and BAR1 (`0x14`). It
/// disambiguates "a configuration write did not take" from "every
/// register is programmed yet the controller still does not decode"
/// (a link/firmware fault past our control), measuring the cause rather
/// than guessing the next fix (`AGENTS.md` §15.7). A read that faults is
/// rendered as an all-ones sentinel and never propagated — the readback
/// is diagnostic, not a bring-up step (`AGENTS.md` §2.9). One-shot at
/// bring-up, never on the poll path (`AGENTS.md` §2.16 / §19.4); part of
/// the audit contract (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_CONFIG: EventId = EventId(4110);

/// Audit event: the response value word returned by the `VideoCore`
/// `NOTIFY_XHCI_RESET` property tag.
///
/// A healthy firmware normally echoes the VL805 device address (`0x10_0000`);
/// the value is diagnostic only, never authority. The tag's per-response bit
/// has already been verified by `rustos_vcmailbox`, so this event records the
/// firmware's value for metal correlation without deciding success on it
/// (`AGENTS.md` §5.4 / §19.4).
const USB_KEYBOARD_FW_RESPONSE: EventId = EventId(4113);

/// Audit event: a read-back of the controller's outbound (CPU→PCIe)
/// memory-window registers and link status, read straight off the trained
/// register block right after the link trains, before the windowed config
/// accessor is built over it.
///
/// The `4110` configuration read-back proved every PCI-config register
/// reads back what bring-up wrote (bus numbers, Memory Base/Limit, the
/// VL805's BAR and command), yet the mapped BAR still returns the BCM2711
/// `dead_dead` master-abort poison (`4107`/`4109`) — and that the
/// integrated RC's own Command register reads back `0x0000` regardless of
/// the write, so it does not gate memory forwarding. Configuration and
/// memory take *different* paths through the controller — configuration
/// through the internal `EXT_CFG` window, memory through the CPU→PCIe
/// outbound translation window — so a memory access that aborts while
/// configuration works isolates the fault to that outbound path. This
/// event reads the outbound-window registers (`MEM_WIN0_LO`/`HI`,
/// `BASE_LIMIT`, `BASE_HI`, `LIMIT_HI`) and the link `STATUS` register
/// back, so a metal capture shows whether the window actually holds the
/// programmed CPU base (`0x6_0000_0000`, MiB-encoded) and PCIe base
/// (`0xc000_0000`) and whether the data link reports up — measuring the
/// cause rather than guessing the next fix (`AGENTS.md` §15.7). A faulting
/// read renders the all-ones sentinel and is never propagated — the
/// read-back is diagnostic (`AGENTS.md` §2.9). One-shot at bring-up, never
/// on the poll path (`AGENTS.md` §2.16 / §19.4); part of the audit
/// contract (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_OUTBOUND: EventId = EventId(4111);

/// Audit event: a re-read of the VL805's configuration space and
/// capability header *after* the firmware-version wait and the bounded
/// readiness settle (`4109`).
///
/// The metal symptom is that every capability read stays `dead_dead` — the
/// VL805's "firmware not loaded" pattern — even though all PCI-config
/// programming reads back correct (`4110`). This event re-reads
/// the VL805's vendor/device (`0x00`), command/status (`0x04`), BAR0
/// (`0x10`) and BAR1 (`0x14`) — and the mapped BAR's `CAPLENGTH`/
/// `HCIVERSION` header — *after* the firmware-version wait and BAR settle,
/// so a metal capture distinguishes "the function is still present but has
/// no firmware" from "the function is present and firmware-loaded but the
/// controller still does not decode" (`vendor_device=ffff_ffff` means the
/// device dropped off the bus). A faulting read renders the all-ones
/// sentinel and is never
/// propagated — the readback is diagnostic, not a bring-up step
/// (`AGENTS.md` §2.9). One-shot at bring-up, never on the poll path
/// (`AGENTS.md` §2.16 / §19.4); part of the audit contract
/// (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_POST_RELOAD: EventId = EventId(4114);

/// Audit event: the per-phase wall-time breakdown of the PCIe
/// root-complex `bring_up`, in microseconds.
///
/// A timestamped metal capture localised a ~11 s USB bring-up pause
/// entirely to `BrcmPcieRc::bring_up`, yet that routine's coded delays
/// (the two ~200 µs reset settles plus the ≤ 100 ms link-training wait)
/// total only a few hundred milliseconds — so the seconds are spent in a
/// stalling register access. The `4117` per-phase split pinned the
/// seconds to the reset phase, and the reset sub-spans pinned them to the
/// **first access to the MISC register block** (`0x4xxx`): at OS entry
/// the controller core is held off, so a MISC access does not complete
/// until the always-accessible RGR1 bridge `sw_init` reset (`0x9210`) has
/// been cycled — touching MISC first master-aborts ~10.8 s on the `SoC`
/// bus completion timeout (the same accesses cost microseconds once the
/// controller is out of reset, as the configuration phase confirms). So
/// `BrcmPcieRc::reset_controller` releases the bridge reset **before**
/// touching MISC, matching U-Boot/Linux `pcie-brcmstb`; the split is
/// retained so a metal capture pins any residual stall to the exact MMIO
/// group: `reset_swinit_us` (releasing the `RGR1_SW_INIT_1` bridge
/// `sw_init` the previous boot stage left asserted), `reset_settle_us`
/// (the post-de-reset MISC settle — the gentlest no-touch-probe bring-up
/// does **not** toggle the SerDes `IDDQ` or re-assert a fundamental
/// reset, either of which could drop the resident VL805 firmware;
/// `train_link` deasserts the already-asserted `PERST#` as the single
/// firmware-(re)load edge), `config_us` (the `MISC_*` and type-1 bridge
/// configuration-space programming), `linkwait_us` (the `PERST#`-deassert
/// link-retrain settle plus the bounded link-up poll), and `link_polls`
/// (`AGENTS.md` §15.7 — measure, don't guess).
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); rendered on the stack (`AGENTS.md` §2.9); part of the audit
/// contract (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_BRINGUP_TIMING: EventId = EventId(4117);

/// Audit event: the bounded wait for the VL805's **XHCI MCU firmware
/// version** ([`VL805_FW_VERSION_OFFSET`] `0x50`) to read non-zero after
/// the link trains — the Linux-faithful firmware-load readiness signal.
///
/// On a Raspberry Pi 4 the boot chain owns the VL805 firmware load. The
/// firmware version lives in **configuration space**, which is reachable
/// on metal (the `4104` scan reads vendor/device fine) while the VL805's
/// MMIO BAR returns the BCM2711 `dead_dead` master-abort poison until the
/// MCU decodes — so this polls `0x50` (the register Linux's
/// `rpi_firmware_init_vl805` checks), the *working* readiness signal,
/// rather than the aborting BAR ([`wait_for_caps_ready`], which runs only
/// once the firmware is loaded). Records how many reads it took
/// (`polls_hex`), the final version dword (`fw_version_hex`), and whether
/// it became non-zero (`ready_hex`). Bounded by [`FW_LOADED_BUDGET_US`] of
/// elapsed wall time (`AGENTS.md` §2.1 / §2.16); a board that never loads
/// is left to fail closed at [`Xhci::open`] (`AGENTS.md` §2.9). One-shot
/// at bring-up, never on the poll path (`AGENTS.md` §2.16 / §19.4); fields
/// rendered on the stack (`AGENTS.md` §2.9); part of the audit contract
/// (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_FW_READY: EventId = EventId(4118);

/// The Pi firmware reset controller's encoded VL805 PCI address
/// (`bus << 20 | slot << 15 | func << 12`) for the hardwired bus-1,
/// device-0, function-0 controller.
pub const VL805_FIRMWARE_DEV_ADDR: u32 = 0x0010_0000;

/// Minimum post-`NOTIFY_XHCI_RESET` settle before polling config `0x50`
/// again; Linux waits `200..1000 µs`, so this uses the lower bound and then
/// the existing bounded firmware-version wait handles the remainder.
const FW_RELOAD_SETTLE_US: u32 = 200;

/// Stable reason reported when the optional VL805 firmware reload is refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FirmwareResetFailure {
    /// The mailbox register or property-buffer window was not usable.
    Window,
    /// The firmware mailbox did not complete within the bounded poll budget.
    Timeout,
    /// The firmware returned its top-level error response.
    FirmwareError,
    /// The firmware returned a malformed or unhonoured tag response.
    MalformedResponse,
    /// The property buffer was outside the `VideoCore` DMA aperture.
    BadAperture,
    /// The discovered mailbox or buffer geometry was unusable.
    BadGeometry,
    /// A newer mailbox error reached an older firmware-reset mapper.
    Unknown,
}

impl FirmwareResetFailure {
    const fn as_str(self) -> &'static str {
        match self {
            FirmwareResetFailure::Window => "window",
            FirmwareResetFailure::Timeout => "timeout",
            FirmwareResetFailure::FirmwareError => "firmware_error",
            FirmwareResetFailure::MalformedResponse => "malformed_response",
            FirmwareResetFailure::BadAperture => "bad_aperture",
            FirmwareResetFailure::BadGeometry => "bad_geometry",
            FirmwareResetFailure::Unknown => "unknown",
        }
    }
}

/// Result of one optional VL805 firmware reload attempt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FirmwareResetOutcome {
    /// No firmware mailbox is available for this boot shape.
    NotAvailable,
    /// The firmware honoured the tag and returned `response_value`.
    Reloaded {
        /// Diagnostic response value written by the firmware.
        response_value: u32,
    },
    /// The mailbox transport or firmware refused the tag.
    Failed {
        /// Stable failure reason for the diagnostic log.
        reason: FirmwareResetFailure,
    },
}

/// Optional `VideoCore` firmware reload seam used when config `0x50` stays
/// zero after PCI/BAR setup.
pub trait FirmwareReset {
    /// Attempt the single Linux-style `NOTIFY_XHCI_RESET` fallback.
    fn reload(&self) -> FirmwareResetOutcome;
}

/// Firmware-reset implementation for host tests and boot shapes with no
/// discovered `VideoCore` firmware mailbox.
pub struct NoFirmwareReset;

impl FirmwareReset for NoFirmwareReset {
    fn reload(&self) -> FirmwareResetOutcome {
        FirmwareResetOutcome::NotAvailable
    }
}

/// Audit event: a read-back of the controller's **inbound**
/// (PCIe→system-memory) viewport registers after bring-up.
///
/// On the Raspberry Pi 4 the `VideoCore` co-processor's firmware handoff
/// may rely on the inbound DMA window (the "xHCI firmware window").
/// `bring_up` programs the active inbound
/// viewport in `RC_BAR2` and disables the unused `RC_BAR1`/`RC_BAR3`
/// windows; this event re-reads them so a metal capture can compare our
/// inbound translation against the working-Linux
/// `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000` rather than guessing the
/// next change (`AGENTS.md` §15.7). A faulting read renders the all-ones
/// sentinel and is never propagated (`AGENTS.md` §2.9). One-shot at
/// bring-up, never on the poll path (`AGENTS.md` §2.16 / §19.4); fields
/// rendered on the stack (`AGENTS.md` §2.9); part of the audit contract
/// (`AGENTS.md` §5.4.4).
const USB_KEYBOARD_INBOUND: EventId = EventId(4119);

/// Stable, allocation-free name for a [`DriverError`], for logging the
/// stage a bring-up failed at without rendering a bare number
/// (`AGENTS.md` §2.9 — the log path never allocates).
const fn driver_error_name(err: DriverError) -> &'static str {
    match err {
        DriverError::BufferTooSmall => "buffer_too_small",
        DriverError::BadMagic => "bad_magic",
        DriverError::AbiVersionUnsupported => "abi_version_unsupported",
        DriverError::LengthOutOfRange => "length_out_of_range",
        DriverError::OutOfRange => "out_of_range",
        DriverError::PermissionDenied => "permission_denied",
        DriverError::NotFound => "not_found",
        DriverError::SignatureInvalid => "signature_invalid",
        DriverError::Unsupported => "unsupported",
        DriverError::DeviceFault => "device_fault",
        DriverError::Busy => "busy",
        DriverError::NotImplemented => "not_implemented",
        DriverError::NoSpace => "no_space",
        // `DriverError` is `#[non_exhaustive]`: a future variant logs as
        // `unknown` rather than failing the build (`AGENTS.md` §2.9).
        _ => "unknown",
    }
}

/// Log a bring-up stage milestone with no extra fields (`Info`).
fn log_stage(sink: &dyn Sink, message: &'static str) {
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_BRINGUP,
            message,
            fields: &[],
        },
    );
}

/// Log a bring-up stage *failure* with the failing [`DriverError`]
/// (`Error`), so a metal capture pins which stage refused and why.
fn log_stage_err(sink: &dyn Sink, message: &'static str, err: DriverError) {
    log(
        sink,
        &Event {
            level: Level::Error,
            id: USB_KEYBOARD_BRINGUP,
            message,
            fields: &[Field {
                key: "err",
                value: driver_error_name(err),
            }],
        },
    );
}

fn option_hex(value: Option<u32>, buf: &mut [u8; 16]) -> &str {
    match value {
        Some(value) => format_hex_u64(u64::from(value), buf),
        None => "unreadable",
    }
}

fn log_xhci_open_err(sink: &dyn Sink, err: XhciOpenError) {
    let mut cmd_buf = [0u8; 16];
    let mut status_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Error,
            id: USB_KEYBOARD_BRINGUP,
            message: "usb-keyboard: vl805 xhci controller open (capability/reset) failed",
            fields: &[
                Field {
                    key: "err",
                    value: driver_error_name(err.error),
                },
                Field {
                    key: "stage",
                    value: err.stage.as_str(),
                },
                Field {
                    key: "usbcmd_hex",
                    value: option_hex(err.registers.usbcmd, &mut cmd_buf),
                },
                Field {
                    key: "usbsts_hex",
                    value: option_hex(err.registers.usbsts, &mut status_buf),
                },
            ],
        },
    );
}

/// Upper bound on functions the one-shot diagnostic scan reports.
///
/// A defence bound (`AGENTS.md` §24.4), not a capacity: the Pi 4 root
/// complex carries exactly two functions (the bridge and the VL805), so
/// this comfortably covers a healthy bus while bounding the log a
/// malfunctioning controller could otherwise drive.
const SCAN_REPORT_LIMIT: usize = 32;

/// Enumerate the PCIe configuration space once and log every responding
/// function, so a metal capture shows whether the VL805 is answering
/// configuration reads at all before the bring-up tries to claim it.
///
/// This is purely diagnostic: it runs once at bring-up, never on the
/// per-report poll path (`AGENTS.md` §2.16 / §19.4), renders its fields
/// on the stack with no allocation (`AGENTS.md` §2.9), and an
/// enumeration error is itself logged rather than propagated — the
/// authoritative controller search is `open_discovered`, which the
/// caller runs next and whose `NotFound` is the real failure
/// (`AGENTS.md` §5.4).
fn log_bus_scan(sink: &dyn Sink, bus: &dyn Bus) {
    let mut devices = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; SCAN_REPORT_LIMIT];
    let found = match bus.enumerate(&mut devices) {
        Ok(n) => n,
        // The bus filled the buffer before reporting the overflow; report
        // the populated prefix rather than dropping the whole scan.
        Err(DriverError::BufferTooSmall) => devices.len(),
        Err(err) => {
            log_stage_err(sink, "usb-keyboard: pcie configuration scan faulted", err);
            return;
        }
    };
    let mut count_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_PCI_SCAN,
            message: "usb-keyboard: pcie configuration scan complete",
            fields: &[Field {
                key: "function_count_hex",
                value: format_hex_u64(found as u64, &mut count_buf),
            }],
        },
    );
    for device in &devices[..found] {
        let mut bdf_buf = [0u8; 16];
        let mut vendor_buf = [0u8; 16];
        let mut device_buf = [0u8; 16];
        let mut class_buf = [0u8; 16];
        log(
            sink,
            &Event {
                level: Level::Info,
                id: USB_KEYBOARD_PCI_SCAN,
                message: "usb-keyboard: pcie function discovered",
                fields: &[
                    Field {
                        key: "bdf_hex",
                        value: format_hex_u64(device.address, &mut bdf_buf),
                    },
                    Field {
                        key: "vendor_hex",
                        value: format_hex_u64(u64::from(device.vendor), &mut vendor_buf),
                    },
                    Field {
                        key: "device_hex",
                        value: format_hex_u64(u64::from(device.device), &mut device_buf),
                    },
                    Field {
                        key: "class_hex",
                        value: format_hex_u64(u64::from(device.class), &mut class_buf),
                    },
                ],
            },
        );
    }
}

/// Log the device-shared DMA carve the bring-up will program into the
/// controller, against the inbound-aperture bound it must lie below.
///
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); fields rendered on the stack, no allocation (`AGENTS.md`
/// §2.9). `dma_phys` is the **device-visible** (PCIe-space) base the
/// controller's descriptors carry.
fn log_dma_carve(sink: &dyn Sink, dma: &DmaSlab, dma_aperture_top: u64, window_len: usize) {
    let mut phys_buf = [0u8; 16];
    let mut len_buf = [0u8; 16];
    let mut end_buf = [0u8; 16];
    let mut top_buf = [0u8; 16];
    let mut win_buf = [0u8; 16];
    let dma_phys = dma.phys();
    let dma_len = dma.len() as u64;
    let dma_end = dma_phys.saturating_add(dma_len);
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_GEOMETRY,
            message: "usb-keyboard: xhci dma carve and bar window mapped",
            fields: &[
                Field {
                    key: "dma_phys_hex",
                    value: format_hex_u64(dma_phys, &mut phys_buf),
                },
                Field {
                    key: "dma_len_hex",
                    value: format_hex_u64(dma_len, &mut len_buf),
                },
                Field {
                    key: "dma_end_hex",
                    value: format_hex_u64(dma_end, &mut end_buf),
                },
                Field {
                    key: "dma_aperture_top_hex",
                    value: format_hex_u64(dma_aperture_top, &mut top_buf),
                },
                Field {
                    key: "bar_window_len_hex",
                    value: format_hex_u64(window_len as u64, &mut win_buf),
                },
            ],
        },
    );
}

/// Log the controller's capability-block geometry read by [`Xhci::open`]
/// (the offsets [`UsbDevice::start`] then programs through), so a metal
/// capture shows whether a register offset lands past the mapped BAR
/// window.
///
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); fields rendered on the stack, no allocation (`AGENTS.md`
/// §2.9).
fn log_xhci_geometry(sink: &dyn Sink, xhci: &Xhci<RegisterWindow>) {
    let mut cap_buf = [0u8; 16];
    let mut ver_buf = [0u8; 16];
    let mut db_buf = [0u8; 16];
    let mut rt_buf = [0u8; 16];
    let mut slots_buf = [0u8; 16];
    let mut ports_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_GEOMETRY,
            message: "usb-keyboard: xhci capability block read",
            fields: &[
                Field {
                    key: "caplength_hex",
                    value: format_hex_u64(xhci.caplength() as u64, &mut cap_buf),
                },
                Field {
                    key: "hci_version_hex",
                    value: format_hex_u64(u64::from(xhci.hci_version()), &mut ver_buf),
                },
                Field {
                    key: "doorbell_off_hex",
                    value: format_hex_u64(xhci.doorbell_base() as u64, &mut db_buf),
                },
                Field {
                    key: "runtime_off_hex",
                    value: format_hex_u64(xhci.runtime_base() as u64, &mut rt_buf),
                },
                Field {
                    key: "max_slots_hex",
                    value: format_hex_u64(u64::from(xhci.max_slots()), &mut slots_buf),
                },
                Field {
                    key: "max_ports_hex",
                    value: format_hex_u64(u64::from(xhci.max_ports()), &mut ports_buf),
                },
            ],
        },
    );
}

/// Read one capability-register dword raw from the mapped BAR window,
/// rendering the value read, or the `ffff_ffff_ffff_ffff` sentinel if
/// the window itself refused the (bounds/alignment-checked) read.
fn read_cap_dword(window: &RegisterWindow, offset: usize) -> u64 {
    window.read_u32(offset).map_or(u64::MAX, u64::from)
}

/// Maximum *elapsed wall time*, in microseconds, to wait for the VL805 to
/// present its register block after the firmware-version wait (~256 ms).
///
/// A defence bound (`AGENTS.md` §2.1 / §24.4), not a tunable capacity:
/// the controller's internal core boots in well under this budget, and a
/// controller that never decodes must fail closed rather than spin
/// forever.
///
/// This is a *time* budget, deliberately not a poll count: each read of an
/// un-decoded BAR master-aborts, and on the BCM2711 each such access
/// stalls for tens of milliseconds (the metal `4116` capture measured
/// ~54 ms per read). A fixed 256-poll budget therefore inflated the
/// intended ~256 ms wait into ~14 s of real time (256 × ~55 ms). Bounding
/// by [`Delay::now_us`] caps the *wall* duration regardless of how slow
/// each read is (`AGENTS.md` §2.16 — a poll-count budget silently assumes
/// cheap reads).
const CAPS_READY_BUDGET_US: u64 = 256_000;

/// Delay between capability-header readiness polls, in microseconds.
const CAPS_READY_POLL_INTERVAL_US: u32 = 1_000;

/// Maximum *elapsed wall time*, in microseconds, to wait for the VL805's
/// XHCI MCU firmware version ([`VL805_FW_VERSION_OFFSET`]) to read
/// non-zero after the link trains (~2 s).
///
/// A defence bound (`AGENTS.md` §2.1 / §24.4), not a tunable capacity:
/// on a healthy Pi 4 the boot firmware leaves or restores the VL805
/// firmware before RustOS touches the BAR, and the version reads non-zero
/// within a few hundred milliseconds (Linux sees the controller live ~0.3 s
/// after link-up), so 2 s is generous headroom while a board that never
/// loads still fails closed rather than spinning. Unlike
/// [`CAPS_READY_BUDGET_US`] these are *configuration-space* reads, which
/// complete promptly on metal (no master-abort), so the budget is set by
/// the firmware-load latency, not the read cost.
const FW_LOADED_BUDGET_US: u64 = 2_000_000;

/// Delay between firmware-version readiness polls, in microseconds.
const FW_LOADED_POLL_INTERVAL_US: u32 = 2_000;

/// The VL805's PCI bus/device/function address as the configuration
/// accessor keys it: the lone device on the secondary bus the
/// root-complex bring-up named.
fn vl805_bdf() -> u64 {
    u64::from(pcie_brcm::regs::RC_SECONDARY_BUS) << 16
}

/// Whether the `CAPLENGTH`/`HCIVERSION` header dword looks like a live
/// xHCI controller presenting its capability block, rather than an
/// uninitialised or aborted bus pattern.
///
/// A live header carries a plausible `CAPLENGTH` (the operational-register
/// offset, at least `0x20` since the capability registers occupy that
/// much) in its low byte and a plausible `HCIVERSION` (xHCI 0.96‥1.2) in
/// its high half-word. The pre-firmware patterns the metal capture showed
/// — `0` (unpowered), the all-ones UR sentinel (and the
/// [`read_cap_dword`] refused-read sentinel `u64::MAX`), and the BCM2711
/// `dead_dead` poison (`HCIVERSION` `0xdead`) — all fail this test, so a
/// `true` result means the controller is actually decoding. Takes the
/// [`read_cap_dword`] value (a `u64` carrying a 32-bit register or the
/// all-ones sentinel) directly, so there is no truncating cast.
fn caps_block_is_live(header: u64) -> bool {
    if header == u64::MAX {
        return false;
    }
    let caplength = header & 0xFF;
    let hci_version = (header >> 16) & 0xFFFF;
    caplength >= 0x20 && (0x0090..=0x0120).contains(&hci_version)
}

/// Poll the mapped BAR's `CAPLENGTH`/`HCIVERSION` header until the
/// controller presents a live capability block, bounded by
/// [`CAPS_READY_BUDGET_US`] of *elapsed wall time*, and log the outcome
/// once (`4109`).
///
/// The VL805's internal xHCI core boots only once firmware is present;
/// until it does, the header reads back the `dead_dead`/UR/zero patterns
/// [`caps_block_is_live`] rejects. This gives that boot a bounded window before [`Xhci::open`]
/// interprets the registers, turning "the controller just needed time"
/// into a clean bring-up while a controller that never decodes still
/// fails closed at `open` (`AGENTS.md` §2.1 bounded / §2.9 fail closed).
///
/// The bound is *wall time* via [`Delay::now_us`], not a poll count,
/// because each read of an un-decoded BAR master-aborts and stalls for
/// tens of milliseconds on the BCM2711 (the `4116` capture): a fixed
/// 256-poll budget stretched the intended ~256 ms wait into ~14 s. The
/// `polls_hex` field still reports how many reads were taken — on metal
/// far fewer than before, since each costs real time the budget now caps
/// (`AGENTS.md` §2.16). `now_us` is read once per iteration so the loop
/// always terminates within ~one poll/read of the budget.
///
/// Returns whether the block became live. One-shot at bring-up, never on
/// the poll path (`AGENTS.md` §2.16 / §19.4); fields rendered on the
/// stack, no allocation (`AGENTS.md` §2.9). Read-only.
fn wait_for_caps_ready(window: &RegisterWindow, delay: &dyn Delay, sink: &dyn Sink) -> bool {
    use rustos_drv_bus_usb::regs;

    let start_us = delay.now_us();
    let mut polls = 0u32;
    let (ready, header) = loop {
        let header = read_cap_dword(window, regs::CAPLENGTH_HCIVERSION);
        if caps_block_is_live(header) {
            break (true, header);
        }
        if delay.now_us().wrapping_sub(start_us) >= CAPS_READY_BUDGET_US {
            break (false, header);
        }
        delay.delay_us(CAPS_READY_POLL_INTERVAL_US);
        polls += 1;
    };

    let mut polls_buf = [0u8; 16];
    let mut header_buf = [0u8; 16];
    let mut ready_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_CAPS_READY,
            message: "usb-keyboard: waited for the xhci capability block to come live",
            fields: &[
                Field {
                    key: "polls_hex",
                    value: format_hex_u64(u64::from(polls), &mut polls_buf),
                },
                Field {
                    key: "caplength_hciversion_hex",
                    value: format_hex_u64(header, &mut header_buf),
                },
                Field {
                    key: "ready_hex",
                    value: format_hex_u64(u64::from(ready), &mut ready_buf),
                },
            ],
        },
    );
    ready
}

/// Whether the VL805 firmware-version dword indicates a loaded MCU
/// firmware: a non-zero build id, and not the faulting-read all-ones
/// sentinel ([`read_config_or_sentinel`]). Reset value is `0` (firmware
/// not loaded); Linux's `rpi_firmware_init_vl805` treats any non-zero
/// value as loaded.
fn firmware_version_is_loaded(version: u64) -> bool {
    version != 0 && version != 0xFFFF_FFFF
}

/// Poll the VL805's XHCI MCU firmware version
/// ([`VL805_FW_VERSION_OFFSET`]) over configuration space until it reads a
/// non-zero build id, bounded by [`FW_LOADED_BUDGET_US`] of *elapsed wall
/// time*, and log the outcome once (`4118`).
///
/// This is the firmware-load readiness signal, read over the
/// configuration path that works on metal — unlike the VL805's MMIO BAR,
/// which master-aborts to the BCM2711 `dead_dead` poison until the MCU
/// firmware is loaded and decoding. RustOS leaves the boot firmware's VL805
/// state alone and uses this bounded wait only to measure whether firmware
/// is resident before the BAR-readiness poll; polling the working register
/// rather than the aborting BAR is exactly how Linux confirms the load
/// (`AGENTS.md` §15.7 — measure, don't guess).
///
/// Returns whether the firmware became loaded. One-shot at bring-up,
/// never on the poll path (`AGENTS.md` §2.16 / §19.4); fields rendered on
/// the stack, no allocation (`AGENTS.md` §2.9). Read-only.
fn wait_for_firmware_loaded(bus: &dyn PciBus, delay: &dyn Delay, sink: &dyn Sink) -> bool {
    let bdf = vl805_bdf();
    let start_us = delay.now_us();
    let mut polls = 0u32;
    let (ready, version) = loop {
        let version = read_config_or_sentinel(bus, bdf, VL805_FW_VERSION_OFFSET);
        if firmware_version_is_loaded(version) {
            break (true, version);
        }
        if delay.now_us().wrapping_sub(start_us) >= FW_LOADED_BUDGET_US {
            break (false, version);
        }
        delay.delay_us(FW_LOADED_POLL_INTERVAL_US);
        polls += 1;
    };

    let mut polls_buf = [0u8; 16];
    let mut version_buf = [0u8; 16];
    let mut ready_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_FW_READY,
            message: "usb-keyboard: waited for the vl805 xhci mcu firmware version to load",
            fields: &[
                Field {
                    key: "polls_hex",
                    value: format_hex_u64(u64::from(polls), &mut polls_buf),
                },
                Field {
                    key: "fw_version_hex",
                    value: format_hex_u64(version, &mut version_buf),
                },
                Field {
                    key: "ready_hex",
                    value: format_hex_u64(u64::from(ready), &mut ready_buf),
                },
            ],
        },
    );
    ready
}

fn log_firmware_response(sink: &dyn Sink, response_value: u32) {
    let mut dev_buf = [0u8; 16];
    let mut response_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_FW_RESPONSE,
            message: "usb-keyboard: vl805 firmware reset mailbox response",
            fields: &[
                Field {
                    key: "dev_addr_hex",
                    value: format_hex_u64(u64::from(VL805_FIRMWARE_DEV_ADDR), &mut dev_buf),
                },
                Field {
                    key: "response_value_hex",
                    value: format_hex_u64(u64::from(response_value), &mut response_buf),
                },
            ],
        },
    );
}

fn log_firmware_reset(sink: &dyn Sink, outcome: FirmwareResetOutcome) {
    match outcome {
        FirmwareResetOutcome::NotAvailable => {
            log(
                sink,
                &Event {
                    level: Level::Info,
                    id: USB_KEYBOARD_FW_RESET,
                    message:
                        "usb-keyboard: vl805 firmware reload skipped because no videocore mailbox is available",
                    fields: &[],
                },
            );
        }
        FirmwareResetOutcome::Reloaded { .. } => {
            log(
                sink,
                &Event {
                    level: Level::Info,
                    id: USB_KEYBOARD_FW_RESET,
                    message: "usb-keyboard: vl805 firmware reloaded via the videocore mailbox",
                    fields: &[],
                },
            );
        }
        FirmwareResetOutcome::Failed { reason } => {
            let fields = [Field {
                key: "reason",
                value: reason.as_str(),
            }];
            log(
                sink,
                &Event {
                    level: Level::Error,
                    id: USB_KEYBOARD_FW_RESET,
                    message: "usb-keyboard: vl805 firmware reload via the videocore mailbox failed",
                    fields: &fields,
                },
            );
        }
    }
}

fn ensure_firmware_loaded(
    bus: &dyn PciBus,
    delay: &dyn Delay,
    firmware_reset: &dyn FirmwareReset,
    sink: &dyn Sink,
) -> bool {
    if wait_for_firmware_loaded(bus, delay, sink) {
        return true;
    }
    let outcome = firmware_reset.reload();
    if let FirmwareResetOutcome::Reloaded { response_value } = outcome {
        log_firmware_response(sink, response_value);
    }
    log_firmware_reset(sink, outcome);
    if !matches!(outcome, FirmwareResetOutcome::Reloaded { .. }) {
        return false;
    }
    delay.delay_us(FW_RELOAD_SETTLE_US);
    wait_for_firmware_loaded(bus, delay, sink)
}

/// Dump the first capability-register dwords straight off the mapped
/// VL805 BAR window, one-shot, before [`Xhci::open`] interprets them.
///
/// [`Xhci::open`] failed `out_of_range` *after* the BAR mapped, which can
/// only be a [`RegisterWindow`] bounds/alignment refusal on the
/// operational base it derives from `CAPLENGTH` (the offsets `open` reads
/// are otherwise tiny and 4-aligned). Logging the raw `CAPLENGTH`/
/// `HCIVERSION` dword (and the neighbouring capability dwords) shows
/// whether the BAR decodes at all and the exact `CAPLENGTH` byte, so the
/// next metal capture pins the concrete value (`AGENTS.md` §15.7 —
/// measure, don't guess).
///
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); fields rendered on the stack, no allocation (`AGENTS.md`
/// §2.9). Read-only: it never writes a register.
fn log_raw_caps(sink: &dyn Sink, window: &RegisterWindow) {
    use rustos_drv_bus_usb::regs;

    let mut header_buf = [0u8; 16];
    let mut structural_buf = [0u8; 16];
    let mut capability_buf = [0u8; 16];
    let mut doorbell_buf = [0u8; 16];
    let mut runtime_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_CAPS_RAW,
            message: "usb-keyboard: xhci capability registers raw",
            fields: &[
                Field {
                    key: "caplength_hciversion_hex",
                    value: format_hex_u64(
                        read_cap_dword(window, regs::CAPLENGTH_HCIVERSION),
                        &mut header_buf,
                    ),
                },
                Field {
                    key: "hcsparams1_hex",
                    value: format_hex_u64(
                        read_cap_dword(window, regs::HCSPARAMS1),
                        &mut structural_buf,
                    ),
                },
                Field {
                    key: "hccparams1_hex",
                    value: format_hex_u64(
                        read_cap_dword(window, regs::HCCPARAMS1),
                        &mut capability_buf,
                    ),
                },
                Field {
                    key: "dboff_hex",
                    value: format_hex_u64(read_cap_dword(window, regs::DBOFF), &mut doorbell_buf),
                },
                Field {
                    key: "rtsoff_hex",
                    value: format_hex_u64(read_cap_dword(window, regs::RTSOFF), &mut runtime_buf),
                },
            ],
        },
    );
}

/// Log the controller's outbound (CPU→PCIe) memory-window registers and
/// link status back from the trained register block, one-shot.
///
/// The mapped BAR returns the BCM2711 `dead_dead` master-abort poison
/// even though configuration reads succeed and every PCI-config register
/// reads back what bring-up wrote (`4110`). Configuration and memory take
/// different paths through the controller — configuration through the
/// internal `EXT_CFG` window, memory through the CPU→PCIe outbound
/// translation window — so this reads the outbound-window registers back
/// (the raw `MEM_WIN0_LO`/`HI`, `BASE_LIMIT`, `BASE_HI`, `LIMIT_HI`) plus
/// the link `STATUS`, to show whether the window holds the programmed
/// bases and whether the link reports up (`AGENTS.md` §15.7).
///
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); fields rendered on the stack, no allocation (`AGENTS.md`
/// §2.9). The values are produced fail-closed by
/// [`pcie_brcm::BrcmPcieRc::outbound_window_readback`].
fn log_outbound_window(sink: &dyn Sink, rb: OutboundWindowReadback) {
    let mut lo_buf = [0u8; 16];
    let mut hi_buf = [0u8; 16];
    let mut bl_buf = [0u8; 16];
    let mut bhi_buf = [0u8; 16];
    let mut lhi_buf = [0u8; 16];
    let mut status_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_OUTBOUND,
            message: "usb-keyboard: pcie outbound (cpu->pcie) window read-back",
            fields: &[
                Field {
                    key: "mem_win0_lo_hex",
                    value: format_hex_u64(u64::from(rb.mem_win0_lo), &mut lo_buf),
                },
                Field {
                    key: "mem_win0_hi_hex",
                    value: format_hex_u64(u64::from(rb.mem_win0_hi), &mut hi_buf),
                },
                Field {
                    key: "mem_win0_base_limit_hex",
                    value: format_hex_u64(u64::from(rb.mem_win0_base_limit), &mut bl_buf),
                },
                Field {
                    key: "mem_win0_base_hi_hex",
                    value: format_hex_u64(u64::from(rb.mem_win0_base_hi), &mut bhi_buf),
                },
                Field {
                    key: "mem_win0_limit_hi_hex",
                    value: format_hex_u64(u64::from(rb.mem_win0_limit_hi), &mut lhi_buf),
                },
                Field {
                    key: "pcie_status_hex",
                    value: format_hex_u64(u64::from(rb.pcie_status), &mut status_buf),
                },
            ],
        },
    );
}

/// Log the controller's inbound (PCIe→system-memory) viewport registers
/// back from the trained register block, one-shot (`4119`).
///
/// On the Raspberry Pi 4 the `VideoCore` VL805 firmware handoff may depend
/// on the inbound DMA window, so this reads the inbound viewport registers
/// back (`RC_BAR1_LO`, `RC_BAR2_LO`/`HI`, `RC_BAR3_LO`) plus the link
/// `STATUS`; a metal capture can compare our translation with the
/// working-Linux `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`
/// (`AGENTS.md` §15.7).
///
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); fields rendered on the stack, no allocation (`AGENTS.md`
/// §2.9). The values are produced fail-closed by
/// [`pcie_brcm::BrcmPcieRc::inbound_window_readback`].
fn log_inbound_window(sink: &dyn Sink, rb: InboundWindowReadback) {
    let mut bar1_buf = [0u8; 16];
    let mut bar2lo_buf = [0u8; 16];
    let mut bar2hi_buf = [0u8; 16];
    let mut bar3_buf = [0u8; 16];
    let mut status_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_INBOUND,
            message: "usb-keyboard: pcie inbound (pcie->memory) viewport read-back",
            fields: &[
                Field {
                    key: "rc_bar1_lo_hex",
                    value: format_hex_u64(u64::from(rb.rc_bar1_lo), &mut bar1_buf),
                },
                Field {
                    key: "rc_bar2_lo_hex",
                    value: format_hex_u64(u64::from(rb.rc_bar2_lo), &mut bar2lo_buf),
                },
                Field {
                    key: "rc_bar2_hi_hex",
                    value: format_hex_u64(u64::from(rb.rc_bar2_hi), &mut bar2hi_buf),
                },
                Field {
                    key: "rc_bar3_lo_hex",
                    value: format_hex_u64(u64::from(rb.rc_bar3_lo), &mut bar3_buf),
                },
                Field {
                    key: "pcie_status_hex",
                    value: format_hex_u64(u64::from(rb.pcie_status), &mut status_buf),
                },
            ],
        },
    );
}

/// Log the PCIe root-complex bring-up's per-phase wall-time split
/// (`4117`), so a metal capture localises a multi-second bring-up to the
/// exact MMIO access rather than guessing which register stalls
/// (`AGENTS.md` §15.7). The split pinned the ~10.8 s on the **first MISC
/// access**: at OS entry the controller core is held off until the
/// always-accessible RGR1 bridge `sw_init` reset (`0x9210`) is cycled, so
/// touching MISC first master-aborts. The bring-up now cycles the bridge
/// reset before touching MISC (matching U-Boot/Linux); the split is
/// retained to localise any residual stall to the exact MMIO group. The
/// fields are `reset_swinit_us` (releasing the `RGR1_SW_INIT_1` bridge
/// `sw_init` the previous boot stage left asserted), `reset_settle_us`
/// (the post-de-reset MISC settle — the gentlest no-touch-probe bring-up
/// does **not** toggle the SerDes `IDDQ` or re-assert a fundamental
/// reset, and `train_link` deasserts the already-asserted `PERST#` as the
/// single firmware-(re)load edge), `config_us`,
/// `linkwait_us`, and `link_polls`. The `*_us`
/// spans sum to the whole `bring_up`, so any one carrying seconds names
/// the stalling access. `entry_rgr1_sw_init_hex` is the raw
/// `RGR1_SW_INIT_1` reset register sampled at bring-up entry, before the
/// reset cycles it: a set `PERST#` bit (`bit 0`) means the previous boot
/// stage already held the VL805 in fundamental reset at OS entry (its
/// bootloader-loaded firmware dropped before any RustOS code ran), while
/// a clear bit means the firmware should still be resident — the decisive
/// datapoint for whether the persistent `dead_dead`/`fw_version=0` is a
/// firmware we drop or a firmware never present at entry (`AGENTS.md`
/// §15.7).
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); rendered on the stack (`AGENTS.md` §2.9).
fn log_bring_up_timing(sink: &dyn Sink, timing: BringUpTiming) {
    let mut swinit_buf = [0u8; 16];
    let mut settle_buf = [0u8; 16];
    let mut config_buf = [0u8; 16];
    let mut linkwait_buf = [0u8; 16];
    let mut polls_buf = [0u8; 16];
    let mut rgr1_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_BRINGUP_TIMING,
            message: "usb-keyboard: pcie bring-up per-phase timing",
            fields: &[
                Field {
                    key: "reset_swinit_us_hex",
                    value: format_hex_u64(timing.reset_swinit_us, &mut swinit_buf),
                },
                Field {
                    key: "reset_settle_us_hex",
                    value: format_hex_u64(timing.reset_settle_us, &mut settle_buf),
                },
                Field {
                    key: "config_us_hex",
                    value: format_hex_u64(timing.config_us, &mut config_buf),
                },
                Field {
                    key: "linkwait_us_hex",
                    value: format_hex_u64(timing.linkwait_us, &mut linkwait_buf),
                },
                Field {
                    key: "link_polls_hex",
                    value: format_hex_u64(u64::from(timing.link_polls), &mut polls_buf),
                },
                Field {
                    key: "entry_rgr1_sw_init_hex",
                    value: format_hex_u64(u64::from(timing.entry_rgr1_sw_init), &mut rgr1_buf),
                },
            ],
        },
    );
}

/// The VL805's **XHCI MCU Firmware Version** PCI configuration-space
/// register (offset `0x50`, read-only, reset value `0`).
///
/// This is the register Linux's `rpi_firmware_init_vl805` reads to decide
/// whether the controller's firmware is loaded: it is `0` while the MCU
/// has no firmware and a non-zero build id (e.g. `0x0001_38c0`) once the
/// `VideoCore` / EEPROM handoff has loaded the blob.
/// Crucially it lives in **configuration space**, which is reachable on
/// metal (the `4104` scan reads vendor/device/class fine), unlike the
/// VL805's MMIO BAR, which returns the BCM2711 `dead_dead` master-abort
/// poison until the controller decodes — so reading `0x50` measures the
/// firmware-load outcome directly, without depending on the BAR window
/// (`AGENTS.md` §15.7 — measure, don't guess).
const VL805_FW_VERSION_OFFSET: u16 = 0x50;

/// Read a configuration-space dword back, rendering a faulting read as
/// the all-ones sentinel: the readback is diagnostic, never propagated
/// (`AGENTS.md` §2.9).
fn read_config_or_sentinel(bus: &dyn PciBus, bdf: u64, offset: u16) -> u64 {
    u64::from(bus.read_config(bdf, offset).unwrap_or(0xFFFF_FFFF))
}

/// Read configuration space back after the BAR is assigned and the
/// command register enabled, one-shot, before [`Xhci::open`].
///
/// The mapped BAR returns the BCM2711 `dead_dead` master-abort poison
/// even though configuration reads succeed, while the controller/bridge
/// programming chain is all present in code. This reads each programmed
/// register *back* — the root port's bus numbers (`0x18`), Memory
/// Base/Limit (`0x20`) and command/status (`0x04`), and the VL805's
/// command/status (`0x04`), BAR0 (`0x10`) and BAR1 (`0x14`), plus the
/// VL805's XHCI MCU firmware version ([`VL805_FW_VERSION_OFFSET`],
/// `0x50`) — so a metal capture shows which write actually stuck and
/// whether the fault is a configuration write that did not take or a
/// controller that does not decode despite correct programming
/// (`AGENTS.md` §15.7). Captured here after the link trains, a non-zero
/// firmware version means the boot chain left or restored the VL805 firmware
/// without RustOS issuing an explicit reload; `0` means the controller still
/// has no firmware and the BAR is expected to stay dark.
///
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); fields rendered on the stack, no allocation (`AGENTS.md`
/// §2.9). Read-only.
fn log_config_readback(sink: &dyn Sink, bus: &dyn PciBus) {
    // The root port presents as the bus-0 bridge; the VL805 is the lone
    // device on the secondary bus the root-complex bring-up named.
    const BRIDGE_BDF: u64 = 0;
    let vl805_bdf = vl805_bdf();

    let mut bus_buf = [0u8; 16];
    let mut mem_buf = [0u8; 16];
    let mut brcmd_buf = [0u8; 16];
    let mut vlcmd_buf = [0u8; 16];
    let mut bar0_buf = [0u8; 16];
    let mut bar1_buf = [0u8; 16];
    let mut fwver_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_CONFIG,
            message: "usb-keyboard: pcie configuration read-back after bar assign + command enable",
            fields: &[
                Field {
                    key: "bridge_bus_numbers_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, BRIDGE_BDF, 0x18),
                        &mut bus_buf,
                    ),
                },
                Field {
                    key: "bridge_mem_base_limit_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, BRIDGE_BDF, 0x20),
                        &mut mem_buf,
                    ),
                },
                Field {
                    key: "bridge_command_status_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, BRIDGE_BDF, 0x04),
                        &mut brcmd_buf,
                    ),
                },
                Field {
                    key: "vl805_command_status_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x04),
                        &mut vlcmd_buf,
                    ),
                },
                Field {
                    key: "vl805_bar0_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x10),
                        &mut bar0_buf,
                    ),
                },
                Field {
                    key: "vl805_bar1_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x14),
                        &mut bar1_buf,
                    ),
                },
                Field {
                    key: "vl805_fw_version_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, VL805_FW_VERSION_OFFSET),
                        &mut fwver_buf,
                    ),
                },
            ],
        },
    );
}

/// Re-read the VL805's configuration space and capability header *after*
/// the firmware-version wait and the bounded readiness settle, one-shot
/// ([`USB_KEYBOARD_POST_RELOAD`]).
///
/// [`log_config_readback`] captures the function before the no-touch
/// firmware-version wait; this captures it after the BAR-readiness settle,
/// so a metal capture shows whether the function stayed present and whether
/// the firmware version changed without RustOS issuing a reload. Comparing
/// the two read-backs distinguishes "the function stayed configured but has
/// no firmware" from "the function is firmware-loaded yet the controller
/// still does not decode" (`AGENTS.md` §15.7 — measure, don't guess).
///
/// The decisive field is `vl805_fw_version_hex` (the XHCI MCU firmware
/// version, [`VL805_FW_VERSION_OFFSET`] `0x50`): this is exactly how
/// Linux's `rpi_firmware_init_vl805` confirms a successful load. A
/// non-zero version proves the boot firmware has made the VL805 firmware
/// resident (so a still-`dead_dead` BAR is a memory-window/decode fault,
/// not a load failure), while `0` proves RustOS should leave the controller
/// untouched and fail closed rather than attempting a destructive redundant
/// reload. It is read over configuration space, which works on metal even
/// while the BAR aborts.
///
/// One-shot at bring-up, never on the poll path (`AGENTS.md` §2.16 /
/// §19.4); fields rendered on the stack, no allocation (`AGENTS.md`
/// §2.9). Read-only — a faulting read renders the all-ones sentinel and
/// is never propagated.
fn log_post_reload_state(sink: &dyn Sink, bus: &dyn PciBus, window: &RegisterWindow) {
    use rustos_drv_bus_usb::regs;

    let vl805_bdf = vl805_bdf();
    let mut id_buf = [0u8; 16];
    let mut cmd_buf = [0u8; 16];
    let mut bar0_buf = [0u8; 16];
    let mut bar1_buf = [0u8; 16];
    let mut fwver_buf = [0u8; 16];
    let mut cap_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_POST_RELOAD,
            message:
                "usb-keyboard: vl805 config + caps re-read after firmware-version wait + settle",
            fields: &[
                Field {
                    key: "vl805_vendor_device_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x00),
                        &mut id_buf,
                    ),
                },
                Field {
                    key: "vl805_command_status_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x04),
                        &mut cmd_buf,
                    ),
                },
                Field {
                    key: "vl805_bar0_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x10),
                        &mut bar0_buf,
                    ),
                },
                Field {
                    key: "vl805_bar1_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x14),
                        &mut bar1_buf,
                    ),
                },
                Field {
                    key: "vl805_fw_version_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, VL805_FW_VERSION_OFFSET),
                        &mut fwver_buf,
                    ),
                },
                Field {
                    key: "caplength_hciversion_hex",
                    value: format_hex_u64(
                        read_cap_dword(window, regs::CAPLENGTH_HCIVERSION),
                        &mut cap_buf,
                    ),
                },
            ],
        },
    );
}

/// Map and bring up the discovered VL805 xHCI controller on `bus` in
/// stages — map the register BAR and carve DMA
/// ([`map_controller`](rustos_drv_bus_usb::wiring::map_controller)),
/// [`Xhci::open`] (capability block + reset), then [`UsbDevice::start`]
/// (DMA program + run) — logging the carve and capability-block geometry
/// one-shot ([`USB_KEYBOARD_GEOMETRY`]) between the map and the bring-up
/// and reporting each stage's failure distinctly, so a metal
/// `out_of_range` is localised to the concrete value that overran rather
/// than a bare error code (`AGENTS.md` §15.7 — measure, don't guess;
/// §5.4.4 — audit).
///
/// # Errors
///
/// The first failing stage's [`DriverError`], logged at `Error`.
fn open_controller(
    host: &dyn DriverHost,
    bus: &dyn PciBus,
    dma_aperture_top: u64,
    outbound_window: (u64, u64),
    firmware_reset: &dyn FirmwareReset,
    delay: &dyn Delay,
    sink: &dyn Sink,
) -> Result<UsbDevice<RegisterWindow, DmaSlab>, DriverError> {
    let mapped = match rustos_drv_bus_usb::wiring::map_controller(
        host,
        bus,
        dma_aperture_top,
        outbound_window,
    ) {
        Ok(mapped) => mapped,
        Err(err) => {
            log_stage_err(
                sink,
                "usb-keyboard: mapping the vl805 controller (discovery/dma/bar) failed",
                err,
            );
            return Err(err);
        }
    };
    log_dma_carve(sink, &mapped.dma, dma_aperture_top, mapped.window.len());
    // Read configuration space back now that the BAR is assigned and the
    // command register enabled: a metal capture then shows which write
    // stuck (bridge bus numbers / memory window / command, VL805 command
    // / BARs) and whether a register failed to program or the controller
    // simply does not decode despite correct setup (`AGENTS.md` §15.7).
    log_config_readback(sink, bus);
    // Wait for the VL805's MCU firmware over the *working* configuration-space
    // firmware-version register (`0x50`) rather than the master-aborting BAR —
    // the signal Linux's `rpi_firmware_init_vl805` checks. If it stays zero,
    // issue exactly one mailbox fallback now that PCI/BAR setup is complete;
    // a non-zero version skips the reload, so RustOS never double-loads an
    // already resident firmware blob.
    if !ensure_firmware_loaded(bus, delay, firmware_reset, sink) {
        return Err(DriverError::DeviceFault);
    }
    // The firmware should now be loaded, so give the BAR a bounded window to
    // present a live capability block before `Xhci::open` interprets it (the
    // MCU core boots after the firmware load; until it does the header reads
    // the `dead_dead`/UR/zero patterns). Non-fatal — a controller that never
    // decodes fails closed at `Xhci::open` below (`AGENTS.md` §2.1 bounded /
    // §2.9 fail closed).
    wait_for_caps_ready(&mapped.window, delay, sink);
    // Re-read the VL805's config + capability header now, after the
    // readiness settle: compared with the pre-wait `4110` read-back this
    // shows whether the function decoded on its own or stayed dark
    // (`AGENTS.md` §15.7). Diagnostic only.
    log_post_reload_state(sink, bus, &mapped.window);
    log_raw_caps(sink, &mapped.window);
    let xhci = match Xhci::open_diagnostic(mapped.window) {
        Ok(xhci) => xhci,
        Err(err) => {
            log_xhci_open_err(sink, err);
            return Err(err.error);
        }
    };
    log_xhci_geometry(sink, &xhci);
    match UsbDevice::start(xhci, mapped.dma, DEFAULT_POLL_BUDGET) {
        Ok(usb) => Ok(usb),
        Err(err) => {
            log_stage_err(
                sink,
                "usb-keyboard: vl805 xhci controller start (dma program/run) failed",
                err,
            );
            Err(err)
        }
    }
}

/// The enumerated boot keyboard the bring-up chain yields: a
/// [`BootKeyboard`] decoding reports out of the started [`UsbDevice`]
/// (the xHCI controller over its mapped register window + DMA region).
pub type KeyboardChain = BootKeyboard<UsbDevice<RegisterWindow, DmaSlab>>;

/// The discovered inputs the VL805 bring-up needs, all read from the
/// `brcm,bcm2711-pcie` [`HwNode`] (`AGENTS.md` §18.1) — never compiled-in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PcieBringup {
    /// CPU-physical base of the PCIe controller register block (the
    /// translated `reg` MMIO window).
    pub regs_phys: u64,
    /// The inbound (`dma-ranges`) and outbound (`ranges`) address windows
    /// the root complex is programmed with. The device-visible exclusive
    /// upper bound the xHCI DMA carve must lie below
    /// (`inbound_pcie_base + inbound_size`) is derived from these in
    /// [`bring_up_keyboard`], so it is not stored separately (`AGENTS.md`
    /// §2.2 — one definition).
    pub windows: PcieWindows,
}

/// Why a `brcm,bcm2711-pcie` [`HwNode`] could not be turned into a
/// [`PcieBringup`]: a required discovered resource is absent. Each is a
/// fail-closed refusal — the chain never invents a window (`AGENTS.md`
/// §2.9 / §18.5).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BringupError {
    /// The node carries no controller register (`Mmio`) window.
    NoControllerWindow,
    /// The node carries no inbound-DMA aperture (`Dma`) resource.
    NoInboundAperture,
    /// The node carries no outbound (`BusWindow`) resource.
    NoOutboundWindow,
}

/// Assemble the VL805 bring-up inputs from a discovered
/// `brcm,bcm2711-pcie` [`HwNode`].
///
/// The node carries three resources the chain needs (all discovered by
/// `kernel/arch/aarch64::platform`, `AGENTS.md` §18.1):
///
/// * the controller register window — the first [`Mmio`](HwResourceKind::Mmio)
///   resource, whose base is [`PcieBringup::regs_phys`];
/// * the inbound viewport — the [`Dma`](HwResourceKind::Dma) resource,
///   whose `length` is the viewport size and `translated_base` the
///   PCIe-space base the inbound BAR is programmed at (the device-visible
///   DMA-reachability top `translated_base + length` is derived from these
///   in [`bring_up_keyboard`]); and
/// * the outbound window — the [`BusWindow`](HwResourceKind::BusWindow)
///   resource (`base` CPU aperture, `length` size, `translated_base` the
///   PCIe-space base it maps to).
///
/// # Errors
///
/// A [`BringupError`] naming the first missing resource; the inputs are
/// never partially assembled (`AGENTS.md` §5.4).
pub fn pcie_bringup_from_node(node: &HwNode) -> Result<PcieBringup, BringupError> {
    let resources = node.resources();
    let find = |kind| resources.iter().find(|r| r.kind() == Some(kind));

    let regs = find(HwResourceKind::Mmio).ok_or(BringupError::NoControllerWindow)?;
    let inbound = find(HwResourceKind::Dma).ok_or(BringupError::NoInboundAperture)?;
    let outbound = find(HwResourceKind::BusWindow).ok_or(BringupError::NoOutboundWindow)?;

    Ok(PcieBringup {
        regs_phys: regs.base(),
        windows: PcieWindows {
            inbound_pcie_base: inbound.translated_base(),
            inbound_size: inbound.length(),
            outbound_cpu_base: outbound.base(),
            outbound_pcie_base: outbound.translated_base(),
            outbound_size: outbound.length(),
        },
    })
}

/// A [`DriverHost`] view assembled for the in-kernel VL805 chain: the
/// capabilities the bus-driver task holds plus the kernel's
/// capability-gated MMIO mapper and per-driver DMA host.
///
/// The bring-up driver crates consume the host only through this trait, so
/// it cannot widen its own authority (`AGENTS.md` §4 / §8): every
/// [`MmioMapper::map_window`] and [`VirtioHost::alloc_dma_zeroed`] call is
/// re-checked kernel-side against the same capabilities (`AGENTS.md`
/// §5.4). The view borrows the mapper and DMA host for `'a`; the kernel
/// reclaims every window and DMA pool when they are torn down at unload.
pub struct ChainHost<'a> {
    capabilities: CapabilitySet,
    mmio: &'a dyn MmioMapper,
    dma: &'a dyn VirtioHost,
}

impl<'a> ChainHost<'a> {
    /// Build the view over the bus-driver task's `capabilities` and the
    /// kernel's `mmio` mapper and `dma` host.
    #[must_use]
    pub fn new(
        capabilities: CapabilitySet,
        mmio: &'a dyn MmioMapper,
        dma: &'a dyn VirtioHost,
    ) -> Self {
        Self {
            capabilities,
            mmio,
            dma,
        }
    }
}

impl DriverHost for ChainHost<'_> {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.capabilities.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        // The composition runs inside the kernel image, so the chain's
        // drivers observe an in-kernel host.
        DriverKind::InKernel
    }

    fn virtio_host(&self) -> Option<&dyn VirtioHost> {
        Some(self.dma)
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(self.mmio)
    }
}

/// A [`ConsoleSink`] that injects produced keyboard records into the kernel
/// input-focus arbiter (`AGENTS.md` §17.4 / §20, `plans/PI.md` P11).
///
/// The HID producer emits one [`KeyInput`] record per key edge; this sink is
/// the in-kernel counterpart of the `key_inject` syscall, handing each record
/// straight to the arbiter rather than crossing the user/kernel boundary (the
/// keyboard driver runs in-kernel on the Pi, `AGENTS.md` §8). The arbiter then
/// decides the encoding and destination by who holds input focus: with the
/// text console foreground a press is encoded to the video console's tty bytes
/// (drained by the login reading that console), and with the desktop
/// foreground the whole record is routed to the kernel keyboard channel. The
/// arbiter never blocks (a full bounded sink drops the oldest/overflow,
/// `AGENTS.md` §2.1).
pub struct ArbiterConsoleSink<'a> {
    focus: &'a InputFocus,
}

impl<'a> ArbiterConsoleSink<'a> {
    /// Build a sink delivering to the input-focus arbiter `focus`.
    #[must_use]
    pub fn new(focus: &'a InputFocus) -> Self {
        Self { focus }
    }
}

impl ConsoleSink for ArbiterConsoleSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        // The producer always writes exactly one whole record. Decode it
        // fail-closed and hand it to the arbiter; a malformed record or a
        // fail-closed sink (a build with no injectable text console) surfaces
        // as a `DeviceFault` rather than dropping input silently
        // (`AGENTS.md` §2.9).
        let record = KeyInput::from_bytes(bytes).map_err(|_| DriverError::DeviceFault)?;
        self.focus
            .inject(record)
            .map(|_| ())
            .map_err(|_| DriverError::DeviceFault)
    }
}

/// Bring the VL805 keyboard online over `host`, from the discovered
/// `bringup` inputs, using `delay` for the link bring-up's timed waits.
///
/// Runs the full chain: train the BCM2711 root-complex link
/// ([`pcie_brcm::wiring::open_discovered`]), build the windowed PCI config
/// accessor over the same register window
/// ([`rustos_drv_bus_pci::mechanism_brcm`]), bring the VL805 xHCI up
/// ([`rustos_drv_bus_usb::wiring::open_discovered`]), and enumerate the
/// first connected root-hub port as a boot keyboard. The returned
/// [`KeyboardChain`] is then polled with [`rustos_drv_input_usb_hid::pump_once`]
/// in the driver's service loop, feeding each produced [`KeyInput`] record to
/// an [`ArbiterConsoleSink`].
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if `host` did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * Any error of the link bring-up (the controller is not a root port or
///   the link never trains), the VL805 bring-up (no USB function, a DMA
///   carve above the aperture, a mapping failure), or the enumeration
///   ([`DriverError::NotFound`] for an empty root hub). Every failure is
///   fail-closed (`AGENTS.md` §5.4); nothing is left half-configured.
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (the register windows and the BAR)
/// and the host's DMA capability (the xHCI DMA carve), both re-checked
/// kernel-side at each map/allocation (`AGENTS.md` §5.4).
///
/// # Logging
///
/// Each stage (PCIe link training, the full PCIe configuration scan, xHCI
/// controller bring-up, root-hub enumeration) is logged to `log` on
/// success and on failure: the configuration scan lists every responding
/// function (so a capture shows whether the VL805 answers at all), and
/// the enumerated device's vendor/product id and xHCI slot are logged
/// when it is found, so a metal capture localises a silent keyboard to
/// the stage that stalled (the issue's "what is discovered on USB"). The
/// logging is one-shot bring-up diagnostics — never on the per-report
/// poll path (`AGENTS.md` §2.16 / §19.4).
pub fn bring_up_keyboard(
    host: &dyn DriverHost,
    bringup: &PcieBringup,
    firmware_reset: &dyn FirmwareReset,
    delay: &dyn Delay,
    sink: &dyn Sink,
) -> Result<KeyboardChain, DriverError> {
    log_stage(
        sink,
        "usb-keyboard: training brcm,bcm2711-pcie root-complex link",
    );
    let mut rc = match pcie_brcm::wiring::open_discovered(
        host,
        bringup.regs_phys,
        &bringup.windows,
        delay,
    ) {
        Ok(rc) => rc,
        Err(err) => {
            log_stage_err(
                sink,
                "usb-keyboard: pcie root-complex link bring-up failed",
                err,
            );
            return Err(err);
        }
    };
    log_stage(sink, "usb-keyboard: pcie root-complex link trained");
    // One-shot diagnostic: split the bring-up's wall time across its
    // reset / configuration-programming / link-wait phases. A timestamped
    // metal capture pinned a ~11 s pause inside `bring_up` that its coded
    // delays (~hundreds of ms) cannot explain, so this localises the stall
    // to the exact phase rather than guessing (`AGENTS.md` §15.7).
    log_bring_up_timing(sink, rc.bring_up_timing());
    // One-shot diagnostic: read the outbound (CPU→PCIe) translation window
    // and link status back off the trained register block before it is
    // consumed into the windowed config accessor. The mapped BAR aborts
    // (`dead_dead`) while configuration reads succeed, and config vs memory
    // take different controller paths, so the outbound window is where a
    // memory-only abort must be measured (`AGENTS.md` §15.7).
    log_outbound_window(sink, rc.outbound_window_readback());
    // One-shot diagnostic: read the inbound (PCIe→system-memory) viewport
    // back. On the Pi 4 the VideoCore VL805 firmware handoff may depend on
    // this inbound DMA window, so this captures the last PCIe element for
    // comparison with working Linux (`AGENTS.md` §15.7).
    log_inbound_window(sink, rc.inbound_window_readback());
    // Recover the trained controller's register window and reach the VL805
    // through the BCM2711 windowed config accessor built over it. The
    // accessor forwards configuration only to the single device on the
    // secondary bus, so the flat enumeration below never emits a TLP to an
    // absent downstream target (which would CPU-abort and wedge the boot).
    let bus = rustos_drv_bus_pci::mechanism_brcm(rc.into_regs(), pcie_brcm::regs::RC_SECONDARY_BUS);
    // One-shot diagnostic: log every function the trained link exposes
    // before the controller search runs, so a metal capture distinguishes
    // "the VL805 never answered configuration reads" (no downstream
    // function listed) from "enumerated but xHCI did not come up". The
    // authoritative search is `open_discovered` below; this only reports.
    log_bus_scan(sink, &bus);
    // The xHCI DMA carve is bounded against the bridge's inbound aperture
    // in the *device-visible* (PCIe) address space — the space the
    // controller's DMA descriptors carry, and the space `DmaSlab::phys`
    // returns. That exclusive top is `inbound_pcie_base + inbound_size`
    // (e.g. the Pi 4 maps PCIe `[0x4_0000_0000, 0x6_0000_0000)` onto RAM);
    // it is *not* the CPU-physical aperture top (`AGENTS.md` §5.4 — the
    // bound must match the address space it guards). An overflow here is a
    // malformed discovery, refused fail-closed.
    let Some(dma_aperture_top) = bringup
        .windows
        .inbound_pcie_base
        .checked_add(bringup.windows.inbound_size)
    else {
        log_stage_err(
            sink,
            "usb-keyboard: inbound DMA aperture top overflows the address space",
            DriverError::OutOfRange,
        );
        return Err(DriverError::OutOfRange);
    };
    // The bridge's outbound (CPU→PCIe) window, in the PCIe-bus address
    // space the VL805's BAR decodes: the BAR is assigned a size-aligned
    // address inside it when firmware left it unassigned (the metal
    // `length_out_of_range` shape), so the mapped window resolves to a
    // real CPU address (`AGENTS.md` §5.4).
    let outbound_window = (
        bringup.windows.outbound_pcie_base,
        bringup.windows.outbound_size,
    );
    let mut usb = open_controller(
        host,
        &bus,
        dma_aperture_top,
        outbound_window,
        firmware_reset,
        delay,
        sink,
    )?;
    log_stage(
        sink,
        "usb-keyboard: vl805 xhci controller online, enumerating root hub",
    );
    let descriptor = match usb.enumerate_first_connected() {
        Ok(descriptor) => descriptor,
        Err(err) => {
            log_stage_err(
                sink,
                "usb-keyboard: no usb device enumerated on the root hub",
                err,
            );
            return Err(err);
        }
    };
    // Read the assigned slot before `usb` is moved into the keyboard.
    let slot = usb.slot();
    // Allocation-free hex rendering on the bring-up stack (one-shot, not on
    // the poll path): show the keyboard the chain actually found.
    let mut vid_buf = [0u8; 16];
    let mut pid_buf = [0u8; 16];
    let mut slot_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_DEVICE,
            message: "usb-keyboard: enumerated usb device on the vl805 root hub",
            fields: &[
                Field {
                    key: "vendor_id_hex",
                    value: format_hex_u64(u64::from(descriptor.vendor_id), &mut vid_buf),
                },
                Field {
                    key: "product_id_hex",
                    value: format_hex_u64(u64::from(descriptor.product_id), &mut pid_buf),
                },
                Field {
                    key: "xhci_slot",
                    value: format_hex_u64(u64::from(slot), &mut slot_buf),
                },
            ],
        },
    );
    Ok(BootKeyboard::new(usb))
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use core::ptr::NonNull;

    use rustos_abi::driver::dma::PoolId;
    use rustos_abi::driver::mmio::MmioMapError;
    use rustos_abi::input::{KeyValue, Modifiers};
    use rustos_abi::{HwDeviceClass, HwResource};
    use rustos_kernel_core::{ConsoleInputQueue, ConsoleRead};

    /// A [`Sink`] that records the `(level, id)` of every event it
    /// receives, so a test can assert the bring-up emitted its staged
    /// diagnostics (`AGENTS.md` §23.4 — the new logging is covered).
    /// Single-threaded `RefCell` is sufficient under `cargo test`.
    struct RecordingSink {
        events: RefCell<Vec<(Level, u32)>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
            }
        }

        /// Number of recorded events whose `EventId` equals `id`.
        fn count(&self, id: EventId) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|(_, recorded)| *recorded == id.0)
                .count()
        }

        /// Number of recorded events at [`Level::Error`].
        fn errors(&self) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|(level, _)| *level == Level::Error)
                .count()
        }
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push((event.level, event.id.0));
        }
    }

    #[test]
    fn firmware_reset_failure_logs_the_mailbox_reason() {
        struct ReasonSink {
            saw_timeout: Cell<bool>,
        }

        impl Sink for ReasonSink {
            fn write_event(&self, event: &Event<'_>) {
                assert_eq!(event.level, Level::Error);
                assert_eq!(event.id, USB_KEYBOARD_FW_RESET);
                self.saw_timeout.set(
                    event
                        .fields
                        .iter()
                        .any(|field| field.key == "reason" && field.value == "timeout"),
                );
            }
        }

        let sink = ReasonSink {
            saw_timeout: Cell::new(false),
        };
        log_firmware_reset(
            &sink,
            FirmwareResetOutcome::Failed {
                reason: FirmwareResetFailure::Timeout,
            },
        );

        assert!(sink.saw_timeout.get());
    }

    /// The Pi 4 discovered values: controller `reg`, inbound `dma-ranges`
    /// (PCIe base 0, 3 GiB), outbound `ranges` (CPU `0x6_0000_0000` → PCIe
    /// `0xc000_0000`, 1 GiB).
    const REGS_PHYS: u64 = 0xfd50_0000;
    const APERTURE_TOP: u64 = 0xc000_0000;
    const OUTBOUND_CPU: u64 = 0x6_0000_0000;
    const OUTBOUND_PCIE: u64 = 0xc000_0000;
    const OUTBOUND_SIZE: u64 = 0x4000_0000;

    fn pcie_node() -> HwNode {
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
            .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        node
    }

    #[test]
    fn bringup_inputs_are_assembled_from_the_node() {
        let bringup = pcie_bringup_from_node(&pcie_node()).expect("all resources present");
        assert_eq!(bringup.regs_phys, REGS_PHYS);
        assert_eq!(bringup.windows.inbound_pcie_base, 0);
        assert_eq!(bringup.windows.inbound_size, APERTURE_TOP);
        assert_eq!(bringup.windows.outbound_cpu_base, OUTBOUND_CPU);
        assert_eq!(bringup.windows.outbound_pcie_base, OUTBOUND_PCIE);
        assert_eq!(bringup.windows.outbound_size, OUTBOUND_SIZE);
    }

    #[test]
    fn bringup_carries_a_nonzero_inbound_pcie_base() {
        // A viewport not anchored at PCIe address 0: the translation rides
        // the DMA resource's far-side base, distinct from the CPU top.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::dma_translated(
            APERTURE_TOP,
            APERTURE_TOP,
            0x4000_0000,
        ))
        .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        let bringup = pcie_bringup_from_node(&node).expect("resources present");
        assert_eq!(bringup.windows.inbound_pcie_base, 0x4000_0000);
        assert_eq!(bringup.windows.inbound_size, APERTURE_TOP);
        // The device-visible DMA top `bring_up_keyboard` derives from these
        // is `inbound_pcie_base + inbound_size`, distinct from the CPU top.
        assert_eq!(
            bringup.windows.inbound_pcie_base + bringup.windows.inbound_size,
            0x4000_0000 + APERTURE_TOP,
        );
    }

    #[test]
    fn bringup_fails_closed_on_each_missing_resource() {
        // No controller register window.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
            .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        assert_eq!(
            pcie_bringup_from_node(&node),
            Err(BringupError::NoControllerWindow)
        );

        // No inbound aperture.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::bus_window(
            OUTBOUND_CPU,
            OUTBOUND_SIZE,
            OUTBOUND_PCIE,
        ))
        .unwrap();
        assert_eq!(
            pcie_bringup_from_node(&node),
            Err(BringupError::NoInboundAperture)
        );

        // No outbound window.
        let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
        node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
            .unwrap();
        node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
            .unwrap();
        assert_eq!(
            pcie_bringup_from_node(&node),
            Err(BringupError::NoOutboundWindow)
        );
    }

    /// A pressed-character [`KeyInput`] record with no modifiers.
    fn press(c: char) -> KeyInput {
        KeyInput::Pressed {
            key: KeyValue::Char(c),
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn arbiter_console_sink_delivers_a_press_to_the_text_sink() {
        // The arbiter starts in text focus; its text sink is the video
        // console's input queue, drained by the login reading that console.
        let queue: &'static ConsoleInputQueue = Box::leak(Box::new(ConsoleInputQueue::new()));
        let focus = InputFocus::new(queue);
        let mut sink = ArbiterConsoleSink::new(&focus);
        sink.write(&press('h').to_le_bytes()).expect("delivered");
        let mut buf = [0u8; 8];
        let read = queue.read(&mut buf).expect("read");
        assert_eq!(&buf[..read], b"h");
    }

    #[test]
    fn arbiter_console_sink_fails_closed_without_an_injectable_text_sink() {
        // `NULL_INPUT_FOCUS`'s text sink accepts no injected input: a press
        // that would be enqueued there surfaces a `DeviceFault` rather than
        // dropping it (`AGENTS.md` §2.9).
        let mut sink = ArbiterConsoleSink::new(&rustos_kernel_core::NULL_INPUT_FOCUS);
        assert_eq!(
            sink.write(&press('x').to_le_bytes()),
            Err(DriverError::DeviceFault)
        );
        // A malformed record is refused too.
        assert_eq!(sink.write(&[0u8; 4]), Err(DriverError::DeviceFault));
    }

    /// Leak a `len`-byte, 4-byte-aligned buffer (the mock host's `'static`
    /// storage, mirroring the usb `wiring_tests` strategy).
    fn leak_aligned(len: usize) -> NonNull<u8> {
        let words = len.div_ceil(4).max(1);
        let buf: Box<[u32]> = alloc::vec![0u32; words].into_boxed_slice();
        NonNull::new(Box::leak(buf).as_mut_ptr().cast::<u8>()).expect("non-null")
    }

    struct MockMapper {
        grant: bool,
    }
    impl MmioMapper for MockMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.grant {
                return Err(MmioMapError::CapabilityMissing);
            }
            let base = leak_aligned(len);
            // SAFETY: `base` covers `len` zeroed bytes, is 4-byte aligned,
            // lives for the whole test process (leaked), and is unaliased.
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
        }
    }

    struct MockDmaHost;
    impl VirtioHost for MockDmaHost {
        fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
            let ptr = leak_aligned(size);
            // SAFETY: `ptr` covers `size` zeroed bytes and lives for the
            // whole test process; the device-visible base is in-aperture
            // (below `APERTURE_TOP`). Drop is a no-op (`from_leaked`).
            Ok(unsafe { DmaSlab::from_leaked(0x1000_0000, ptr, size, PoolId::MOCK, 0) })
        }
        fn notify_wait(&self, _queue_index: u16) {}
    }

    fn caps(set: &[CapabilityId]) -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        for c in set {
            caps.insert(*c);
        }
        caps
    }

    /// A host [`Delay`] that does not sleep but advances a virtual clock by
    /// exactly the requested microseconds on every `delay_us`, so any poll
    /// loop bounded by [`Delay::now_us`] (the caps-readiness wait)
    /// terminates deterministically — modelling, without real time, the
    /// metal behaviour where each iteration consumes wall time the
    /// [`CAPS_READY_BUDGET_US`] bound caps (`AGENTS.md` §2.16). A fixed
    /// clock would spin that loop forever, so the stepping clock is the
    /// safe default for every test.
    struct NoDelay {
        now_us: core::cell::Cell<u64>,
    }

    impl NoDelay {
        fn new() -> Self {
            Self {
                now_us: core::cell::Cell::new(0),
            }
        }
    }

    impl Delay for NoDelay {
        fn delay_us(&self, us: u32) {
            self.now_us.set(self.now_us.get() + u64::from(us));
        }

        fn now_us(&self) -> u64 {
            self.now_us.get()
        }
    }

    #[test]
    fn chain_host_reports_caps_mapper_and_dma() {
        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
        );
        assert!(host.has_capability(CapabilityId::MMIO_MAP));
        assert!(host.has_capability(CapabilityId::MEM_DMA));
        assert!(!host.has_capability(CapabilityId::DRV_LOAD));
        assert_eq!(host.kind(), DriverKind::InKernel);
        assert!(host.virtio_host().is_some());
        assert!(host.mmio_mapper().is_some());
    }

    #[test]
    fn bring_up_requires_the_mmio_capability() {
        // A host without MMIO_MAP fails the chain closed at the very first
        // step (the PCIe controller-window map), before any hardware.
        let mapper = MockMapper { grant: false };
        let dma = MockDmaHost;
        let host = ChainHost::new(caps(&[CapabilityId::MEM_DMA]), &mapper, &dma);
        let bringup = pcie_bringup_from_node(&pcie_node()).unwrap();
        let sink = RecordingSink::new();
        // `.err()` drops the unenumerated keyboard (which is neither
        // `Debug` nor `PartialEq`) and compares only the error.
        assert_eq!(
            bring_up_keyboard(&host, &bringup, &NoFirmwareReset, &NoDelay::new(), &sink).err(),
            Some(DriverError::PermissionDenied)
        );
        // The bring-up logged the failing stage as an `Error` event under
        // the bring-up id, so a metal capture localises the wedge
        // (`AGENTS.md` §23.4 — the staged logging is covered). `Error`
        // events clear the default `Info` threshold regardless of any
        // concurrent test's level, so this is deterministic.
        assert!(sink.errors() >= 1);
        assert!(sink.count(USB_KEYBOARD_BRINGUP) >= 1);
    }

    #[test]
    fn bring_up_reaches_the_pcie_link_bringup_over_a_mapped_window() {
        // With the capability granted the chain maps the controller window
        // and runs the BCM2711 root-complex bring-up; over the inert zeroed
        // mock window the root-port status check reads 0 and fails closed
        // with DeviceFault — exactly the metal boundary the host test can
        // reach (`AGENTS.md` §0.4). That the chain got this far proves the
        // window was assembled and mapped and pcie_brcm was driven.
        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
        );
        let bringup = pcie_bringup_from_node(&pcie_node()).unwrap();
        let sink = RecordingSink::new();
        assert_eq!(
            bring_up_keyboard(&host, &bringup, &NoFirmwareReset, &NoDelay::new(), &sink).err(),
            Some(DriverError::DeviceFault)
        );
        // The chain logged the link-training start and then the
        // root-complex failure, so the staged diagnostics fired before the
        // metal boundary refused (`AGENTS.md` §23.4).
        assert!(sink.errors() >= 1);
        assert!(sink.count(USB_KEYBOARD_BRINGUP) >= 1);
    }

    /// A [`Bus`] returning a fixed device list, modelling the Pi 4's
    /// trained root complex (the bridge plus the VL805) so the scan
    /// diagnostic can be exercised without a live controller.
    struct MockBus {
        devices: Vec<BusDevice>,
    }

    impl Bus for MockBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            let n = self.devices.len().min(out.len());
            out[..n].copy_from_slice(&self.devices[..n]);
            if out.len() < self.devices.len() {
                Err(DriverError::BufferTooSmall)
            } else {
                Ok(n)
            }
        }
    }

    fn bus_device(address: u64, vendor: u32, device: u32, class: u16) -> BusDevice {
        BusDevice {
            vendor,
            device,
            class,
            reserved0: 0,
            address,
        }
    }

    #[test]
    fn bus_scan_logs_a_summary_and_one_event_per_function() {
        // The healthy Pi 4 shape: the root complex (14e4:2711, bridge) and
        // the VL805 USB host behind it (1106:3483, USB class 0x0c03).
        let bus = MockBus {
            devices: alloc::vec![
                bus_device(0x0000, 0x14e4, 0x2711, 0x0604),
                bus_device(0x0100, 0x1106, 0x3483, 0x0c03),
            ],
        };
        let sink = RecordingSink::new();
        log_bus_scan(&sink, &bus);
        // One summary event plus one per discovered function, all under the
        // scan id and none at `Error` (`AGENTS.md` §23.4 — the diagnostic
        // is covered).
        assert_eq!(sink.count(USB_KEYBOARD_PCI_SCAN), 3);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn bus_scan_reports_an_empty_bus_without_faulting() {
        // The failure shape the issue points at: the link trained but no
        // function answers configuration reads. The scan still emits its
        // summary (function count zero) and logs no error — the real
        // `NotFound` comes from the controller search that follows.
        let bus = MockBus {
            devices: Vec::new(),
        };
        let sink = RecordingSink::new();
        log_bus_scan(&sink, &bus);
        assert_eq!(sink.count(USB_KEYBOARD_PCI_SCAN), 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn bus_scan_caps_an_oversized_bus_at_the_report_limit() {
        // A malfunctioning bus reporting more functions than the bound is
        // truncated to `SCAN_REPORT_LIMIT` (plus the summary), never an
        // unbounded log (`AGENTS.md` §24.4), and never an error.
        let devices = (0..(SCAN_REPORT_LIMIT + 8) as u64)
            .map(|i| bus_device(i, 0x1234, 0x5678, 0x0c03))
            .collect();
        let bus = MockBus { devices };
        let sink = RecordingSink::new();
        log_bus_scan(&sink, &bus);
        assert_eq!(sink.count(USB_KEYBOARD_PCI_SCAN), SCAN_REPORT_LIMIT + 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn bus_scan_logs_an_error_when_enumeration_faults() {
        // A transport that faults enumeration is logged as an error rather
        // than panicking or being swallowed (`AGENTS.md` §2.9).
        struct FaultingBus;
        impl Bus for FaultingBus {
            fn enumerate(&self, _out: &mut [BusDevice]) -> Result<usize, DriverError> {
                Err(DriverError::DeviceFault)
            }
        }
        let sink = RecordingSink::new();
        log_bus_scan(&sink, &FaultingBus);
        assert_eq!(sink.errors(), 1);
        assert_eq!(sink.count(USB_KEYBOARD_BRINGUP), 1);
        assert_eq!(sink.count(USB_KEYBOARD_PCI_SCAN), 0);
    }

    /// A [`PciBus`] recording every `read_config(bdf, offset)` it serves
    /// and returning a canned dword, so the configuration read-back
    /// diagnostic ([`log_config_readback`]) can be exercised without a
    /// live bus. Only the `read_config` arm is reachable from the test;
    /// the other `PciBus`/`Bus` methods are unused.
    struct ConfigMockBus {
        reads: RefCell<Vec<(u64, u16)>>,
    }

    impl Bus for ConfigMockBus {
        fn enumerate(&self, _out: &mut [BusDevice]) -> Result<usize, DriverError> {
            Err(DriverError::Unsupported)
        }
    }

    impl PciBus for ConfigMockBus {
        fn map_bar_window(
            &self,
            _bdf: u64,
            _bar_index: u8,
            _mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            Err(DriverError::Unsupported)
        }

        fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }

        fn assign_bar(
            &self,
            _bdf: u64,
            _bar_index: u8,
            _window_base: u64,
            _window_size: u64,
        ) -> Result<u64, DriverError> {
            Err(DriverError::Unsupported)
        }

        fn read_config(&self, bdf: u64, offset: u16) -> Result<u32, DriverError> {
            self.reads.borrow_mut().push((bdf, offset));
            // A plausible programmed value per register, enough to assert
            // the readback reached each one.
            Ok(match (bdf, offset) {
                (0, 0x18) => 0x00ff_0100,        // bridge primary/secondary/subordinate
                (0, 0x20) => 0xfff0_c000,        // bridge mem base/limit
                (0, 0x04) => 0x0010_0407,        // bridge command/status (io+mem+busmaster)
                (0x1_0000, 0x04) => 0x0010_0006, // VL805 command/status (mem+busmaster)
                (0x1_0000, 0x10) => 0xc000_0004, // VL805 BAR0 (64-bit mem)
                (0x1_0000, 0x14) => 0x0000_0000, // VL805 BAR1 high
                _ => 0xffff_ffff,
            })
        }

        fn describe_function(
            &self,
            _bdf: u64,
            _parent_id: u32,
            _node_id: u32,
        ) -> Result<HwNode, DriverError> {
            Err(DriverError::Unsupported)
        }
    }

    #[test]
    fn config_readback_dumps_each_register_once() {
        let bus = ConfigMockBus {
            reads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        log_config_readback(&sink, &bus);
        // Exactly one one-shot 4110 event, at Info (a readback is never an
        // error; a faulting read renders the sentinel, §2.9).
        assert_eq!(sink.count(USB_KEYBOARD_CONFIG), 1);
        assert_eq!(sink.errors(), 0);
        // It read back every register the bring-up programmed: the bridge
        // bus numbers / memory window / command, the VL805 command and
        // both BAR dwords, and the VL805 XHCI MCU firmware version
        // (`0x50`) — the config-space register Linux's
        // `rpi_firmware_init_vl805` uses to confirm the firmware load
        // (`AGENTS.md` §15.7 / §23.4).
        let reads = bus.reads.borrow();
        assert!(reads.contains(&(0, 0x18)));
        assert!(reads.contains(&(0, 0x20)));
        assert!(reads.contains(&(0, 0x04)));
        assert!(reads.contains(&(0x1_0000, 0x04)));
        assert!(reads.contains(&(0x1_0000, 0x10)));
        assert!(reads.contains(&(0x1_0000, 0x14)));
        assert!(reads.contains(&(0x1_0000, VL805_FW_VERSION_OFFSET)));
    }

    #[test]
    fn config_readback_renders_a_sentinel_for_a_faulting_read() {
        // A bus whose config reads fault must not propagate or panic: the
        // diagnostic still emits its one-shot event (`AGENTS.md` §2.9).
        struct FaultingConfigBus;
        impl Bus for FaultingConfigBus {
            fn enumerate(&self, _out: &mut [BusDevice]) -> Result<usize, DriverError> {
                Err(DriverError::Unsupported)
            }
        }
        impl PciBus for FaultingConfigBus {
            fn map_bar_window(
                &self,
                _bdf: u64,
                _bar_index: u8,
                _mapper: &dyn MmioMapper,
            ) -> Result<RegisterWindow, DriverError> {
                Err(DriverError::Unsupported)
            }
            fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
                Err(DriverError::Unsupported)
            }
            fn assign_bar(
                &self,
                _bdf: u64,
                _bar_index: u8,
                _window_base: u64,
                _window_size: u64,
            ) -> Result<u64, DriverError> {
                Err(DriverError::Unsupported)
            }
            fn read_config(&self, _bdf: u64, _offset: u16) -> Result<u32, DriverError> {
                Err(DriverError::DeviceFault)
            }
            fn describe_function(
                &self,
                _bdf: u64,
                _parent_id: u32,
                _node_id: u32,
            ) -> Result<HwNode, DriverError> {
                Err(DriverError::Unsupported)
            }
        }
        let sink = RecordingSink::new();
        log_config_readback(&sink, &FaultingConfigBus);
        assert_eq!(sink.count(USB_KEYBOARD_CONFIG), 1);
        assert_eq!(sink.errors(), 0);
    }

    /// A 4-byte-aligned host buffer presented as a [`RegisterWindow`], so
    /// the raw-capability probe runs over the real window read path. The
    /// `u32` backing guarantees the 4-byte alignment `from_mapping`
    /// requires; the bytes are little-endian, matching every Tier-1
    /// target (`AGENTS.md` §23.2).
    fn window_over(backing: &mut [u32]) -> RegisterWindow {
        let len = core::mem::size_of_val(backing);
        let ptr = NonNull::new(backing.as_mut_ptr().cast::<u8>()).expect("non-null");
        // SAFETY: `backing` is a live, 4-aligned `u32` slice of exactly
        // `len` bytes that outlives the window (the caller holds it for
        // the test body), and nothing else aliases it.
        unsafe { RegisterWindow::from_mapping(0xc000_0000, ptr, len) }
    }

    #[test]
    fn raw_cap_dword_reads_the_value_or_a_sentinel() {
        // CAPLENGTH=0x20, HCIVERSION=0x0100 in the first dword; the read
        // returns that value, and an out-of-window offset fails closed to
        // the all-ones sentinel rather than reading past the mapping
        // (`AGENTS.md` §5.4).
        let mut backing = [0u32; 0x400];
        backing[0] = 0x0100_0020;
        let window = window_over(&mut backing);
        assert_eq!(read_cap_dword(&window, 0x00), 0x0100_0020);
        assert_eq!(read_cap_dword(&window, 0x1000), u64::MAX);
    }

    #[test]
    fn raw_caps_probe_dumps_the_capability_block_once() {
        // The probe emits exactly one `4107` record at `Info`, reading the
        // real BAR window — the measurement a metal `out_of_range` capture
        // needs (`AGENTS.md` §15.7 / §23.4).
        let mut backing = [0u32; 0x400];
        backing[0] = 0x0100_0020;
        let window = window_over(&mut backing);
        let sink = RecordingSink::new();
        log_raw_caps(&sink, &window);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_RAW), 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn caps_block_liveness_rejects_the_uninitialised_bus_patterns() {
        // A live header: CAPLENGTH=0x20, HCIVERSION=0x0100.
        assert!(caps_block_is_live(0x0100_0020));
        // The pre-firmware patterns the metal capture showed are all
        // rejected: the BCM2711 `dead_dead` poison (HCIVERSION 0xdead),
        // the all-ones UR sentinel, and an unpowered zero.
        assert!(!caps_block_is_live(0xdead_dead));
        assert!(!caps_block_is_live(0xffff_ffff));
        assert!(!caps_block_is_live(0));
        // The `read_cap_dword` refused-read sentinel is rejected too.
        assert!(!caps_block_is_live(u64::MAX));
        // A real version but an implausibly small CAPLENGTH is not live.
        assert!(!caps_block_is_live(0x0100_0010));
    }

    #[test]
    fn caps_readiness_returns_immediately_when_the_block_is_live() {
        // A controller already presenting its capability block needs no
        // wait: ready on the first read (zero polls), one `4109` at Info.
        let mut backing = [0u32; 0x400];
        backing[0] = 0x0100_0020;
        let window = window_over(&mut backing);
        let sink = RecordingSink::new();
        assert!(wait_for_caps_ready(&window, &NoDelay::new(), &sink));
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn caps_readiness_fails_closed_after_the_bounded_budget() {
        // A controller that never decodes (the `dead_dead` capture) is
        // polled until the wall-time budget elapses and then reported
        // not-ready, so the bring-up falls through to a fail-closed
        // `Xhci::open` rather than spinning forever (`AGENTS.md` §2.1 /
        // §2.9). The `4109` line is still Info — the not-ready report is
        // diagnostic, not an error. `NoDelay` advances its virtual
        // clock by each requested delay, so the loop terminates by the
        // wall budget the way it does on metal (where each master-aborting
        // read costs real time), not by an iteration count.
        let mut backing = [0u32; 0x400];
        backing[0] = 0xdead_dead;
        let window = window_over(&mut backing);
        let sink = RecordingSink::new();
        assert!(!wait_for_caps_ready(&window, &NoDelay::new(), &sink));
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn caps_readiness_wall_budget_caps_the_poll_count_under_slow_reads() {
        // The defect the `4116` capture exposed: a poll-count budget
        // assumed each read was cheap, but on the BCM2711 each read of an
        // un-decoded BAR master-aborts and stalls for tens of milliseconds,
        // so 256 polls inflated the intended ~256 ms wait into ~14 s.
        // Bounding by elapsed wall time fixes this — a delay whose clock
        // also jumps a large per-read cost forward (here far larger than
        // the poll interval) makes the loop stop after only a handful of
        // reads, never the old 256. We assert the loop honours the wall
        // budget regardless of how few iterations that takes
        // (`AGENTS.md` §2.16).
        struct SlowReadDelay {
            now_us: core::cell::Cell<u64>,
            per_read_us: u64,
        }
        impl Delay for SlowReadDelay {
            fn delay_us(&self, us: u32) {
                // Model each iteration's wall cost as the requested delay
                // plus the master-abort read stall the previous read paid.
                self.now_us
                    .set(self.now_us.get() + u64::from(us) + self.per_read_us);
            }
            fn now_us(&self) -> u64 {
                self.now_us.get()
            }
        }
        let mut backing = [0u32; 0x400];
        backing[0] = 0xdead_dead;
        let window = window_over(&mut backing);
        let sink = RecordingSink::new();
        // ~54 ms per read (the metal `4116` figure): the loop must give up
        // within a handful of polls, not the old 256.
        let delay = SlowReadDelay {
            now_us: core::cell::Cell::new(0),
            per_read_us: 54_000,
        };
        assert!(!wait_for_caps_ready(&window, &delay, &sink));
        // ~256 ms budget / ~55 ms per iteration ≈ 5 polls; assert it is
        // far below the old 256-poll ceiling and that the virtual wall
        // clock did not blow far past the budget.
        assert!(delay.now_us() <= CAPS_READY_BUDGET_US + 55_000);
    }

    #[test]
    fn outbound_window_readback_logs_one_4111_record() {
        // The outbound-window read-back emits exactly one `4111` record at
        // `Info` — the measurement a metal `dead_dead` BAR-abort capture
        // needs to confirm the CPU→PCIe translation window holds the
        // programmed bases (`AGENTS.md` §15.7 / §23.4).
        let sink = RecordingSink::new();
        log_outbound_window(
            &sink,
            OutboundWindowReadback {
                mem_win0_lo: 0xc000_0000,
                mem_win0_hi: 0,
                mem_win0_base_limit: 0x3ff0_0000,
                mem_win0_base_hi: 6,
                mem_win0_limit_hi: 6,
                pcie_status: 0xb0,
            },
        );
        assert_eq!(sink.count(USB_KEYBOARD_OUTBOUND), 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn inbound_window_readback_logs_one_4119_record() {
        // The inbound-window read-back emits exactly one `4119` record at
        // `Info` — the measurement a metal capture needs to compare our
        // inbound DMA (VideoCore VL805-firmware) window with working Linux
        // (raspberrypi/firmware #1617; `AGENTS.md` §15.7 / §23.4).
        let sink = RecordingSink::new();
        log_inbound_window(
            &sink,
            InboundWindowReadback {
                rc_bar1_lo: 0,
                rc_bar2_lo: 0x11,
                rc_bar2_hi: 4,
                rc_bar3_lo: 0,
                pcie_status: 0xb0,
            },
        );
        assert_eq!(sink.count(USB_KEYBOARD_INBOUND), 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn bring_up_timing_log_emits_one_4117_record() {
        // The per-phase bring-up timing is recorded once at `Info`; this
        // is the instrument that confirms the reset-first fix (the
        // post-de-reset `reset_settle_us` span stays at microseconds) and
        // localises any residual multi-second bring-up to the exact MMIO
        // group (`AGENTS.md` §15.7 / §23.4). These are healthy post-fix
        // spans.
        let sink = RecordingSink::new();
        log_bring_up_timing(
            &sink,
            BringUpTiming {
                reset_swinit_us: 200,
                reset_settle_us: 200,
                config_us: 9,
                linkwait_us: 100_000,
                link_polls: 0,
                entry_rgr1_sw_init: 0,
            },
        );
        assert_eq!(sink.count(USB_KEYBOARD_BRINGUP_TIMING), 1);
        assert_eq!(sink.errors(), 0);
    }

    /// A [`PciBus`] that enumerates a single VL805-class function and
    /// bases/maps its BAR, so `map_controller` succeeds end to end. The
    /// mapped window is the mock host's zeroed BAR, so `Xhci::open` then
    /// fails closed at the metal boundary (`AGENTS.md` §0.4).
    struct OrderingMockBus<'a> {
        firmware_version: Option<&'a core::cell::Cell<u32>>,
    }

    impl<'a> OrderingMockBus<'a> {
        const fn not_loaded() -> Self {
            Self {
                firmware_version: None,
            }
        }

        const fn with_firmware_version(firmware_version: &'a core::cell::Cell<u32>) -> Self {
            Self {
                firmware_version: Some(firmware_version),
            }
        }
    }

    impl Bus for OrderingMockBus<'_> {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.is_empty() {
                return Ok(0);
            }
            // USB-class function (PCI class 0x0c03) — the controller search
            // matches this class (`rustos_drv_bus_usb::wiring`).
            out[0] = bus_device(0x1_0000, 0x1106, 0x3483, 0x0c03);
            Ok(1)
        }
    }

    impl PciBus for OrderingMockBus<'_> {
        fn map_bar_window(
            &self,
            _bdf: u64,
            _bar_index: u8,
            mapper: &dyn MmioMapper,
        ) -> Result<RegisterWindow, DriverError> {
            mapper
                .map_window(0xc000_0000, 0x1000)
                .map_err(MmioMapError::as_driver_error)
        }

        fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
            Ok(())
        }

        fn assign_bar(
            &self,
            _bdf: u64,
            _bar_index: u8,
            window_base: u64,
            _window_size: u64,
        ) -> Result<u64, DriverError> {
            Ok(window_base)
        }

        fn read_config(&self, _bdf: u64, offset: u16) -> Result<u32, DriverError> {
            if offset == VL805_FW_VERSION_OFFSET {
                return Ok(self.firmware_version.map_or(0, core::cell::Cell::get));
            }
            Ok(0)
        }

        fn describe_function(
            &self,
            _bdf: u64,
            _parent_id: u32,
            _node_id: u32,
        ) -> Result<HwNode, DriverError> {
            Err(DriverError::Unsupported)
        }
    }

    #[test]
    fn open_controller_stops_before_firmware_wait_when_mapping_fails() {
        // A host lacking MMIO_MAP fails `map_controller` closed before the
        // firmware-version wait or BAR readiness diagnostics run.
        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(caps(&[CapabilityId::MEM_DMA]), &mapper, &dma);
        let bus = OrderingMockBus::not_loaded();
        let sink = RecordingSink::new();
        assert_eq!(
            open_controller(
                &host,
                &bus,
                0x2_0000_0000,
                (0xc000_0000, 0x4000_0000),
                &NoFirmwareReset,
                &NoDelay::new(),
                &sink,
            )
            .err(),
            Some(DriverError::PermissionDenied)
        );
        assert_eq!(sink.count(USB_KEYBOARD_FW_READY), 0);
    }

    #[test]
    fn open_controller_stops_when_reload_does_not_make_version_loaded() {
        // With a granted host and a bus that enumerates the VL805 and bases
        // its BAR, `open_controller` maps the controller and waits for the
        // VL805's firmware version (config `0x50`) to read non-zero. The mock
        // bus returns `0` for every config read, so the first firmware-loaded
        // wait fails. The bring-up then issues exactly one Linux-style
        // `NOTIFY_XHCI_RESET` fallback; because the version still stays zero,
        // it fails closed before touching the uninitialised BAR.
        struct RecordingFirmwareReset {
            calls: core::cell::Cell<u32>,
        }

        impl FirmwareReset for RecordingFirmwareReset {
            fn reload(&self) -> FirmwareResetOutcome {
                self.calls.set(self.calls.get() + 1);
                FirmwareResetOutcome::Reloaded {
                    response_value: VL805_FIRMWARE_DEV_ADDR,
                }
            }
        }

        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
        );
        let bus = OrderingMockBus::not_loaded();
        let firmware_reset = RecordingFirmwareReset {
            calls: core::cell::Cell::new(0),
        };
        let sink = RecordingSink::new();
        // The carved DMA region (mock base 0x1000_0000) lies below this
        // aperture top; the outbound window is the Pi 4's PCIe MMIO window.
        let result = open_controller(
            &host,
            &bus,
            0x2_0000_0000,
            (0xc000_0000, 0x4000_0000),
            &firmware_reset,
            &NoDelay::new(),
            &sink,
        );
        assert_eq!(result.err(), Some(DriverError::DeviceFault));
        assert_eq!(firmware_reset.calls.get(), 1);
        // One wait before the reload, and one after Linux's 200 µs settle.
        assert_eq!(sink.count(USB_KEYBOARD_FW_READY), 2);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESET), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESPONSE), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 0);
        assert_eq!(sink.count(USB_KEYBOARD_POST_RELOAD), 0);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_RAW), 0);
    }

    #[test]
    fn open_controller_proceeds_after_reload_makes_version_loaded() {
        struct LoadingFirmwareReset<'a> {
            calls: core::cell::Cell<u32>,
            firmware_version: &'a core::cell::Cell<u32>,
        }

        impl FirmwareReset for LoadingFirmwareReset<'_> {
            fn reload(&self) -> FirmwareResetOutcome {
                self.calls.set(self.calls.get() + 1);
                self.firmware_version.set(0x0001_38c0);
                FirmwareResetOutcome::Reloaded {
                    response_value: VL805_FIRMWARE_DEV_ADDR,
                }
            }
        }

        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
        );
        let firmware_version = core::cell::Cell::new(0);
        let bus = OrderingMockBus::with_firmware_version(&firmware_version);
        let firmware_reset = LoadingFirmwareReset {
            calls: core::cell::Cell::new(0),
            firmware_version: &firmware_version,
        };
        let sink = RecordingSink::new();
        let result = open_controller(
            &host,
            &bus,
            0x2_0000_0000,
            (0xc000_0000, 0x4000_0000),
            &firmware_reset,
            &NoDelay::new(),
            &sink,
        );

        assert!(result.is_err());
        assert_eq!(firmware_reset.calls.get(), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_READY), 2);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESET), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESPONSE), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_POST_RELOAD), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_RAW), 1);
    }

    #[test]
    fn open_controller_stops_when_firmware_reload_fails() {
        struct FailingFirmwareReset {
            calls: core::cell::Cell<u32>,
        }

        impl FirmwareReset for FailingFirmwareReset {
            fn reload(&self) -> FirmwareResetOutcome {
                self.calls.set(self.calls.get() + 1);
                FirmwareResetOutcome::Failed {
                    reason: FirmwareResetFailure::Timeout,
                }
            }
        }

        let mapper = MockMapper { grant: true };
        let dma = MockDmaHost;
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
        );
        let bus = OrderingMockBus::not_loaded();
        let firmware_reset = FailingFirmwareReset {
            calls: core::cell::Cell::new(0),
        };
        let sink = RecordingSink::new();
        let result = open_controller(
            &host,
            &bus,
            0x2_0000_0000,
            (0xc000_0000, 0x4000_0000),
            &firmware_reset,
            &NoDelay::new(),
            &sink,
        );

        assert_eq!(result.err(), Some(DriverError::DeviceFault));
        assert_eq!(firmware_reset.calls.get(), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_READY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESET), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESPONSE), 0);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 0);
        assert_eq!(sink.count(USB_KEYBOARD_POST_RELOAD), 0);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_RAW), 0);
        assert_eq!(sink.errors(), 1);
    }

    #[test]
    fn firmware_reload_is_skipped_when_version_is_already_loaded() {
        struct LoadedBus;

        impl Bus for LoadedBus {
            fn enumerate(&self, _out: &mut [BusDevice]) -> Result<usize, DriverError> {
                Ok(0)
            }
        }

        impl PciBus for LoadedBus {
            fn map_bar_window(
                &self,
                _bdf: u64,
                _bar_index: u8,
                _mapper: &dyn MmioMapper,
            ) -> Result<RegisterWindow, DriverError> {
                Err(DriverError::Unsupported)
            }

            fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
                Err(DriverError::Unsupported)
            }

            fn assign_bar(
                &self,
                _bdf: u64,
                _bar_index: u8,
                _window_base: u64,
                _window_size: u64,
            ) -> Result<u64, DriverError> {
                Err(DriverError::Unsupported)
            }

            fn read_config(&self, _bdf: u64, offset: u16) -> Result<u32, DriverError> {
                if offset == VL805_FW_VERSION_OFFSET {
                    Ok(0x0001_38c0)
                } else {
                    Ok(0)
                }
            }

            fn describe_function(
                &self,
                _bdf: u64,
                _parent_id: u32,
                _node_id: u32,
            ) -> Result<HwNode, DriverError> {
                Err(DriverError::Unsupported)
            }
        }

        struct PanicFirmwareReset;

        impl FirmwareReset for PanicFirmwareReset {
            fn reload(&self) -> FirmwareResetOutcome {
                panic!("firmware reload must be skipped when config 0x50 is non-zero");
            }
        }

        let sink = RecordingSink::new();
        assert!(ensure_firmware_loaded(
            &LoadedBus,
            &NoDelay::new(),
            &PanicFirmwareReset,
            &sink
        ));
        assert_eq!(sink.count(USB_KEYBOARD_FW_READY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESET), 0);
    }
}
