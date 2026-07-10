//! The `Run` entry-point binary of the USB **HID boot-mouse class driver**,
//! installed as a signed `/System/Drivers/` bundle and autoloaded into user
//! space by `devmgr` when a HID boot-mouse **interface** node is discovered
//! (`plans/USB.md` §1.2).
//!
//! This is a pure *class* driver: it touches **no** controller register, owns
//! **no** controller DMA, and holds no IRQ line. The USB host-controller
//! driver (`drivers/bus/usb/xhci`) owns the controller, enumerates the device,
//! publishes one node per interface carrying the device's `vid:pid:class`
//! match keys, and **serves that interface's transfers** over the bus-agnostic
//! URB transport. This driver binds the HID boot-mouse interface node, maps
//! the shared URB data buffer it was granted, submits interrupt-IN URBs to
//! read reports, decodes each boot report through the arch-neutral
//! `rustos_hid` composition, and injects the decoded pointer records through
//! `pointer_inject` — the same shared device→seat mapping
//! (`PointerInput::from_device_event`) the virtio pointer driver uses, so the
//! two can never diverge. It knows neither the controller type nor the bus —
//! the same binary works unchanged behind any host controller that speaks the
//! URB transport.
//!
//! # Least privilege
//!
//! It holds only `CAP_INPUT_INJECT` (inject decoded pointer records),
//! `CAP_SHM` (map the granted URB buffer), `CAP_IPC_ENDPOINT` (submit URBs on
//! its one interface's transport endpoint), and `CAP_LOG_EMIT` (one-shot
//! diagnostics). A compromised mouse driver cannot reprogram the controller,
//! reach another device's buffer, or touch the bus.
//!
//! # Event-driven, never a busy-poll
//!
//! Reading the next report is a **blocking** `ipc_call` (the URB submit): the
//! host-controller driver leaves the call outstanding and replies only when
//! the controller's completion interrupt delivers a report, so this driver
//! parks in the kernel between movements. Each poll drains one decoded event
//! (the [`EVENT_BATCH`]-sized buffer), so every event a report decoded is
//! injected before the next report read can park.
//!
//! It is a **pure-Rust** program; on the host it is an inert stub so `cargo
//! build --workspace`, clippy, and fmt still cover the file. The live report
//! path is metal-only because QEMU models no Pi USB.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// The driver's identity — its [`BIND_KEYS`](rustos_drv_input_usb_mouse::BIND_KEYS)
// bind table — lives in the crate's `lib` target so the host image builder can
// author the signed manifest from it; this binary is the `Run` entry point.

#[cfg(any(test, freestanding))]
use rustos_abi::{DriverError, Errno};

#[cfg(any(test, freestanding))]
fn pump_error_limit_reached(consecutive_errors: &mut u8, limit: u8) -> bool {
    *consecutive_errors = consecutive_errors.saturating_add(1);
    *consecutive_errors >= limit
}

