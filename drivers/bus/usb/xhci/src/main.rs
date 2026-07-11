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

// The per-index URB transport table grows with the devices the controller
// actually serves; `rustos-rt` supplies the process heap.
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
    use rustos_usb::device::{HubEvent, MAX_INTERFACES, XHCI_MAX_SLOTS};
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
    /// ([`rustos_usb::device::BULK_BUF_LEN`], the engine's per-TD ceiling —
    /// one definition, never a second constant), which also comfortably
    /// holds a boot report and any control-IN descriptor a class driver
    /// reads. One page, so the mass-storage data path costs the keyboard
    /// path nothing extra.
    const SHM_LEN: usize = rustos_usb::device::BULK_BUF_LEN;

    /// Outstanding-URB capacity of the per-interface endpoint. The class
    /// driver submits one at a time (it blocks on the reply); a small queue
    /// absorbs a re-submit racing the previous reply.
    const ENDPOINT_CAPACITY: usize = 4;

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
                    value: rustos_log::FieldValue::Str(format_hex_u64(value, &mut value_buf)),
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
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
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
    }

    /// Retract `transport`'s published interface node (best-effort) and
    /// abort its outstanding URB, so the class driver being unloaded never
    /// stays parked on a dead device.
    fn retract_interface(transport: &mut Transport) {
        if transport.node_live {
            if rustos_rt::hw_remove_node(transport.node_id) < 0 {
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
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
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

    /// Service one pending hub status-change after a fault detach re-armed
    /// the watch, reconciling the published interfaces with whatever the
    /// change attached or detached.
    fn service_hub_after_fault_detach(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: &ClockDelay,
    ) {
        match device.next_hub_change(delay) {
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
    fn retract_after_fault_if_gone(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        reply: UrbReply,
        delay: &ClockDelay,
    ) -> bool {
        if urb_reply_errno(&reply) != Some(Errno::NotImplemented) {
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

    /// Reset the controller and re-enumerate from scratch, re-arming the
    /// interrupter the reset cleared and publishing a fresh interface node
    /// for every device found back, so `devmgr` re-autoloads each class
    /// driver onto the same per-index transport.
    ///
    /// This is the recovery a root-port re-attach uses and the recovery from
    /// a latched controller fault uses: in both cases the controller is
    /// returned to the same state a cold boot reaches, from which the next
    /// connect enumerates through the normal attach path. With no device
    /// present yet it simply leaves the controller awaiting that connect.
    /// The caller refreshes its watched root port from `device.root_port()`
    /// afterwards.
    fn reset_reenumerate_and_publish(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
        delay: &ClockDelay,
    ) {
        if device.reset_and_reenumerate(delay).is_err() {
            return;
        }
        let _ = device.enable_interrupter();
        reconcile_interfaces(device, transports, set, urb_base);
    }

    /// Recover if the controller has latched a fatal error or halted
    /// (`USBSTS.HSE`/HCHalted). Such a controller raises no further interrupts
    /// until it is reset (xHCI §4.24.1), so a watched device's hot-plug and
    /// transfers go silent — on the Pi 4 the VL805 latches a Host System Error
    /// during a downstream-device hot-removal teardown, after its Disable Slot
    /// has already completed, which is why an unplug worked but the controller
    /// never saw the re-plug. Retract every still-live interface, abort the
    /// held URBs, then reset and re-enumerate so the controller returns to the
    /// proven await-connect state and a re-plug enumerates normally. Returns
    /// whether a recovery ran (so the caller refreshes its watched root port).
    fn recover_if_controller_faulted(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
        transports: &mut Vec<Option<Transport>>,
        set: u64,
        urb_base: u64,
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
        for transport in transports.iter_mut().flatten() {
            retract_interface(transport);
        }
        reset_reenumerate_and_publish(device, transports, set, urb_base, delay);
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
        rustos_rt::call_create(
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
        let shm_base = rustos_rt::shm_create(SHM_LEN, &mut shm_id);
        if shm_base < 0 {
            return None;
        }
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
            unsafe { core::slice::from_raw_parts_mut(shm_base as usize as *mut u8, SHM_LEN) };
        let token = TOKEN_URB_BASE + u64::try_from(index).ok()?;
        let endpoint_add = rustos_rt::waitset_ctl(
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

    /// Whether the enumerated device is still connected on its root port (the
    /// `CCS` connect bit). A read fault defaults to "connected" so a transient
    /// read never triggers a spurious retraction.
    fn still_connected(
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
        root_port: u8,
    ) -> bool {
        device
            .port_status_raw(root_port)
            .is_none_or(|portsc| portsc & 1 != 0)
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
        device: &mut rustos_drv_bus_usb::bringup::ControllerDevice<'_>,
        index: usize,
        endpoint_id: u64,
        shm_id: u64,
    ) -> Option<u32> {
        let node = device.describe_device(index, HW_NODE_ROOT, 0).ok()?;
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
                            value: rustos_log::FieldValue::Str(err.phase.as_str()),
                        }],
                    },
                );
                return EXIT_BRINGUP_FAILED;
            }
        };

        // Build the wait-set the loop parks on: every transport endpoint and
        // the controller IRQ line. All must succeed before any interface is
        // published, because interrupt-IN URBs complete only through that
        // event-driven wake path.
        let set = rustos_rt::waitset_create();
        if set < 0 {
            return EXIT_NO_TRANSPORT;
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
        let set = set as u64;

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

        // Publish an interface node for every device enumerated at bring-up
        // (creating each served index's transport on the way). A cold boot
        // with nothing plugged in is a first-class state: the controller
        // comes up with no node, and the first hot-plug connect — delivered
        // through the onboard hub's status-change watch, or a root-port
        // connect — publishes from the event loop below.
        reconcile_interfaces(&mut device, &mut transports, set, urb_base);
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
                token if token >= TOKEN_URB_BASE => {
                    #[allow(clippy::cast_possible_truncation)]
                    // The token was registered as `TOKEN_URB_BASE + index`.
                    let index = (token - TOKEN_URB_BASE) as usize;
                    let Some(transport) = transports.get_mut(index).and_then(Option::as_mut) else {
                        continue;
                    };
                    let mut request = [0u8; URB_REQUEST_LEN];
                    let mut ticket = 0u64;
                    match rustos_rt::call_recv(transport.endpoint_id, &mut request, &mut ticket) {
                        Ok(n) => {
                            match transport.service.on_submit(
                                transport.node_live,
                                ticket,
                                &request[..n],
                                transport.shm,
                                &mut device.engine_for(index),
                            ) {
                                UrbOutcome::Reply(reply) => {
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
                    // Hot-plug. When a hub is watched (the devices sit behind
                    // the onboard hub, so its root port never changes), a hub
                    // status-change report drives connect/disconnect: a fresh
                    // device is enumerated and a new interface node published
                    // on its index's transport (so `devmgr` autoloads the
                    // class driver onto the same endpoint across a re-plug),
                    // and a disconnect retracts only that device's node. A
                    // directly-attached device instead has its root port
                    // watched for disconnect, and a root-port connect — whether
                    // the first ever (cold boot, nothing attached at bring-up)
                    // or a re-attach — drives a fresh re-enumeration. All leave
                    // the controller up.
                    if device.hub_watch_active() {
                        // Every external device on the Pi 4 hangs off a hub,
                        // and pulling that assembly out takes the hub with it:
                        // the unplug surfaces as the *root* port (where the hub
                        // sat) clearing its connect bit, not as a downstream
                        // hub-port change — the hub is gone, so it answers
                        // neither its status-change endpoint nor a control
                        // transfer. Check the hub's own root port first; if it
                        // is gone, retract every interface and tear down so a
                        // re-plug re-enumerates from scratch.
                        match device.detach_if_hub_root_gone() {
                            Ok(true) => {
                                for transport in transports.iter_mut().flatten() {
                                    retract_interface(transport);
                                }
                                log(
                                    &LogSink,
                                    &Event {
                                        level: Level::Info,
                                        id: HCD_DISCONNECT,
                                        message: "usb-hcd: hub assembly disconnected at root port, interfaces retracted",
                                        fields: &[],
                                    },
                                );
                            }
                            Ok(false) => match device.next_hub_change(&delay) {
                                Ok(HubEvent::Attached(_) | HubEvent::HubAttached(_)) => {
                                    // A fresh leaf device — or a fresh hub
                                    // tier whose downstream devices were
                                    // enumerated with it — is published by
                                    // diffing every live index.
                                    reconcile_interfaces(
                                        &mut device,
                                        &mut transports,
                                        set,
                                        urb_base,
                                    );
                                }
                                Ok(HubEvent::Detached(_) | HubEvent::HubDetached(_)) => {
                                    // A vanished leaf device — or a vanished
                                    // hub tier with everything behind it —
                                    // is retracted by the same diff.
                                    reconcile_interfaces(
                                        &mut device,
                                        &mut transports,
                                        set,
                                        urb_base,
                                    );
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
                    } else if device.any_device_live() {
                        // Directly-attached device: retract on a root-port
                        // disconnect (the `CCS` connect bit clearing).
                        if !still_connected(&mut device, root_port) {
                            for transport in transports.iter_mut().flatten() {
                                retract_interface(transport);
                            }
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
                    } else if device.any_root_port_connected() {
                        // A device appeared on a root port — either the first
                        // connect after a cold boot with nothing attached at
                        // bring-up, or a re-attach after a disconnect. Reset
                        // the controller and re-run the bring-up walk, re-arm
                        // the interrupter the reset cleared (so the next
                        // connect/disconnect still wakes the loop), refresh
                        // the watched root port, and publish fresh interface
                        // nodes so each class driver is autoloaded onto its
                        // index's transport.
                        reset_reenumerate_and_publish(
                            &mut device,
                            &mut transports,
                            set,
                            urb_base,
                            &delay,
                        );
                        root_port = device.root_port();
                    }
                    // A disconnect-handling teardown above (a hub status-change
                    // detach or a hub-assembly detach) can leave the controller
                    // halted with a latched Host System Error on the Pi 4 VL805;
                    // recover before servicing so the re-plug is still seen.
                    if recover_if_controller_faulted(
                        &mut device,
                        &mut transports,
                        set,
                        urb_base,
                        &delay,
                    ) {
                        root_port = device.root_port();
                        continue;
                    }
                    // Drive every transport with a URB outstanding: the drained
                    // event(s) may complete any of them, and a completion the
                    // hot-plug handling above parked must not wait for another
                    // interrupt. A transport whose device just detached has
                    // already had its URB aborted, so it is simply not busy.
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
                                let node_live = transports[index]
                                    .as_ref()
                                    .is_some_and(|transport| transport.node_live);
                                let detached = node_live
                                    && retract_after_fault_if_gone(
                                        &mut device,
                                        index,
                                        &mut transports,
                                        set,
                                        urb_base,
                                        reply,
                                        &delay,
                                    );
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
                                        message: "usb-hcd: IRQ did not complete held URB yet",
                                        fields: &[],
                                    },
                                );
                            }
                            // `is_busy` was checked above, so an Idle outcome
                            // cannot occur; nothing to service either way.
                            UrbOutcome::Idle => {}
                        }
                    }
                    // The transfer-fault disconnect teardown (the Disable Slot in
                    // `retract_after_fault_if_gone`) latches the same controller
                    // fault on the Pi 4 VL805 after it completes; recover here too
                    // so the re-plug is seen rather than the controller staying
                    // halted and silent.
                    if recover_if_controller_faulted(
                        &mut device,
                        &mut transports,
                        set,
                        urb_base,
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
