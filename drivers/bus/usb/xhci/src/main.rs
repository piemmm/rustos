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
//! creates (`shm_create`) for each interface node it publishes, and no other
//! node ever carries, forwarded as a grant on that node; the class driver
//! inherits the grant and `shm_map`s the same frames. The buffer is plain
//! cacheable RAM with no DMA properties — the class driver holds **zero** DMA
//! authority — and the HCD bounce-copies between it and its own DMA-granted
//! ring.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `tairix-rt` (`_start`, the stack canary, the panic handler, the
//! syscall wrappers); on the host it is an inert stub so `cargo build
//! --workspace`, clippy, and fmt still cover the file. The live controller
//! bring-up and report path are metal-only because QEMU models no Pi USB; the
//! HCD's host-testable logic lives in the crate's `lib` target
//! ([`tairix_drv_bus_usb::bringup`], [`tairix_drv_bus_usb::serve`],
//! [`tairix_drv_bus_usb::interfaces`]).

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
    use tairix_abi::hwtree::HW_NODE_ROOT;
    use tairix_abi::usb_urb::{URB_COMPLETION_LEN, URB_REQUEST_LEN};
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{CapabilityId, DriverError, Errno, HwNode, RegisterWindow};
    use tairix_caps::CapabilitySet;
    use tairix_drv_bus_usb::bringup::{
        bring_up_controller_diagnostic, derive_controller_resources, BringupPhase, ControllerDevice,
    };
    use tairix_drv_bus_usb::domain::{ControllerDomainEvent, ControllerHealth, SkippedPortRetry};
    use tairix_drv_bus_usb::interfaces::{Interfaces, Note, Seam, UrbBuffer, ENDPOINT_CAPACITY};
    use tairix_drv_bus_usb::serve::UrbReply;
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_hid::{ReportFieldSummary, ReportMapSummary};
    use tairix_log::{log, Event, EventId, Field, Level};
    use tairix_rt::shm::SharedRegion;
    use tairix_rt::{ClockDelay, LogSink};
    use tairix_usb::device::{
        DeviceEngine, DeviceIdentity, EnumStage, EventWait, HubEvent, MAX_INTERFACES,
        XHCI_MAX_SLOTS,
    };
    use tairix_usb::{SlabBank, XhciOpenStage};
    use tairix_util::fmt::{format_hex_bytes, format_hex_u64};

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

    /// Exit code when the controller did not come back within its recovery
    /// grace window. Every interface node is retracted first; exiting hands
    /// the controller's memory to the kernel, which quarantines what a
    /// controller that would not reset may still reach.
    const EXIT_CONTROLLER_FAILED: i32 = 85;

    /// Exit code when the event loop's own wait-set failed, so nothing it
    /// serves can wake it again. Every interface node is retracted first.
    const EXIT_WAIT_FAILED: i32 = 86;

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

    /// Diagnostic event id: a URB reply was sent or attempted.
    const HCD_URB_REPLY: EventId = EventId(4152);

    /// Diagnostic event id: a wait-set or IPC transport error happened.
    const HCD_WAIT_ERROR: EventId = EventId(4154);

    /// Diagnostic event id: a served HID interface's enumeration decision
    /// (report vs boot protocol, parsed field layout, armed transfer size) and
    /// the Report Descriptor it was derived from. Logged once per interface at
    /// node publish, so a metal capture shows how a keyboard/mouse's reports
    /// will be read *and* whether that reading is right (QEMU models no Pi
    /// USB).
    const HCD_HID_ENUM: EventId = EventId(4150);

    /// Field slots the HID enumeration record fills: the five interface facts,
    /// `keyboard` + `report_id`, and the widest map's located fields (a mouse's
    /// buttons with a count, plus three two-key axes).
    const HID_ENUM_FIELDS_MAX: usize = 16;

    const _: () = assert!(HID_ENUM_FIELDS_MAX <= tairix_abi::LOG_FIELDS_MAX);

    /// Bytes of Report Descriptor per dump record. Two hex characters each, so
    /// a record's value stays inside the ABI's field-value bound; a longer
    /// descriptor spans further records naming their own byte offset.
    const HID_DESC_CHUNK: usize = 64;

    const _: () = assert!(HID_DESC_CHUNK * 2 <= tairix_abi::LOG_FIELD_VALUE_MAX);

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

    /// Append the map's Report ID, distinguishing "this device declares none"
    /// from an id that happens to be zero.
    ///
    /// A device with no Report IDs needs no demux at all, while one with them
    /// must have every report matched — logging both as `0` hid exactly the
    /// difference a mis-read report turns on, so the absent case is `Null`.
    fn push_report_id(fields: &mut [Field<'static>], count: &mut usize, report_id: Option<u8>) {
        let value = report_id.map_or(tairix_log::FieldValue::Null, |id| {
            tairix_log::FieldValue::UnsignedInt(u64::from(id))
        });
        push_field(fields, count, "report_id", value);
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
    /// one per transport, as the interface table opens them.
    const URB_ENDPOINT_BASE: u64 = 0x0055_5242_0000_0000;

    /// Ids per claimed endpoint block: one per transport, and a controller
    /// never needs more transports than interfaces it serves at once — the
    /// xHCI protocol's 255-slot ceiling plus the DCBAA's scratchpad slot
    /// ([`XHCI_MAX_SLOTS`] + 1), times the servable interfaces a composite
    /// device can put on one slot ([`MAX_INTERFACES`]). Derived from
    /// protocol maxima, never a tuning knob, so any controller's full
    /// device complement fits its block.
    const URB_ENDPOINT_BLOCK: u64 = ((XHCI_MAX_SLOTS + 1) * MAX_INTERFACES) as u64;

    /// How many [`URB_ENDPOINT_BLOCK`]-id blocks to probe before giving up
    /// (a generous bound on simultaneous controllers; the first HCD claims
    /// the first block on its first try).
    const URB_ENDPOINT_BLOCKS: u64 = 64;

    /// Bytes of shared buffer per interface node: one bulk chunk
    /// ([`tairix_usb::device::BULK_BUF_LEN`], the engine's per-TD ceiling —
    /// one definition, never a second constant), which also comfortably
    /// holds a boot report and any control-IN descriptor a class driver
    /// reads. One page, so the mass-storage data path costs the keyboard
    /// path nothing extra.
    const SHM_LEN: usize = tairix_usb::device::BULK_BUF_LEN;

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
    /// Base wait-set token for "a URB submit arrived on transport slot
    /// `token - TOKEN_URB_BASE`'s endpoint".
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

    fn reply_to_urb(endpoint_id: u64, reply: UrbReply) {
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

    /// The HCD's own mapping of one interface node's shared URB buffer.
    struct NodeBuffer(SharedRegion);

    impl UrbBuffer for NodeBuffer {
        fn region(&self) -> u64 {
            self.0.id()
        }

        fn bytes(&mut self) -> &mut [u8] {
            self.0.bytes_mut()
        }
    }

    /// The live controller and kernel the interface table drives.
    struct Live<'d, 'h> {
        device: &'d mut ControllerDevice<'h>,
        delay: ClockDelay,
        /// The event loop's wait-set.
        set: u64,
        /// This controller's endpoint block ([`claim_urb_block`]).
        urb_base: u64,
    }

    impl<'h> Seam for Live<'_, 'h> {
        type Buffer = NodeBuffer;
        type Engine<'a>
            = DeviceEngine<'a, 'h, RegisterWindow, SlabBank<'h>>
        where
            Self: 'a;

        fn table_len(&self) -> usize {
            self.device.device_table_len()
        }

        fn identity(&self, index: usize) -> Option<DeviceIdentity> {
            self.device.device_identity(index)
        }

        fn describe(&self, index: usize) -> Result<HwNode, DriverError> {
            self.device.describe_device(index, HW_NODE_ROOT, 0)
        }

        fn engine(&mut self, index: usize) -> Self::Engine<'_> {
            self.device.engine_for(index)
        }

        fn detach_if_gone(&mut self, index: usize) -> Result<bool, DriverError> {
            self.device.detach_if_device_gone(index)
        }

        fn next_hub_change(&mut self) -> Result<HubEvent, DriverError> {
            self.device.next_hub_change(&self.delay)
        }

        fn faulted(&mut self) -> bool {
            self.device.controller_faulted()
        }

        fn reset(&mut self) -> Result<(), DriverError> {
            // Which USBSTS fault bit latched is the only evidence a metal
            // capture gets for why the controller died (QEMU models no Pi USB).
            log(
                &LogSink,
                &Event {
                    level: Level::Warn,
                    id: HCD_DISCONNECT,
                    message: "usb-hcd: resetting the controller to recover it",
                    fields: &[
                        opt_u32_field("usbsts", self.device.read_usbsts()),
                        opt_u32_field("usbcmd", self.device.read_usbcmd()),
                    ],
                },
            );
            self.device.reset_and_reenumerate(&self.delay)
        }

        fn open_endpoint(&mut self, slot: usize) -> Option<u64> {
            let offset = u64::try_from(slot)
                .ok()
                .filter(|&offset| offset < URB_ENDPOINT_BLOCK)?;
            let endpoint = self.urb_base + offset;
            // Claiming the block bound its base, which serves slot 0.
            (offset == 0 || bind_urb_endpoint(endpoint)).then_some(endpoint)
        }

        fn watch_endpoint(&mut self, slot: usize, endpoint: u64) -> bool {
            let Some(token) = u64::try_from(slot)
                .ok()
                .and_then(|slot| TOKEN_URB_BASE.checked_add(slot))
            else {
                return false;
            };
            let added = tairix_rt::waitset_ctl(
                self.set,
                WaitSetOp::Add,
                WaitSourceKind::Endpoint,
                endpoint,
                token,
            );
            if super::waitset_ctl_result(added).is_err() {
                return false;
            }
            log_hex_event(
                HCD_URB_SETUP,
                Level::Info,
                "usb-hcd: URB transport created",
                "endpoint_hex",
                endpoint,
            );
            true
        }

        fn create_buffer(&mut self) -> Option<NodeBuffer> {
            SharedRegion::create(SHM_LEN).map(NodeBuffer)
        }

        fn receive(
            &mut self,
            endpoint: u64,
            request: &mut [u8],
        ) -> Result<Option<(u64, usize)>, Errno> {
            let mut ticket = 0u64;
            match tairix_rt::call_recv_nonblock(endpoint, request, &mut ticket) {
                Ok(len) => Ok(Some((ticket, len))),
                Err(err) => match Errno::from_syscall(err) {
                    Errno::WouldBlock => Ok(None),
                    errno => Err(errno),
                },
            }
        }

        fn reply(&mut self, endpoint: u64, reply: UrbReply) {
            reply_to_urb(endpoint, reply);
        }

        fn emit(&mut self, node: &HwNode) -> Option<u32> {
            // A negative return is the errno; anything else is the node id.
            u32::try_from(tairix_rt::hw_emit_node(node)).ok()
        }

        fn remove(&mut self, id: u32) {
            // A device that vanished is never refused for being in use, so
            // the removal is a surprise one: an empty flag set.
            if tairix_rt::hw_remove_node(id, tairix_abi::HwRemoveFlags::empty()) < 0 {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: interface retraction failed",
                    "node_hex",
                    u64::from(id),
                );
            }
        }

        fn now_ns(&self) -> u64 {
            tairix_rt::clock_get()
        }

        fn note(&mut self, note: Note) {
            match note {
                Note::Published { index, node } => {
                    log_hex_event(
                        HCD_ATTACHED,
                        Level::Info,
                        "usb-hcd: interface node emitted",
                        "node_hex",
                        u64::from(node),
                    );
                    // How the interface's reports will be read, and the
                    // descriptor that decision came from: a silenced device's
                    // only diagnosis on metal.
                    log_hid_enum_diag(self.device, index);
                    log_hid_report_descriptor(self.device, index);
                }
                Note::UrbFailed { index, errno } => log_urb_error(self.device, index, errno),
                Note::FaultDetached => {
                    log(
                        &LogSink,
                        &Event {
                            level: Level::Info,
                            id: HCD_DISCONNECT,
                            message: "usb-hcd: device transfer fault confirmed disconnect, interface retracted",
                            fields: &[],
                        },
                    );
                }
                Note::DetachUnconfirmed(err) => log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: disconnect confirmation after transfer fault failed",
                    "err_hex",
                    err as u64,
                ),
                Note::HubServiceFailed(err) => log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: hub watch re-arm after transfer fault failed",
                    "err_hex",
                    err as u64,
                ),
                Note::ReceiveFailed(errno) => log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Warn,
                    "usb-hcd: call_recv failed after endpoint wake",
                    "errno_hex",
                    errno as u64,
                ),
                Note::Domain { event, owner } => log_domain_event(event, owner),
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
        live: &mut Live<'_, '_>,
        interfaces: &mut Interfaces<NodeBuffer>,
    ) -> bool {
        let mut changed = false;
        loop {
            match live.device.next_root_change(&live.delay) {
                Ok(HubEvent::None) => return changed,
                Ok(HubEvent::Attached(_) | HubEvent::HubAttached(_)) => {
                    interfaces.reconcile(live);
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
                    interfaces.reconcile(live);
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
                        live.device,
                        "usb-hcd: root-port hot-plug service failed",
                        err,
                    );
                    return changed;
                }
            }
        }
    }

    /// Audit a controller fault-domain edge, naming the controller's owner
    /// id, and publish the state it leaves onto the controller's own
    /// hardware-tree node (`plans/FIX-IO.md` IO4), so the leaves below read
    /// one recovery episode rather than a failure each. The publish is
    /// best-effort: the audit record is authoritative, and a refused publish
    /// never fails the recovery.
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
            // The serve loop exits on this edge, so it states why.
            ControllerDomainEvent::FailedClosed => (
                HCD_DOMAIN_OFFLINE,
                Level::Error,
                "usb-hcd: controller did not come back within its grace window; interfaces retracted, exiting",
                tairix_abi::blkio::FaultDomainState::Offline,
            ),
        };
        log_hex_event(id, level, message, "owner_hex", u64::from(owner));
        let _ = tairix_rt::hw_node_health(health);
    }

    /// Spend the single deferred re-attach the bring-up walk owed a port it
    /// could not serve (`SkippedPortRetry`): re-drive every connected but
    /// unserved port, publish an interface for whatever that served, and log
    /// the outcome.
    ///
    /// A port skipped by the walk had its connect latch consumed there, so no
    /// hot-plug event will ever wake it again — this is the one chance a
    /// device that merely lost the boot race gets before a user has to unplug
    /// it. A port still unserved afterwards is logged with its failing
    /// snapshot and left alone; the retry is never re-armed, so a genuinely
    /// broken device cannot loop.
    fn retry_skipped_ports(live: &mut Live<'_, '_>, interfaces: &mut Interfaces<NodeBuffer>) {
        if let Err(err) = live.device.retry_skipped_ports(&live.delay) {
            log_hex_event(
                HCD_WAIT_ERROR,
                Level::Warn,
                "usb-hcd: deferred re-attach of unserved port(s) failed",
                "err_hex",
                err as u64,
            );
        }
        interfaces.reconcile(live);
        if live.device.skipped_port_count() == 0 {
            log(
                &LogSink,
                &Event {
                    level: Level::Info,
                    id: HCD_READY,
                    message: "usb-hcd: deferred re-attach served every connected port",
                    fields: &[],
                },
            );
            return;
        }
        log_skipped_ports(live.device);
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
    /// per transport — are bound as the interface table opens them
    /// ([`Seam::open_endpoint`]), so an idle controller holds one endpoint,
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
            // A refused interface delivers no report at all, so it must never
            // read in the log as a working boot-protocol device: it is the one
            // outcome a user would otherwise see only as silent hardware.
            let (level, message) = if diag.reports_refused {
                (
                    Level::Warn,
                    "usb-hcd: HID interface refused (device in report protocol, no usable map)",
                )
            } else {
                (Level::Info, "usb-hcd: HID interface boot-protocol fallback")
            };
            log(
                &LogSink,
                &Event {
                    level,
                    id: HCD_HID_ENUM,
                    message,
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
                push_report_id(&mut fields, &mut count, report_id);
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
                push_report_id(&mut fields, &mut count, report_id);
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

    /// Log interface `index`'s HID Report Descriptor as hex, [`HID_DESC_CHUNK`]
    /// bytes per record, each naming the byte offset it starts at.
    ///
    /// [`log_hid_enum_diag`] records what the parsed map *says*; this records
    /// what it was derived *from*, which is what makes a wrong map diagnosable
    /// rather than merely visible — an interface whose descriptor declares
    /// Report IDs the map did not pin to reads its sibling collections' reports
    /// as its own, and only the bytes show it. A Report Descriptor is a device
    /// capability blob, so no keystroke or pointer movement passes through it.
    /// An interface that declared none logs nothing.
    fn log_hid_report_descriptor(
        device: &tairix_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
    ) {
        for (record, chunk) in device
            .hid_report_descriptor(index)
            .chunks(HID_DESC_CHUNK)
            .enumerate()
        {
            let mut hex = [0u8; HID_DESC_CHUNK * 2];
            log(
                &LogSink,
                &Event {
                    level: Level::Info,
                    id: HCD_HID_ENUM,
                    message: "usb-hcd: HID interface report descriptor",
                    fields: &[
                        Field {
                            key: "index",
                            value: tairix_log::FieldValue::UnsignedInt(index as u64),
                        },
                        Field {
                            key: "offset",
                            value: tairix_log::FieldValue::UnsignedInt(
                                (record * HID_DESC_CHUNK) as u64,
                            ),
                        },
                        Field {
                            key: "hex",
                            value: tairix_log::FieldValue::Str(format_hex_bytes(chunk, &mut hex)),
                        },
                    ],
                },
            );
        }
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
        log_skipped_ports(device);
    }

    /// Warn that connected device(s) were present but left unserved, naming
    /// the **first failing port's snapshot** — the port, the enumeration step
    /// it failed in, and the completion/event-type/reject codes the failing
    /// transfer saw, captured at the failure before the cleanup transfers
    /// overwrote the live state.
    ///
    /// The live engine breadcrumb is not usable here: the walk continues past
    /// a skip, so after a multi-port controller it describes whichever port
    /// ran *last*, not the one that failed. QEMU models no Pi USB, so this is
    /// the diagnostic a metal capture localises an unserved device with.
    fn log_skipped_ports(device: &mut tairix_drv_bus_usb::bringup::ControllerDevice<'_>) {
        if device.skipped_port_count() == 0 {
            return;
        }
        let u = |v: u64| tairix_log::FieldValue::UnsignedInt(v);
        let skipped = Field {
            key: "skipped_ports",
            value: u(u64::from(device.skipped_port_count())),
        };
        let message = "usb-hcd: connected device(s) failed enumeration and were skipped";
        match device.last_attach_fault() {
            Some(fault) => {
                log(
                    &LogSink,
                    &Event {
                        level: Level::Warn,
                        id: HCD_BRINGUP_FAILED,
                        message,
                        fields: &[
                            skipped,
                            Field {
                                key: "err",
                                value: u(fault.error as u64),
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
                        ],
                    },
                );
            }
            None => {
                log(
                    &LogSink,
                    &Event {
                        level: Level::Warn,
                        id: HCD_BRINGUP_FAILED,
                        message,
                        fields: &[skipped],
                    },
                );
            }
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

    /// Publish an interface node for every device enumerated at bring-up,
    /// then announce that the controller is serving.
    ///
    /// A cold boot with nothing plugged in is a first-class state: the
    /// controller comes up with no node, and the first hot-plug connect —
    /// delivered through the onboard hub's status-change watch, or a root-port
    /// connect — publishes from the event loop.
    fn publish_initial_interfaces(
        live: &mut Live<'_, '_>,
        interfaces: &mut Interfaces<NodeBuffer>,
    ) {
        interfaces.reconcile(live);
        if !live.device.any_device_live() {
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

        // Transports are opened as devices need them, so the controller pays
        // for the devices attached, never a fixed table.
        let Some(urb_base) = claim_urb_block() else {
            return EXIT_NO_TRANSPORT;
        };
        let mut live = Live {
            device: &mut device,
            delay,
            set,
            urb_base,
        };
        let mut interfaces = Interfaces::new();
        publish_initial_interfaces(&mut live, &mut interfaces);

        // The owner id names the controller in the audit log: its own
        // discovered endpoint block, never a board constant.
        let controller_owner = u32::try_from(urb_base & 0xFFFF_FFFF).unwrap_or(u32::MAX);
        let mut controller_health = ControllerHealth::new(controller_owner);

        // A port the walk could not serve had its connect latch consumed
        // there, so no hot-plug event will ever wake it again. Owe it one
        // deferred re-attach so a device that merely lost the boot race comes
        // up without the user unplugging it.
        let mut port_retry = SkippedPortRetry::default();
        if live.device.skipped_port_count() > 0 {
            port_retry.arm(tairix_rt::clock_get());
        }

        serve_events(
            &mut live,
            &mut interfaces,
            &mut controller_health,
            &mut port_retry,
        )
    }

    /// The asynchronous event loop: park — unbounded, with no periodic wakes —
    /// until a transport endpoint or the controller interrupt is ready, never
    /// spinning a quiet controller. Downstream hot-plug arrives through the
    /// watched hub's status-change interrupt-IN completion; a root-port
    /// connect/disconnect through the controller's Port Status Change
    /// interrupt.
    ///
    /// Returns the process exit code: once the controller fails closed, or
    /// once the wait-set is torn down under us.
    fn serve_events(
        live: &mut Live<'_, '_>,
        interfaces: &mut Interfaces<NodeBuffer>,
        health: &mut ControllerHealth,
        port_retry: &mut SkippedPortRetry,
    ) -> i32 {
        // Running total of interrupt reports the engine has dropped because a
        // class driver stalled past the buffer depth; logged (once per new
        // loss) so a genuinely stuck consumer is never silent.
        let mut reported_drops = 0u64;
        loop {
            let mut token = 0u64;
            // Park only as long as the nearest armed one-shot allows. Two can
            // be pending: the controller's grace window — a faulted
            // controller raises no further interrupt (xHCI §4.24.1), so it
            // must be retried and failed closed off a timer — and the single
            // deferred re-attach owed to a port the walk could not serve.
            // With neither armed the loop parks unbounded (never a spin).
            let now_ns = tairix_rt::clock_get();
            let timeout = [health.wait_timeout(now_ns), port_retry.wait_timeout(now_ns)]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(super::WAIT_FOREVER_NS);
            let wait_ret = tairix_rt::waitset_wait(live.set, timeout, &mut token);
            if wait_ret >= 0 {
                match token {
                    TOKEN_IRQ => {
                        service_controller_interrupt(live, interfaces, health, &mut reported_drops);
                    }
                    // Registered as `TOKEN_URB_BASE + slot`.
                    token => {
                        let slot = token
                            .checked_sub(TOKEN_URB_BASE)
                            .and_then(|slot| usize::try_from(slot).ok());
                        if let Some(slot) = slot {
                            interfaces.serve_submit(slot, health, live);
                        }
                    }
                }
            } else if Errno::from_syscall(wait_ret) == Errno::TimedOut {
                // Spend the deferred re-attach whenever its deadline has
                // passed, even if the controller's own recovery runs instead:
                // an overdue one-shot left armed would bound every later park
                // at zero and spin the loop.
                let retry_due = port_retry.take_if_due(tairix_rt::clock_get());
                // A recovery re-runs the whole bring-up walk, which subsumes
                // the re-attach.
                let attempted = interfaces.recover(health, live);
                if retry_due && !attempted && !health.is_failed_closed() {
                    retry_skipped_ports(live, interfaces);
                }
            } else {
                log_hex_event(
                    HCD_WAIT_ERROR,
                    Level::Error,
                    "usb-hcd: wait-set wait failed; stopping",
                    "ret_hex",
                    wait_ret.unsigned_abs(),
                );
                interfaces.retract_all(live);
                return EXIT_WAIT_FAILED;
            }
            // Nothing is retried below a controller failed closed; its reason
            // is already logged and every node retracted.
            if health.is_failed_closed() {
                return EXIT_CONTROLLER_FAILED;
            }
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
        live: &mut Live<'_, '_>,
        interfaces: &mut Interfaces<NodeBuffer>,
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
        let faulted = live
            .device
            .acknowledge_interrupt()
            .is_ok_and(|status| status.faulted);
        if faulted && interfaces.recover(health, live) {
            return;
        }
        // Nothing is served through a controller whose last reset did not
        // bring it back — the grace one-shot owns the next attempt — nor
        // through one failed closed.
        if health.is_recovering() || health.is_failed_closed() {
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
        pump_reports(live.device, reported_drops);
        // Hot-plug. Root-port connects/disconnects come from the `PORTSC.CSC`
        // latches (a `SuperSpeed` device trains directly on a root port;
        // pulling a hub assembly clears the root port it sat on — either way
        // the change stays latched even when its Port Status Change Event was
        // drained by an engine wait). Then a watched hub's status-change report
        // drives downstream connect/disconnect. Both leave the controller up.
        let mut topology_changed = service_root_changes(live, interfaces);
        // Every hub with a report parked is serviced, not just the first: a
        // hub's status-change endpoint is re-armed only once its report is
        // serviced, so a second reporting hub left until "the next interrupt"
        // may never get one. Bounded by the watched-hub count — each keeps one
        // status transfer outstanding, so that is every report the drain can
        // have parked — which stops a flapping hub whose endpoint re-completes
        // during each service from holding this loop and starving the other
        // devices' URBs.
        for _ in 0..live.device.watched_hub_count() {
            match live.device.next_hub_change(&live.delay) {
                Ok(HubEvent::Attached(_) | HubEvent::HubAttached(_)) => {
                    // A fresh leaf device, or a fresh hub tier with the
                    // devices enumerated behind it.
                    interfaces.reconcile(live);
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
                    // A vanished leaf device, or a vanished hub tier with
                    // everything behind it.
                    interfaces.reconcile(live);
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
                        live.device,
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
        if topology_changed && interfaces.recover(health, live) {
            return;
        }
        // Hand the buffered reports to any held URB: a drained completion may
        // satisfy any transport. A transfer fault that proves an unplug
        // recovers the controller its teardown may have halted.
        interfaces.drive_busy(health, live);
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
