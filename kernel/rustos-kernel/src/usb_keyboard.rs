//! VL805/xHCI USB-keyboard composition (`plans/PI.md` P10).
//!
//! On the Pi 4 (BCM2711) the USB-A ports hang off a VL805 xHCI controller
//! behind the `SoC`'s PCIe root complex, whose link ships **down** and
//! whose config space is windowed (not flat ECAM). Bringing a keyboard to
//! the video-console login composes four loadable driver crates:
//!
//! 1. [`rustos_drv_bus_pcie_brcm`] resets the root complex and trains its
//!    link with the discovered address windows;
//! 2. [`rustos_drv_bus_pci::mechanism_brcm`] enumerates the VL805 over the
//!    windowed config accessor;
//! 3. [`rustos_drv_bus_usb`] maps the BAR, carves DMA, and brings xHCI up;
//! 4. [`rustos_hid`] decodes reports into [`KeyInput`] records
//!    and hands each to the input-focus arbiter via [`ArbiterConsoleSink`]
//!    (`AGENTS.md` §17.4).
//!
//! A driver may not name another (`AGENTS.md` §17.4); the image-assembly
//! binary (`rustos-kernel`) is the one place permitted to name them all, so
//! the composition lives here. The engine is architecture-neutral (only
//! `lib/abi` seams + the discovered [`HwNode`]) and host-tested; the aarch64
//! boot path supplies the concrete [`DriverHost`] and [`Delay`].
//!
//! # No QEMU vertical
//!
//! QEMU models no Pi PCIe/USB (`AGENTS.md` §0.4), so host tests prove the
//! composition up to the controller hand-off (the mock window faults there);
//! live link training and a keyboard driving the login are metal-only.

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::dma::{DmaHost, DmaSlab};
use rustos_abi::input::KeyInput;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwNode, MmioMapper, PciBus, RegisterWindow,
};
use rustos_caps::CapabilitySet;
use rustos_drv_bus_pcie_brcm::{
    self as pcie_brcm, BringUpTiming, Delay, InboundWindowReadback, OutboundWindowReadback,
};
// The discovered-node parsing now lives in the PCIe device's own driver
// crate (`drivers/bus/pcie_brcm`), beside the link-training engine it feeds
// (`AGENTS.md` §2.2 / §2.21 — it is hwtree parsing, not kernel
// orchestration). Re-exported so the composition keeps one definition; the
// autonomous `wiring::bring_up_from_node` floor entry consumes it directly.
pub use rustos_drv_bus_pcie_brcm::wiring::PcieBringup;
use rustos_hid::{BootKeyboard, ConsoleSink};
use rustos_kernel_core::InputFocus;
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_usb::device::UsbDevice;
use rustos_usb::{Xhci, XhciOpenError, DEFAULT_POLL_BUDGET};
use rustos_util::fmt::format_hex_u64;
// The VL805 firmware-reset vocabulary now lives in the device's own driver
// crate (`drivers/bus/usb/vl805`), reached over the `lib/abi` `MailboxChannel`
// seam (`AGENTS.md` §2.20 / §2.2 / §17.4). This composition consumes those
// types; the in-kernel `FirmwareReset` seam below is the host's reactive
// wrapper the bring-up calls (the kernel owns the mailbox *mechanism*).
use rustos_drv_bus_usb_vl805::{FirmwareResetOutcome, VL805_FIRMWARE_DEV_ADDR};

/// Audit event: a progress/failure milestone of the VL805 USB-keyboard
/// bring-up chain (PCIe link training, xHCI bring-up, root-hub
/// enumeration), so a metal capture shows which stage stalls.
const USB_KEYBOARD_BRINGUP: EventId = EventId(4101);

/// Audit event: a USB device enumerated on the VL805 root hub. Carries its
/// vendor/product id and assigned xHCI slot.
const USB_KEYBOARD_DEVICE: EventId = EventId(4102);

/// Audit event: the optional `VideoCore` VL805 firmware reload fallback,
/// issued once when config `0x50` (firmware version) stays zero. One-shot,
/// best-effort: a failure is logged but does not stop bring-up; the
/// fail-closed gate is [`Xhci::open`] (`AGENTS.md` §2.9).
const USB_KEYBOARD_FW_RESET: EventId = EventId(4108);

/// Audit event: a function seen by the one-shot PCIe configuration scan
/// (plus a summary count). A healthy Pi 4 shows two: the root complex
/// (`14e4:2711`, class `0604`) and the VL805 USB host (`1106:3483`, class
/// `0c03`).
const USB_KEYBOARD_PCI_SCAN: EventId = EventId(4104);

/// Audit event: the one-shot xHCI DMA carve (base/length/aperture) and
/// capability-block geometry (BAR window length, `CAPLENGTH`/`DBOFF`/
/// `RTSOFF`, `MaxSlots`/`MaxPorts`/`AC64`/`CSZ`) read after mapping the
/// BAR, so an `out_of_range` bring-up localises to a concrete value.
const USB_KEYBOARD_GEOMETRY: EventId = EventId(4106);

/// Audit event: the raw capability-register dwords (`CAPLENGTH`/
/// `HCIVERSION`, `HCSPARAMS1`, `HCCPARAMS1`, `DBOFF`, `RTSOFF`) read off
/// the BAR before [`Xhci::open`] validates them, so a capture shows
/// whether the BAR decodes and the exact `CAPLENGTH` driving a refusal.
const USB_KEYBOARD_CAPS_RAW: EventId = EventId(4107);

/// Audit event: the bounded wait for the VL805 capability block to come
/// live after firmware loads. Records reads taken (`polls_hex`), the final
/// header dword, and whether it became live (`ready_hex`). Bounded by
/// elapsed wall time ([`CAPS_READY_BUDGET_US`]); fails closed at
/// [`Xhci::open`].
const USB_KEYBOARD_CAPS_READY: EventId = EventId(4109);

/// Audit event: a read-back of configuration space after BAR assignment
/// and command-enable: the bridge bus numbers (`0x18`), Memory Base/Limit
/// (`0x20`) and command/status (`0x04`), and the VL805's command/status
/// (`0x04`), BAR0 (`0x10`), BAR1 (`0x14`) — to show which write stuck. A
/// faulting read renders an all-ones sentinel and is not propagated.
const USB_KEYBOARD_CONFIG: EventId = EventId(4110);

/// Audit event: the response word from the `VideoCore` `NOTIFY_XHCI_RESET`
/// tag (normally echoes the VL805 address `0x10_0000`). Diagnostic only,
/// never authority.
const USB_KEYBOARD_FW_RESPONSE: EventId = EventId(4113);

