//! The `Run` entry-point binary of the USB **host-controller driver** (HCD),
//! installed as a signed `/System/Drivers/` bundle and autoloaded into user
//! space by `devmgr` when an `usb,xhci` controller node is discovered
//! (`plans/USB.md` U3b).
//!
//! This process is the **sole owner** of one xHCI controller: it maps the
//! controller's register BAR, owns its DMA rings and root-hub ports, brings
//! it up, and enumerates the attached device. It then **publishes one
//! hardware-tree node per USB interface** (carrying the device's
//! `vid:pid:class` match keys) so `devmgr` autoloads the matching **class**
//! driver (`drivers/input/usb_kbd`, …), and **serves that class driver's URB
//! transfers** over the bus-agnostic URB transport seam. It names no class
//! driver, no board, and no private bus implementation.
//!
//! It holds no class-specific authority: the keyboard driver decodes reports
//! and injects keystrokes; this HCD only moves bytes between the controller
//! and the shared buffer.
//!
//! # The asynchronous event loop (`plans/USB.md` §1.1)
//!
//! The HCD multiplexes two independent event streams on one kernel **wait-set**
//! (`U3a3`) so it never busy-polls and never blocks one interface inside
//! another's handler (the charter forbids spinning a core):
//!
//! * **A URB-submit IPC call** on the per-interface endpoint it serves: it
//!   `call_recv`s the URB and drives it. An interrupt-IN report not yet
//!   arrived is left **outstanding** (the class driver's `ipc_call` parks in
//!   the kernel); a control transfer or a ready report is replied at once.
//! * **The controller's completion interrupt**: it drains the event ring and
//!   **replies to the now-complete outstanding URB**, bounce-copying the
//!   report from its own DMA ring into the shared buffer the class driver
//!   reads. It also watches the root-hub port and retracts the interface node
//!   on a disconnect (`hw_remove_node`), so `devmgr` unloads the class driver
//!   while the controller stays up.
//!
//! # Data path (`plans/USB.md` U3a2, Option B)
//!
//! The URB data buffer is a cross-process **shared-memory** region this HCD
//! creates (`shm_create`) and forwards as a grant on the interface node; the
//! class driver inherits the grant and `shm_map`s the same frames. The buffer
//! is plain cacheable RAM with no DMA properties — the class driver holds
//! **zero** DMA authority — and the HCD bounce-copies between it and its own
//! DMA-granted ring.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `rustos-rt` (`_start`, the stack canary, the panic handler, the
//! syscall wrappers); on the host it is an inert stub so `cargo build
//! --workspace`, clippy, and fmt still cover the file. The live controller
//! bring-up and report path are metal-only because QEMU models no Pi USB; the
//! HCD's host-testable logic lives in the crate's `lib` target
//! ([`rustos_drv_bus_usb::bringup`] / [`rustos_drv_bus_usb::serve`]).

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(any(freestanding, test))]
fn waitset_ctl_result(ret: i64) -> Result<(), i64> {
    if ret == 0 {
        Ok(())
    } else {
        Err(ret)
    }
}