#[cfg(any(test, freestanding))]
fn transport_error(err: Errno) -> DriverError {
    match err {
        Errno::NotFound => DriverError::NotFound,
        _ => DriverError::DeviceFault,
    }
}

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use super::pump_error_limit_reached;
    use rustos_abi::driver::input::{Input, InputEvent, InputEventKind, ReportSource};
    use rustos_abi::input::PointerInput;
    use rustos_abi::{CapabilityId, DriverError, Errno};
    use rustos_caps::CapabilitySet;
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_hid::{BootMouse, REPORT_BUF_LEN};
    use rustos_log::{log, Event, EventId, Level};
    use rustos_rt::LogSink;
    use rustos_usb::transport::{UrbCall, UrbClient};
    use rustos_util::fmt::format_hex_u64;

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the matched interface node did not carry the URB
    /// transport endpoint and shared-buffer grants this driver needs.
    const EXIT_NO_TRANSPORT: i32 = 81;

    /// Diagnostic event id: the one-shot "bound, pumping reports" beacon.
    const USB_MOUSE_READY: EventId = EventId(4157);

    /// Diagnostic event id: a mouse-driver setup step completed.
    const USB_MOUSE_SETUP: EventId = EventId(4158);

    /// Diagnostic event id: a mouse-driver URB/syscall failed.
    const USB_MOUSE_URB_ERROR: EventId = EventId(4159);

    /// Diagnostic event id: a pointer-record injection was refused.
    const USB_MOUSE_INJECT_ERROR: EventId = EventId(4166);

    /// Diagnostic event id: one poll iteration failed.
    const USB_MOUSE_PUMP_ERROR: EventId = EventId(4167);

    /// Consecutive immediate pump faults tolerated before the driver exits
    /// fail-closed rather than retrying hot.
    const MAX_CONSECUTIVE_PUMP_ERRORS: u8 = 4;

    /// Events drained from the decoder per poll. The URB report source
    /// blocks, so the pump asks for one event at a time: every event a
    /// report decoded is injected before the next report read can park for
    /// a later movement (the keyboard pump's `EVENT_BATCH` discipline).
    const EVENT_BATCH: usize = 1;

    /// A zeroed [`InputEvent`] used to initialise the poll batch; overwritten
    /// by [`Input::poll`] before it is read.
    const EVENT_ZERO: InputEvent = InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    };

    /// The interrupt-IN endpoint number named in the URB. The host-controller
    /// driver serves the device's single enumerated interrupt-IN endpoint
    /// regardless of this value (it only rejects endpoint 0), so any non-zero
    /// number names "the boot mouse's report endpoint" for a single-endpoint
    /// boot device.
    const INTERRUPT_ENDPOINT: u8 = 1;

    /// The capability set the driver host re-checks up front; the kernel is
    /// the authority and re-checks every trap. It is the least-privilege set a
    /// pure HID class driver needs — no MMIO, DMA, or IRQ.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::INPUT_INJECT);
        caps.insert(CapabilityId::SHM);
        caps.insert(CapabilityId::IPC_ENDPOINT);
        caps.insert(CapabilityId::LOG_EMIT);
        caps
    }

    /// The class-side URB transport: one synchronous, capability-checked
    /// `ipc_call` to the host-controller driver's per-interface endpoint.
    struct IpcUrbCall {
        endpoint: u64,
    }

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
                fields: &[rustos_log::Field {
                    key,
                    value: rustos_log::FieldValue::Str(format_hex_u64(value, &mut value_buf)),
                }],
            },
        );
    }

    impl UrbCall for IpcUrbCall {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            // The call blocks in the kernel until the HCD replies (when the
            // report arrives), so this driver parks rather than busy-polling.
            match rustos_rt::ipc_call(self.endpoint, request, reply) {
                Ok(len) => Ok(len),
                Err(neg) => {
                    let errno = Errno::from_i32(i32::try_from(-neg).unwrap_or(0))
                        .unwrap_or(Errno::NotFound);
                    log_hex_event(
                        USB_MOUSE_URB_ERROR,
                        Level::Warn,
                        "usb-mouse: URB ipc_call failed",
                        "errno_hex",
                        errno as u64,
                    );
                    Err(errno)
                }
            }
        }
    }

    /// A [`ReportSource`] over the URB transport: each `next_report` submits an
    /// interrupt-IN URB and copies the delivered report out of the shared
    /// buffer the host-controller driver wrote it into.
    struct UrbReportSource {
        client: UrbClient<IpcUrbCall>,
        /// Base user virtual address of this driver's mapping of the shared URB
        /// data buffer (`RtDriverHost::map_shared`). The host-controller driver
        /// maps the same frames and writes each report here before replying.
        shm_base: u64,
    }

    impl ReportSource for UrbReportSource {
        fn next_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
            // Submit the interrupt-IN URB and block until the HCD delivers a
            // report into the shared buffer; a transport/controller fault is a
            // non-fatal poll error the service loop retries.
            let transferred = match self.client.interrupt_in(
                INTERRUPT_ENDPOINT,
                self.shm_base,
                REPORT_BUF_LEN as u32,
            ) {
                Ok(transferred) => transferred,
                Err(err) => {
                    log_hex_event(
                        USB_MOUSE_URB_ERROR,
                        Level::Warn,
                        "usb-mouse: interrupt-in URB completion carried an error",
                        "errno_hex",
                        err as u64,
                    );
                    return Err(super::transport_error(err));
                }
            };
            let n = (transferred as usize).min(REPORT_BUF_LEN).min(buf.len());
            // SAFETY: `RtDriverHost::map_shared` mapped the granted shared
            // region RW into this process at `shm_base`, `main` verified the
            // kernel-reported length holds at least `REPORT_BUF_LEN` bytes
            // before constructing this source, and that mapping outlives
            // this read.
            // The HCD's write to the same frames happens-before this read: the
            // URB reply we just received is the kernel's release of that write.
            // We read only `n ≤ REPORT_BUF_LEN` bytes, wholly in-bounds.
            let shm = unsafe {
                core::slice::from_raw_parts(self.shm_base as usize as *const u8, REPORT_BUF_LEN)
            };
            buf[..n].copy_from_slice(&shm[..n]);
            Ok(Some(n))
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the report pump runs for the life of the
    /// driver process.
    fn main() -> i32 {
        // No MMIO/DMA grants to map, so no coherency shim is needed.
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        // The matched interface node carried two transport grants: the URB
        // call endpoint (its id) and the shared data buffer (mapped here).
        let Some(endpoint) = host.endpoint_grant() else {
            return EXIT_NO_TRANSPORT;
        };
        log_hex_event(
            USB_MOUSE_SETUP,
            Level::Info,
            "usb-mouse: URB endpoint grant discovered",
            "endpoint_hex",
            endpoint,
        );
        // The kernel reports the mapped region's true length; a region too
        // small for one boot report is a mis-provisioned node refused here,
        // before any shared-buffer read is built over it.
        let Ok((shm_base, shm_len)) = host.map_shared() else {
            return EXIT_NO_TRANSPORT;
        };
        if shm_len < REPORT_BUF_LEN {
            return EXIT_NO_TRANSPORT;
        }
        log_hex_event(
            USB_MOUSE_SETUP,
            Level::Info,
            "usb-mouse: shared report buffer mapped",
            "base_hex",
            shm_base,
        );

        let source = UrbReportSource {
            client: UrbClient::new(IpcUrbCall { endpoint }),
            shm_base,
        };
        let mut mouse = BootMouse::new(source);
        let mut consecutive_pump_errors = 0u8;

        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: USB_MOUSE_READY,
                message: "usb-mouse: bound, pumping reports over URB transport",
                fields: &[],
            },
        );

        // Event-driven service loop: `poll` reads the next report through the
        // URB-backed source, which blocks in the kernel on the `ipc_call`
        // until the host-controller driver delivers one — so this loop parks
        // between movements and never busy-polls. A single poll error is
        // retried, but repeated immediate faults exit fail-closed rather than
        // retrying hot.
        loop {
            let mut events = [EVENT_ZERO; EVENT_BATCH];
            match mouse.poll(&mut events) {
                Ok(drained) => {
                    consecutive_pump_errors = 0;
                    for event in &events[..drained] {
                        // The shared pointer mapping claims the pointer
                        // vocabulary (axis deltas, button edges); a boot
                        // mouse decodes to nothing else, so an unmapped
                        // event (a scroll tick with no desktop consumer
                        // yet) is deliberately not injected.
                        let Some(record) = PointerInput::from_device_event(event) else {
                            continue;
                        };
                        if rustos_rt::pointer_inject(rustos_abi::seat::SEAT_PRIMARY, &record) < 0 {
                            log_hex_event(
                                USB_MOUSE_INJECT_ERROR,
                                Level::Warn,
                                "usb-mouse: pointer injection refused",
                                "code_hex",
                                u64::from(event.code),
                            );
                        }
                    }
                }
                Err(DriverError::NotFound) => {
                    log_hex_event(
                        USB_MOUSE_PUMP_ERROR,
                        Level::Info,
                        "usb-mouse: transport disappeared, exiting for reload",
                        "consecutive_hex",
                        u64::from(consecutive_pump_errors),
                    );
                    return 0;
                }
                Err(_) => {
                    let exhausted = pump_error_limit_reached(
                        &mut consecutive_pump_errors,
                        MAX_CONSECUTIVE_PUMP_ERRORS,
                    );
                    log_hex_event(
                        USB_MOUSE_PUMP_ERROR,
                        Level::Warn,
                        "usb-mouse: poll returned an error",
                        "consecutive_hex",
                        u64::from(consecutive_pump_errors),
                    );
                    if exhausted {
                        log_hex_event(
                            USB_MOUSE_PUMP_ERROR,
                            Level::Error,
                            "usb-mouse: repeated pump errors, exiting fail-closed",
                            "consecutive_hex",
                            u64::from(consecutive_pump_errors),
                        );
                        return EXIT_NO_TRANSPORT;
                    }
                }
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
    use super::{pump_error_limit_reached, transport_error};
    use rustos_abi::{DriverError, Errno};

    #[test]
    fn pump_error_limit_fails_closed_without_wrapping() {
        let mut errors = u8::MAX - 1;
        assert!(pump_error_limit_reached(&mut errors, u8::MAX));
        assert_eq!(errors, u8::MAX);
        assert!(pump_error_limit_reached(&mut errors, u8::MAX));
        assert_eq!(errors, u8::MAX);
    }

    #[test]
    fn pump_error_limit_allows_transient_errors() {
        let mut errors = 0;
        assert!(!pump_error_limit_reached(&mut errors, 3));
        assert_eq!(errors, 1);
        assert!(!pump_error_limit_reached(&mut errors, 3));
        assert_eq!(errors, 2);
    }

    #[test]
    fn disconnected_transport_is_terminal_for_the_pump_loop() {
        assert_eq!(transport_error(Errno::NotFound), DriverError::NotFound);
        assert_eq!(
            transport_error(Errno::NotImplemented),
            DriverError::DeviceFault
        );
    }
}
