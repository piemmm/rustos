//! The `Run` entry-point binary of the framebuffer display service,
//! installed as a signed `/System/Drivers/` bundle and **autoloaded into
//! user space** by `devmgr` when a display node carrying a
//! `HwResourceKind::Framebuffer` resource is discovered
//! (`plans/DISPLAY.md` D7b).
//!
//! This is the display half of the zero-copy, lease-gated present path:
//! a desktop session composes frames into one `shm_grant`ed region and
//! presents by index over the reserved `DISPLAY_ENDPOINT`; this process
//! blits the presented frame to the scan-out surface. It names no board,
//! bus, or firmware detail: the surface's physical base, geometry, and
//! pixel format are read from the kernel-issued device-resource grants
//! its matched node requested (`sole_framebuffer`), never a build-time
//! constant, and the blit engine is the shared
//! `tairix_display::Framebuffer` — the one definition the framebuffer
//! QEMU verticals also drive.
//!
//! It is a **pure-Rust** program: TAIRiX is Rust-only, so it links the
//! Rust userland runtime `tairix-rt`, never the C ABI (which exists solely
//! for non-Rust programs). `tairix-rt` provides `_start`, the per-process
//! stack canary, the panic handler, and the syscall wrappers;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! `main` wires the real seams the shared `DisplayServer` engine drives:
//!
//! * `RtDriverHost::from_grants_query` over `RtGrantSyscalls`: the host
//!   learns its kernel-issued grants through the `resource_grants` syscall
//!   and maps the scan-out window through `mmio_map`. Every capability and
//!   bound is re-checked kernel-side, on the far side of the trap; the
//!   host adds no authority.
//! * `sole_framebuffer` over the delivered grants: the surface's
//!   `(phys_base, mode)` is read from the grants the kernel delivered and
//!   validated fail-closed — a missing, ambiguous, or malformed surface
//!   grant exits rather than scanning out a guessed geometry.
//! * The reserved `DISPLAY_ENDPOINT` bind (`call_create`): binding a
//!   reserved rendezvous requires the manifest's `CAP_IPC_BIND_PRIVILEGED`
//!   (kernel-enforced), so a squatter cannot intercept presents.
//! * `RtSeatCheck` over `call_peer_seat`: the engine asks the kernel, per
//!   request, whether the *in-flight caller* holds the seat's live lease —
//!   never a claimed lease, and only about a task this server is actively
//!   servicing.
//! * `RtShmMapper` over `shm_map`/`shm_unmap`: a `Configure` maps the
//!   client's granted frame region **once**, sized from the kernel's own
//!   record of the region length (never the client's claimed geometry);
//!   the present hot path only indexes the mapped bytes.
//!
//! After bring-up `main` serves forever, **parking on its wait-set**
//! between requests: `waitset_wait` holds the task off the run queue until
//! a request is posted (endpoint readiness is a non-consuming peek drained
//! by `call_recv`), so an idle display service costs no CPU — never a
//! yield-poll loop. A bind or wait-set failure exits fail-loud with a
//! reserved code, leaving the seat without a display service rather than
//! wedged; the spawning supervisor decides whether to relaunch.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::display_ipc::{DISPLAY_ENDPOINT, DISPLAY_MAX_REQUEST};
    use tairix_abi::driver::sole_framebuffer;
    use tairix_abi::{CapabilityId, Errno, WaitSetOp, WaitSourceKind};
    use tairix_caps::CapabilitySet;
    use tairix_display::{
        DisplayServer, Framebuffer, FramebufferConfig, RtShmMapper, SeatCheck, DISPLAY_REPLY_MAX,
    };
    use tairix_drv_display_framebuffer::{FIRST_PRESENT, FIRST_PRESENT_MESSAGE};
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_rt::LogSink;

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused or
    /// the delivery did not fit). A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name exactly one valid
    /// scan-out surface — an unbound or mis-provisioned node. A reserved,
    /// fail-closed value; the service never scans out a guessed geometry.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the surface bring-up failed (the scan-out window
    /// could not be mapped). A reserved, fail-closed value.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// Exit code when the reserved `DISPLAY_ENDPOINT` could not be bound
    /// (already bound, no registry, or the manifest lacks the privileged
    /// bind right). A reserved, fail-closed value: exiting leaves the seat
    /// without a display service, never a squattable half-service.
    const EXIT_NO_ENDPOINT: i32 = 83;

    /// Exit code when the wait-set the serve loop parks on could not be
    /// created, populated, or waited on. A reserved, fail-closed value: the
    /// service exits rather than degrade into a busy re-poll.
    const EXIT_WAIT_FAILED: i32 = 84;

    /// Outstanding-call capacity of the endpoint (a fail-closed memory
    /// bound): one session presents synchronously, so a small queue is
    /// ample.
    const CAPACITY: usize = 4;

    /// The serve loop's opaque token for its single wait-set member.
    const ENDPOINT_TOKEN: u64 = 1;

    /// The capability set the driver host re-checks up front before issuing
    /// an `mmio_map` trap, so a missing grant fails fast without a round
    /// trip. It mirrors the resources the matched node requested — the
    /// scan-out window (`CAP_MMIO_MAP`) and the client frame regions the
    /// engine maps (`CAP_SHM`). The kernel is the authority and re-checks
    /// every trap regardless: claiming a capability the process was not
    /// granted only fails the trap kernel-side, never widens authority.
    fn service_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::SHM);
        caps
    }

    /// The production [`SeatCheck`]: the kernel's `call_peer_seat` on the
    /// served endpoint, so the lease fact is always about the in-flight
    /// caller of *this* service — never a claim the request carried.
    struct RtSeatCheck;

    impl SeatCheck for RtSeatCheck {
        fn live_generation(&mut self, ticket: u64, seat_id: u64) -> Result<u64, Errno> {
            let ret = tairix_rt::call_peer_seat(DISPLAY_ENDPOINT, ticket, seat_id);
            if ret >= 1 {
                #[allow(clippy::cast_sign_loss)] // `ret >= 1` checked above.
                Ok(ret as u64)
            } else {
                Err(Errno::from_syscall(ret))
            }
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    ///
    /// On success this never returns: the serve loop runs for the life of
    /// the display service.
    fn main() -> i32 {
        // Build the host from the grants the kernel minted for this driver
        // and resolve the single granted scan-out surface — the one
        // definition of "which surface did the kernel grant me".
        let Ok(host) = RtDriverHost::from_grants_query(service_caps(), RtGrantSyscalls, None)
        else {
            return EXIT_NO_HOST;
        };
        let Ok((phys_base, mode)) = sole_framebuffer(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        let config = FramebufferConfig {
            phys_base,
            width_px: mode.width_px,
            height_px: mode.height_px,
            stride_bytes: mode.stride_bytes,
            format: mode.format,
        };
        // Map the surface through the host's capability-gated MMIO seam.
        // The host wires no seat gate: the per-present lease check is the
        // engine's kernel-attested `call_peer_seat`, run on every request.
        let Ok(mut surface) = Framebuffer::open(&host, config) else {
            return EXIT_BRINGUP_FAILED;
        };

        // Bind the reserved display rendezvous. The endpoint is
        // unrestricted-sender (empty capability sets): the engine itself
        // gates every request — Query included — on the caller's live seat
        // lease, so an unentitled sender receives a typed refusal.
        let empty = CapabilitySet::empty();
        let bound = tairix_rt::call_create(
            DISPLAY_ENDPOINT,
            &empty,
            &empty,
            DISPLAY_MAX_REQUEST,
            DISPLAY_REPLY_MAX,
            CAPACITY,
        );
        if bound != 0 {
            return EXIT_NO_ENDPOINT;
        }

        // Park on a wait-set between requests: endpoint readiness is a
        // non-consuming peek drained by `call_recv`, so the loop never
        // spins and an idle service holds the task off the run queue.
        let wait_set = tairix_rt::waitset_create();
        if wait_set < 0 {
            return EXIT_WAIT_FAILED;
        }
        #[allow(clippy::cast_sign_loss)] // `wait_set >= 0` checked above; it is a kernel handle.
        let wait_set = wait_set as u64;
        if tairix_rt::waitset_ctl(
            wait_set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            DISPLAY_ENDPOINT,
            ENDPOINT_TOKEN,
        ) != 0
        {
            return EXIT_WAIT_FAILED;
        }

        let mut server = DisplayServer::new(RtShmMapper);
        let mut seat = RtSeatCheck;
        let mut request = [0u8; DISPLAY_MAX_REQUEST];
        let mut reply = [0u8; DISPLAY_REPLY_MAX];
        let mut token = 0u64;
        // One-shot: emitted the first time the engine reports a client
        // frame reached the surface, then never again — the operational
        // witness that the session → service → scan-out path is live,
        // checked after the reply so the present hot path pays nothing.
        let mut first_present_logged = false;
        loop {
            if tairix_rt::waitset_wait(wait_set, u64::MAX, &mut token) != 0 {
                // A dead wait-set would degrade the loop into a busy poll;
                // exit fail-loud instead and let the supervisor decide.
                return EXIT_WAIT_FAILED;
            }
            let mut ticket: u64 = 0;
            // Non-blocking: the loop's park point is the wait-set, and the
            // queued call the wake reported may have been cancelled by its
            // poster's exit. A transient recv error (an empty queue, an
            // oversize request left queued) must not kill the server; drop
            // it and re-park.
            let Ok(len) =
                tairix_rt::call_recv_nonblock(DISPLAY_ENDPOINT, &mut request, &mut ticket)
            else {
                continue;
            };
            // Every outcome — including a malformed request — is a
            // well-formed reply; the engine never leaves a caller parked.
            let n = server.serve(&mut surface, &mut seat, ticket, &request[..len], &mut reply);
            let _ = tairix_rt::call_reply(DISPLAY_ENDPOINT, ticket, &reply[..n]);
            if !first_present_logged && server.has_presented() {
                first_present_logged = true;
                tairix_log::log(
                    &LogSink,
                    &tairix_log::Event {
                        level: tairix_log::Level::Info,
                        id: FIRST_PRESENT,
                        message: FIRST_PRESENT_MESSAGE,
                        fields: &[],
                    },
                );
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