/// Audit event: a read-back of the outbound (CPU→PCIe) memory-window
/// registers (`MEM_WIN0_LO`/`HI`, `BASE_LIMIT`, `BASE_HI`, `LIMIT_HI`) and
/// link `STATUS` after the link trains, to show whether the window holds
/// the programmed CPU/PCIe bases and the link is up. A faulting read
/// renders an all-ones sentinel and is not propagated.
const USB_KEYBOARD_OUTBOUND: EventId = EventId(4111);

/// Audit event: a re-read of the VL805's vendor/device (`0x00`),
/// command/status (`0x04`), BAR0 (`0x10`), BAR1 (`0x14`) and the mapped
/// BAR's `CAPLENGTH`/`HCIVERSION` after the firmware-version wait and BAR
/// settle, so a capture distinguishes "present but no firmware" from
/// "firmware-loaded but still not decoding". A faulting read renders an
/// all-ones sentinel and is not propagated.
const USB_KEYBOARD_POST_RELOAD: EventId = EventId(4114);

/// Audit event: the per-phase wall-time breakdown of the PCIe
/// root-complex `bring_up`, in microseconds, so a capture pins any stall
/// to the exact MMIO group: `reset_swinit_us` (releasing the bridge
/// `sw_init`), `reset_settle_us` (post-de-reset MISC settle), `config_us`
/// (MISC + type-1 bridge config), `linkwait_us` (the `PERST#`-deassert
/// retrain settle + bounded link-up poll) and `link_polls`. The bridge
/// reset is released before touching MISC, else the MISC access
/// master-aborts on the `SoC` bus completion timeout (~10.8 s).
const USB_KEYBOARD_BRINGUP_TIMING: EventId = EventId(4117);

/// Audit event: the bounded wait for the VL805's XHCI MCU firmware
/// version ([`VL805_FW_VERSION_OFFSET`] `0x50`) to read non-zero after the
/// link trains. The version is in configuration space (reachable while the
/// BAR still aborts), so it is the working readiness signal. Records
/// `polls_hex`, `fw_version_hex`, `ready_hex`; bounded by
/// [`FW_LOADED_BUDGET_US`], fails closed at [`Xhci::open`].
const USB_KEYBOARD_FW_READY: EventId = EventId(4118);

/// Audit event: the firmware-version gate decision
/// (`firmware_loaded_hex`). A zero version or failed reload no longer
/// aborts bring-up: the authoritative xHCI liveness signal is the
/// capability block, so the bring-up proceeds to [`wait_for_caps_ready`]
/// and [`Xhci::open`], the real fail-closed gate (`AGENTS.md` §2.9).
const USB_KEYBOARD_FW_GATE: EventId = EventId(4123);

/// Audit event: the PCIe root-port error/status read **read-only** after
/// the `NOTIFY_XHCI_RESET` reload — bridge command/status (`0x04`), bridge
/// secondary status (`0x1C`), VL805 command/status (`0x04`). A set
/// Received-Master-Abort means the firmware-load could not reach the VL805
/// over PCIe, pinning a hang to the bus rather than the mailbox. A
/// post-reload snapshot, not a delta (no write issued).
const USB_KEYBOARD_PCIE_ERR: EventId = EventId(4124);

/// Audit event: one record per root-hub port's `PORTSC` when the scan
/// finds no connected device, carrying the raw `portsc_hex` and decoded
/// `ccs_hex`/`pp_hex`/`ped_hex`/`speed_hex`. `pp_hex=1 ccs_hex=0` means
/// power is asserted but nothing is attached; `pp_hex=0` means the power
/// write did not stick. A faulting read is logged as an all-ones sentinel.
const USB_KEYBOARD_ROOT_PORTS: EventId = EventId(4125);

/// Audit event: the enumeration step last entered when
/// [`UsbDevice::enumerate_first_connected`] errors. `stage_hex` is
/// [`UsbDevice::enum_stage`] and `completion_hex` is
/// [`UsbDevice::last_completion_code`] (the last event's raw xHCI
/// completion code): `stage_hex=0` is an empty hub; a later stage with
/// `completion_hex=0` is a stuck controller; a non-zero code is the device
/// answering with that error.
const USB_KEYBOARD_ENUM_STAGE: EventId = EventId(4126);

/// Audit event: the first keyboard report drained on the poll loop
/// (one-shot, with cumulative poll/event counts). Its presence proves the
/// interrupt-IN endpoint completes transfers; its absence while the
/// heartbeat climbs localises a silent keyboard to "never completes the
/// interrupt endpoint", distinct from [`USB_KEYBOARD_PUMP_ERROR`].
const USB_KEYBOARD_FIRST_REPORT: EventId = EventId(4129);

/// Audit event: the poll loop's `pump_once` returned an error. Logged when
/// the error *kind* changes, capped at
/// [`KeyboardPumpDiagnostics::MAX_ERROR_LOGS`] so a wedged controller
/// cannot flood the log. Carries the `DriverError` name and cumulative
/// poll/error counts.
const USB_KEYBOARD_PUMP_ERROR: EventId = EventId(4130);

/// Audit event: a periodic liveness heartbeat of the keyboard poll loop,
/// every [`KeyboardPumpDiagnostics::HEARTBEAT_POLLS`] polls and capped at
/// [`KeyboardPumpDiagnostics::MAX_HEARTBEATS`] so the log stays finite.
/// Polls climbing while events/errors stay zero proves the loop is alive
/// but the keyboard delivers no reports.
const USB_KEYBOARD_POLL_HEARTBEAT: EventId = EventId(4131);

/// Minimum post-`NOTIFY_XHCI_RESET` settle before polling config `0x50`
/// again; the vendor bring-up waits `200..1000 µs`, so this uses the lower bound and then
/// the existing bounded firmware-version wait handles the remainder.
const FW_RELOAD_SETTLE_US: u32 = 200;

