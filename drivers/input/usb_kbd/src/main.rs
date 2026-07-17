//! The `Run` entry-point binary of the USB **HID boot-keyboard class driver**,
//! installed as a signed `/System/Drivers/` bundle and autoloaded into user
//! space by `devmgr` when a HID boot-keyboard **interface** node is discovered
//! (`plans/USB.md` U4).
//!
//! This is a pure *class* driver: it touches **no** controller register, owns
//! **no** controller DMA, and holds no IRQ line. The USB host-controller
//! driver (`drivers/bus/usb/xhci`) owns the controller, enumerates the device,
//! publishes one node per interface carrying the device's `vid:pid:class`
//! match keys, and **serves that interface's transfers** over the bus-agnostic
//! URB transport. This driver binds the HID boot-keyboard interface node, maps
//! the shared URB data buffer it was granted, submits interrupt-IN URBs to read
//! reports, decodes each boot report through the arch-neutral `tairix_hid`
//! composition, and injects keystrokes through `key_inject`. It knows neither
//! the controller type nor the bus — the same binary works unchanged behind
//! any host controller that speaks the URB transport.
//!
//! # Least privilege
//!
//! It holds only `CAP_INPUT_INJECT` (inject decoded key edges), `CAP_SHM` (map
//! the granted URB buffer), `CAP_IPC_ENDPOINT` (submit URBs on its one
//! interface's transport endpoint), and `CAP_LOG_EMIT` (one-shot diagnostics).
//! A compromised keyboard driver cannot reprogram the controller, reach
//! another device's buffer, or touch the bus.
//!
//! # Event-driven, never a busy-poll
//!
//! Reading the next report is a **blocking** `ipc_call` (the URB submit): the
//! host-controller driver leaves the call outstanding and replies only when the
//! controller's completion interrupt delivers a report, so this driver parks in
//! the kernel between keystrokes rather than spinning. The service loop is just
//! `pump_once` over the URB-backed report source.
//!
//! It is a **pure-Rust** program; on the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the file. The
//! live report path is metal-only because QEMU models no Pi USB.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// The driver's identity — its [`BIND_KEYS`](tairix_drv_input_usb_kbd::BIND_KEYS)
// bind table — lives in the crate's `lib` target so the host image builder can
// author the signed manifest from it; this binary is the `Run` entry point.

