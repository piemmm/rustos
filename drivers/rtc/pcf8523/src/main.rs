//! The `Run` entry-point binary of the NXP PCF8523 real-time-clock driver,
//! installed as a signed `/System/Drivers/` bundle and **autoloaded into user
//! space** by `devmgr` when a `nxp,pcf8523` node is discovered
//! (`plans/TIMESYNC.md` TS-4).
//!
//! The process reaches its chip over one thing only: the transfer endpoint
//! its matched node's grant names. Discovery split the bus child in two — the
//! bus driver got the duty (that endpoint paired with this part's bus
//! address) and this driver got the authority (the endpoint alone) — so the
//! wire carries no address and this driver cannot reach a neighbour on the
//! bus however it is compromised.
//!
//! It holds **no** clock authority either: the machine clock is set by the
//! one holder of `CAP_TIME_SET` (`userland/system/timed`), which reads the
//! RTC service endpoint this process serves and tags the reading `Firmware`
//! itself. That split is what makes the wall clock's provenance ladder
//! enforceable.
//!
//! It is a **pure-Rust** program linking the Rust userland runtime
//! `tairix-rt`, never the C ABI. Bring-up puts the part in 24-hour mode with its time circuits running. A bring-up failure exits with a
//! reserved fail-closed code, leaving the machine without a local time source
//! rather than wedged. On the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::rtc_ipc::{self, RTC_ENDPOINT};
    use tairix_abi::CapabilityId;
    use tairix_caps::CapabilitySet;
    use tairix_drv_rtc_pcf8523::Pcf8523;
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants. A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants name no transfer endpoint — an
    /// unbound or mis-provisioned node, or a bus whose duty list could not
    /// hold this child. A reserved, fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the chip could not be brought up over its transfer
    /// endpoint. A reserved value.
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

    /// The capability set the host re-checks before issuing a trap: the
    /// per-endpoint call gate that reaches the bus driver, plus the bind
    /// privilege the served RTC endpoint needs. No mapping authority — this
    /// driver owns no registers. The kernel re-checks every trap.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::IPC_ENDPOINT);
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the serve loop runs for the life of the
    /// driver process.
    fn main() -> i32 {
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        // The endpoint grant *is* this driver's chip. Without one there is no
        // part to talk to, so it stands down rather than guessing an id.
        if host.endpoint_grant().is_none() {
            return EXIT_NO_RESOURCES;
        }
        let Ok(mut rtc) = Pcf8523::open(&host) else {
            return EXIT_BRINGUP_FAILED;
        };

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
    fn serve(rtc: &mut Pcf8523<&RtDriverHost<RtGrantSyscalls>>) -> i32 {
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
