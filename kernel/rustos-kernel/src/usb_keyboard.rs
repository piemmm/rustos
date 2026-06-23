//! VL805/xHCI USB-keyboard composition (`plans/PI.md` P10).
//!
//! On the Pi 4 (BCM2711) the USB-A ports hang off a VL805 xHCI controller
//! behind the `SoC`'s PCIe root complex, whose link ships **down** and
//! whose config space is windowed (not flat ECAM). Bringing a keyboard to
//! the video-console login composes four building blocks (the BCM2711
//! PCIe + PCI pieces are `lib/*`, the USB/HID pieces are driver crates):
//!
//! 1. [`rustos_pcie_brcm`] resets the root complex and trains its
//!    link with the discovered address windows;
//! 2. [`rustos_pci::mechanism_brcm`] enumerates the VL805 over the
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

use rustos_abi::driver::dma::{DmaHost, DmaSlab};
use rustos_abi::driver::mailbox::MailboxChannel;
use rustos_abi::input::KeyInput;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwNode, MmioMapper, RegisterWindow,
};
use rustos_caps::CapabilitySet;
use rustos_pcie_brcm::{self as pcie_brcm, BringUpTiming, Delay, InboundWindowReadback};
// The discovered-node parsing now lives in the PCIe device's own support
// crate (`lib/pcie_brcm`), beside the link-training engine it feeds
// (`AGENTS.md` §2.2 / §2.21 — it is hwtree parsing, not kernel
// orchestration). Re-exported so the composition keeps one definition; the
// autonomous `wiring::bring_up_from_node` floor entry consumes it directly.
use rustos_hid::{BootKeyboard, ConsoleSink};
use rustos_kernel_core::InputFocus;
use rustos_log::{log, Event, EventId, Field, Level, Sink};
pub use rustos_pcie_brcm::wiring::PcieBringup;
use rustos_usb::device::UsbDevice;
use rustos_util::fmt::format_hex_u64;
// The VL805 firmware-reset *policy* lives in the device's own driver crate
// (`drivers/bus/usb/vl805`), reached over the board-neutral `lib/abi`
// `MailboxChannel` seam (`AGENTS.md` §2.20 / §2.2 / §17.4); the xHCI
// controller bring-up + boot-keyboard enumeration + child-node emission
// lives in the generic `drivers/bus/usb` floor entry. This composition only
// sequences the floor drivers over the `DriverHost` contract.
use rustos_drv_bus_usb::wiring::bring_up_boot_input;
use rustos_drv_bus_usb_vl805::{self as vl805, FirmwareResetOutcome};

/// Audit event: a progress/failure milestone of the VL805 USB-keyboard
/// bring-up chain (PCIe link training, xHCI bring-up, root-hub
/// enumeration), so a metal capture shows which stage stalls.
const USB_KEYBOARD_BRINGUP: EventId = EventId(4101);

/// Audit event: the per-boot `VideoCore` VL805 firmware reload, issued
/// once after the PCIe link trains (its `PERST#` drops the VL805's
/// VideoCore-loaded firmware on EEPROM-less Pi 4 boards). One-shot,
/// best-effort: a missing mailbox or a refused tag is logged but does not
/// stop bring-up; the authoritative fail-closed liveness gate is
/// `Xhci::open` inside the xHCI floor entry (`AGENTS.md` §2.9).
const USB_KEYBOARD_FW_RESET: EventId = EventId(4108);

