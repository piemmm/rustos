//! The `Run` entry-point binary of the seat-manager service, installed at
//! `/System/Services/seatmgr.app/Run` — the long-running user-space service
//! PID 1 `init` launches to hold the seat-multiplexing authority
//! (`plans/DISPLAY.md` D3).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the
//! Rust userland runtime `tairix-rt` — never the C ABI, which exists solely
//! for programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `#[global_allocator]`,
//! and the syscall wrappers (`call_create`/`call_recv`/`call_reply`/
//! `call_peer_origin`/`seat_switch`/`seat_revoke`); `tairix_rt::entry!`
//! names this program's `main`.
//!
//! # What this service does
//!
//! `seatmgr` is the sole holder of `CAP_SEAT_ADMIN`. At startup it binds
//! the well-known [`tairix_abi::seat::SEATMGR_ENDPOINT`] (a reserved
//! rendezvous, so binding it needs the manifest's
//! `CAP_IPC_BIND_PRIVILEGED`: a squatter could otherwise intercept
//! seat-administration requests) and then blocks in a serve loop: receive
//! a request, read the requester's kernel-attested `Origin`
//! (`call_peer_origin`, never a caller claim), run the capability-checked
//! [`tairix_seatmgr::serve`] dispatcher against the kernel-backed
//! [`tairix_seatmgr::SeatAdmin`], and reply with the status frame. The
//! kernel re-checks `CAP_SEAT_ADMIN` and every index on each forwarded
//! syscall, so this broker adds audited policy without widening reach.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the optional
// `tairix-rt` runtime through the default `program` feature. The kernel and
// host tooling build only this crate's *library*, so this module (and
// `tairix-rt`) never enter those builds.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::reply::encode_status_reply;
    use tairix_abi::seat::{SEATMGR_ENDPOINT, SEATMGR_MAX_REQUEST, SEATMGR_REPLY_LEN};
    use tairix_abi::{Errno, Origin, ORIGIN_WIRE_LEN};
    use tairix_caps::CapabilitySet;
    use tairix_rt::LogSink;
    use tairix_seatmgr::{serve, SeatAdmin};

    /// Outstanding-call capacity of the endpoint (a fail-closed memory
    /// bound): seat administration is low-volume, so a small queue is ample.
    const CAPACITY: usize = 4;

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// The production [`SeatAdmin`]: a thin shim over the kernel's
    /// `seat_switch` / `seat_revoke` syscalls, which re-check
    /// `CAP_SEAT_ADMIN` and validate every index on each call.
    struct KernelSeatAdmin;

    impl SeatAdmin for KernelSeatAdmin {
        fn switch(&self, seat_id: u64, console: u32) -> Result<(), Errno> {
            let ret = tairix_rt::seat_switch(seat_id, console);
            if ret == 0 {
                Ok(())
            } else {
                Err(errno_from(ret))
            }
        }

        fn revoke(&self, seat_id: u64) -> Result<(), Errno> {
            let ret = tairix_rt::seat_revoke(seat_id);
            if ret == 0 {
                Ok(())
            } else {
                Err(errno_from(ret))
            }
        }
    }

    /// Bind the endpoint and serve requests for the life of the service.
    ///
    /// The endpoint is unrestricted-sender (empty `send_caps`): the
    /// dispatcher itself requires each requester's attested origin to carry
    /// `CAP_SEAT_ADMIN`, so an unprivileged sender receives a typed refusal
    /// (and an audit record) rather than a silent drop.
    fn main() -> i32 {
        let empty = CapabilitySet::empty();
        let bound = tairix_rt::call_create(
            SEATMGR_ENDPOINT,
            &empty,
            &empty,
            SEATMGR_MAX_REQUEST,
            SEATMGR_REPLY_LEN,
            CAPACITY,
        );
        if bound != 0 {
            // Could not publish the endpoint (already bound, or no registry):
            // fail closed; PID 1 supervises and relaunches.
            return 1;
        }

        let admin = KernelSeatAdmin;
        let mut request = [0u8; SEATMGR_MAX_REQUEST];
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        loop {
            let mut ticket: u64 = 0;
            // A transient recv error (e.g. an oversize request left queued)
            // must not kill the server; drop it and continue.
            let Ok(request_len) = tairix_rt::call_recv(SEATMGR_ENDPOINT, &mut request, &mut ticket)
            else {
                continue;
            };

            // Attest the requester. A failure to read the peer origin is
            // fail-closed: reply an error rather than serving an unattested
            // request.
            let outcome =
                match tairix_rt::call_peer_origin(SEATMGR_ENDPOINT, ticket, &mut origin_buf) {
                    Ok(n) => match Origin::from_bytes(&origin_buf[..n]) {
                        Ok(origin) => serve(&admin, &origin, &LogSink, &request[..request_len]),
                        Err(err) => Err(err),
                    },
                    Err(ret) => Err(errno_from(ret)),
                };

            let reply = encode_status_reply(outcome);
            let _ = tairix_rt::call_reply(SEATMGR_ENDPOINT, ticket, &reply);
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the real freestanding `tairix-rt` `_start` path is not compiled —
// on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
