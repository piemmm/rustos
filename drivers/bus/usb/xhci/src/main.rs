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
//! driver, no board, and no bus (`AGENTS.md` §2.20 / §17.4).
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
//! It is a **pure-Rust** program (`AGENTS.md` §1): it links the Rust userland
//! runtime `rustos-rt` (`_start`, the stack canary, the panic handler, the
//! syscall wrappers); on the host it is an inert stub so `cargo build
//! --workspace`, clippy, and fmt still cover the file. The live controller
//! bring-up and report path are metal-only (QEMU models no Pi USB, §0.4); the
//! HCD's host-testable logic lives in the crate's `lib` target
//! ([`rustos_drv_bus_usb::bringup`] / [`rustos_drv_bus_usb::serve`]).

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::hwtree::HW_NODE_ROOT;
    use rustos_abi::usb_urb::{URB_COMPLETION_LEN, URB_REQUEST_LEN};
    use rustos_abi::waitset::{WaitSetOp, WaitSourceKind};
    use rustos_abi::CapabilityId;
    use rustos_caps::CapabilitySet;
    use rustos_drv_bus_usb::bringup::{
        bring_up_controller_diagnostic, derive_controller_resources,
    };
    use rustos_drv_bus_usb::serve::{attach_transport_grants, UrbOutcome, UrbService};
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_log::{log, Event, EventId, Field, Level};
    use rustos_rt::{ClockDelay, LogSink};

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

        let Some(endpoint_id) = bind_urb_endpoint() else {
            return EXIT_NO_TRANSPORT;
        };

        // Publish one interface node carrying the device's USB match keys plus
        // the transport grants the class driver inherits. The kernel assigns
        // and returns the node id, which the HCD keeps so it can retract the
        // node when the device disconnects.
        let node = match device.describe_device(HW_NODE_ROOT, 0) {
            Ok(node) => node,
            Err(_) => return EXIT_EMIT_FAILED,
        };
        let node = match attach_transport_grants(node, endpoint_id, shm_id) {
            Ok(node) => node,
            Err(_) => return EXIT_EMIT_FAILED,
        };
        let emit = rustos_rt::hw_emit_node(&node);
        if emit < 0 {
            return EXIT_EMIT_FAILED;
        }
        #[allow(clippy::cast_sign_loss)] // `emit >= 0` here is the assigned node id.
        let interface_node_id = emit as u32;
        let root_port = device.root_port();

        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: HCD_READY,
                message: "usb-hcd: controller up, serving URB transport",
                fields: &[],
            },
        );

        // Arm the completion interrupter and bind the controller's IRQ line if
        // the matched node carried one, so the loop parks on the interrupt
        // rather than polling a quiet controller.
        let irq_handle = match host.irq_line() {
            Some(line) if device.enable_interrupter().is_ok() => {
                let handle = rustos_rt::irq_bind(line);
                if handle >= 0 {
                    #[allow(clippy::cast_sign_loss)] // `handle >= 0` is the bound IrqHandle.
                    Some(handle as u64)
                } else {
                    None
                }
            }
            _ => None,
        };

        // Build the wait-set the loop parks on: the transport endpoint always,
        // the controller IRQ line when one was bound.
        let set = rustos_rt::waitset_create();
        if set < 0 {
            return EXIT_NO_TRANSPORT;
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
        let set = set as u64;
        if rustos_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            endpoint_id,
            TOKEN_URB,
        ) != 0
        {
            return EXIT_NO_TRANSPORT;
        }
        if let Some(handle) = irq_handle {
            // A failure to add the IRQ member is non-fatal: the endpoint is
            // still serviced; only interrupt-driven completion is lost.
            let _ =
                rustos_rt::waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::Irq, handle, TOKEN_IRQ);
        }

        let mut service = UrbService::new();
        let mut node_live = true;

        // The asynchronous event loop: park until the transport endpoint or the
        // controller interrupt is ready, never spinning a quiet controller.
        loop {
            let mut token = 0u64;
            if rustos_rt::waitset_wait(set, u64::MAX, &mut token) < 0 {
                // With an unbounded timeout on a wait-set we own, a negative
                // result means the set was torn down — stop rather than spin.
                return 0;
            }
            match token {
                TOKEN_URB => {
                    let mut request = [0u8; URB_REQUEST_LEN];
                    let mut ticket = 0u64;
                    if let Ok(n) = rustos_rt::call_recv(endpoint_id, &mut request, &mut ticket) {
                        if let UrbOutcome::Reply(reply) =
                            service.on_submit(ticket, &request[..n], shm, &mut device)
                        {
                            let _ = rustos_rt::call_reply(
                                endpoint_id,
                                reply.ticket,
                                &reply.bytes[..reply.len],
                            );
                        }
                    }
                }
                TOKEN_IRQ => {
                    // Acknowledge before draining so a completion posted during
                    // the drain re-asserts rather than being lost.
                    let _ = device.acknowledge_interrupt();
                    if let UrbOutcome::Reply(reply) = service.on_event(shm, &mut device) {
                        let _ = rustos_rt::call_reply(
                            endpoint_id,
                            reply.ticket,
                            &reply.bytes[..reply.len],
                        );
                    }
                    // Watch the root-hub port: on a disconnect, retract the
                    // interface node once so `devmgr` unloads the class driver
                    // while this controller stays up.
                    if node_live && !still_connected(&mut device, root_port) {
                        if rustos_rt::hw_remove_node(interface_node_id) >= 0 {
                            node_live = false;
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
