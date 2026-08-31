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
//! * **The controller's completion interrupt**: *acknowledge, drain once, then
//!   dispatch*. The acknowledgement's single `USBSTS` read carries the fault and
//!   port-change latches, so the whole service needs no second read of it. Then
//!   one pass over the event ring (`UsbDevice::pump_reports`) sorts every posted
//!   event into its consumer's buffer — each served interrupt-IN endpoint's
//!   reports into its per-device FIFO with the endpoint re-armed, each watched
//!   hub's status-change report into its parked slot, bulk completions into
//!   their FIFOs, a Port Status Change Event into the root-scan arming.
//!   Everything after that reads those buffers rather than walking the ring
//!   again: hot-plug is serviced, then any now-satisfiable outstanding URB is
//!   **replied** from the buffered report, bounce-copied into the shared buffer
//!   the class driver reads.
//!
//!   Capturing on the interrupt rather than only when a class driver submits
//!   decouples device polling from a CPU-starved class driver, so reports are
//!   never dropped under load (`plans/USB.md`). It also watches the root-hub
//!   port and retracts the interface node on a disconnect (`hw_remove_node`),
//!   so `devmgr` unloads the class driver while the controller stays up.
//!
//! # Per-interrupt cost
//!
//! A device streaming reports — a mouse in motion, at the 1 ms
//! interrupt-moderation ceiling — makes this path run ~1000 times a second, so
//! what it touches per pass is the driver's whole steady-state CPU cost. Three
//! properties keep it small, each with a budget regression in `lib/usb`:
//!
//! * **One register read.** The port scan is armed by a latched event rather
//!   than run unconditionally, and the fault check rides the acknowledgement's
//!   own read. On a PCIe controller a register read is a non-posted round trip
//!   and is the most expensive operation here.
//! * **One TRB per ring probe.** The engine reads the single 16-byte entry at
//!   the dequeue point, never the whole segment out of non-cacheable memory.
//! * **No allocation.** The path holds only fixed-capacity state; the heap is
//!   touched on attach and detach alone.
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
//! runtime `tairix-rt` (`_start`, the stack canary, the panic handler, the
//! syscall wrappers); on the host it is an inert stub so `cargo build
//! --workspace`, clippy, and fmt still cover the file. The live controller
//! bring-up and report path are metal-only because QEMU models no Pi USB; the
//! HCD's host-testable logic lives in the crate's `lib` target
//! ([`tairix_drv_bus_usb::bringup`] / [`tairix_drv_bus_usb::serve`]).

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// The per-index URB transport table grows with the devices the controller
// actually serves; `tairix-rt` supplies the process heap.
#[cfg(freestanding)]
extern crate alloc;

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
    use alloc::vec::Vec;
    use tairix_abi::hwtree::HW_NODE_ROOT;
    use tairix_abi::usb_urb::{decode_completion, URB_COMPLETION_LEN, URB_REQUEST_LEN};
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{CapabilityId, Errno};
    use tairix_caps::CapabilitySet;
    use tairix_drv_bus_usb::bringup::{
        bring_up_controller_diagnostic, derive_controller_resources, BringupPhase,
    };
    use tairix_drv_bus_usb::domain::{ControllerDomainEvent, ControllerHealth};
    use tairix_drv_bus_usb::serve::{attach_transport_grants, UrbOutcome, UrbReply, UrbService};
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_hid::{ReportFieldSummary, ReportMapSummary};
    use tairix_log::{log, Event, EventId, Field, Level};
    use tairix_rt::{ClockDelay, LogSink};
    use tairix_usb::device::{EnumStage, EventWait, HubEvent, MAX_INTERFACES, XHCI_MAX_SLOTS};
    use tairix_usb::XhciOpenStage;
    use tairix_util::fmt::format_hex_u64;

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

    /// Exit code when the controller's interrupt line could not be bound.
    /// The engine's synchronous event waits park on that line, so a
    /// controller with no usable interrupt cannot be served — refused
    /// fail-closed before any register is touched.
    const EXIT_NO_IRQ: i32 = 84;

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

    /// Diagnostic event id: a wait-set or IPC transport error happened.
    const HCD_WAIT_ERROR: EventId = EventId(4154);

    /// Diagnostic event id: a served HID interface's enumeration decision
    /// (report vs boot protocol, parsed field layout, armed transfer size).
    /// Logged once per interface at node publish, so a metal capture shows how
    /// a keyboard/mouse's reports will be read (QEMU models no Pi USB).
    const HCD_HID_ENUM: EventId = EventId(4150);

    /// Field slots the HID enumeration record fills: the five interface facts,
    /// `keyboard` + `report_id`, and the widest map's located fields (a mouse's
    /// buttons with a count, plus three two-key axes).
    const HID_ENUM_FIELDS_MAX: usize = 16;

    const _: () = assert!(HID_ENUM_FIELDS_MAX <= tairix_abi::LOG_FIELDS_MAX);

    /// The diagnostic keys one located report field logs under: its bit
    /// offset, its per-element width, and its element count where a count is
    /// meaningful (a scalar axis has none).
    #[derive(Copy, Clone)]
    struct LocKeys {
        offset: &'static str,
        size: &'static str,
        count: Option<&'static str>,
    }

    const BUTTON_KEYS: LocKeys = LocKeys {
        offset: "btn_off_bits",
        size: "btn_size_bits",
        count: Some("btn_count"),
    };
    const X_KEYS: LocKeys = LocKeys {
        offset: "x_off_bits",
        size: "x_size_bits",
        count: None,
    };
    const Y_KEYS: LocKeys = LocKeys {
        offset: "y_off_bits",
        size: "y_size_bits",
        count: None,
    };
    const WHEEL_KEYS: LocKeys = LocKeys {
        offset: "wheel_off_bits",
        size: "wheel_size_bits",
        count: None,
    };
    const MODIFIER_KEYS: LocKeys = LocKeys {
        offset: "mod_off_bits",
        size: "mod_size_bits",
        count: None,
    };
    const KEY_ARRAY_KEYS: LocKeys = LocKeys {
        offset: "keys_off_bits",
        size: "keys_size_bits",
        count: Some("keys_count"),
    };

    /// Append `key = value` at `count`, advancing it. A full buffer drops the
    /// field rather than panicking; the compile-time bound above is what keeps
    /// that from happening.
    fn push_field(
        fields: &mut [Field<'static>],
        count: &mut usize,
        key: &'static str,
        value: tairix_log::FieldValue<'static>,
    ) {
        if let Some(slot) = fields.get_mut(*count) {
            *slot = Field { key, value };
            *count += 1;
        }
    }

    /// Append one located field's offset, width, and element count.
    fn push_loc(
        fields: &mut [Field<'static>],
        count: &mut usize,
        keys: LocKeys,
        loc: ReportFieldSummary,
    ) {
        let u = tairix_log::FieldValue::UnsignedInt;
        push_field(fields, count, keys.offset, u(u64::from(loc.offset_bits)));
        push_field(fields, count, keys.size, u(u64::from(loc.size_bits)));
        if let Some(key) = keys.count {
            push_field(fields, count, key, u(u64::from(loc.count)));
        }
    }

    /// Interior fault-domain event id: the controller faulted and the whole
    /// subtree entered its shared recovery grace window (`plans/FIX-IO.md`
    /// IO4/IO5). Classified through the shared `for_fault_domain` vocabulary.
    const HCD_DOMAIN_RECOVERING: EventId = EventId(4190);

    /// Interior fault-domain event id: the controller demonstrably returned and
    /// the subtree recovered with no reboot.
    const HCD_DOMAIN_RECOVERED: EventId = EventId(4191);

    /// Interior fault-domain event id: the recovery grace window elapsed with
    /// the controller still faulted — the subtree is failed closed (the
    /// fault-domain owner's own distinct fail-closed event, sticky but
    /// recoverable).
    const HCD_DOMAIN_OFFLINE: EventId = EventId(4192);

    /// Reserved base of the URB call-endpoint id range the HCDs allocate from.
    ///
    /// A grant-restricted endpoint id the class driver reaches only through
    /// the kernel-minted grant on its matched node — distinct from the
    /// well-known `DRIVER_STORE_ENDPOINT`. Each controller claims one
    /// contiguous block of [`URB_ENDPOINT_BLOCK`] ids ([`claim_urb_block`]):
    /// creating a block's *base* id claims the whole block, so a second
    /// controller's HCD probes on to the next block and two controllers
    /// never collide on one id; the block's interior ids are bound lazily,
    /// one per device-table index, as devices actually serve
    /// ([`Transports::reconcile`]).
    const URB_ENDPOINT_BASE: u64 = 0x0055_5242_0000_0000;

    /// Ids per claimed endpoint block: one per device-table index one
    /// controller can ever serve concurrently — the xHCI protocol's
    /// 255-slot ceiling plus the DCBAA's scratchpad slot
    /// ([`XHCI_MAX_SLOTS`] + 1), times the servable interfaces a composite
    /// device can put on one slot ([`MAX_INTERFACES`]). Derived from
    /// protocol maxima, never a tuning knob, so any controller's full
    /// device complement fits its block.
    const URB_ENDPOINT_BLOCK: u64 = ((XHCI_MAX_SLOTS + 1) * MAX_INTERFACES) as u64;

    /// How many [`URB_ENDPOINT_BLOCK`]-id blocks to probe before giving up
    /// (a generous bound on simultaneous controllers; the first HCD claims
    /// the first block on its first try).
    const URB_ENDPOINT_BLOCKS: u64 = 64;

    /// Bytes of shared buffer per interface: one bulk chunk
    /// ([`tairix_usb::device::BULK_BUF_LEN`], the engine's per-TD ceiling —
    /// one definition, never a second constant), which also comfortably
    /// holds a boot report and any control-IN descriptor a class driver
    /// reads. One page, so the mass-storage data path costs the keyboard
    /// path nothing extra.
    const SHM_LEN: usize = tairix_usb::device::BULK_BUF_LEN;

    /// Outstanding-URB capacity of the per-interface endpoint. The class
    /// driver submits one at a time (it blocks on the reply); a small queue
    /// absorbs a re-submit racing the previous reply.
    const ENDPOINT_CAPACITY: usize = 4;

    /// The engine's parked event-wait seam on metal: waits park on the
    /// controller's bound interrupt line with the remaining wall-clock
    /// budget as the deadline, so a completion wakes the wait early, a
    /// timeout returns it to the caller's deadline check, and a quiet
    /// controller costs no CPU. The clock is the kernel monotonic clock,
    /// the same source [`tairix_rt::ClockDelay`] reads.
    struct IrqEventWait {
        /// The bound controller interrupt line ([`tairix_rt::irq_bind`]).
        handle: u64,
    }

    impl EventWait for IrqEventWait {
        fn now_us(&self) -> u64 {
            tairix_rt::clock_get() / 1_000
        }

        fn wait_us(&self, budget_us: u64) {
            // A refused wait (a revoked handle) degrades to the caller's
            // deadline check rather than spinning: the caller re-reads the
            // clock and fails closed when the budget is spent.
            let _ = tairix_rt::irq_wait(self.handle, budget_us.saturating_mul(1_000));
        }
    }

    /// Wait-set token for "the controller completion interrupt fired".
    const TOKEN_IRQ: u64 = 0;
    /// Base wait-set token for "a URB submit arrived on device index
    /// `token - TOKEN_URB_BASE`'s transport endpoint".
    const TOKEN_URB_BASE: u64 = 1;

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
                    value: tairix_log::FieldValue::Str(format_hex_u64(value, &mut value_buf)),
                }],
            },
        );
    }

    fn reply_to_urb(endpoint_id: u64, reply: tairix_drv_bus_usb::serve::UrbReply) {
        let ret = tairix_rt::call_reply(endpoint_id, reply.ticket, &reply.bytes[..reply.len]);
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
                ret.unsigned_abs(),
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
        let len = tairix_usb::transport::frame_completion(&mut bytes, Err(errno)).unwrap_or(0);
        reply_to_urb(endpoint_id, UrbReply { ticket, bytes, len });
    }

    fn urb_reply_errno(reply: &UrbReply) -> Option<Errno> {
        decode_completion(&reply.bytes[..reply.len]).err()
    }

    /// One device index's URB transport: the call endpoint and shared buffer
    /// created once at start-up (and reused across attach/detach cycles at
    /// the same index, so a re-plugged device's class driver lands on the
    /// same transport), the per-interface URB service, and the published
    /// interface node.
    struct Transport {
        /// The URB call endpoint the class driver submits on.
        endpoint_id: u64,
        /// The shared data buffer's kernel id, forwarded as a node grant.
        shm_id: u64,
        /// The HCD's own mapping of the shared buffer. The process serves
        /// this transport for its whole life, so the mapping is permanent.
        shm: &'static mut [u8],
        /// The per-interface URB service (at most one outstanding URB).
        service: UrbService,
        /// The published interface node id; meaningful only while
        /// [`Self::node_live`].
        node_id: u32,
        /// Whether an interface node is currently published for this index.
        node_live: bool,
    }

    /// Publish the interface node for the served device at `index` onto its
    /// transport, logging the attach. A device that cannot be described or
    /// whose node the kernel refuses stays unpublished (fail closed).
    fn publish_interface(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
        transport: &mut Transport,
    ) {
        if transport.node_live || !device.device_live(index) {
            return;
        }
        let Some(id) = emit_interface_node(device, index, transport.endpoint_id, transport.shm_id)
        else {
            return;
        };
        transport.node_id = id;
        transport.node_live = true;
        log_hex_event(
            HCD_ATTACHED,
            Level::Info,
            "usb-hcd: interface node emitted",
            "node_hex",
            u64::from(id),
        );
        // One-shot: record how this interface's reports will be read (report
        // vs boot protocol, parsed field layout, armed transfer size) so a
        // metal capture can diagnose a silenced device without guessing.
        log_hid_enum_diag(device, index);
    }

    /// Retract `transport`'s published interface node (best-effort) and
    /// abort its outstanding URB, so the class driver being unloaded never
    /// stays parked on a dead device.
    fn retract_interface(transport: &mut Transport) {
        if transport.node_live {
            // A physically-vanished device is a surprise removal: it is never
            // refused for being in use, so the flag set is empty.
            if tairix_rt::hw_remove_node(transport.node_id, tairix_abi::HwRemoveFlags::empty()) < 0
            {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: interface retraction failed",
                    "node_hex",
                    u64::from(transport.node_id),
                );
            }
            transport.node_live = false;
        }
        abort_pending_urb(transport.endpoint_id, &mut transport.service);
    }

    /// Reconcile every device-table index's published node with the
    /// engine's live device table: grow the per-index transport list to
    /// cover the table, create a missing transport when its index first
    /// serves (binding its endpoint id from the claimed block and creating
    /// its shared buffer), publish a node for each newly served index, and
    /// retract the node of each no-longer-served index (aborting its held
    /// URB). A composite device — a wireless keyboard+mouse receiver —
    /// attaches or detaches **several** indices in one hub event, so the
    /// whole table is trued up rather than a single index. An index whose
    /// transport cannot be created stays unpublished (fail closed) and is
    /// retried on the next reconcile; a created transport is kept across
    /// detaches so a re-plug finds it waiting.
    fn reconcile_interfaces(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
    ) {
        while transports.len() < device.device_table_len() {
            transports.push(None);
        }
        for (index, slot) in transports.iter_mut().enumerate() {
            if device.device_live(index) {
                if slot.is_none() {
                    *slot = create_transport(set, index, urb_base);
                }
                if let Some(transport) = slot {
                    publish_interface(device, index, transport);
                }
            } else if let Some(transport) = slot {
                if transport.node_live {
                    retract_interface(transport);
                }
            }
        }
    }

    /// Service every pending root-port connect/disconnect: the engine scans
    /// the `PORTSC.CSC` latches (a `SuperSpeed` device trains directly on a
    /// root port — on the Pi 4 the USB3 side of every jack is one — and
    /// pulling a hub assembly clears the root port it sat on), attaches or
    /// detaches what changed, and the published interfaces are reconciled
    /// after each event. Loops until the scan reports quiet, since one
    /// interrupt can carry several ports' changes; a failed service is
    /// logged with its whole attach-fault breadcrumb and the loop stops
    /// (the latch was consumed, so it cannot re-fire spuriously).
    ///
    /// Returns whether anything was attached or detached, so the caller only
    /// pays for the post-teardown controller-fault check when a teardown
    /// actually happened.
    fn service_root_changes(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
    ) -> bool {
        let mut changed = false;
        loop {
            match device.next_root_change(&delay) {
                Ok(HubEvent::None) => return changed,
                Ok(HubEvent::Attached(_) | HubEvent::HubAttached(_)) => {
                    reconcile_interfaces(device, transports, set, urb_base);
                    changed = true;
                    log(
                        &LogSink,
                        &Event {
                            level: Level::Info,
                            id: HCD_READY,
                            message: "usb-hcd: root-port device attached and served",
                            fields: &[],
                        },
                    );
                }
                Ok(HubEvent::Detached(_) | HubEvent::HubDetached(_)) => {
                    reconcile_interfaces(device, transports, set, urb_base);
                    changed = true;
                    log(
                        &LogSink,
                        &Event {
                            level: Level::Info,
                            id: HCD_DISCONNECT,
                            message: "usb-hcd: root-port device disconnected, interfaces retracted",
                            fields: &[],
                        },
                    );
                }
                Err(err) => {
                    log_topology_service_failure(
                        device,
                        "usb-hcd: root-port hot-plug service failed",
                        err,
                    );
                    return changed;
                }
            }
        }
    }

    /// Service one pending hub status-change after a fault detach re-armed
    /// the watch, reconciling the published interfaces with whatever the
    /// change attached or detached.
    fn service_hub_after_fault_detach(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
    ) {
        match device.next_hub_change(&delay) {
            Ok(
                HubEvent::Attached(_)
                | HubEvent::Detached(_)
                | HubEvent::HubAttached(_)
                | HubEvent::HubDetached(_),
            ) => {
                reconcile_interfaces(device, transports, set, urb_base);
            }
            Ok(HubEvent::None) => {}
            Err(err) => {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: hub watch re-arm after transfer fault failed",
                    "err_hex",
                    err as u64,
                );
            }
        }
    }

    /// Whether the failed URB reply for device `index` was caused by the
    /// device physically vanishing; if so, detach it, retract its interface,
    /// answer the URB `NotFound`, and service the hub watch (which may
    /// already carry the re-attach). `true` when the device was detached
    /// (the reply has then been answered); `false` leaves the reply for the
    /// caller to send.
    ///
    /// The engine surfaces a faulted transfer as [`Errno::DeviceFault`] (a
    /// report endpoint that could not be recovered, a bulk endpoint fault);
    /// only such a reply is a candidate for a disconnect confirmation, so any
    /// other error (a class-driver protocol violation, a malformed URB) is
    /// passed straight back to the class driver and never triggers a port
    /// read. `detach_if_device_gone` itself fails safe — a device whose port
    /// still reads connected is left live and its fault returned to the class
    /// driver unchanged.
    fn retract_after_fault_if_gone(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        reply: UrbReply,
        delay: ClockDelay,
    ) -> bool {
        if urb_reply_errno(&reply) != Some(Errno::DeviceFault) {
            return false;
        }
        match device.detach_if_device_gone(index) {
            Ok(true) => {
                // The detach freed every entry riding the device's slot (a
                // composite device's siblings vanish together), so true up
                // the whole node table, then answer the faulted URB.
                reconcile_interfaces(device, transports, set, urb_base);
                if let Some(transport) = transports.get_mut(index).and_then(Option::as_mut) {
                    reply_error(transport.endpoint_id, reply.ticket, Errno::NotFound);
                }
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
                service_hub_after_fault_detach(device, transports, set, urb_base, delay);
                true
            }
            Ok(false) => false,
            Err(err) => {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: disconnect confirmation after transfer fault failed",
                    "err_hex",
                    err as u64,
                );
                false
            }
        }
    }

    /// Reset the controller and re-enumerate from scratch (the engine
    /// re-programs the controller and re-enables its interrupter as part of
    /// the reset), publishing a fresh interface node for every device found
    /// back, so `devmgr` re-autoloads each class driver onto the same
    /// per-index transport.
    ///
    /// This is the recovery from a latched controller fault: the controller
    /// is returned to the same state a cold boot reaches, from which the
    /// next connect enumerates through the normal attach path. With no
    /// device present yet it simply leaves the controller awaiting that
    /// connect.
    fn reset_reenumerate_and_publish(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
    ) {
        if device.reset_and_reenumerate(&delay).is_err() {
            return;
        }
        reconcile_interfaces(device, transports, set, urb_base);
    }

    /// Recover if the controller has latched a fatal error or halted
    /// (`USBSTS.HSE`/`HCHalted`). Such a controller raises no further interrupts
    /// until it is reset (xHCI §4.24.1), so a watched device's hot-plug and
    /// transfers go silent — on the Pi 4 the VL805 latches a Host System Error
    /// during a downstream-device hot-removal teardown, after its Disable Slot
    /// has already completed, which is why an unplug worked but the controller
    /// never saw the re-plug. Retract every still-live interface, abort the
    /// held URBs, then reset and re-enumerate so the controller returns to the
    /// proven await-connect state and a re-plug enumerates normally. Returns
    /// whether a recovery ran (the caller then restarts its service pass on
    /// the freshly reset controller).
    fn recover_if_controller_faulted(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
    ) -> bool {
        if !device.controller_faulted() {
            return false;
        }
        // The whole register breadcrumb: which USBSTS fault bit latched
        // (HSE/HCE/HCHalted) is the only evidence a metal capture gets for
        // *why* a controller died mid-service (QEMU models no Pi USB).
        log(
            &LogSink,
            &Event {
                level: Level::Warn,
                id: HCD_DISCONNECT,
                message: "usb-hcd: controller fault latched, resetting to recover",
                fields: &[
                    opt_u32_field("usbsts", device.read_usbsts()),
                    opt_u32_field("usbcmd", device.read_usbcmd()),
                ],
            },
        );
        for transport in transports.iter_mut().flatten() {
            retract_interface(transport);
        }
        reset_reenumerate_and_publish(device, transports, set, urb_base, delay);
        true
    }

    /// Record a controller interior fault-domain edge as one audit event,
    /// naming the controller's owner id, **and** publish the coherent
    /// fault-domain state onto our own hardware-tree node so the observers
    /// beneath us react to one recovery episode rather than N spurious child
    /// failures (`plans/FIX-IO.md` IO4 cross-process propagation).
    ///
    /// Recovering/Recovered use the shared `BlkHealthTransition` vocabulary (via
    /// [`ControllerHealth`], over `for_fault_domain`), the same the leaf devices
    /// and the mount overlay use, so a controller recovery and a disk recovery
    /// cannot be classified differently; the fail-closed edge is the
    /// fault-domain owner's own distinct event.
    ///
    /// The health published on the tree is the *same edge* mapped to the
    /// [`FaultDomainState`](tairix_abi::blkio::FaultDomainState) an interior
    /// node reports: `Recovering` while the grace window is open, `Healthy`
    /// once the controller returns, `Offline` when the window elapses. The
    /// kernel records it against the controller's *own* matched node
    /// (resolved kernel-side; the driver never names a node), and the device
    /// manager's reactive watch reacts. It is best-effort cross-process
    /// hinting: a build with no hardware-tree store, or a controller not
    /// autoloaded for a node, simply has no observer to notify, and the leaf
    /// consumers still ride out their own transport blips as before — so a
    /// refused publish never fails the recovery, which the audit record above
    /// already captured loudly.
    fn log_domain_event(event: ControllerDomainEvent, owner: u32) {
        let (id, level, message, health) = match event {
            ControllerDomainEvent::Recovering => (
                HCD_DOMAIN_RECOVERING,
                Level::Warn,
                "usb-hcd: controller faulted, subtree held recovering under one grace window",
                tairix_abi::blkio::FaultDomainState::Recovering,
            ),
            ControllerDomainEvent::Recovered => (
                HCD_DOMAIN_RECOVERED,
                Level::Info,
                "usb-hcd: controller returned, subtree recovered",
                tairix_abi::blkio::FaultDomainState::Healthy,
            ),
            ControllerDomainEvent::FailedClosed => (
                HCD_DOMAIN_OFFLINE,
                Level::Warn,
                "usb-hcd: controller recovery grace window elapsed, subtree failed closed",
                tairix_abi::blkio::FaultDomainState::Offline,
            ),
        };
        log_hex_event(id, level, message, "owner_hex", u64::from(owner));
        let _ = tairix_rt::hw_node_health(health);
    }

    /// Recover a faulted controller under its interior fault domain, folding the
    /// outcome into `health` and auditing each edge.
    ///
    /// This wraps [`recover_if_controller_faulted`] with the controller's
    /// [`ControllerHealth`] machine so a controller blip is one coherent
    /// recovery episode over the whole subtree (`plans/FIX-IO.md` IO4), ridden
    /// out within a bounded grace window rather than either silently retried
    /// forever or — on a failed reset — left faulted with no timer to retry it
    /// (a faulted controller raises no further interrupts, xHCI §4.24.1, so the
    /// event loop would otherwise park indefinitely).
    ///
    /// A controller already **failed closed** (its grace window elapsed) is
    /// declared dead and is not retried: one that raises no interrupt and will
    /// not reset stays failed closed rather than re-opening its window forever
    /// (fail closed, sticky-but-recoverable — a later successful reset clears
    /// it). Returns whether a recovery was attempted, so the caller restarts its
    /// service pass on the freshly reset controller.
    fn recover_controller(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
        health: &mut ControllerHealth,
    ) -> bool {
        if !device.controller_faulted() || health.is_failed_closed() {
            return false;
        }
        let owner = health.owner();
        if let Some(event) = health.begin_recovery(tairix_rt::clock_get()) {
            log_domain_event(event, owner);
        }
        recover_if_controller_faulted(device, transports, set, urb_base, delay);
        let recovered = !device.controller_faulted();
        if let Some(event) = health.note_reset(recovered, tairix_rt::clock_get()) {
            log_domain_event(event, owner);
        }
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
        caps.insert(CapabilityId::SCHED_REALTIME);
        caps
    }

    /// Bind the URB transport endpoint `id`. Binding it grant-restricted
    /// (`send_caps` carries `CAP_IPC_ENDPOINT`) makes the kernel mint this
    /// HCD the matching per-endpoint grant, which it forwards onto the
    /// interface node so the class driver inherits exactly the right to
    /// submit URBs on this one interface. `false` if the kernel refused the
    /// id (already bound, or the create was rejected).
    fn bind_urb_endpoint(id: u64) -> bool {
        let mut send_caps = CapabilitySet::empty();
        send_caps.insert(CapabilityId::IPC_ENDPOINT);
        let recv_caps = CapabilitySet::empty();
        tairix_rt::call_create(
            id,
            &send_caps,
            &recv_caps,
            URB_REQUEST_LEN,
            URB_COMPLETION_LEN,
            ENDPOINT_CAPACITY,
        ) == 0
    }

    /// Claim this controller's URB endpoint-id block: binding a block's
    /// *base* id claims the whole block — that create is the only contended
    /// one, so a second controller's HCD moves on to the next block and two
    /// controllers never collide on an id. The block's interior ids — one
    /// per device-table index — are bound lazily as devices first serve
    /// ([`create_transport`]), so an idle controller holds one endpoint,
    /// not a table of them. Returns the claimed base id, or `None` when
    /// every block is taken.
    fn claim_urb_block() -> Option<u64> {
        for block in 0..URB_ENDPOINT_BLOCKS {
            let base = URB_ENDPOINT_BASE + block * URB_ENDPOINT_BLOCK;
            if bind_urb_endpoint(base) {
                return Some(base);
            }
        }
        None
    }

    /// Create device index `index`'s URB transport: bind its call endpoint
    /// from the claimed block (`urb_base + index`; the block's base id was
    /// already bound by [`claim_urb_block`], and doubles as index 0's
    /// endpoint), create its shared data buffer, and register the endpoint
    /// on the wait-set under the index's token. `None` on any refusal (the
    /// caller fails closed — a transport-less device is never published —
    /// and retries on the next reconcile).
    fn create_transport(set: u64, index: usize, urb_base: u64) -> Option<Transport> {
        let endpoint_id = urb_base + u64::try_from(index).ok()?;
        if index > 0 && !bind_urb_endpoint(endpoint_id) {
            return None;
        }
        let mut shm_id = 0u64;
        // A negative return is the errno and a base this pointer width cannot
        // hold is equally unusable: either way there is no transport.
        let shm_base = usize::try_from(tairix_rt::shm_create(SHM_LEN, &mut shm_id)).ok()?;
        // SAFETY: `shm_create` mapped `SHM_LEN` bytes of zeroed, cacheable,
        // RW (non-executable) memory into this process at `shm_base` and
        // returned that base. The region is owned by this process for the
        // rest of its life (never unmapped), and no other reference in this
        // address space aliases it — each transport owns its own region — so
        // a single exclusive `&mut [u8]` over exactly the requested length is
        // sound (and `'static`, as the mapping is permanent). The class
        // driver maps the same frames in its *own* address space;
        // cross-process sharing is outside Rust's aliasing model (like
        // DMA/MMIO) and is synchronised by the URB reply, which
        // happens-after the HCD's write here.
        let shm: &'static mut [u8] =
            unsafe { core::slice::from_raw_parts_mut(shm_base as *mut u8, SHM_LEN) };
        let token = TOKEN_URB_BASE + u64::try_from(index).ok()?;
        let endpoint_add = tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            endpoint_id,
            token,
        );
        if super::waitset_ctl_result(endpoint_add).is_err() {
            return None;
        }
        log_hex_event(
            HCD_URB_SETUP,
            Level::Info,
            "usb-hcd: URB transport created",
            "endpoint_hex",
            endpoint_id,
        );
        Some(Transport {
            endpoint_id,
            shm_id,
            shm,
            service: UrbService::new(),
            node_id: 0,
            node_live: false,
        })
    }

    /// Build and publish the served device at `index`'s interface node — its
    /// `vid:pid:class` match keys plus the per-interface URB-transport grants
    /// (the call endpoint and the shared buffer) the class driver inherits —
    /// returning the kernel-assigned node id.
    ///
    /// Used for the initial publish and, identically, for a re-attach after a
    /// hot-plug: a *fresh* node so `devmgr` re-autoloads the class driver onto
    /// the same transport endpoint, so the device's data resumes to the same
    /// OS sink. `None` if the device is not enumerated or the kernel refuses
    /// the node.
    fn emit_interface_node(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
        endpoint_id: u64,
        shm_id: u64,
    ) -> Option<u32> {
        let node = device.describe_device(index, HW_NODE_ROOT, 0).ok()?;
        let node = attach_transport_grants(node, endpoint_id, shm_id).ok()?;
        // A negative return is the errno; anything else is the assigned node id.
        u32::try_from(tairix_rt::hw_emit_node(&node)).ok()
    }

    /// Drive every transport with a URB outstanding: drained controller
    /// events may complete any of them. Called after the IRQ arm's drain
    /// **and** after a URB submit is serviced — a synchronous engine wait
    /// inside a submit parks on the same interrupt line and consumes its
    /// edge, stashing any asynchronous completion that edge carried, so the
    /// stash must be drained here rather than waiting for another
    /// interrupt. A transport whose device just detached has already had
    /// its URB aborted, so it is simply not busy.
    ///
    /// Returns whether a transfer fault detached a device, so the caller only
    /// pays for the post-teardown controller-fault check when a teardown
    /// actually happened.
    fn service_busy_urbs(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
    ) -> bool {
        let mut any_detached = false;
        for index in 0..transports.len() {
            let busy = transports[index]
                .as_ref()
                .is_some_and(|transport| transport.service.is_busy());
            if !busy {
                continue;
            }
            let Some(outcome) = transports[index].as_mut().map(|transport| {
                transport
                    .service
                    .on_event(transport.shm, &mut device.engine_for(index))
            }) else {
                continue;
            };
            match outcome {
                UrbOutcome::Reply(reply) => {
                    if let Some(errno) = urb_reply_errno(&reply) {
                        log_urb_error(device, index, errno);
                    }
                    let node_live = transports[index]
                        .as_ref()
                        .is_some_and(|transport| transport.node_live);
                    let detached = node_live
                        && retract_after_fault_if_gone(
                            device, index, transports, set, urb_base, reply, delay,
                        );
                    any_detached |= detached;
                    if !detached {
                        if let Some(transport) = transports[index].as_ref() {
                            reply_to_urb(transport.endpoint_id, reply);
                        }
                    }
                }
                UrbOutcome::Held => {
                    let _ = log(
                        &LogSink,
                        &Event {
                            level: Level::Debug,
                            id: HCD_URB_HELD,
                            message: "usb-hcd: event did not complete held URB yet",
                            fields: &[],
                        },
                    );
                }
                // `is_busy` was checked above, so an Idle outcome cannot
                // occur; nothing to service either way.
                UrbOutcome::Idle => {}
            }
        }
        any_detached
    }

    /// Run the engine's consumer-independent report pump on a controller
    /// interrupt: capture every served interrupt-IN device's reports into the
    /// per-device buffers and keep every such endpoint armed, independent of
    /// any class-driver URB ([`tairix_usb::device::UsbDevice::pump_reports`]).
    ///
    /// A drain fault and any newly dropped reports (a class driver that has
    /// stalled past the buffer depth) are logged so a stuck consumer is never
    /// silent; neither is fatal to the loop — the pump is best-effort and the
    /// hot-plug watch owns device teardown.
    fn pump_reports(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        reported_drops: &mut u64,
    ) {
        if let Err(err) = device.pump_reports() {
            log_hex_event(
                HCD_WAIT_ERROR,
                Level::Warn,
                "usb-hcd: report pump failed",
                "err_hex",
                err as u64,
            );
        }
        let dropped = device.dropped_report_total();
        if dropped > *reported_drops {
            log_hex_event(
                HCD_WAIT_ERROR,
                Level::Warn,
                "usb-hcd: interrupt reports dropped; class driver stalled",
                "dropped",
                dropped,
            );
            *reported_drops = dropped;
        }
    }

    /// Log the HID enumeration decision for the interface at `index` once, at
    /// node publish: whether it runs report or boot protocol, its declared
    /// report-descriptor length, the interrupt endpoint's `wMaxPacketSize` and
    /// the armed transfer length, and — when a report map was parsed — the
    /// located field layout. This is the metal window on *how* a device's
    /// reports will be read (QEMU models no Pi USB); a non-HID interface has no
    /// diagnostic and logs nothing.
    fn log_hid_enum_diag(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
    ) {
        let Some(diag) = device.hid_enum_diag(index) else {
            return;
        };
        let u = |v: u64| tairix_log::FieldValue::UnsignedInt(v);
        let b = tairix_log::FieldValue::Bool;
        // The interface's own enumeration facts, common to both arms.
        let mut fields = [Field {
            key: "",
            value: tairix_log::FieldValue::Null,
        }; HID_ENUM_FIELDS_MAX];
        let mut count = 0usize;
        push_field(&mut fields, &mut count, "index", u(index as u64));
        push_field(
            &mut fields,
            &mut count,
            "report_proto",
            b(diag.report_protocol),
        );
        push_field(
            &mut fields,
            &mut count,
            "desc_len",
            u(u64::from(diag.report_descriptor_len)),
        );
        push_field(
            &mut fields,
            &mut count,
            "max_packet",
            u(u64::from(diag.int_max_packet)),
        );
        push_field(
            &mut fields,
            &mut count,
            "capture_len",
            u(u64::from(diag.capture_len)),
        );
        let Some(map) = diag.map else {
            log(
                &LogSink,
                &Event {
                    level: Level::Info,
                    id: HCD_HID_ENUM,
                    message: "usb-hcd: HID interface boot-protocol fallback",
                    fields: &fields[..count],
                },
            );
            return;
        };
        // Every field the parser located, so the log shows where each one is
        // read from rather than only the first of them: a pointer whose axes
        // are misread produces flickering button bits, and only the offsets
        // side by side show it.
        match map {
            ReportMapSummary::Mouse {
                report_id,
                buttons,
                x,
                y,
                wheel,
            } => {
                push_field(&mut fields, &mut count, "keyboard", b(false));
                push_field(
                    &mut fields,
                    &mut count,
                    "report_id",
                    u(u64::from(report_id)),
                );
                push_loc(&mut fields, &mut count, BUTTON_KEYS, buttons);
                push_loc(&mut fields, &mut count, X_KEYS, x);
                push_loc(&mut fields, &mut count, Y_KEYS, y);
                match wheel {
                    Some(loc) => push_loc(&mut fields, &mut count, WHEEL_KEYS, loc),
                    // An absent wheel is stated, never a zero offset a reader
                    // would take for a located field.
                    None => push_field(
                        &mut fields,
                        &mut count,
                        WHEEL_KEYS.offset,
                        tairix_log::FieldValue::Null,
                    ),
                }
            }
            ReportMapSummary::Keyboard {
                report_id,
                modifiers,
                keys,
            } => {
                push_field(&mut fields, &mut count, "keyboard", b(true));
                push_field(
                    &mut fields,
                    &mut count,
                    "report_id",
                    u(u64::from(report_id)),
                );
                push_loc(&mut fields, &mut count, MODIFIER_KEYS, modifiers);
                push_loc(&mut fields, &mut count, KEY_ARRAY_KEYS, keys);
            }
        }
        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: HCD_HID_ENUM,
                message: "usb-hcd: HID interface report protocol",
                fields: &fields[..count],
            },
        );
    }

    /// A diagnostic field carrying a controller value that may not have been
    /// readable: an unreadable register is logged `Null`, never a fabricated
    /// zero.
    fn opt_u32_field(key: &'static str, value: Option<u32>) -> Field<'static> {
        Field {
            key,
            value: value.map_or(tairix_log::FieldValue::Null, |v| {
                tairix_log::FieldValue::UnsignedInt(u64::from(v))
            }),
        }
    }

    /// A URB completed with an error: log the errno the class driver will
    /// see **and** the engine's latched raw completion code for the
    /// device's own endpoint, so a metal capture shows the controller's
    /// verdict (transaction error, stall, babble, …) behind the coarse
    /// errno — e.g. the keyboard's collateral fault while a sibling
    /// port's attach was being serviced.
    fn log_urb_error(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
        errno: Errno,
    ) {
        log(
            &LogSink,
            &Event {
                level: Level::Warn,
                id: HCD_WAIT_ERROR,
                message: "usb-hcd: URB completed with an error",
                fields: &[
                    Field {
                        key: "index",
                        value: tairix_log::FieldValue::UnsignedInt(index as u64),
                    },
                    Field {
                        key: "errno",
                        value: tairix_log::FieldValue::UnsignedInt(errno as u64),
                    },
                    Field {
                        key: "fault_code",
                        value: tairix_log::FieldValue::UnsignedInt(u64::from(
                            device.last_report_fault_code(index),
                        )),
                    },
                ],
            },
        );
    }

    /// Emit a topology (hub status-change or root-port) service failure
    /// with its **whole** breadcrumb: the coarse error alone cannot name
    /// the failing hot-plug step, so a failed attach's snapshot — the
    /// stage it failed in, the last observed completion/event-type/reject,
    /// the targeted port and its final observed `wPortStatus` (`0` for a
    /// root port, which has none) — is logged when one exists (the
    /// snapshot is taken at the failure, before the cleanup transfers
    /// overwrite the live state). A failure outside an attach (a status
    /// read, retire, or watch re-arm) logs the live diagnostics instead.
    /// This is how a metal capture localises a failed hot-plug (QEMU
    /// models no Pi USB).
    fn log_topology_service_failure(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        message: &'static str,
        err: tairix_abi::DriverError,
    ) {
        let u = |v: u64| tairix_log::FieldValue::UnsignedInt(v);
        let usbsts = opt_u32_field("usbsts", device.read_usbsts());
        let event = |fields: &[Field<'_>]| {
            log(
                &LogSink,
                &Event {
                    level: Level::Warn,
                    id: HCD_WAIT_ERROR,
                    message,
                    fields,
                },
            );
        };
        if let Some(fault) = device.last_attach_fault() {
            event(&[
                Field {
                    key: "err",
                    value: u(err as u64),
                },
                Field {
                    key: "attach_port",
                    value: u(u64::from(fault.port)),
                },
                Field {
                    key: "enum_stage",
                    value: u(u64::from(fault.stage.as_u8())),
                },
                Field {
                    key: "completion",
                    value: u(u64::from(fault.completion)),
                },
                Field {
                    key: "event_type",
                    value: u(u64::from(fault.event_type)),
                },
                Field {
                    key: "reject",
                    value: u(u64::from(fault.reject)),
                },
                Field {
                    key: "port_status",
                    value: u(u64::from(fault.port_status)),
                },
                usbsts,
            ]);
        } else {
            event(&[
                Field {
                    key: "err",
                    value: u(err as u64),
                },
                Field {
                    key: "enum_stage",
                    value: u(u64::from(device.enum_stage().as_u8())),
                },
                Field {
                    key: "completion",
                    value: u(u64::from(device.last_completion_code())),
                },
                Field {
                    key: "event_type",
                    value: u(u64::from(device.last_event_type())),
                },
                Field {
                    key: "reject",
                    value: u(u64::from(device.last_reject_reason())),
                },
                usbsts,
            ]);
        }
    }

    /// Emit the one-shot controller bring-up failure with its **whole**
    /// breadcrumb: QEMU models no Pi USB, so this diagnostic is how a metal
    /// run localises the stall. The phase alone cannot separate a timeout
    /// from a rejected completion or name the failing enumeration step, so
    /// the phase-specific controller state is always included.
    fn log_bringup_failure(err: &tairix_drv_bus_usb::bringup::ControllerBringupError) {
        let phase = Field {
            key: "phase",
            value: tairix_log::FieldValue::Str(err.phase.as_str()),
        };
        let error = Field {
            key: "error",
            value: tairix_log::FieldValue::UnsignedInt(err.error as u64),
        };
        let event = |fields: &[Field<'_>]| {
            log(
                &LogSink,
                &Event {
                    level: Level::Error,
                    id: HCD_BRINGUP_FAILED,
                    message: "usb-hcd: controller bring-up failed",
                    fields,
                },
            );
        };
        match err.phase {
            BringupPhase::ControllerOpen => event(&[
                phase,
                error,
                Field {
                    key: "open_stage",
                    value: tairix_log::FieldValue::Str(
                        err.open_stage.map_or("-", XhciOpenStage::as_str),
                    ),
                },
                opt_u32_field("usbcmd", err.usbcmd),
                opt_u32_field("usbsts", err.usbsts),
            ]),
            BringupPhase::Enumerate => event(&[
                phase,
                error,
                Field {
                    key: "enum_stage",
                    value: tairix_log::FieldValue::UnsignedInt(u64::from(
                        err.enum_stage.map_or(0, EnumStage::as_u8),
                    )),
                },
                Field {
                    key: "completion",
                    value: tairix_log::FieldValue::UnsignedInt(u64::from(err.last_completion)),
                },
                Field {
                    key: "event_type",
                    value: tairix_log::FieldValue::UnsignedInt(u64::from(err.last_event_type)),
                },
                Field {
                    key: "reject",
                    value: tairix_log::FieldValue::UnsignedInt(u64::from(err.last_reject)),
                },
                opt_u32_field("port1_portsc", err.port1_portsc),
            ]),
            BringupPhase::Setup | BringupPhase::BarMap | BringupPhase::ControllerStart => {
                event(&[phase, error]);
            }
        }
    }

    /// Emit the post-bring-up topology summary, so a metal capture shows
    /// what the walk actually served — and warn when a connected device was
    /// present but failed enumeration and was skipped, which otherwise looks
    /// exactly like an empty port.
    fn log_bringup_summary(device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>) {
        let live = (0..device.device_table_len())
            .filter(|&index| device.device_live(index))
            .count();
        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: HCD_READY,
                message: "usb-hcd: bring-up walk complete",
                fields: &[
                    Field {
                        key: "devices",
                        value: tairix_log::FieldValue::UnsignedInt(live as u64),
                    },
                    Field {
                        key: "hub_watch",
                        value: tairix_log::FieldValue::Bool(device.hub_watch_active()),
                    },
                ],
            },
        );
        if device.skipped_port_count() > 0 {
            log(
                &LogSink,
                &Event {
                    level: Level::Warn,
                    id: HCD_BRINGUP_FAILED,
                    message: "usb-hcd: connected device(s) failed enumeration and were skipped",
                    fields: &[Field {
                        key: "skipped_ports",
                        value: tairix_log::FieldValue::UnsignedInt(u64::from(
                            device.skipped_port_count(),
                        )),
                    }],
                },
            );
        }
    }

    /// Enter the strict-priority real-time scheduling class so the
    /// controller-interrupt report pump ([`pump_reports`]) preempts CPU-bound
    /// work and cannot be starved: under a load like `stress --cpu N` the
    /// IRQ-woken wake that drains the interrupt-IN endpoints and re-arms them
    /// must run before the armed transfer ring fills, no matter how busy
    /// userland is, or reports are dropped at the hardware (the on-metal
    /// "missed keypresses under load" defect; `plans/USB.md`).
    ///
    /// The manifest grants `CAP_SCHED_REALTIME`; a build that somehow runs
    /// without it degrades gracefully to fair scheduling — the report pump
    /// still runs, only without the strict guarantee — rather than refusing to
    /// start.
    fn enter_realtime_class() {
        let rt = tairix_rt::sched_set_realtime(true);
        if rt == 0 {
            log(
                &LogSink,
                &Event {
                    level: Level::Info,
                    id: HCD_READY,
                    message: "usb-hcd: entered real-time scheduling class (report pump cannot be starved)",
                    fields: &[],
                },
            );
        } else {
            log_hex_event(
                HCD_WAIT_ERROR,
                Level::Warn,
                "usb-hcd: real-time scheduling class refused; serving time-shared",
                "err_hex",
                rt.unsigned_abs(),
            );
        }
    }

    /// Bind the controller's interrupt line, returning the kernel handle.
    ///
    /// Called **before** the controller is brought up: the engine's
    /// synchronous event waits park on this line (its interrupter is enabled
    /// as part of starting the controller), so it must already be kernel-owned
    /// — a completion posted the moment interrupts are enabled then latches
    /// instead of going astray. [`None`] refuses the controller before any
    /// register is touched: one with no usable interrupt line cannot be served
    /// event-driven.
    fn bind_controller_irq(line: Option<u32>) -> Option<u64> {
        let Some(line) = line else {
            log(
                &LogSink,
                &Event {
                    level: Level::Warn,
                    id: HCD_URB_SETUP,
                    message: "usb-hcd: no IRQ line grant for event-driven service",
                    fields: &[],
                },
            );
            return None;
        };
        // A negative return is the errno; anything else is the bound handle.
        let Ok(handle) = u64::try_from(tairix_rt::irq_bind(line)) else {
            log_hex_event(
                HCD_URB_SETUP,
                Level::Warn,
                "usb-hcd: IRQ bind failed",
                "line_hex",
                u64::from(line),
            );
            return None;
        };
        log_hex_event(
            HCD_URB_SETUP,
            Level::Info,
            "usb-hcd: controller IRQ line bound",
            "handle_hex",
            handle,
        );
        Some(handle)
    }

    /// Create the wait-set the event loop parks on and register the controller
    /// interrupt on it under [`TOKEN_IRQ`]; each transport endpoint joins as it
    /// is created. All of this must succeed before any interface is published,
    /// because interrupt-IN URBs complete only through that event-driven wake
    /// path, so [`None`] refuses the controller.
    fn create_event_set(irq_handle: u64) -> Option<u64> {
        // A negative return is the errno; anything else is the wait-set handle.
        let set = u64::try_from(tairix_rt::waitset_create()).ok()?;
        let irq_add = tairix_rt::waitset_ctl(
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
                ret.unsigned_abs(),
            );
            return None;
        }
        log_hex_event(
            HCD_URB_SETUP,
            Level::Info,
            "usb-hcd: IRQ source added to wait-set",
            "handle_hex",
            irq_handle,
        );
        Some(set)
    }

    /// Publish an interface node for every device enumerated at bring-up
    /// (creating each served index's transport on the way), then announce that
    /// the controller is serving.
    ///
    /// A cold boot with nothing plugged in is a first-class state: the
    /// controller comes up with no node, and the first hot-plug connect —
    /// delivered through the onboard hub's status-change watch, or a root-port
    /// connect — publishes from the event loop.
    fn publish_initial_interfaces(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
    ) {
        reconcile_interfaces(device, transports, set, urb_base);
        if !device.any_device_live() {
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
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // Coherent DMA is carved kernel-side, so no architecture-specific
        // cache-maintenance shim is supplied (`coherency = None`).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        enter_realtime_class();
        let Ok(resources) = derive_controller_resources(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        let delay = ClockDelay::new();
        let Some(irq_handle) = bind_controller_irq(host.irq_line()) else {
            return EXIT_NO_IRQ;
        };
        let wait = IrqEventWait { handle: irq_handle };

        let mut device = match bring_up_controller_diagnostic(
            &host,
            &delay,
            &wait,
            resources.bar_base,
            resources.bar_len,
            resources.dma_aperture_top,
        ) {
            Ok(device) => device,
            Err(err) => {
                log_bringup_failure(&err);
                return EXIT_BRINGUP_FAILED;
            }
        };
        log_bringup_summary(&mut device);

        let Some(set) = create_event_set(irq_handle) else {
            return EXIT_NO_TRANSPORT;
        };

        // Claim this controller's URB endpoint-id block. The per-interface
        // transports — one shared data buffer and one grant-restricted call
        // endpoint per served device index, each minting this HCD the grant
        // it forwards onto that index's interface node — are created lazily
        // as device-table indices first serve (`reconcile_interfaces`), so
        // the controller pays for the devices actually attached, never a
        // fixed table.
        let Some(urb_base) = claim_urb_block() else {
            return EXIT_NO_TRANSPORT;
        };
        let mut transports: Vec<Option<Transport>> = Vec::new();
        publish_initial_interfaces(&mut device, &mut transports, set, urb_base);

        // The controller is the interior fault-domain owner of every device
        // below it (`plans/FIX-IO.md` IO4): a controller-wide fault (a latched
        // Host System Error, HCHalted, the HCRST reset) is one recovery episode
        // over the whole subtree, ridden out within a bounded grace window
        // before it is failed closed — not one spurious failure per device. The
        // owner id is this controller's own runtime-discovered URB endpoint
        // block base (never a board constant), naming the owner in the audit
        // log.
        let controller_owner = u32::try_from(urb_base & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        let mut controller_health = ControllerHealth::new(controller_owner);

        serve_events(
            &mut device,
            &mut transports,
            set,
            urb_base,
            delay,
            &mut controller_health,
        )
    }

    /// The asynchronous event loop: park — unbounded, with no periodic wakes —
    /// until a transport endpoint or the controller interrupt is ready, never
    /// spinning a quiet controller. Downstream hot-plug arrives through the
    /// watched hub's status-change interrupt-IN completion; a root-port
    /// connect/disconnect through the controller's Port Status Change
    /// interrupt.
    ///
    /// Returns the process exit code; it returns only once the wait-set is
    /// torn down under us.
    fn serve_events(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
        health: &mut ControllerHealth,
    ) -> i32 {
        // Running total of interrupt reports the engine has dropped because a
        // class driver stalled past the buffer depth; logged (once per new
        // loss) so a genuinely stuck consumer is never silent.
        let mut reported_drops = 0u64;
        loop {
            let mut token = 0u64;
            // While the controller is recovering, park only until its grace
            // one-shot comes due, so a faulted controller — which raises no
            // further interrupt (xHCI §4.24.1) — is retried and failed closed
            // on time off a one-shot rather than parking forever. With nothing
            // recovering the loop parks unbounded (never a spin).
            let timeout = health
                .wait_timeout(tairix_rt::clock_get())
                .unwrap_or(super::WAIT_FOREVER_NS);
            let wait_ret = tairix_rt::waitset_wait(set, timeout, &mut token);
            if wait_ret < 0 {
                if Errno::from_syscall(wait_ret) == Errno::TimedOut {
                    // The controller grace one-shot fired. Retry the reset (a
                    // faulted controller raises no interrupt to wake us); this
                    // fails it closed once the window has elapsed. If it is no
                    // longer faulted it returned on its own — record that.
                    if device.controller_faulted() {
                        let _ =
                            recover_controller(device, transports, set, urb_base, delay, health);
                    } else if let Some(event) = health.note_reset(true, tairix_rt::clock_get()) {
                        log_domain_event(event, health.owner());
                    }
                    continue;
                }
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: wait-set wait failed",
                    "ret_hex",
                    wait_ret.unsigned_abs(),
                );
                // Any other negative result on a wait-set we own means the
                // set was torn down — stop rather than spin.
                return 0;
            }
            match token {
                token if token >= TOKEN_URB_BASE => {
                    // The token was registered as `TOKEN_URB_BASE + index`.
                    let Ok(index) = usize::try_from(token - TOKEN_URB_BASE) else {
                        continue;
                    };
                    serve_urb_endpoint(device, transports, set, urb_base, delay, index);
                }
                TOKEN_IRQ => service_controller_interrupt(
                    device,
                    transports,
                    set,
                    urb_base,
                    delay,
                    health,
                    &mut reported_drops,
                ),
                _ => {}
            }
        }
    }

    /// Serve the URB submit that woke device `index`'s transport endpoint,
    /// then drain any completion the submit's own synchronous wait stashed.
    ///
    /// The receive is non-blocking: the wait-set's readiness peek is not a
    /// guarantee — the queued call may have been cancelled by its poster's
    /// exit (the kernel scrubs a dead caller's in-flight calls) — and the loop
    /// serves every transport plus the controller IRQ, so it must never park
    /// on one endpoint.
    fn serve_urb_endpoint(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
        index: usize,
    ) {
        let Some(transport) = transports.get_mut(index).and_then(Option::as_mut) else {
            return;
        };
        let mut request = [0u8; URB_REQUEST_LEN];
        let mut ticket = 0u64;
        // Whether servicing the submit could have consumed another transport's
        // completion. Only a submit that ran a synchronous engine wait can: it
        // parks on the shared interrupt line and stashes whatever else that
        // edge carried. A held interrupt-IN URB ran no wait — the report simply
        // was not buffered yet — so there is nothing stashed to drive.
        let mut wait_may_have_stashed = false;
        match tairix_rt::call_recv_nonblock(transport.endpoint_id, &mut request, &mut ticket) {
            Ok(n) => {
                match transport.service.on_submit(
                    transport.node_live,
                    ticket,
                    &request[..n],
                    transport.shm,
                    &mut device.engine_for(index),
                ) {
                    UrbOutcome::Reply(reply) => {
                        wait_may_have_stashed = true;
                        reply_to_urb(transport.endpoint_id, reply);
                    }
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
            // An empty queue after a wake is benign: the queued call was
            // cancelled (its poster exited) between the readiness peek and
            // this receive.
            Err(err) if Errno::from_syscall(err) == Errno::WouldBlock => {}
            Err(err) => {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: call_recv failed after endpoint wake",
                    "errno_hex",
                    err.unsigned_abs(),
                );
            }
        }
        if wait_may_have_stashed {
            service_busy_urbs(device, transports, set, urb_base, delay);
        }
    }

    /// Service the controller interrupt: acknowledge it, drain the event ring
    /// **once** into the per-consumer buffers, then dispatch from those buffers
    /// — hot-plug, then the buffered reports handed to any outstanding URB —
    /// recovering the controller around each teardown path that can latch a
    /// fault.
    ///
    /// Draining before dispatching is what keeps the ring walked once per
    /// interrupt with one shared classifier, rather than each consumer walking
    /// it again with its own copy of that decision.
    fn service_controller_interrupt(
        device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: ClockDelay,
        health: &mut ControllerHealth,
        reported_drops: &mut u64,
    ) {
        // Acknowledge IMAN.IP before draining so a completion posted during
        // the drain re-asserts rather than being lost. Event Handler Busy is
        // released only by the per-event ERDP advance the drain performs,
        // never by a standalone write on an empty ring: writing ERDP while the
        // controller still has an un-dequeued event re-asserts immediately and
        // spins the loop, while a per-event advance only ever clears EHB once
        // the ring is genuinely caught up.
        // The acknowledgement's single USBSTS read also carries the fault and
        // port-change latches, so the whole service needs no further read of
        // it. A controller already faulted when we woke raises no further
        // interrupt, so recover before touching anything else.
        let faulted = device
            .acknowledge_interrupt()
            .is_ok_and(|status| status.faulted);
        if faulted && recover_controller(device, transports, set, urb_base, delay, health) {
            return;
        }
        // Drain the event ring once, here, into the per-consumer buffers: every
        // served interrupt-IN device's reports into its FIFO (with its endpoint
        // re-armed), each watched hub's status-change completion into its parked
        // slot, bulk completions into their FIFOs, and a Port Status Change
        // Event into the root-scan arming. Capturing reports off the interrupt
        // rather than only when a class driver submits is what makes the report
        // path immune to a CPU-starved class driver — no report is lost merely
        // because the software above it was not scheduled — and it covers every
        // interrupt-IN device the controller serves, not just the one whose URB
        // happens to be in flight. Everything below dispatches from those
        // buffers rather than walking the ring again.
        pump_reports(device, reported_drops);
        // Hot-plug. Root-port connects/disconnects come from the `PORTSC.CSC`
        // latches (a `SuperSpeed` device trains directly on a root port;
        // pulling a hub assembly clears the root port it sat on — either way
        // the change stays latched even when its Port Status Change Event was
        // drained by an engine wait). Then a watched hub's status-change report
        // drives downstream connect/disconnect: a fresh device is enumerated
        // and a new interface node published on its index's transport (so
        // `devmgr` autoloads the class driver onto the same endpoint across a
        // re-plug), and a disconnect retracts only that device's node. All
        // leave the controller up.
        let mut topology_changed = service_root_changes(device, transports, set, urb_base, delay);
        // Every hub with a report parked is serviced, not just the first: a
        // hub's status-change endpoint is re-armed only once its report is
        // serviced, so a second reporting hub left until "the next interrupt"
        // may never get one. Bounded by the watched-hub count — each keeps one
        // status transfer outstanding, so that is every report the drain can
        // have parked — which stops a flapping hub whose endpoint re-completes
        // during each service from holding this loop and starving the other
        // devices' URBs.
        for _ in 0..device.watched_hub_count() {
            match device.next_hub_change(&delay) {
                Ok(HubEvent::Attached(_) | HubEvent::HubAttached(_)) => {
                    // A fresh leaf device — or a fresh hub tier whose
                    // downstream devices were enumerated with it — is
                    // published by diffing every live index.
                    reconcile_interfaces(device, transports, set, urb_base);
                    topology_changed = true;
                    log(
                        &LogSink,
                        &Event {
                            level: Level::Info,
                            id: HCD_READY,
                            message: "usb-hcd: hub-port device attached and served",
                            fields: &[],
                        },
                    );
                }
                Ok(HubEvent::Detached(_) | HubEvent::HubDetached(_)) => {
                    // A vanished leaf device — or a vanished hub tier with
                    // everything behind it — is retracted by the same diff.
                    reconcile_interfaces(device, transports, set, urb_base);
                    topology_changed = true;
                    log(
                        &LogSink,
                        &Event {
                            level: Level::Info,
                            id: HCD_DISCONNECT,
                            message: "usb-hcd: device disconnected, interface retracted",
                            fields: &[],
                        },
                    );
                }
                Ok(HubEvent::None) => break,
                Err(err) => {
                    log_topology_service_failure(
                        device,
                        "usb-hcd: hub status-change service failed",
                        err,
                    );
                    break;
                }
            }
        }
        // A disconnect-handling teardown above (a hub status-change detach or
        // a hub-assembly detach) can leave the controller halted with a
        // latched Host System Error on the Pi 4 VL805; recover before
        // servicing so the re-plug is still seen. Only a teardown can latch it,
        // so a routine report interrupt pays nothing to check.
        if topology_changed && recover_controller(device, transports, set, urb_base, delay, health)
        {
            return;
        }
        // Hand the already-buffered reports to any outstanding URB. A drained
        // completion may satisfy any transport, and one the hot-plug handling
        // above parked must not wait for another interrupt.
        let detached = service_busy_urbs(device, transports, set, urb_base, delay);
        // The transfer-fault disconnect teardown (the Disable Slot in
        // `retract_after_fault_if_gone`) latches the same controller fault on
        // the Pi 4 VL805 after it completes; recover here too so the re-plug
        // is seen rather than the controller staying halted and silent.
        if detached {
            let _ = recover_controller(device, transports, set, urb_base, delay, health);
        }
    }

    tairix_rt::entry!(main);
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