/// Optional `VideoCore` firmware reload seam used when config `0x50` stays
/// zero after PCI/BAR setup.
pub trait FirmwareReset {
    /// Attempt the single `NOTIFY_XHCI_RESET` fallback.
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

/// Audit event: read-back of the **inbound** (PCIe→system-memory) viewport
/// registers after bring-up, to compare our translation against the
/// known-good `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`. A faulting read
/// renders the all-ones sentinel and is never propagated. One-shot at
/// bring-up (`AGENTS.md` §15.7 / §2.9 / §19.4).
const USB_KEYBOARD_INBOUND: EventId = EventId(4119);

/// Audit event: the **inbound** viewport registers **as the previous boot
/// stage (`start4.elf`) left them**, sampled before bring-up programs
/// `RC_BAR2`. `VideoCore`'s `NOTIFY_XHCI_RESET` load assumes a particular
/// `RC_BAR2` state (raspberrypi/firmware #1495), so comparing this entry
/// capture against the post-program `4119` read-back shows whether our
/// reprogramming diverges from that assumption. Faulting reads render the
/// all-ones sentinel. One-shot at bring-up (`AGENTS.md` §15.7 / §2.9 /
/// §19.4).
const USB_KEYBOARD_INBOUND_ENTRY: EventId = EventId(4120);

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

/// Bounded diagnostics for the forever-running keyboard poll loop.
///
/// Folds each `pump_once` result into cumulative counts and emits three
/// bounded audit events — a one-shot first-report (`4129`), an on-change
/// capped pump error (`4130`), and a capped heartbeat (`4131`) — so the log
/// stays finite while still pinning where the report path stalls. Holds no
/// authority; logging only.
#[derive(Debug, Default)]
pub struct KeyboardPumpDiagnostics {
    polls: u64,
    events: u64,
    errors: u64,
    first_report_logged: bool,
    last_error: Option<DriverError>,
    error_logs: u32,
    heartbeats: u32,
}

impl KeyboardPumpDiagnostics {
    /// Polls between two consecutive heartbeats. A diagnostic cadence, not
    /// a capacity (`AGENTS.md` §24.4): large enough that a healthy,
    /// frequently-yielding loop logs sparsely.
    const HEARTBEAT_POLLS: u64 = 1024;
    /// Maximum heartbeats ever emitted, bounding the log of a forever loop
    /// to a finite capture window (`AGENTS.md` §2.16 / §19.4).
    const MAX_HEARTBEATS: u32 = 32;
    /// Maximum pump-error records ever emitted, bounding a controller that
    /// faults every poll (`AGENTS.md` §2.16 / §19.4).
    const MAX_ERROR_LOGS: u32 = 16;

    /// A fresh diagnostics state: nothing polled, nothing logged.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            polls: 0,
            events: 0,
            errors: 0,
            first_report_logged: false,
            last_error: None,
            error_logs: 0,
            heartbeats: 0,
        }
    }

    /// Fold one `pump_once` result into the counts and emit any bounded
    /// audit event it triggers.
    ///
    /// `Ok(drained)` adds to the event count and, the first time it is
    /// non-zero, emits the one-shot first-report event; `Err` adds to the
    /// error count and emits the pump-error event when the error kind
    /// changes (capped). Either way a heartbeat is emitted on the cadence
    /// boundary (capped). Never panics and never allocates (`AGENTS.md`
    /// §2.9).
    pub fn record(&mut self, result: Result<usize, DriverError>, sink: &dyn Sink) {
        self.polls = self.polls.saturating_add(1);
        match result {
            Ok(drained) => {
                self.events = self
                    .events
                    .saturating_add(u64::try_from(drained).unwrap_or(u64::MAX));
                if drained > 0 {
                    // Genuine progress clears the last-error latch so a
                    // fault that recurs after recovery is logged again.
                    self.last_error = None;
                    if !self.first_report_logged {
                        self.first_report_logged = true;
                        self.log_first_report(sink);
                    }
                }
            }
            Err(err) => {
                self.errors = self.errors.saturating_add(1);
                if self.last_error != Some(err) && self.error_logs < Self::MAX_ERROR_LOGS {
                    self.last_error = Some(err);
                    self.error_logs += 1;
                    self.log_error(sink, err);
                }
            }
        }
        if self.polls % Self::HEARTBEAT_POLLS == 0 && self.heartbeats < Self::MAX_HEARTBEATS {
            self.heartbeats += 1;
            self.log_heartbeat(sink);
        }
    }

    fn log_first_report(&self, sink: &dyn Sink) {
        let mut polls = [0u8; 16];
        let mut events = [0u8; 16];
        log(
            sink,
            &Event {
                level: Level::Info,
                id: USB_KEYBOARD_FIRST_REPORT,
                message: "usb-keyboard: first keyboard report drained on the poll loop",
                fields: &[
                    Field {
                        key: "polls_hex",
                        value: format_hex_u64(self.polls, &mut polls),
                    },
                    Field {
                        key: "events_hex",
                        value: format_hex_u64(self.events, &mut events),
                    },
                ],
            },
        );
    }

    fn log_error(&self, sink: &dyn Sink, err: DriverError) {
        let mut polls = [0u8; 16];
        let mut errors = [0u8; 16];
        log(
            sink,
            &Event {
                level: Level::Error,
                id: USB_KEYBOARD_PUMP_ERROR,
                message: "usb-keyboard: keyboard poll-loop pump_once returned an error",
                fields: &[
                    Field {
                        key: "err",
                        value: driver_error_name(err),
                    },
                    Field {
                        key: "polls_hex",
                        value: format_hex_u64(self.polls, &mut polls),
                    },
                    Field {
                        key: "errors_hex",
                        value: format_hex_u64(self.errors, &mut errors),
                    },
                ],
            },
        );
    }

    fn log_heartbeat(&self, sink: &dyn Sink) {
        let mut polls = [0u8; 16];
        let mut events = [0u8; 16];
        let mut errors = [0u8; 16];
        log(
            sink,
            &Event {
                level: Level::Info,
                id: USB_KEYBOARD_POLL_HEARTBEAT,
                message: "usb-keyboard: keyboard poll-loop heartbeat",
                fields: &[
                    Field {
                        key: "polls_hex",
                        value: format_hex_u64(self.polls, &mut polls),
                    },
                    Field {
                        key: "events_hex",
                        value: format_hex_u64(self.events, &mut events),
                    },
                    Field {
                        key: "errors_hex",
                        value: format_hex_u64(self.errors, &mut errors),
                    },
                ],
            },
        );
    }
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

/// Upper bound on functions the diagnostic scan reports: a defence bound
/// (`AGENTS.md` §24.4), not a capacity. A healthy Pi 4 bus has two.
const SCAN_REPORT_LIMIT: usize = 32;

/// Enumerate PCIe configuration space once and log every responding
/// function, so a capture shows whether the VL805 answers config reads.
/// Purely diagnostic: an enumeration error is logged, not propagated (the
/// authoritative search is `open_discovered`).
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

/// Log the device-shared DMA carve against the inbound-aperture bound it
/// must lie below. `dma_phys` is the device-visible (PCIe-space) base.
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