/// Audit event: the per-phase wall-time breakdown of the PCIe
/// root-complex `bring_up`, in microseconds, so a capture pins any stall
/// to the exact MMIO group: `reset_swinit_us` (releasing the bridge
/// `sw_init`), `reset_settle_us` (post-de-reset MISC settle), `config_us`
/// (MISC + type-1 bridge config), `linkwait_us` (the `PERST#`-deassert
/// retrain settle + bounded link-up poll) and `link_polls`. The bridge
/// reset is released before touching MISC, else the MISC access
/// master-aborts on the `SoC` bus completion timeout (~10.8 s).
const USB_KEYBOARD_BRINGUP_TIMING: EventId = EventId(4117);

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
///
/// Emitted at [`Level::Debug`], **not** `Info`: it is the routine, periodic
/// case, and the report pump emits it *synchronously* from its own kthread.
/// On a debug build the diagnostic sink is the flow-blocked serial UART
/// (~116 ms/line), so an `Info` heartbeat once blocked the pump for ~72 %
/// of the `Root passphrase:` window — starving key delivery so typed keys
/// were slow or dropped (a §20 progress-spam / §2.16 defect, the same class
/// as `devmgr`'s `NODE_UNBOUND` flood). At `Debug` it is dropped in O(1) by
/// the default `Info` filter *before* the sink write, so the pump never
/// blocks on it, while the heartbeat is still captured when diagnostics
/// lower the threshold. The one-shot [`USB_KEYBOARD_FIRST_REPORT`] (`Info`)
/// proves the path is live, and [`USB_KEYBOARD_PUMP_ERROR`] (`Error`) keeps
/// real faults visible — those are the actionable events.
const USB_KEYBOARD_POLL_HEARTBEAT: EventId = EventId(4131);

/// The boot-tree publication seam the [`ChainHost`] forwards
/// [`DriverHost::emit_node`] to.
///
/// The floor xHCI bring-up ([`bring_up_boot_input`]) publishes the
/// enumerated child [`HwNode`] through `DriverHost::emit_node`; in the
/// in-kernel composition that mutation is attaching the node to the
/// discovered boot hardware tree so the pre-unlock autoload sees it
/// (`AGENTS.md` §18.2). The metal boot path implements this over
/// `unlock_service::augment_boot_tree`; host tests pass a recording
/// double. It is a thin seam so the arch-neutral [`ChainHost`] never names
/// the kernel boot-tree global directly (`AGENTS.md` §2.2 / §17.4).
pub trait BootTreeEmitter {
    /// Publish `node` into the discovered boot hardware tree.
    ///
    /// # Errors
    ///
    /// Fails closed with a [`DriverError`] if the tree mutation is refused
    /// (`AGENTS.md` §5.4).
    fn emit_node(&self, node: &HwNode) -> Result<(), DriverError>;
}

/// Audit event: the **inbound** viewport registers **as the previous boot
/// stage (`start4.elf`) left them**, sampled before bring-up programs
/// `RC_BAR2`. `VideoCore`'s `NOTIFY_XHCI_RESET` load assumes a particular
/// `RC_BAR2` state (raspberrypi/firmware #1495), so this entry capture
/// shows whether the previous boot stage already configured the inbound
/// window the way `VideoCore` assumes for the firmware load. Faulting
/// reads render the all-ones sentinel. One-shot at bring-up (`AGENTS.md`
/// §15.7 / §2.9 / §19.4).
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
/// bounded audit events — a one-shot first-report (`4129`, `Info`), an
/// on-change capped pump error (`4130`, `Error`), and a capped heartbeat
/// (`4131`, `Debug`) — so the log stays finite while still pinning where the
/// report path stalls. The heartbeat is `Debug` so it is filtered out on a
/// default-`Info` boot and never blocks the pump on the slow serial UART
/// (see the `USB_KEYBOARD_POLL_HEARTBEAT` event). Holds no authority;
/// logging only.
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
                level: Level::Debug,
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

/// Log the inbound (PCIe→system-memory) viewport registers **as the
/// previous boot stage left them**, sampled before bring-up programs
/// `RC_BAR2` (`4120`): the firmware's own `RC_BAR2` state a metal run
/// compares to detect a divergence from the state `VideoCore` assumes for
/// the firmware load. Renders `rb`'s `RC_BAR1`/`RC_BAR2`/`RC_BAR3` and
/// link-status registers; values produced fail-closed by
/// [`pcie_brcm::BrcmPcieRc::entry_inbound_window`].
fn log_entry_inbound_window(sink: &dyn Sink, rb: InboundWindowReadback) {
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
            id: USB_KEYBOARD_INBOUND_ENTRY,
            message: "usb-keyboard: pcie inbound (pcie->memory) viewport as firmware left it (pre-program)",
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
    mailbox: Option<&'a dyn MailboxChannel>,
    emitter: &'a dyn BootTreeEmitter,
}

