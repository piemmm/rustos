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
    CapabilityId, DriverError, DriverHost, DriverKind, HwNode, HwResourceKind, MmioMapper,
    RegisterWindow,
};
use rustos_caps::CapabilitySet;
use rustos_drv_bus_pcie_brcm::{self as pcie_brcm, Delay, PcieWindows};
use rustos_drv_bus_usb::device::UsbDevice;
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
    delay: &dyn Delay,
    sink: &dyn Sink,
) -> Result<KeyboardChain, DriverError> {
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
    let mut usb = match rustos_drv_bus_usb::wiring::open_discovered(host, &bus, dma_aperture_top) {
        Ok(usb) => usb,
        Err(err) => {
            log_stage_err(
                sink,
                "usb-keyboard: vl805 xhci controller bring-up failed",
                err,
            );
            return Err(err);
        }
    };
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
    use core::cell::RefCell;
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

    struct NoDelay;
    impl Delay for NoDelay {
        fn delay_us(&self, _us: u32) {}
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
            bring_up_keyboard(&host, &bringup, &NoDelay, &sink).err(),
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
            bring_up_keyboard(&host, &bringup, &NoDelay, &sink).err(),
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
}