/// Log the controller's capability-block geometry read by [`Xhci::open`],
/// so a capture shows whether a register offset lands past the mapped BAR.
fn log_xhci_geometry(sink: &dyn Sink, xhci: &Xhci<RegisterWindow>) {
    let mut cap_buf = [0u8; 16];
    let mut ver_buf = [0u8; 16];
    let mut db_buf = [0u8; 16];
    let mut rt_buf = [0u8; 16];
    let mut slots_buf = [0u8; 16];
    let mut ports_buf = [0u8; 16];
    let mut scratch_buf = [0u8; 16];
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
                // Max Scratchpad Buffers (xHCI §4.20): page-sized buffers
                // `UsbDevice::start` reserves and points `DCBAA[0]` at.
                Field {
                    key: "max_scratchpad_hex",
                    value: format_hex_u64(
                        u64::from(xhci.max_scratchpad_buffers()),
                        &mut scratch_buf,
                    ),
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

/// Maximum elapsed wall time (µs) to wait for the VL805 register block
/// after the firmware-version wait (~256 ms). A defence bound
/// (`AGENTS.md` §2.1 / §24.4), not a capacity. A *time* budget, not a poll
/// count: each un-decoded BAR read master-aborts and stalls tens of ms on
/// the BCM2711, so a poll-count budget would inflate the wait wildly.
const CAPS_READY_BUDGET_US: u64 = 256_000;

/// Delay between capability-header readiness polls, in microseconds.
const CAPS_READY_POLL_INTERVAL_US: u32 = 1_000;

/// Maximum elapsed wall time (µs) to wait for the VL805's XHCI MCU
/// firmware version ([`VL805_FW_VERSION_OFFSET`]) to read non-zero after
/// the link trains (~2 s). A defence bound (`AGENTS.md` §2.1 / §24.4): a
/// healthy board loads within a few hundred ms; one that never loads fails
/// closed. These are config-space reads (no master-abort), so the budget
/// tracks firmware-load latency, not read cost.
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
/// xHCI controller, rather than an uninitialised/aborted bus pattern. A
/// live header has a plausible `CAPLENGTH` (≥ `0x20`) and `HCIVERSION`
/// (xHCI 0.96‥1.2); the `0`/UR-sentinel/`dead_dead` pre-firmware patterns
/// all fail. Takes the [`read_cap_dword`] `u64` (no truncating cast).
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
/// [`CAPS_READY_BUDGET_US`] of elapsed wall time, logging the outcome once
/// (`4109`). Read-only; returns whether the block became live. A
/// controller that never decodes fails closed at [`Xhci::open`]. The bound
/// is wall time, not a poll count, because each un-decoded BAR read
/// master-aborts and stalls tens of ms on the BCM2711.
fn wait_for_caps_ready(window: &RegisterWindow, delay: &dyn Delay, sink: &dyn Sink) -> bool {
    use rustos_usb::regs;

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
/// not loaded); any non-zero value means loaded.
fn firmware_version_is_loaded(version: u64) -> bool {
    version != 0 && version != 0xFFFF_FFFF
}

/// Poll the VL805's XHCI MCU firmware version
/// ([`VL805_FW_VERSION_OFFSET`]) over configuration space until it reads a
/// non-zero build id, bounded by [`FW_LOADED_BUDGET_US`] of elapsed wall
/// time, logging the outcome once (`4118`). Read-only; returns whether the
/// firmware loaded. The config path works on metal while the MMIO BAR
/// master-aborts until the MCU firmware is loaded, so this is the working
/// readiness signal.
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

/// Log the firmware-version gate decision (`4123`) — whether the VL805's
/// configuration-space firmware-version register became loaded — recording
/// that the bring-up proceeds to probe the controller's own BAR capability
/// block regardless (see [`USB_KEYBOARD_FW_GATE`]).
fn log_firmware_gate(sink: &dyn Sink, firmware_loaded: bool) {
    let mut loaded_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_FW_GATE,
            message:
                "usb-keyboard: firmware-version gate evaluated; probing the controller bar regardless",
            fields: &[Field {
                key: "firmware_loaded_hex",
                value: format_hex_u64(u64::from(firmware_loaded), &mut loaded_buf),
            }],
        },
    );
}

/// Log every root-hub port's `PORTSC` (`4125`), one record per port, when
/// the root-hub scan found no connected device. Read-only; reflects the
/// post-power state, pinning whether power stuck (`pp_hex`) and whether
/// any port sees a device (`ccs_hex`). A faulting read renders the
/// all-ones sentinel.
fn log_root_ports(sink: &dyn Sink, usb: &mut UsbDevice<RegisterWindow, DmaSlab>) {
    use rustos_usb::regs;

    let count = usb.root_port_count();
    for port in 1..=count {
        let raw = usb.root_port_status_raw(port).unwrap_or(u32::MAX);
        let ccs = u64::from(raw & regs::PORTSC_CCS != 0);
        let pp = u64::from(raw & regs::PORTSC_PP != 0);
        let ped = u64::from(raw & regs::PORTSC_PED != 0);
        let speed = u64::from((raw >> regs::PORTSC_SPEED_SHIFT) & regs::PORTSC_SPEED_MASK);
        let mut port_buf = [0u8; 16];
        let mut portsc_buf = [0u8; 16];
        let mut ccs_buf = [0u8; 16];
        let mut pp_buf = [0u8; 16];
        let mut ped_buf = [0u8; 16];
        let mut speed_buf = [0u8; 16];
        log(
            sink,
            &Event {
                level: Level::Info,
                id: USB_KEYBOARD_ROOT_PORTS,
                message: "usb-keyboard: root-hub port status after the empty scan",
                fields: &[
                    Field {
                        key: "port_hex",
                        value: format_hex_u64(u64::from(port), &mut port_buf),
                    },
                    Field {
                        key: "portsc_hex",
                        value: format_hex_u64(u64::from(raw), &mut portsc_buf),
                    },
                    Field {
                        key: "ccs_hex",
                        value: format_hex_u64(ccs, &mut ccs_buf),
                    },
                    Field {
                        key: "pp_hex",
                        value: format_hex_u64(pp, &mut pp_buf),
                    },
                    Field {
                        key: "ped_hex",
                        value: format_hex_u64(ped, &mut ped_buf),
                    },
                    Field {
                        key: "speed_hex",
                        value: format_hex_u64(speed, &mut speed_buf),
                    },
                ],
            },
        );
    }
}

/// Log which enumeration step the root-hub bring-up last entered (`4126`)
/// when [`UsbDevice::enumerate_first_connected`] fails: the
/// [`UsbDevice::enum_stage`] breadcrumb with the last event's raw xHCI
/// completion code ([`UsbDevice::last_completion_code`]), pinning which
/// operation faulted and whether it timed out. Read-only.
fn log_enum_stage(sink: &dyn Sink, usb: &UsbDevice<RegisterWindow, DmaSlab>) {
    let stage = u64::from(usb.enum_stage().as_u8());
    let completion = u64::from(usb.last_completion_code());
    let mut stage_buf = [0u8; 16];
    let mut completion_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_ENUM_STAGE,
            message: "usb-keyboard: enumeration stage the root-hub bring-up last entered",
            fields: &[
                Field {
                    key: "stage_hex",
                    value: format_hex_u64(stage, &mut stage_buf),
                },
                Field {
                    key: "completion_hex",
                    value: format_hex_u64(completion, &mut completion_buf),
                },
            ],
        },
    );
}

