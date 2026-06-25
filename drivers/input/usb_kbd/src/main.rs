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
//! reports, decodes each boot report through the arch-neutral `rustos_hid`
//! composition, and injects keystrokes through `key_inject`. It knows neither
//! the controller type nor the bus — the same binary works unchanged behind
//! any host controller that speaks the URB transport (`AGENTS.md` §2.20 /
//! §17.4).
//!
//! # Least privilege (`AGENTS.md` §5.4)
//!
//! It holds only `CAP_INPUT_INJECT` (inject decoded key edges), `CAP_SHM` (map
//! the granted URB buffer), `CAP_IPC_ENDPOINT` (submit URBs on its one
//! interface's transport endpoint), and `CAP_LOG_EMIT` (one-shot diagnostics).
//! A compromised keyboard driver cannot reprogram the controller, reach
//! another device's buffer, or touch the bus.
//!
//! # Event-driven, never a busy-poll (`AGENTS.md` §2.23)
//!
//! Reading the next report is a **blocking** `ipc_call` (the URB submit): the
//! host-controller driver leaves the call outstanding and replies only when the
//! controller's completion interrupt delivers a report, so this driver parks in
//! the kernel between keystrokes rather than spinning. The service loop is just
//! `pump_once` over the URB-backed report source.
//!
//! It is a **pure-Rust** program (`AGENTS.md` §1); on the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the file. The
//! live report path is metal-only (QEMU models no Pi USB, §0.4).

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// The driver's identity — its [`BIND_KEYS`](rustos_drv_input_usb_kbd::BIND_KEYS)
// bind table — lives in the crate's `lib` target so the host image builder can
// author the signed manifest from it; this binary is the `Run` entry point.

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::driver::input::ReportSource;
    use rustos_abi::input::KeyInput;
    use rustos_abi::{CapabilityId, DriverError, Errno};
    use rustos_caps::CapabilitySet;
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_hid::{pump_once, BootKeyboard, ConsoleSink, KeyboardConsole, REPORT_BUF_LEN};
    use rustos_log::{log, Event, EventId, Level};
    use rustos_rt::LogSink;
    use rustos_usb::transport::{UrbCall, UrbClient};

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the matched interface node did not carry the URB
    /// transport endpoint and shared-buffer grants this driver needs.
    const EXIT_NO_TRANSPORT: i32 = 81;

    /// Diagnostic event id: the one-shot "bound, pumping reports" beacon.
    const USB_KBD_READY: EventId = EventId(4101);

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

    impl UrbCall for IpcUrbCall {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            // The call blocks in the kernel until the HCD replies (when the
            // report arrives), so this driver parks rather than busy-polling.
            rustos_rt::ipc_call(self.endpoint, request, reply).map_err(|neg| {
                Errno::from_i32(i32::try_from(-neg).unwrap_or(0)).unwrap_or(Errno::NotFound)
            })
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
            let transferred = self
                .client
                .interrupt_in(INTERRUPT_ENDPOINT, self.shm_base, REPORT_BUF_LEN as u32)
                .map_err(|_| DriverError::DeviceFault)?;
            let n = (transferred as usize).min(REPORT_BUF_LEN).min(buf.len());
            // SAFETY: `RtDriverHost::map_shared` mapped at least
            // `REPORT_BUF_LEN` bytes of the granted shared region RW into this
            // process at `shm_base` (the host-controller driver sized the
            // region for a boot report), and that mapping outlives this read.
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

    /// A [`ConsoleSink`] that injects each decoded keyboard record into the
    /// kernel input-focus arbiter through the `key_inject` syscall.
    ///
    /// [`pump_once`] hands it one whole [`KeyInput`] record's wire bytes per
    /// key edge; it decodes them fail-closed and injects the record. A
    /// malformed record or a refused injection surfaces as
    /// [`DriverError::DeviceFault`] (a non-fatal poll error), never silently
    /// dropping input.
    struct KeyInjectSink;

    impl ConsoleSink for KeyInjectSink {
        fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
            let record = KeyInput::from_bytes(bytes).map_err(|_| DriverError::DeviceFault)?;
            if rustos_rt::key_inject(&record) < 0 {
                return Err(DriverError::DeviceFault);
            }
            Ok(())
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
        let Some(endpoint) = host.urb_endpoint() else {
            return EXIT_NO_TRANSPORT;
        };
        let Ok(shm_base) = host.map_shared() else {
            return EXIT_NO_TRANSPORT;
        };

        let source = UrbReportSource {
            client: UrbClient::new(IpcUrbCall { endpoint }),
            shm_base,
        };
        let mut keyboard = BootKeyboard::new(source);
        let mut console = KeyboardConsole::new();
        let mut sink = KeyInjectSink;

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
        // between keystrokes and never busy-polls (§2.23). A `pump_once` error
        // is non-fatal: the next iteration re-submits.
        loop {
            let _ = pump_once(&mut keyboard, &mut console, &mut sink);
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