#[cfg(freestanding)]
const WAIT_FOREVER_NS: u64 = u64::MAX;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::hwtree::HW_NODE_ROOT;
    use rustos_abi::usb_urb::{decode_completion, URB_COMPLETION_LEN, URB_REQUEST_LEN};
    use rustos_abi::waitset::{WaitSetOp, WaitSourceKind};
    use rustos_abi::{CapabilityId, Errno};
    use rustos_caps::CapabilitySet;
    use rustos_drv_bus_usb::bringup::{
        bring_up_controller_diagnostic, derive_controller_resources,
    };
    use rustos_drv_bus_usb::serve::{attach_transport_grants, UrbOutcome, UrbReply, UrbService};
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_log::{log, Event, EventId, Field, Level};
    use rustos_rt::{ClockDelay, LogSink};
    use rustos_usb::device::{BringUp, HubEvent};
    use rustos_util::fmt::format_hex_u64;

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the register BAR and a
    /// DMA constraint this controller needs.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when controller bring-up / enumeration failed.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// Exit code when the controller came up but the URB transport seam (the
    /// shared buffer or the call endpoint) could not be created.
    const EXIT_NO_TRANSPORT: i32 = 83;

    /// Exit code when the interface node could not be published.
    const EXIT_EMIT_FAILED: i32 = 84;

    /// Diagnostic event id: a one-shot controller bring-up failure.
    const HCD_BRINGUP_FAILED: EventId = EventId(4126);

    /// Diagnostic event id: the one-shot "controller up, serving URBs" beacon.
    const HCD_READY: EventId = EventId(4101);

    /// Diagnostic event id: the device disconnected and its interface node was
    /// retracted.
    const HCD_DISCONNECT: EventId = EventId(4127);

    /// Diagnostic event id: a device (re)attached behind the hub and a fresh
    /// interface node was published.
    const HCD_ATTACHED: EventId = EventId(4156);

    /// Diagnostic event id: URB transport setup or IRQ arming state.
    const HCD_URB_SETUP: EventId = EventId(4149);

    /// Diagnostic event id: a URB was held awaiting a controller event.
    const HCD_URB_HELD: EventId = EventId(4151);

    /// Diagnostic event id: a URB reply was sent or attempted.
    const HCD_URB_REPLY: EventId = EventId(4152);

    /// Diagnostic event id: a controller IRQ woke the HCD loop.
    const HCD_IRQ_WAKE: EventId = EventId(4153);

    /// Diagnostic event id: a wait-set or IPC transport error happened.
    const HCD_WAIT_ERROR: EventId = EventId(4154);

    /// Reserved base of the URB call-endpoint id range the HCD allocates from.
    ///
    /// A grant-restricted endpoint id the class driver reaches only through
    /// the kernel-minted grant on its matched node — distinct from the
    /// well-known `DRIVER_STORE_ENDPOINT`. The HCD probes upward from the base
    /// until `call_create` succeeds, so two controllers (a future second HCD)
    /// never collide on one id.
    const URB_ENDPOINT_BASE: u64 = 0x0055_5242_0000_0000;

    /// How many endpoint ids to probe before giving up (a generous bound; one
    /// controller takes the base on the first try).
    const URB_ENDPOINT_PROBES: u64 = 64;

    /// Bytes of shared buffer per interface — comfortably holds a boot report
    /// (8 bytes) and any control-IN descriptor a class driver reads.
    const SHM_LEN: usize = 64;

    /// Outstanding-URB capacity of the per-interface endpoint. The class
    /// driver submits one at a time (it blocks on the reply); a small queue
    /// absorbs a re-submit racing the previous reply.
    const ENDPOINT_CAPACITY: usize = 4;

    /// Wait-set token for "a URB submit arrived on the transport endpoint".
    const TOKEN_URB: u64 = 1;
    /// Wait-set token for "the controller completion interrupt fired".
    const TOKEN_IRQ: u64 = 2;

    fn log_hex_event(
        id: EventId,
        level: Level,
        message: &'static str,
        key: &'static str,
        value: u64,
    ) {
        let mut value_buf = [0u8; 16];
        log(
            &LogSink,
            &Event {
                level,
                id,
                message,
                fields: &[Field {
                    key,
                    value: format_hex_u64(value, &mut value_buf),
                }],
            },
        );
    }

    fn reply_to_urb(endpoint_id: u64, reply: rustos_drv_bus_usb::serve::UrbReply) {
        let ret = rustos_rt::call_reply(endpoint_id, reply.ticket, &reply.bytes[..reply.len]);
        if ret == 0 {
            log_hex_event(
                HCD_URB_REPLY,
                Level::Debug,
                "usb-hcd: URB reply sent",
                "ticket_hex",
                reply.ticket,
            );
        } else {
            log_hex_event(
                HCD_URB_REPLY,
                Level::Warn,
                "usb-hcd: URB reply failed",
                "ret_hex",
                ret as u64,
            );
        }
    }

    fn abort_pending_urb(endpoint_id: u64, service: &mut UrbService) {
        if let UrbOutcome::Reply(reply) = service.abort_outstanding(Errno::NotFound) {
            reply_to_urb(endpoint_id, reply);
        }
    }

    fn reply_error(endpoint_id: u64, ticket: u64, errno: Errno) {
        let mut bytes = [0u8; URB_COMPLETION_LEN];
        let len = rustos_usb::transport::frame_completion(&mut bytes, Err(errno)).unwrap_or(0);
        reply_to_urb(endpoint_id, UrbReply { ticket, bytes, len });
    }

    fn urb_reply_errno(reply: &UrbReply) -> Option<Errno> {
        match decode_completion(&reply.bytes[..reply.len]) {
            Ok(_) => None,
            Err(err) => Some(err),
        }
    }

    enum FaultDetachOutcome {
        NotDetached,
        Detached,
        Reattached,
    }

    fn service_hub_after_fault_detach(
        endpoint_id: u64,
        shm_id: u64,
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice,
        delay: &ClockDelay,
    ) -> Option<u32> {
        match device.next_hub_change(delay) {
            Ok(HubEvent::Attached(_)) => {
                let id = emit_interface_node(device, endpoint_id, shm_id);
                if let Some(node) = id {
                    log_hex_event(
                        HCD_ATTACHED,
                        Level::Info,
                        "usb-hcd: device attached while re-arming hub watch after fault detach",
                        "node_hex",
                        u64::from(node),
                    );
                }
                id
            }
            Ok(HubEvent::Detached | HubEvent::None) => None,
            Err(err) => {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: hub watch re-arm after transfer fault failed",
                    "err_hex",
                    err as u64,
                );
                None
            }
        }
    }

    fn retract_after_fault_if_gone(
        endpoint_id: u64,
        shm_id: u64,
        interface_node_id: &mut u32,
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice,
        reply: UrbReply,
        delay: &ClockDelay,
    ) -> FaultDetachOutcome {
        if urb_reply_errno(&reply) != Some(Errno::NotImplemented) {
            return FaultDetachOutcome::NotDetached;
        }
        match device.detach_if_watched_device_gone() {
            Ok(true) => {
                if rustos_rt::hw_remove_node(*interface_node_id) >= 0 {
                    reply_error(endpoint_id, reply.ticket, Errno::NotFound);
                    log(
                        &LogSink,
                        &Event {
                            level: Level::Info,
                            id: HCD_DISCONNECT,
                            message:
                                "usb-hcd: device transfer fault confirmed disconnect, interface retracted",
                            fields: &[],
                        },
                    );
                    if let Some(node) =
                        service_hub_after_fault_detach(endpoint_id, shm_id, device, delay)
                    {
                        *interface_node_id = node;
                        FaultDetachOutcome::Reattached
                    } else {
                        FaultDetachOutcome::Detached
                    }
                } else {
                    log_hex_event(
                        HCD_WAIT_ERROR,
                        Level::Warn,
                        "usb-hcd: interface retraction failed after transfer fault",
                        "node_hex",
                        u64::from(*interface_node_id),
                    );
                    FaultDetachOutcome::NotDetached
                }
            }
            Ok(false) => FaultDetachOutcome::NotDetached,
            Err(err) => {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: disconnect confirmation after transfer fault failed",
                    "err_hex",
                    err as u64,
                );
                FaultDetachOutcome::NotDetached
            }
        }
    }

    /// Reset the controller and re-enumerate from scratch, re-arming the
    /// interrupter the reset cleared and — if a device is already back —
    /// publishing a fresh interface node so `devmgr` re-autoloads the class
    /// driver onto the same transport.
    ///
    /// This is the recovery a root-port re-attach uses and the recovery from a
    /// latched controller fault uses: in both cases the controller is returned
    /// to the same state a cold boot with nothing attached reaches, from which
    /// the next connect enumerates through the normal attach path. With no
    /// device present yet (`BringUp::AwaitingDevice`) it simply leaves the
    /// controller awaiting that connect. The caller refreshes its watched root
    /// port from `device.root_port()` afterwards.
    fn reset_reenumerate_and_publish(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice,
        endpoint_id: u64,
        shm_id: u64,
        interface_node_id: &mut u32,
        node_live: &mut bool,
        delay: &ClockDelay,
    ) {
        let Ok(outcome) = device.reset_and_reenumerate(delay) else {
            return;
        };
        let _ = device.enable_interrupter();
        if matches!(outcome, BringUp::Device(_)) {
            if let Some(id) = emit_interface_node(device, endpoint_id, shm_id) {
                *interface_node_id = id;
                *node_live = true;
                log_hex_event(
                    HCD_ATTACHED,
                    Level::Info,
                    "usb-hcd: device attached, interface published",
                    "node_hex",
                    u64::from(id),
                );
            }
        }
    }

    /// Recover if the controller has latched a fatal error or halted
    /// (`USBSTS.HSE`/HCHalted). Such a controller raises no further interrupts
    /// until it is reset (xHCI §4.24.1), so a watched device's hot-plug and
    /// transfers go silent — on the Pi 4 the VL805 latches a Host System Error
    /// during a downstream-device hot-removal teardown, after its Disable Slot
    /// has already completed, which is why an unplug worked but the controller
    /// never saw the re-plug. Retract any still-live interface, abort a held
    /// URB, then reset and re-enumerate so the controller returns to the proven
    /// await-connect state and a re-plug enumerates normally. Returns whether a
    /// recovery ran (so the caller refreshes its watched root port).
    fn recover_if_controller_faulted(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice,
        endpoint_id: u64,
        shm_id: u64,
        interface_node_id: &mut u32,
        node_live: &mut bool,
        service: &mut UrbService,
        delay: &ClockDelay,
    ) -> bool {
        if !device.controller_faulted() {
            return false;
        }
        log(
            &LogSink,
            &Event {
                level: Level::Warn,
                id: HCD_DISCONNECT,
                message: "usb-hcd: controller fault latched, resetting to recover",
                fields: &[],
            },
        );
        if *node_live {
            let _ = rustos_rt::hw_remove_node(*interface_node_id);
            *node_live = false;
        }
        abort_pending_urb(endpoint_id, service);
        reset_reenumerate_and_publish(
            device,
            endpoint_id,
            shm_id,
            interface_node_id,
            node_live,
            delay,
        );
        true
    }

    /// The capability set the HCD host re-checks up front; the kernel is the
    /// authority and re-checks every trap. It mirrors the resources the
    /// matched node carries plus the privilege to publish the interface node
    /// and stand up its URB transport seam.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::MEM_DMA);
        caps.insert(CapabilityId::IRQ_BIND);
        caps.insert(CapabilityId::SHM);
        caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
        caps.insert(CapabilityId::HW_EMIT);
        caps.insert(CapabilityId::LOG_EMIT);
        caps
    }

    /// Bind the URB transport endpoint, probing the reserved id range until a
    /// free id is found. Binding it grant-restricted (`send_caps` carries
    /// `CAP_IPC_ENDPOINT`) makes the kernel mint this HCD the matching
    /// per-endpoint grant, which it forwards onto the interface node so the
    /// class driver inherits exactly the right to submit URBs on this one
    /// interface. Returns the bound endpoint id, or `None` if the range is
    /// exhausted.
    fn bind_urb_endpoint() -> Option<u64> {
        let mut send_caps = CapabilitySet::empty();
        send_caps.insert(CapabilityId::IPC_ENDPOINT);
        let recv_caps = CapabilitySet::empty();
        for i in 0..URB_ENDPOINT_PROBES {
            let id = URB_ENDPOINT_BASE + i;
            let ret = rustos_rt::call_create(
                id,
                &send_caps,
                &recv_caps,
                URB_REQUEST_LEN,
                URB_COMPLETION_LEN,
                ENDPOINT_CAPACITY,
            );
            if ret == 0 {
                return Some(id);
            }
        }
        None
    }

    /// Whether the enumerated device is still connected on its root port (the
    /// `CCS` connect bit). A read fault defaults to "connected" so a transient
    /// read never triggers a spurious retraction.
    fn still_connected(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice,
        root_port: u8,
    ) -> bool {
        device
            .port_status_raw(root_port)
            .is_none_or(|portsc| portsc & 1 != 0)
    }

    /// Build and publish the enumerated device's interface node — its
    /// `vid:pid:class` match keys plus the per-interface URB-transport grants
    /// (the call endpoint and the shared buffer) the class driver inherits —
    /// returning the kernel-assigned node id.
    ///
    /// Used for the initial publish and, identically, for a re-attach after a
    /// hot-plug: a *fresh* node so `devmgr` re-autoloads the class driver onto
    /// the same transport endpoint, so keystrokes resume to the same OS sink.
    /// `None` if the device is not enumerated or the kernel refuses the node.
    fn emit_interface_node(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice,
        endpoint_id: u64,
        shm_id: u64,
    ) -> Option<u32> {
        let node = device.describe_device(HW_NODE_ROOT, 0).ok()?;
        let node = attach_transport_grants(node, endpoint_id, shm_id).ok()?;
        let emit = rustos_rt::hw_emit_node(&node);
        if emit < 0 {
            return None;
        }
        #[allow(clippy::cast_sign_loss)] // `emit >= 0` is the assigned node id.
        Some(emit as u32)
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // Coherent DMA is carved kernel-side, so no architecture-specific
        // cache-maintenance shim is supplied (`coherency = None`).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        let Ok(resources) = derive_controller_resources(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        let delay = ClockDelay::new();
        let mut device = match bring_up_controller_diagnostic(
            &host,
            &delay,
            resources.bar_base,
            resources.bar_len,
            resources.dma_aperture_top,
        ) {
            Ok(device) => device,
            Err(err) => {
                // Pin the failing controller step on the captured serial log
                // before exiting fail-closed: QEMU models no Pi USB, so this
                // one-shot diagnostic is how a metal run localises the stall.
                log(
                    &LogSink,
                    &Event {
                        level: Level::Error,
                        id: HCD_BRINGUP_FAILED,
                        message: "usb-hcd: controller bring-up failed",
                        fields: &[Field {
                            key: "phase",
                            value: err.phase.as_str(),
                        }],
                    },
                );
                return EXIT_BRINGUP_FAILED;
            }
        };

        // Stand up the per-interface URB transport seam: a shared data buffer
        // and the grant-restricted call endpoint, both minting this HCD the
        // grant it forwards onto the interface node.
        let mut shm_id = 0u64;
        let shm_base = rustos_rt::shm_create(SHM_LEN, &mut shm_id);
        if shm_base < 0 {
            return EXIT_NO_TRANSPORT;
        }
        // SAFETY: `shm_create` mapped `SHM_LEN` bytes of zeroed, cacheable,
        // RW (non-executable) memory into this process at `shm_base` and
        // returned that base. The region is owned by this process for the rest
        // of its life (never unmapped here), and no other reference in this
        // address space aliases it, so a single exclusive `&mut [u8]` over
        // exactly the requested length is sound. The class driver maps the
        // same frames in its *own* address space; cross-process sharing is
        // outside Rust's aliasing model (like DMA/MMIO) and is synchronised by
        // the URB reply, which happens-after the HCD's write here.
        let shm: &mut [u8] =
            unsafe { core::slice::from_raw_parts_mut(shm_base as usize as *mut u8, SHM_LEN) };
        log_hex_event(
            HCD_URB_SETUP,
            Level::Info,
            "usb-hcd: shared URB buffer created",
            "shm_id_hex",
            shm_id,
        );

        let Some(endpoint_id) = bind_urb_endpoint() else {
            return EXIT_NO_TRANSPORT;
        };
        log_hex_event(
            HCD_URB_SETUP,
            Level::Info,
            "usb-hcd: URB endpoint created",
            "endpoint_hex",
            endpoint_id,
        );

        // The interface node (USB match keys + transport grants) is published
        // only after the IRQ wait source is proved live below, so the class
        // driver cannot be autoloaded into a transport with no completion wake.
        // `root_port` tracks the directly-attached device's root port for the
        // disconnect watch; it is refreshed after a re-enumeration. `0` while
        // no directly-attached device is present (a cold boot, or the hub
        // topology where the hub-status watch is used instead).
        let mut root_port = device.root_port();

        // Bind the controller's IRQ line before arming the completion
        // interrupter, so a completion produced immediately after `USBCMD.INTE`
        // is set has a kernel-owned line to latch onto instead of becoming a
        // stray message. The loop then parks on the interrupt rather than
        // polling a quiet controller.
        let irq_handle = match host.irq_line() {
            Some(line) => {
                let handle = rustos_rt::irq_bind(line);
                if handle >= 0 && device.enable_interrupter().is_ok() {
                    #[allow(clippy::cast_sign_loss)] // `handle >= 0` is the bound IrqHandle.
                    let handle = handle as u64;
                    log_hex_event(
                        HCD_URB_SETUP,
                        Level::Info,
                        "usb-hcd: IRQ bound and interrupter enabled",
                        "handle_hex",
                        handle,
                    );
                    Some(handle)
                } else {
                    log_hex_event(
                        HCD_URB_SETUP,
                        Level::Warn,
                        "usb-hcd: IRQ bind or interrupter enable failed",
                        "line_hex",
                        u64::from(line),
                    );
                    None
                }
            }
            _ => {
                log(
                    &LogSink,
                    &Event {
                        level: Level::Warn,
                        id: HCD_URB_SETUP,
                        message: "usb-hcd: no IRQ line grant for event-driven URB transport",
                        fields: &[],
                    },
                );
                None
            }
        };
        let Some(irq_handle) = irq_handle else {
            return EXIT_NO_TRANSPORT;
        };

        // Build the wait-set the loop parks on: the transport endpoint always,
        // and the controller IRQ line. Both must succeed before the interface
        // is published, because interrupt-IN URBs complete only through that
        // event-driven wake path.
        let set = rustos_rt::waitset_create();
        if set < 0 {
            return EXIT_NO_TRANSPORT;
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
        let set = set as u64;
        let endpoint_add = rustos_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            endpoint_id,
            TOKEN_URB,
        );
        if super::waitset_ctl_result(endpoint_add).is_err() {
            return EXIT_NO_TRANSPORT;
        }
        let irq_add = rustos_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Irq,
            irq_handle,
            TOKEN_IRQ,
        );
        if let Err(ret) = super::waitset_ctl_result(irq_add) {
            log_hex_event(
                HCD_URB_SETUP,
                Level::Warn,
                "usb-hcd: IRQ source add to wait-set failed",
                "ret_hex",
                ret as u64,
            );
            return EXIT_NO_TRANSPORT;
        }
        log_hex_event(
            HCD_URB_SETUP,
            Level::Info,
            "usb-hcd: IRQ source added to wait-set",
            "handle_hex",
            irq_handle,
        );

        // Publish the interface node only if a device is actually present.
        // A cold boot with the keyboard unplugged is a first-class state: the
        // controller comes up with no node, and the first hot-plug connect —
        // delivered through the onboard hub's status-change watch, or a
        // root-port connect — publishes the node from the event loop below.
        let mut node_live = device.device_present();
        let mut interface_node_id = 0u32;
        if node_live {
            let Some(id) = emit_interface_node(&mut device, endpoint_id, shm_id) else {
                return EXIT_EMIT_FAILED;
            };
            interface_node_id = id;
            log_hex_event(
                HCD_URB_SETUP,
                Level::Info,
                "usb-hcd: interface node emitted",
                "node_hex",
                u64::from(id),
            );
        } else {
            log(
                &LogSink,
                &Event {
                    level: Level::Info,
                    id: HCD_READY,
                    message: "usb-hcd: controller up, awaiting first device connect",
                    fields: &[],
                },
            );
        }

        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: HCD_READY,
                message: "usb-hcd: controller up, serving URB transport",
                fields: &[],
            },
        );

        let mut service = UrbService::new();

        // The asynchronous event loop: park until the transport endpoint or the
        // controller interrupt is ready, never spinning a quiet controller.
        loop {
            let mut token = 0u64;
            let wait_ret = rustos_rt::waitset_wait(set, super::WAIT_FOREVER_NS, &mut token);
            if wait_ret < 0 {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: wait-set wait failed",
                    "ret_hex",
                    wait_ret as u64,
                );
                // With an unbounded timeout on a wait-set we own, a negative
                // result means the set was torn down — stop rather than spin.
                return 0;
            }
            match token {
                TOKEN_URB => {
                    let mut request = [0u8; URB_REQUEST_LEN];
                    let mut ticket = 0u64;
                    match rustos_rt::call_recv(endpoint_id, &mut request, &mut ticket) {
                        Ok(n) => {
                            match service.on_submit(
                                node_live,
                                ticket,
                                &request[..n],
                                shm,
                                &mut device,
                            ) {
                                UrbOutcome::Reply(reply) => reply_to_urb(endpoint_id, reply),
                                UrbOutcome::Held => {}
                                UrbOutcome::Idle => log_hex_event(
                                    HCD_WAIT_ERROR,
                                    Level::Warn,
                                    "usb-hcd: submit path produced idle outcome",
                                    "ticket_hex",
                                    ticket,
                                ),
                            }
                        }
                        Err(err) => {
                            log_hex_event(
                                HCD_WAIT_ERROR,
                                Level::Warn,
                                "usb-hcd: call_recv failed after endpoint wake",
                                "errno_hex",
                                err as u64,
                            );
                        }
                    }
                }
                TOKEN_IRQ => {
                    // Acknowledge IMAN.IP before draining so a completion
                    // posted during the drain re-asserts rather than being
                    // lost. Event Handler Busy is released only by the per-event
                    // ERDP advance the drain performs, never by a standalone
                    // write on an empty ring: writing ERDP while the controller
                    // still has an un-dequeued event re-asserts immediately and
                    // spins the loop, while a per-event advance only ever clears
                    // EHB once the ring is genuinely caught up.
                    let _ = device.acknowledge_interrupt();
                    // Hot-plug. When a hub is watched (the device sits behind
                    // the onboard hub, so its root port never changes), a hub
                    // status-change report drives connect/disconnect: a fresh
                    // device is enumerated and a new interface node published
                    // (so `devmgr` re-autoloads the class driver onto the same
                    // transport), and a disconnect retracts the node. A
                    // directly-attached device instead has its root port
                    // watched for disconnect, and a root-port connect — whether
                    // the first ever (cold boot, nothing attached at bring-up)
                    // or a re-attach — drives a fresh re-enumeration. Both leave
                    // the controller up.
                    let mut disconnect_handled = false;
                    if device.hub_watch_active() {
                        // A keyboard on the Pi 4 hangs off a hub, and pulling it
                        // out takes that hub with it: the unplug surfaces as the
                        // *root* port (where the hub sat) clearing its connect
                        // bit, not as a downstream hub-port change — the hub is
                        // gone, so it answers neither its status-change endpoint
                        // nor a control transfer. Check the hub's own root port
                        // first; if it is gone, retract the interface and tear
                        // down so a re-plug re-enumerates from scratch.
                        match device.detach_if_hub_root_gone() {
                            Ok(true) => {
                                if node_live && rustos_rt::hw_remove_node(interface_node_id) < 0 {
                                    log_hex_event(
                                        HCD_WAIT_ERROR,
                                        Level::Warn,
                                        "usb-hcd: interface retraction failed after hub assembly detach",
                                        "node_hex",
                                        u64::from(interface_node_id),
                                    );
                                }
                                abort_pending_urb(endpoint_id, &mut service);
                                node_live = false;
                                disconnect_handled = true;
                                log(
                                    &LogSink,
                                    &Event {
                                        level: Level::Info,
                                        id: HCD_DISCONNECT,
                                        message: "usb-hcd: hub assembly disconnected at root port, interface retracted",
                                        fields: &[],
                                    },
                                );
                            }
                            Ok(false) => match device.next_hub_change(&delay) {
                                Ok(HubEvent::Attached(_)) => {
                                    if let Some(id) =
                                        emit_interface_node(&mut device, endpoint_id, shm_id)
                                    {
                                        interface_node_id = id;
                                        node_live = true;
                                        log_hex_event(
                                            HCD_ATTACHED,
                                            Level::Info,
                                            "usb-hcd: device attached, interface published",
                                            "node_hex",
                                            u64::from(id),
                                        );
                                    }
                                }
                                Ok(HubEvent::Detached) => {
                                    if node_live
                                        && rustos_rt::hw_remove_node(interface_node_id) >= 0
                                    {
                                        abort_pending_urb(endpoint_id, &mut service);
                                        node_live = false;
                                        disconnect_handled = true;
                                        log(
                                        &LogSink,
                                        &Event {
                                            level: Level::Info,
                                            id: HCD_DISCONNECT,
                                            message:
                                                "usb-hcd: device disconnected, interface retracted",
                                            fields: &[],
                                        },
                                    );
                                    } else if node_live {
                                        log_hex_event(
                                            HCD_WAIT_ERROR,
                                            Level::Warn,
                                            "usb-hcd: interface retraction failed after hub detach",
                                            "node_hex",
                                            u64::from(interface_node_id),
                                        );
                                    } else {
                                        abort_pending_urb(endpoint_id, &mut service);
                                        disconnect_handled = true;
                                    }
                                }
                                Ok(HubEvent::None) => {}
                                Err(err) => log_hex_event(
                                    HCD_WAIT_ERROR,
                                    Level::Warn,
                                    "usb-hcd: hub status-change service failed",
                                    "err_hex",
                                    err as u64,
                                ),
                            },
                            Err(err) => log_hex_event(
                                HCD_WAIT_ERROR,
                                Level::Warn,
                                "usb-hcd: hub root-port check failed",
                                "err_hex",
                                err as u64,
                            ),
                        }
                    } else if node_live {
                        // Directly-attached device: retract on a root-port
                        // disconnect (the `CCS` connect bit clearing).
                        if !still_connected(&mut device, root_port) {
                            if rustos_rt::hw_remove_node(interface_node_id) >= 0 {
                                abort_pending_urb(endpoint_id, &mut service);
                                node_live = false;
                                disconnect_handled = true;
                                log(
                                    &LogSink,
                                    &Event {
                                        level: Level::Info,
                                        id: HCD_DISCONNECT,
                                        message:
                                            "usb-hcd: device disconnected, interface retracted",
                                        fields: &[],
                                    },
                                );
                            } else {
                                log_hex_event(
                                    HCD_WAIT_ERROR,
                                    Level::Warn,
                                    "usb-hcd: interface retraction failed after root-port detach",
                                    "node_hex",
                                    u64::from(interface_node_id),
                                );
                            }
                        }
                    } else if device.any_root_port_connected() {
                        // A directly-attached device appeared on a root port —
                        // either the first connect after a cold boot with
                        // nothing attached at bring-up, or a re-attach after a
                        // disconnect. Reset the controller and enumerate it as a
                        // brand-new device, re-arm the interrupter the reset
                        // cleared (so the next connect/disconnect still wakes
                        // the loop), refresh the watched root port, and publish a
                        // fresh interface node so the class driver is autoloaded
                        // onto the same transport.
                        reset_reenumerate_and_publish(
                            &mut device,
                            endpoint_id,
                            shm_id,
                            &mut interface_node_id,
                            &mut node_live,
                            &delay,
                        );
                        root_port = device.root_port();
                    }
                    // A disconnect-handling teardown above (a hub status-change
                    // detach or a hub-assembly detach) can leave the controller
                    // halted with a latched Host System Error on the Pi 4 VL805;
                    // recover before the shortcut so the re-plug is still seen.
                    if recover_if_controller_faulted(
                        &mut device,
                        endpoint_id,
                        shm_id,
                        &mut interface_node_id,
                        &mut node_live,
                        &mut service,
                        &delay,
                    ) {
                        root_port = device.root_port();
                        continue;
                    }
                    // A disconnect tore the device down; the endpoint is gone,
                    // so there is nothing left to service this wake.
                    if disconnect_handled {
                        continue;
                    }
                    match service.on_event(shm, &mut device) {
                        UrbOutcome::Reply(reply) => {
                            if node_live {
                                match retract_after_fault_if_gone(
                                    endpoint_id,
                                    shm_id,
                                    &mut interface_node_id,
                                    &mut device,
                                    reply,
                                    &delay,
                                ) {
                                    FaultDetachOutcome::NotDetached => {
                                        reply_to_urb(endpoint_id, reply)
                                    }
                                    FaultDetachOutcome::Detached => node_live = false,
                                    FaultDetachOutcome::Reattached => node_live = true,
                                }
                            } else {
                                reply_to_urb(endpoint_id, reply);
                            }
                        }
                        UrbOutcome::Held => {
                            let _ = log(
                                &LogSink,
                                &Event {
                                    level: Level::Debug,
                                    id: HCD_URB_HELD,
                                    message: "usb-hcd: IRQ did not complete held URB yet",
                                    fields: &[],
                                },
                            );
                        }
                        UrbOutcome::Idle => {
                            let _ = log(
                                &LogSink,
                                &Event {
                                    level: Level::Debug,
                                    id: HCD_IRQ_WAKE,
                                    message: "usb-hcd: IRQ had no outstanding URB",
                                    fields: &[],
                                },
                            );
                        }
                    }
                    // The transfer-fault disconnect teardown (the Disable Slot in
                    // `retract_after_fault_if_gone`) latches the same controller
                    // fault on the Pi 4 VL805 after it completes; recover here too
                    // so the re-plug is seen rather than the controller staying
                    // halted and silent.
                    if recover_if_controller_faulted(
                        &mut device,
                        endpoint_id,
                        shm_id,
                        &mut interface_node_id,
                        &mut node_live,
                        &mut service,
                        &delay,
                    ) {
                        root_port = device.root_port();
                    }
                }
                _ => {}
            }
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
#[cfg(not(freestanding))]
fn main() {
    // On the host this binary is an inert stub: the freestanding `Run` program
    // above is built only for the bare-metal driver targets. Keeping a host
    // `main` lets `cargo build --workspace`, clippy, and fmt still cover the
    // file, mirroring the other driver `Run` binaries.
}

#[cfg(test)]
mod tests {
    use super::waitset_ctl_result;

    #[test]
    fn waitset_ctl_result_preserves_failure_code() {
        assert_eq!(waitset_ctl_result(0), Ok(()));
        assert_eq!(waitset_ctl_result(-2), Err(-2));
    }
}