/// Dump the first capability-register dwords straight off the mapped VL805
/// BAR before [`Xhci::open`] interprets them, so a capture shows whether
/// the BAR decodes and the exact `CAPLENGTH` byte behind an `out_of_range`
/// refusal. Read-only.
fn log_raw_caps(sink: &dyn Sink, window: &RegisterWindow) {
    use rustos_usb::regs;

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

/// Log the controller's outbound (CPU→PCIe) memory-window registers
/// (`MEM_WIN0_LO`/`HI`, `BASE_LIMIT`, `BASE_HI`, `LIMIT_HI`) and link
/// `STATUS`, to show whether the window holds the programmed bases and the
/// link is up — memory takes the outbound path while config (which reads
/// back fine) takes the internal `EXT_CFG` window. Values produced
/// fail-closed by [`pcie_brcm::BrcmPcieRc::outbound_window_readback`].
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
/// (`RC_BAR1_LO`, `RC_BAR2_LO`/`HI`, `RC_BAR3_LO`) plus link `STATUS`
/// (`4119`), so a capture can compare the translation with the known-good
/// `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`. Values produced fail-closed
/// by [`pcie_brcm::BrcmPcieRc::inbound_window_readback`].
fn log_inbound_window(sink: &dyn Sink, rb: InboundWindowReadback) {
    log_inbound_readback(
        sink,
        USB_KEYBOARD_INBOUND,
        "usb-keyboard: pcie inbound (pcie->memory) viewport read-back",
        rb,
    );
}

/// Log the inbound viewport registers **as the previous boot stage left
/// them**, before bring-up programs `RC_BAR2` (`4120`). Comparing the
/// firmware's own `RC_BAR2` against the post-program read-back (`4119`,
/// [`log_inbound_window`]) shows whether bring-up moves the inbound window
/// away from the state `VideoCore` assumes for the firmware load. Values
/// produced fail-closed by [`pcie_brcm::BrcmPcieRc::entry_inbound_window`].
fn log_entry_inbound_window(sink: &dyn Sink, rb: InboundWindowReadback) {
    log_inbound_readback(
        sink,
        USB_KEYBOARD_INBOUND_ENTRY,
        "usb-keyboard: pcie inbound (pcie->memory) viewport as firmware left it (pre-program)",
        rb,
    );
}

/// Shared body for the inbound-viewport diagnostics: render `rb`'s
/// `RC_BAR1`/`RC_BAR2`/`RC_BAR3` and link-status registers under
/// `id`/`message`. One definition for both the entry (`4120`) and
/// post-program (`4119`) captures (`AGENTS.md` §2.2).
fn log_inbound_readback(
    sink: &dyn Sink,
    id: EventId,
    message: &'static str,
    rb: InboundWindowReadback,
) {
    let mut bar1_buf = [0u8; 16];
    let mut bar2lo_buf = [0u8; 16];
    let mut bar2hi_buf = [0u8; 16];
    let mut bar3_buf = [0u8; 16];
    let mut misc_ctrl_buf = [0u8; 16];
    let mut status_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id,
            message,
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
                    key: "misc_ctrl_hex",
                    value: format_hex_u64(u64::from(rb.misc_ctrl), &mut misc_ctrl_buf),
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
/// (`4117`), so a capture localises a multi-second bring-up to the exact
/// MMIO access. The `*_us` spans (`reset_swinit_us`, `reset_settle_us`,
/// `config_us`, `linkwait_us`) sum to the whole `bring_up`, so any one
/// carrying seconds names the stalling access. `entry_rgr1_sw_init_hex` is
/// the raw `RGR1_SW_INIT_1` at entry: a set `PERST#` bit means the prior
/// boot stage already held the VL805 in fundamental reset.
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

/// The VL805's XHCI MCU Firmware Version PCI configuration-space register
/// (offset `0x50`, read-only, reset `0`): `0` while the MCU has no
/// firmware, a non-zero build id once loaded. It lives in config space
/// (reachable on metal) unlike the MMIO BAR, so it measures the
/// firmware-load outcome directly.
const VL805_FW_VERSION_OFFSET: u16 = 0x50;

/// Read a configuration-space dword back, rendering a faulting read as
/// the all-ones sentinel: the readback is diagnostic, never propagated
/// (`AGENTS.md` §2.9).
fn read_config_or_sentinel(bus: &dyn PciBus, bdf: u64, offset: u16) -> u64 {
    u64::from(bus.read_config(bdf, offset).unwrap_or(0xFFFF_FFFF))
}

/// Read configuration space back after BAR assignment and command-enable,
/// before [`Xhci::open`]: the root port's bus numbers (`0x18`), Memory
/// Base/Limit (`0x20`), command/status (`0x04`), and the VL805's
/// command/status (`0x04`), BAR0 (`0x10`), BAR1 (`0x14`) and firmware
/// version ([`VL805_FW_VERSION_OFFSET`], `0x50`) — so a capture shows which
/// write stuck. A non-zero firmware version means the boot chain left it
/// resident; `0` means the BAR is expected to stay dark. Read-only.
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

/// Log the PCIe root-port + VL805 error/status registers
/// ([`USB_KEYBOARD_PCIE_ERR`]) after the `NOTIFY_XHCI_RESET` reload: the
/// bridge command/status (`0x04`), bridge secondary status (`0x1C`), and
/// VL805 command/status (`0x04`). A set Received-Master-Abort in the
/// secondary status pins a hang on the bus rather than the mailbox.
/// Read-only (RW1C abort bits left untouched, so a snapshot not a delta).
fn log_bridge_error_status(sink: &dyn Sink, bus: &dyn PciBus) {
    // The root port presents as the bus-0 bridge; the VL805 is the lone
    // device on the secondary bus the root-complex bring-up named.
    const BRIDGE_BDF: u64 = 0;
    let vl805_bdf = vl805_bdf();

    let mut brstat_buf = [0u8; 16];
    let mut secstat_buf = [0u8; 16];
    let mut vlstat_buf = [0u8; 16];
    log(
        sink,
        &Event {
            level: Level::Info,
            id: USB_KEYBOARD_PCIE_ERR,
            message: "usb-keyboard: pcie root-port + vl805 error/status after firmware reload",
            fields: &[
                Field {
                    key: "bridge_command_status_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, BRIDGE_BDF, 0x04),
                        &mut brstat_buf,
                    ),
                },
                Field {
                    key: "bridge_secondary_status_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, BRIDGE_BDF, 0x1C),
                        &mut secstat_buf,
                    ),
                },
                Field {
                    key: "vl805_command_status_hex",
                    value: format_hex_u64(
                        read_config_or_sentinel(bus, vl805_bdf, 0x04),
                        &mut vlstat_buf,
                    ),
                },
            ],
        },
    );
}

