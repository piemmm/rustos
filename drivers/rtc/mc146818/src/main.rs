//! The `Run` entry-point binary of the MC146818 CMOS real-time-clock driver,
//! installed as a signed `/System/Drivers/` bundle and **autoloaded into user
//! space** by `devmgr` when a `motorola,mc146818` node is discovered
//! (`plans/TIMESYNC.md` TS-3).
//!
//! The process owns the chip's granted port range and serves the RTC class
//! over the well-known `tairix_abi::rtc_ipc::RTC_ENDPOINT` call endpoint. It
//! holds **no** clock authority: the machine clock is set by the one holder
//! of `CAP_TIME_SET` (`userland/system/timed`), which reads this endpoint and
//! tags the reading `Firmware` itself. That split is what makes the wall
//! clock's provenance ladder enforceable — a driver that could assert its own
//! provenance could roll a network sync backwards.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (`_start`, the stack canary, the panic handler, the port-I/O
//! traps, and the `call_create` / `call_recv` / `call_reply` wrappers), never
//! the C ABI. `main` wires the real seams:
//!
//! * `RtDriverHost::from_grants_query` over `RtGrantSyscalls`: the host learns
//!   the grants the kernel minted for its matched node (one port range).
//! * `sole_port_range` over the delivered grants for the range itself and
//!   `port_grant` for the handle the traps resolve, so both come from
//!   discovery and never a build-time board constant.
//! * `call_create` binds the restricted-sender endpoint, then the serve loop
//!   blocks in `call_recv` for the life of the driver — a genuine park
//!   between requests, never a busy spin.
//!
//! Unlike its sibling counter drivers there is no window to map: a port-
//! addressed chip is reached one bounded byte at a time through the
//! capability-gated traps, which re-check `CAP_MMIO_MAP` and re-bound every
//! transfer inside the granted range kernel-side.
//!
//! A bring-up failure exits with a reserved fail-closed code, leaving the
//! machine without a local time source rather than wedged; the spawning
//! supervisor decides whether to relaunch. On the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::driver::sole_port_range;
    use tairix_abi::rtc_ipc::{self, RTC_ENDPOINT};
    use tairix_abi::{CapabilityId, DriverError, PortWidth};
    use tairix_caps::CapabilitySet;
    use tairix_drv_rtc_mc146818::{CmosPorts, Mc146818, PORT_RANGE_LEN};
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the single port range
    /// this driver needs — an unbound or mis-provisioned node. A reserved,
    /// fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the granted range is too short to address the chip's
    /// index/data pair. A reserved value: a mis-provisioned grant fails here
    /// rather than on every request.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// Exit code when the call endpoint could not be created — the id is
    /// already bound (another RTC driver serves it; the first RTC in
    /// hardware-tree order wins and this one stands down) or the driver lacks
    /// `CAP_IPC_BIND_PRIVILEGED`. A reserved, fail-closed value.
    const EXIT_ENDPOINT_FAILED: i32 = 83;

    /// Exit code when the serve loop's `call_recv` failed — a destroyed
    /// endpoint or a torn-down task, both terminal. Exiting fail-loud beats
    /// yield-retrying a dead channel forever, which is a busy spin.
    const EXIT_SERVE_FAILED: i32 = 84;

    /// Bound on the number of in-flight requests the endpoint queues. The
    /// driver answers each request before receiving the next and its one
    /// client asks a handful of times a day, so a small capacity suffices; it
    /// is a queue bound, not a hardware capacity.
    const ENDPOINT_CAPACITY: usize = 4;

    /// The capability set the host re-checks before issuing a port-I/O trap,
    /// plus the bind privilege needed to create a restricted-sender endpoint.
    /// `CAP_MMIO_MAP` is what a `Port` device resource has always required —
    /// reaching a device's registers is one authority whether they are
    /// addressed as memory or as ports — so no new capability is defined. The
    /// kernel is the authority and re-checks every trap.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
        caps
    }

    /// The required-sender capability set of the served endpoint.
    ///
    /// `CAP_TIME_SET` rather than a capability of its own: the only principal
    /// with a reason to read or write the board's clock chip is the one that
    /// sets the machine clock from it, so an existing capability already
    /// expresses the authority exactly and the kernel admits nobody else.
    fn endpoint_send_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::TIME_SET);
        caps
    }

    /// The chip's granted port range, addressed through the capability-gated
    /// byte-wide port traps.
    struct GrantedPorts {
        /// The unforgeable grant handle the kernel resolves owner-checked on
        /// every trap.
        handle: u64,
        /// First port of the granted range.
        base: u16,
        /// Ports the range covers.
        count: u16,
    }

    impl GrantedPorts {
        /// The absolute port `offset` names, refused if it falls outside the
        /// granted range. The kernel re-bounds the transfer regardless; this
        /// keeps a driver bug from becoming a trap the kernel has to reject.
        fn port(&self, offset: u16) -> Result<u16, DriverError> {
            if offset >= self.count {
                return Err(DriverError::DeviceFault);
            }
            self.base
                .checked_add(offset)
                .ok_or(DriverError::DeviceFault)
        }
    }

    impl CmosPorts for GrantedPorts {
        fn read(&mut self, offset: u16) -> Result<u8, DriverError> {
            let port = self.port(offset)?;
            let value = tairix_rt::port_read(self.handle, port, PortWidth::Byte);
            // A byte transfer answers `0..=255`; a negative result is
            // `-errno` and anything wider is a kernel-side inconsistency.
            // Both fail closed rather than yielding a fabricated byte.
            u8::try_from(value).map_err(|_| DriverError::DeviceFault)
        }

        fn write(&mut self, offset: u16, value: u8) -> Result<(), DriverError> {
            let port = self.port(offset)?;
            if tairix_rt::port_write(self.handle, port, PortWidth::Byte, u32::from(value)) != 0 {
                return Err(DriverError::DeviceFault);
            }
            Ok(())
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the serve loop runs for the life of the
    /// driver process.
    fn main() -> i32 {
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        let Ok((base, count)) = sole_port_range(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        let Some(handle) = host.port_grant() else {
            return EXIT_NO_RESOURCES;
        };
        if count < PORT_RANGE_LEN {
            return EXIT_BRINGUP_FAILED;
        }
        let mut rtc = Mc146818::new(GrantedPorts {
            handle,
            base,
            count,
        });

        let send_caps = endpoint_send_caps();
        let recv_caps = CapabilitySet::empty();
        if tairix_rt::call_create(
            RTC_ENDPOINT,
            &send_caps,
            &recv_caps,
            rtc_ipc::REQUEST_LEN,
            rtc_ipc::REPLY_LEN,
            ENDPOINT_CAPACITY,
        ) != 0
        {
            return EXIT_ENDPOINT_FAILED;
        }

        serve(&mut rtc)
    }

    /// The serve loop: block in `call_recv` (a real park between requests),
    /// run the request against the chip, and answer with `call_reply`.
    ///
    /// A `call_recv` error is terminal — the endpoint is sized so an oversize
    /// request is refused at send time, leaving only a destroyed endpoint or
    /// a torn-down task — so it ends the driver fail-loud rather than
    /// yield-retrying a dead channel forever. A reply that fails to encode is
    /// dropped to a zero-length reply, which the client decodes as a
    /// fail-closed truncation.
    fn serve(rtc: &mut Mc146818<GrantedPorts>) -> i32 {
        let mut request = [0u8; rtc_ipc::REQUEST_LEN];
        let mut reply = [0u8; rtc_ipc::REPLY_LEN];
        loop {
            let mut ticket = 0u64;
            match tairix_rt::call_recv(RTC_ENDPOINT, &mut request, &mut ticket) {
                Ok(n) => {
                    let len = rtc_ipc::serve_request(rtc, &request[..n], &mut reply).unwrap_or(0);
                    let _ = tairix_rt::call_reply(RTC_ENDPOINT, ticket, &reply[..len]);
                }
                Err(_) => return EXIT_SERVE_FAILED,
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