impl<'a> ChainHost<'a> {
    /// Build the view over the bus-driver task's `capabilities`, the
    /// kernel's `mmio` mapper and `dma` host, the optional `VideoCore`
    /// `mailbox` channel (the device-specific VL805 firmware reload runs
    /// over it, `AGENTS.md` §2.20), and the boot-tree `emitter` the floor
    /// xHCI bring-up publishes the enumerated child node through
    /// (`DriverHost::emit_node`).
    ///
    /// `mailbox` is [`None`] on a boot shape with no discovered `VideoCore`
    /// mailbox; the VL805 firmware reload then reports
    /// [`FirmwareResetOutcome::NotAvailable`] and the bring-up proceeds
    /// (`AGENTS.md` §2.9 — the authoritative liveness gate is the xHCI
    /// capability block).
    #[must_use]
    pub fn new(
        capabilities: CapabilitySet,
        mmio: &'a dyn MmioMapper,
        dma: &'a dyn DmaHost,
        mailbox: Option<&'a dyn MailboxChannel>,
        emitter: &'a dyn BootTreeEmitter,
    ) -> Self {
        Self {
            capabilities,
            mmio,
            dma,
            mailbox,
            emitter,
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

    fn mailbox(&self) -> Option<&dyn MailboxChannel> {
        self.mailbox
    }

    fn emit_node(&self, node: HwNode) -> Result<(), DriverError> {
        self.emitter.emit_node(&node)
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
/// Sequences the three floor driver crates over the [`DriverHost`] contract
/// alone (`AGENTS.md` §17.4) — this composition names them all (the §17.4
/// carve-out for the image-assembly binary), but no driver names another:
///
/// 1. [`pcie_brcm::wiring::open_discovered`] resets the BCM2711 root complex
///    and trains its link over the discovered register window;
/// 2. the VL805 device driver's [`vl805::reload_firmware`] asks the
///    `VideoCore` over [`DriverHost::mailbox`] to (re)load the controller's
///    firmware, dropped by the link bring-up's `PERST#` (device-specific,
///    `AGENTS.md` §2.20 — best-effort, the authoritative gate is `Xhci::open`
///    inside the next step); and
/// 3. the generic xHCI floor entry [`bring_up_boot_input`] maps the BAR,
///    carves DMA, brings the controller up, enumerates the boot keyboard,
///    and publishes it as a child [`HwNode`] through
///    [`DriverHost::emit_node`] carrying the BAR + DMA `HwResource` grants
///    the matched user-space driver will receive (`AGENTS.md` §4 / §18.3).
///
/// The returned [`KeyboardChain`] is then polled with [`rustos_hid::pump_once`]
/// in the in-kernel report-pump loop, feeding each produced [`KeyInput`]
/// record to an [`ArbiterConsoleSink`] (the live keyboard until the
/// `plans/PI.md` B5 flip, `AGENTS.md` §2.17).
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if `host` did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * [`DriverError::OutOfRange`] if the discovered inbound aperture top
///   overflows the address space.
/// * Any error of the link bring-up (the controller is not a root port or
///   the link never trains) or of [`bring_up_boot_input`] (no USB function,
///   a DMA carve above the aperture, a mapping failure, the controller
///   never running, an empty root hub, or a refused node emission). Every
///   failure is fail-closed (`AGENTS.md` §5.4); nothing is left
///   half-configured.
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (the register windows and the BAR)
/// and the host's DMA capability (the xHCI DMA carve), both re-checked
/// kernel-side at each map/allocation (`AGENTS.md` §5.4).
///
/// # Logging
///
/// Each stage (link training + the trained-window read-backs, the VL805
/// firmware reload, the xHCI bring-up/enumeration) is logged on success and
/// failure, so a capture localises a silent keyboard to the stage that
/// stalled. One-shot bring-up diagnostics, never on the poll path.
pub fn bring_up_keyboard(
    host: &dyn DriverHost,
    bringup: &PcieBringup,
    delay: &dyn Delay,
    sink: &dyn Sink,
) -> Result<BroughtUpKeyboard, DriverError> {
    log_stage(
        sink,
        "usb-keyboard: training brcm,bcm2711-pcie root-complex link",
    );
    let rc = match pcie_brcm::wiring::open_discovered(
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
    // Diagnostics: split the bring-up wall time across its phases, and log
    // the inbound (PCIe→memory) viewport as the prior boot stage left it
    // (`4120`), since the VideoCore firmware handoff assumes the `RC_BAR2`
    // state it set at power-on. The post-bring-up window read-backs are not
    // logged: on real BCM2711 silicon reading those MISC registers after the
    // link trains stalls for seconds, and with the link confirmed up they add
    // no functional value (`AGENTS.md` §2.14 / §2.16 — removed once metal
    // bring-up was confirmed).
    log_bring_up_timing(sink, rc.bring_up_timing());
    log_entry_inbound_window(sink, rc.entry_inbound_window());
    // Reach the VL805 through the BCM2711 windowed config accessor over the
    // trained register window. It forwards config only to the single device
    // on the secondary bus, so the floor xHCI scan below never TLPs an
    // absent target (which would CPU-abort and wedge the boot).
    let bus = rustos_pci::mechanism_brcm(rc.into_regs(), pcie_brcm::regs::RC_SECONDARY_BUS);
    // The link bring-up asserted `PERST#`, which drops the VL805's
    // VideoCore-loaded firmware on EEPROM-less Pi 4 boards. Ask the device's
    // own driver to reload it over the board-neutral mailbox seam *before*
    // the generic xHCI bring-up reads the capability block (`AGENTS.md`
    // §2.20). Best-effort: a missing mailbox or a refused tag is logged but
    // never aborts — the authoritative liveness gate is `Xhci::open` inside
    // `bring_up_boot_input` (`AGENTS.md` §2.9).
    log_stage(
        sink,
        "usb-keyboard: reloading vl805 firmware over the videocore mailbox",
    );
    let firmware_outcome = match host.mailbox() {
        Some(channel) => vl805::reload_firmware(channel),
        None => FirmwareResetOutcome::NotAvailable,
    };
    log_firmware_reset(sink, firmware_outcome);
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
    log_stage(
        sink,
        "usb-keyboard: bringing up the vl805 xhci controller and enumerating the boot keyboard",
    );
    // The generic xHCI floor entry maps the BAR, carves DMA, brings the
    // controller up, enumerates the boot keyboard (transparently descending
    // one tier through the Pi 4B's onboard hub), and `emit_node()`s the
    // enumerated child through `host.emit_node` — which the in-kernel host
    // forwards to the boot hardware tree so the pre-unlock autoload sees the
    // keyboard like every other discovered device (`AGENTS.md` §18.2). The
    // returned device is left pointed at the keyboard's slot for the pump.
    let enumerated = match bring_up_boot_input(
        host,
        &bus,
        dma_aperture_top,
        outbound_window,
        delay,
        HID_NODE_PARENT_ID,
        HID_NODE_ID,
    ) {
        Ok(enumerated) => enumerated,
        Err(err) => {
            log_stage_err(
                sink,
                "usb-keyboard: vl805 xhci bring-up / boot-keyboard enumeration failed",
                err,
            );
            return Err(err);
        }
    };
    log_stage(
        sink,
        "usb-keyboard: boot keyboard enumerated and emitted into the hardware tree",
    );
    Ok(BroughtUpKeyboard {
        keyboard: BootKeyboard::new(enumerated.device),
        hid_node: enumerated.node,
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
    // from now lives in the PCIe device support crate `lib/pcie_brcm`
    // (`AGENTS.md` §2.2 / §2.21).
    use rustos_drv_bus_usb_vl805::FirmwareResetFailure;
    use rustos_kernel_core::{ConsoleInputQueue, ConsoleRead};
    use rustos_pcie_brcm::wiring::pcie_bringup_from_node;

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
        // The heartbeat is a `Debug` record (filtered out on a default `Info`
        // boot, `AGENTS.md` §20); lower the threshold so the test observes it.
        rustos_log::set_max_level(rustos_log::Level::Trace);
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
    /// A [`BootTreeEmitter`] double recording how many nodes the chain
    /// published through [`DriverHost::emit_node`].
    #[derive(Default)]
    struct RecordingEmitter {
        emitted: core::cell::Cell<usize>,
    }

    impl RecordingEmitter {
        fn count(&self) -> usize {
            self.emitted.get()
        }
    }

    impl BootTreeEmitter for RecordingEmitter {
        fn emit_node(&self, _node: &HwNode) -> Result<(), DriverError> {
            self.emitted.set(self.emitted.get() + 1);
            Ok(())
        }
    }

    /// A no-op [`MailboxChannel`] double: the chain only checks that a
    /// channel is present here; the VL805 firmware exchange is exercised
    /// in the `drivers/bus/usb/vl805` crate's own tests.
    struct MockMailbox;

    impl MailboxChannel for MockMailbox {
        fn exchange(
            &self,
            _message: &mut [u32; rustos_abi::driver::mailbox::MAILBOX_PROPERTY_WORDS],
        ) -> Result<(), DriverError> {
            Ok(())
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
        let mailbox = MockMailbox;
        let emitter = RecordingEmitter::default();
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
            Some(&mailbox),
            &emitter,
        );
        assert!(host.has_capability(CapabilityId::MMIO_MAP));
        assert!(host.has_capability(CapabilityId::MEM_DMA));
        assert!(!host.has_capability(CapabilityId::DRV_LOAD));
        assert_eq!(host.kind(), DriverKind::InKernel);
        assert!(host.dma_host().is_some());
        assert!(host.mmio_mapper().is_some());
        // The mailbox + node-emit seams are exposed and forwarded: the
        // emitter records each node published through `emit_node`
        // (`AGENTS.md` §18.2).
        assert!(host.mailbox().is_some());
        assert_eq!(host.emit_node(pcie_node()), Ok(()));
        assert_eq!(emitter.count(), 1);
    }

    #[test]
    fn bring_up_requires_the_mmio_capability() {
        // A host without MMIO_MAP fails the chain closed at the very first
        // step (the PCIe controller-window map), before any hardware.
        let mapper = MockMapper { grant: false };
        let dma = MockDmaHost;
        let emitter = RecordingEmitter::default();
        let host = ChainHost::new(
            caps(&[CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
            None,
            &emitter,
        );
        let bringup = pcie_bringup_from_node(&pcie_node()).unwrap();
        let sink = RecordingSink::new();
        // `.err()` drops the unenumerated keyboard (which is neither
        // `Debug` nor `PartialEq`) and compares only the error.
        assert_eq!(
            bring_up_keyboard(&host, &bringup, &NoDelay::new(), &sink).err(),
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
        let emitter = RecordingEmitter::default();
        let host = ChainHost::new(
            caps(&[CapabilityId::MMIO_MAP, CapabilityId::MEM_DMA]),
            &mapper,
            &dma,
            None,
            &emitter,
        );
        let bringup = pcie_bringup_from_node(&pcie_node()).unwrap();
        let sink = RecordingSink::new();
        assert_eq!(
            bring_up_keyboard(&host, &bringup, &NoDelay::new(), &sink).err(),
            Some(DriverError::DeviceFault)
        );
        // The chain logged the link-training start and then the
        // root-complex failure, so the staged diagnostics fired before the
        // metal boundary refused (`AGENTS.md` §23.4).
        assert!(sink.errors() >= 1);
        assert!(sink.count(USB_KEYBOARD_BRINGUP) >= 1);
    }

    #[test]
    fn entry_inbound_window_logs_one_4120_record() {
        // The pre-program inbound-window capture emits exactly one `4120`
        // record at `Info` — the firmware-left `RC_BAR2` state a metal run
        // compares to detect a divergence from VideoCore's assumption
        // (raspberrypi/firmware #1495; `AGENTS.md` §15.7 / §23.4).
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
}