/// Re-read the VL805's config space and capability header after the
/// firmware-version wait and readiness settle
/// ([`USB_KEYBOARD_POST_RELOAD`]). Compared with [`log_config_readback`]
/// (captured before the wait), it distinguishes "present but no firmware"
/// from "firmware-loaded yet not decoding". The decisive field is
/// `vl805_fw_version_hex` ([`VL805_FW_VERSION_OFFSET`]): non-zero proves
/// the firmware is resident, `0` means leave the controller untouched.
/// Read-only.
fn log_post_reload_state(sink: &dyn Sink, bus: &dyn PciBus, window: &RegisterWindow) {
    use rustos_usb::regs;

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
/// stages — map the BAR and carve DMA
/// ([`map_controller`](rustos_drv_bus_usb::wiring::map_controller)),
/// [`Xhci::open`] (capability block + reset), then [`UsbDevice::start`]
/// (DMA program + run) — logging geometry between the map and bring-up and
/// reporting each stage's failure distinctly.
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
    // Read config space back now the BAR is assigned and command enabled,
    // to show which write stuck.
    log_config_readback(sink, bus);
    // Wait for firmware over the working config-space `0x50` register, not
    // the aborting BAR; if it stays zero, issue one mailbox fallback. This
    // is best-effort and diagnostic — it does NOT gate bring-up. The
    // authoritative fail-closed gate is `Xhci::open` below.
    let firmware_loaded = ensure_firmware_loaded(bus, delay, firmware_reset, sink);
    log_firmware_gate(sink, firmware_loaded);
    // Snapshot root-port error/status after the reload (`4124`): a latched
    // secondary-status master-abort pins a dropped reload on the bus.
    log_bridge_error_status(sink, bus);
    // Give the BAR a bounded window to present a live capability block
    // before `Xhci::open` interprets it. Non-fatal: a controller that never
    // decodes fails closed there.
    wait_for_caps_ready(&mapped.window, delay, sink);
    // Re-read config + capability header after the settle, for comparison
    // with the pre-wait `4110` read-back.
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

/// The outcome of a successful [`bring_up_keyboard`]: the polled
/// [`KeyboardChain`] plus the enumerated HID device described as a
/// discovered [`HwNode`] ([`UsbDevice::describe_device`]), so the caller
/// can re-match it against the driver catalogue and admit the HID driver
/// through the signed load gate before feeding input (`plans/PI.md` P10
/// 5c-ii — the §18 growable-runtime-tree re-autoload step).
pub struct BroughtUpKeyboard {
    /// The boot keyboard, polled with `pump_once` in the service loop.
    pub keyboard: KeyboardChain,
    /// The enumerated HID device as a discovered child node, carrying the
    /// `vid:pid` + interface-class match key read during enumeration
    /// (`AGENTS.md` §18.5 — never fabricated).
    pub hid_node: HwNode,
}

/// Synthetic hardware-tree ids for the [`BroughtUpKeyboard::hid_node`].
/// The node↔driver match resolves on the node's match keys, not its ids
/// (`AGENTS.md` §18.3), so these are tree-local placeholders.
const HID_NODE_PARENT_ID: u32 = 1;
const HID_NODE_ID: u32 = 2;

/// A [`DriverHost`] view for the in-kernel VL805 chain: the bus-driver
/// task's capabilities plus the kernel's capability-gated MMIO mapper and
/// per-driver DMA host. Every [`MmioMapper::map_window`] /
/// [`DmaHost::alloc_dma_zeroed`] call is re-checked kernel-side against
/// those capabilities (`AGENTS.md` §5.4), so the host cannot widen its own
/// authority.
pub struct ChainHost<'a> {
    capabilities: CapabilitySet,
    mmio: &'a dyn MmioMapper,
    dma: &'a dyn DmaHost,
}