#[cfg(any(test, freestanding))]
use tairix_abi::{DriverError, Errno};

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
    use tairix_abi::driver::input::ReportSource;
    use tairix_abi::input::KeyInput;
    use tairix_abi::{CapabilityId, DriverError, Errno};
    use tairix_caps::CapabilitySet;
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_hid::{pump_once, BootKeyboard, ConsoleSink, KeyboardConsole, REPORT_BUF_LEN};
    use tairix_log::{log, Event, EventId, Level};
    use tairix_rt::LogSink;
    use tairix_usb::transport::{UrbCall, UrbClient};
    use tairix_util::fmt::format_hex_u64;

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the matched interface node did not carry the URB
    /// transport endpoint and shared-buffer grants this driver needs.
    const EXIT_NO_TRANSPORT: i32 = 81;

    /// Diagnostic event id: the one-shot "bound, pumping reports" beacon.
    const USB_KBD_READY: EventId = EventId(4101);

    /// Diagnostic event id: a keyboard-driver setup step completed.
    const USB_KBD_SETUP: EventId = EventId(4142);

    /// Diagnostic event id: a keyboard-driver URB request is about to be sent.
    const USB_KBD_URB_SUBMIT: EventId = EventId(4143);

    /// Diagnostic event id: a keyboard-driver URB returned.
    const USB_KBD_URB_REPLY: EventId = EventId(4144);

    /// Diagnostic event id: a keyboard-driver URB/syscall failed.
    const USB_KBD_URB_ERROR: EventId = EventId(4145);

    /// Diagnostic event id: a report was copied from the shared URB buffer.
    const USB_KBD_REPORT: EventId = EventId(4146);

    /// Diagnostic event id: decoded keyboard input reached the injection sink.
    const USB_KBD_INJECT: EventId = EventId(4147);

    /// Diagnostic event id: one `pump_once` iteration failed.
    const USB_KBD_PUMP_ERROR: EventId = EventId(4148);

    /// Consecutive immediate pump faults tolerated before the driver exits
    /// fail-closed rather than retrying hot.
    const MAX_CONSECUTIVE_PUMP_ERRORS: u8 = 4;

    /// The interrupt-IN endpoint number named in the URB. The host-controller
    /// driver serves the device's single enumerated interrupt-IN endpoint
    /// regardless of this value (it only rejects endpoint 0), so any non-zero
    /// number names "the boot keyboard's report endpoint" for a single-endpoint
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
                fields: &[tairix_log::Field {
                    key,
                    value: tairix_log::FieldValue::Str(format_hex_u64(value, &mut value_buf)),
                }],
            },
        );
    }

    impl UrbCall for IpcUrbCall {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            // The call blocks in the kernel until the HCD replies (when the
            // report arrives), so this driver parks rather than busy-polling.
            log_hex_event(
                USB_KBD_URB_SUBMIT,
                Level::Debug,
                "usb-keyboard: submitting interrupt-in URB",
                "endpoint_hex",
                self.endpoint,
            );
            match tairix_rt::ipc_call(self.endpoint, request, reply) {
                Ok(len) => {
                    log_hex_event(
                        USB_KBD_URB_REPLY,
                        Level::Debug,
                        "usb-keyboard: URB reply received",
                        "len_hex",
                        len as u64,
                    );
                    Ok(len)
                }
                Err(neg) => {
                    let errno = Errno::from_i32(i32::try_from(-neg).unwrap_or(0))
                        .unwrap_or(Errno::NotFound);
                    log_hex_event(
                        USB_KBD_URB_ERROR,
                        Level::Warn,
                        "usb-keyboard: URB ipc_call failed",
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
                        USB_KBD_URB_ERROR,
                        Level::Warn,
                        "usb-keyboard: interrupt-in URB completion carried an error",
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
            log_hex_event(
                USB_KBD_REPORT,
                Level::Debug,
                "usb-keyboard: report copied from shared buffer",
                "len_hex",
                n as u64,
            );
            Ok(Some(n))
        }
    }

    /// A [`ConsoleSink`] that injects each decoded keyboard record into the
    /// kernel input-focus arbiter through the `key_inject` syscall.
    ///
    /// [`pump_once`] hands it one whole [`KeyInput`] record's wire bytes per
    /// key edge; it decodes them fail-closed and injects the record for the
    /// boot seat (`SEAT_PRIMARY`) — the seat a directly attached keyboard
    /// belongs to. A malformed record or a refused injection surfaces as
    /// [`DriverError::DeviceFault`] (a non-fatal poll error), never silently
    /// dropping input.
    struct KeyInjectSink;

    impl ConsoleSink for KeyInjectSink {
        fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
            let record = KeyInput::from_bytes(bytes).map_err(|_| DriverError::DeviceFault)?;
            log_hex_event(
                USB_KBD_INJECT,
                Level::Debug,
                "usb-keyboard: decoded key record ready for injection",
                "bytes_hex",
                bytes.len() as u64,
            );
            if tairix_rt::key_inject(tairix_abi::seat::SEAT_PRIMARY, &record) < 0 {
                log_hex_event(
                    USB_KBD_INJECT,
                    Level::Warn,
                    "usb-keyboard: key injection failed",
                    "bytes_hex",
                    bytes.len() as u64,
                );
                return Err(DriverError::DeviceFault);
            }
            log_hex_event(
                USB_KBD_INJECT,
                Level::Debug,
                "usb-keyboard: key injection accepted",
                "bytes_hex",
                bytes.len() as u64,
            );
            Ok(())
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
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
            USB_KBD_SETUP,
            Level::Info,
            "usb-keyboard: URB endpoint grant discovered",
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
            USB_KBD_SETUP,
            Level::Info,
            "usb-keyboard: shared report buffer mapped",
            "base_hex",
            shm_base,
        );

        let source = UrbReportSource {
            client: UrbClient::new(IpcUrbCall { endpoint }),
            shm_base,
        };
        let mut keyboard = BootKeyboard::new(source);
        let mut console = KeyboardConsole::new();
        let mut sink = KeyInjectSink;
        let mut consecutive_pump_errors = 0u8;

        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: USB_KBD_READY,
                message: "usb-keyboard: bound, pumping reports over URB transport",
                fields: &[],
            },
        );

        // Event-driven service loop: `pump_once` reads the next report through
        // the URB-backed source, which blocks in the kernel on the `ipc_call`
        // until the host-controller driver delivers one — so this loop parks
        // between keystrokes and never busy-polls. A single `pump_once` error
        // is retried, but repeated immediate faults exit fail-closed rather
        // than retrying hot.
        loop {
            match pump_once(&mut keyboard, &mut console, &mut sink) {
                Ok(_) => consecutive_pump_errors = 0,
                Err(DriverError::NotFound) => {
                    log_hex_event(
                        USB_KBD_PUMP_ERROR,
                        Level::Info,
                        "usb-keyboard: transport disappeared, exiting for reload",
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
                        USB_KBD_PUMP_ERROR,
                        Level::Warn,
                        "usb-keyboard: pump_once returned an error",
                        "consecutive_hex",
                        u64::from(consecutive_pump_errors),
                    );
                    if exhausted {
                        log_hex_event(
                            USB_KBD_PUMP_ERROR,
                            Level::Error,
                            "usb-keyboard: repeated pump errors, exiting fail-closed",
                            "consecutive_hex",
                            u64::from(consecutive_pump_errors),
                        );
                        return EXIT_NO_TRANSPORT;
                    }
                }
            }
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
    use super::{pump_error_limit_reached, transport_error};
    use tairix_abi::{DriverError, Errno};

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
        errors = 0;
        assert!(!pump_error_limit_reached(&mut errors, 3));
        assert_eq!(errors, 1);
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