impl<'a> ChainHost<'a> {
    /// Build the view over the bus-driver task's `capabilities` and the
    /// kernel's `mmio` mapper and `dma` host.
    #[must_use]
    pub fn new(
        capabilities: CapabilitySet,
        mmio: &'a dyn MmioMapper,
        dma: &'a dyn DmaHost,
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

    fn dma_host(&self) -> Option<&dyn DmaHost> {
        Some(self.dma)
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(self.mmio)
    }
}

/// A [`ConsoleSink`] that injects produced keyboard records into the kernel
/// input-focus arbiter (`AGENTS.md` §17.4 / §20, `plans/PI.md` P11).
///
/// The HID producer emits one [`KeyInput`] record per key edge; this sink
/// is the in-kernel counterpart of the `key_inject` syscall. The arbiter
/// then routes by focus: a press to the video console's tty bytes with the
/// text console foreground, or the whole record to the keyboard channel
/// with the desktop foreground. It never blocks.
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
        // The producer always writes exactly one whole record. A malformed
        // record or a fail-closed sink surfaces as `DeviceFault` rather
        // than dropping input silently (`AGENTS.md` §2.9).
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
/// [`KeyboardChain`] is then polled with [`rustos_hid::pump_once`]
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
/// Each stage (link training, the PCIe configuration scan, xHCI bring-up,
/// root-hub enumeration) is logged on success and failure, so a capture
/// localises a silent keyboard to the stage that stalled. One-shot
/// bring-up diagnostics, never on the poll path.
pub fn bring_up_keyboard(
    host: &dyn DriverHost,
    bringup: &PcieBringup,
    firmware_reset: &dyn FirmwareReset,
    delay: &dyn Delay,
    sink: &dyn Sink,
) -> Result<BroughtUpKeyboard, DriverError> {
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
    // Diagnostics: split the bring-up wall time across its phases, and read
    // the outbound (CPU→PCIe) and inbound (PCIe→memory) windows back off the
    // trained register block. The inbound window is read both as the prior
    // boot stage left it (`4120`) and after bring-up (`4119`), since the
    // VideoCore firmware handoff assumes the `RC_BAR2` state it set at
    // power-on.
    log_bring_up_timing(sink, rc.bring_up_timing());
    log_outbound_window(sink, rc.outbound_window_readback());
    log_entry_inbound_window(sink, rc.entry_inbound_window());
    log_inbound_window(sink, rc.inbound_window_readback());
    // Reach the VL805 through the BCM2711 windowed config accessor over the
    // trained register window. It forwards config only to the single device
    // on the secondary bus, so the flat scan below never TLPs an absent
    // target (which would CPU-abort and wedge the boot).
    let bus = rustos_drv_bus_pci::mechanism_brcm(rc.into_regs(), pcie_brcm::regs::RC_SECONDARY_BUS);
    // Diagnostic: log every function before the controller search, to tell
    // "VL805 never answered config reads" from "enumerated but xHCI did not
    // come up".
    log_bus_scan(sink, &bus);
    // The DMA carve is bounded in the *device-visible* (PCIe) space the
    // descriptors carry: the exclusive top is `inbound_pcie_base +
    // inbound_size`, not the CPU-physical aperture top (`AGENTS.md` §5.4 —
    // the bound must match the address space it guards). Overflow is a
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
    // The bridge's outbound (CPU→PCIe) window in PCIe-bus space: the BAR is
    // assigned a size-aligned address inside it when firmware left it
    // unassigned, so the mapped window resolves to a real CPU address.
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
        "usb-keyboard: vl805 xhci controller online, enumerating boot keyboard",
    );
    // Bring up the boot keyboard, transparently descending one tier through
    // the Pi 4B's onboard 2109:3431 hub when the root-hub device is itself a
    // hub. The arch-neutral root→hub→downstream orchestration lives once in
    // `rustos_usb::device::UsbDevice::enumerate_boot_keyboard` (`AGENTS.md`
    // §2.2), so the keyboard is *discovered*, never a guessed port (§18); on
    // success `usb` is left pointed at the keyboard's slot so `BootKeyboard`
    // drains its reports, and any fault fails closed (§2.9).
    let descriptor = match usb.enumerate_boot_keyboard(delay) {
        Ok(descriptor) => descriptor,
        Err(err) => {
            log_stage_err(
                sink,
                "usb-keyboard: enumerating a boot keyboard failed",
                err,
            );
            // Pin which enumeration step faulted, then dump each root-hub
            // port's post-power `PORTSC` (power stuck? anything attached?).
            log_enum_stage(sink, &usb);
            log_root_ports(sink, &mut usb);
            return Err(err);
        }
    };
    // Read the assigned slot before `usb` is moved into the keyboard.
    let slot = usb.slot();
    log_enumerated_root_device(sink, descriptor, slot);
    let hid_node = describe_enumerated_hid(&usb, sink)?;
    Ok(BroughtUpKeyboard {
        keyboard: BootKeyboard::new(usb),
        hid_node,
    })
}

/// Log the `vid:pid` and assigned xHCI slot of the device enumerated on
/// the VL805 root hub ([`USB_KEYBOARD_DEVICE`]) — a one-shot bring-up
/// diagnostic, never on the poll path.
fn log_enumerated_root_device(
    sink: &dyn Sink,
    descriptor: rustos_usb::device::DeviceDescriptor,
    slot: u8,
) {
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
}

/// Describe the enumerated HID device as a discovered child [`HwNode`] for
/// the §18 re-match step (`plans/PI.md` P10 5c-ii), reading the `vid:pid` +
/// interface class captured during enumeration (never fabricated,
/// `AGENTS.md` §18.5), so the service can re-match it against the driver
/// catalogue and admit the HID driver through the signed load gate before
/// feeding the input arbiter.
///
/// # Errors
///
/// Surfaces [`UsbDevice::describe_device`]'s error fail-closed (logged),
/// most notably [`DriverError::NotFound`] if no HID interface was
/// enumerated.
fn describe_enumerated_hid(
    usb: &UsbDevice<RegisterWindow, DmaSlab>,
    sink: &dyn Sink,
) -> Result<HwNode, DriverError> {
    usb.describe_device(HID_NODE_PARENT_ID, HID_NODE_ID)
        .map_err(|err| {
            log_stage_err(
                sink,
                "usb-keyboard: describing the enumerated HID device for re-match failed",
                err,
            );
            err
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use core::ptr::NonNull;

    use rustos_abi::driver::dma::{DmaHost, PoolId};
    use rustos_abi::driver::mmio::MmioMapError;
    use rustos_abi::input::{KeyValue, Modifiers};
    use rustos_abi::{HwDeviceClass, HwResource};
    // The discovered-node parse the orchestration tests build a `PcieBringup`
    // from now lives in the PCIe device crate (`AGENTS.md` §2.2 / §2.21).
    use rustos_drv_bus_pcie_brcm::wiring::pcie_bringup_from_node;
    use rustos_drv_bus_usb_vl805::FirmwareResetFailure;
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

    #[test]
    fn pump_diagnostics_logs_the_first_report_only_once() {
        let sink = RecordingSink::new();
        let mut diag = KeyboardPumpDiagnostics::new();
        // Empty polls (the keyboard delivered no report) never claim one.
        for _ in 0..4 {
            diag.record(Ok(0), &sink);
        }
        assert_eq!(sink.count(USB_KEYBOARD_FIRST_REPORT), 0);
        // The first non-empty poll logs the one-shot first-report event...
        diag.record(Ok(2), &sink);
        assert_eq!(sink.count(USB_KEYBOARD_FIRST_REPORT), 1);
        // ...and never again, so a typing keyboard does not flood the log.
        diag.record(Ok(3), &sink);
        assert_eq!(sink.count(USB_KEYBOARD_FIRST_REPORT), 1);
    }

    #[test]
    fn pump_diagnostics_logs_a_pump_error_on_change_and_caps_it() {
        let sink = RecordingSink::new();
        let mut diag = KeyboardPumpDiagnostics::new();
        // A steady fault logs once, not on every poll (a wedged
        // controller faulting forever must not flood the log).
        for _ in 0..5 {
            diag.record(Err(DriverError::DeviceFault), &sink);
        }
        assert_eq!(sink.count(USB_KEYBOARD_PUMP_ERROR), 1);
        // A different error kind is genuinely new information, so it logs.
        diag.record(Err(DriverError::BadMagic), &sink);
        assert_eq!(sink.count(USB_KEYBOARD_PUMP_ERROR), 2);
        // Alternating distinct kinds are capped, bounding the log even if
        // the error churns every poll (`AGENTS.md` §2.16 / §19.4).
        let kinds = [DriverError::DeviceFault, DriverError::BadMagic];
        for i in 0..(KeyboardPumpDiagnostics::MAX_ERROR_LOGS as usize + 8) {
            diag.record(Err(kinds[i % 2]), &sink);
        }
        assert_eq!(
            sink.count(USB_KEYBOARD_PUMP_ERROR),
            KeyboardPumpDiagnostics::MAX_ERROR_LOGS as usize
        );
    }

    #[test]
    fn pump_diagnostics_emits_a_bounded_heartbeat() {
        let sink = RecordingSink::new();
        let mut diag = KeyboardPumpDiagnostics::new();
        let interval = KeyboardPumpDiagnostics::HEARTBEAT_POLLS;
        // Two full poll intervals produce exactly two heartbeats.
        for _ in 0..(interval * 2) {
            diag.record(Ok(0), &sink);
        }
        assert_eq!(sink.count(USB_KEYBOARD_POLL_HEARTBEAT), 2);
        // Beyond the cap the heartbeat stops, so the forever loop's log
        // stays finite (`AGENTS.md` §2.16 / §19.4).
        let cap = u64::from(KeyboardPumpDiagnostics::MAX_HEARTBEATS);
        for _ in 0..(interval * (cap + 4)) {
            diag.record(Ok(0), &sink);
        }
        assert_eq!(
            sink.count(USB_KEYBOARD_POLL_HEARTBEAT),
            KeyboardPumpDiagnostics::MAX_HEARTBEATS as usize
        );
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
    impl DmaHost for MockDmaHost {
        fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
            let ptr = leak_aligned(size);
            // SAFETY: `ptr` covers `size` zeroed bytes and lives for the
            // whole test process; the device-visible base is in-aperture
            // (below `APERTURE_TOP`). Drop is a no-op (`from_leaked`).
            Ok(unsafe { DmaSlab::from_leaked(0x1000_0000, ptr, size, PoolId::MOCK, 0) })
        }
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
        assert!(host.dma_host().is_some());
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
        // (`0x50`) — the config-space register the vendor firmware-init
        // sequence uses to confirm the firmware load
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
        // inbound DMA (VideoCore VL805-firmware) window with the known-good
        // translation (raspberrypi/firmware #1617; `AGENTS.md` §15.7 / §23.4).
        let sink = RecordingSink::new();
        log_inbound_window(
            &sink,
            InboundWindowReadback {
                rc_bar1_lo: 0,
                rc_bar2_lo: 0x11,
                rc_bar2_hi: 4,
                rc_bar3_lo: 0,
                misc_ctrl: 0x8800_3000,
                pcie_status: 0xb0,
            },
        );
        assert_eq!(sink.count(USB_KEYBOARD_INBOUND), 1);
        assert_eq!(sink.errors(), 0);
    }

    #[test]
    fn entry_inbound_window_logs_one_4120_record() {
        // The pre-program inbound-window capture emits exactly one `4120`
        // record at `Info`, distinct from the post-program `4119` record —
        // the firmware-left `RC_BAR2` state a metal run compares to detect a
        // divergence from VideoCore's assumption (raspberrypi/firmware
        // #1495; `AGENTS.md` §15.7 / §23.4).
        let sink = RecordingSink::new();
        log_entry_inbound_window(
            &sink,
            InboundWindowReadback {
                rc_bar1_lo: 0,
                rc_bar2_lo: 0xABCD_0012,
                rc_bar2_hi: 4,
                rc_bar3_lo: 0,
                misc_ctrl: 0,
                pcie_status: 0xb0,
            },
        );
        assert_eq!(sink.count(USB_KEYBOARD_INBOUND_ENTRY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_INBOUND), 0);
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
    fn open_controller_probes_the_bar_when_reload_does_not_make_version_loaded() {
        // With a granted host and a bus that enumerates the VL805 and bases
        // its BAR, `open_controller` maps the controller and waits for the
        // VL805's firmware version (config `0x50`) to read non-zero. The mock
        // bus returns `0` for every config read, so the first firmware-loaded
        // wait fails. The bring-up then issues exactly one
        // `NOTIFY_XHCI_RESET` fallback; because the version still stays zero,
        // the firmware-version gate (`4123`) records `firmware_loaded=0` and
        // the bring-up proceeds to probe the controller's own capability
        // block regardless — the config-space `0x50` register is not the
        // authoritative readiness signal. The mock window is the zeroed mock
        // BAR, so the bring-up then fails closed at `Xhci::open` (the real
        // gate), not at the firmware step.
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
        // The bring-up reaches and fails closed at `Xhci::open` on the zeroed
        // mock BAR, not at the firmware step.
        assert!(result.is_err());
        assert_eq!(firmware_reset.calls.get(), 1);
        // One wait before the reload, and one after the 200 µs settle.
        assert_eq!(sink.count(USB_KEYBOARD_FW_READY), 2);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESET), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESPONSE), 1);
        // The gate is logged once with `firmware_loaded=0`, and the bring-up
        // proceeds to probe the controller's own capability block.
        assert_eq!(sink.count(USB_KEYBOARD_FW_GATE), 1);
        // The root-port error/status snapshot is taken once, right after the
        // reload, so a metal capture can tell a bus-reach failure (secondary
        // Received-Master-Abort) from a dropped mailbox reply.
        assert_eq!(sink.count(USB_KEYBOARD_PCIE_ERR), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_POST_RELOAD), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_RAW), 1);
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
        // The gate is logged once (here with `firmware_loaded=1`).
        assert_eq!(sink.count(USB_KEYBOARD_FW_GATE), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_POST_RELOAD), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_RAW), 1);
    }

    #[test]
    fn open_controller_probes_the_bar_when_firmware_reload_fails() {
        // The reload itself fails (the `NOTIFY_XHCI_RESET` timeout the metal
        // capture shows). That no longer aborts the bring-up: the gate
        // (`4123`) records `firmware_loaded=0` and the bring-up proceeds to
        // probe the controller's own capability block, failing closed at
        // `Xhci::open` on the zeroed mock BAR. Two `Error` events result —
        // the reload failure and the `Xhci::open` failure.
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

        assert!(result.is_err());
        assert_eq!(firmware_reset.calls.get(), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_READY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESET), 1);
        assert_eq!(sink.count(USB_KEYBOARD_FW_RESPONSE), 0);
        assert_eq!(sink.count(USB_KEYBOARD_FW_GATE), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_READY), 1);
        assert_eq!(sink.count(USB_KEYBOARD_POST_RELOAD), 1);
        assert_eq!(sink.count(USB_KEYBOARD_CAPS_RAW), 1);
        // The reload failure and the `Xhci::open` failure are both `Error`s.
        assert_eq!(sink.errors(), 2);
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
