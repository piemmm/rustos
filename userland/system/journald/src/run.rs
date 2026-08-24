//! The `Run` entry-point binary of the journal service, installed at
//! `/System/Services/journald` — the long-running user-space service PID 1
//! `init` launches to own authoritative system-log segment writes (SYSLOG
//! §12/§15).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `#[global_allocator]`, and
//! the syscall wrappers (`call_create`/`call_recv`/`call_reply`/
//! `call_peer_origin`/`self_origin`/`boot_id`/`clock_get`/`wall_time`/the
//! `fs_*` file API); `tairix_rt::entry!` names this program's `main`.
//!
//! # What this service does
//!
//! At startup it reads its installation identity — the per-installation
//! machine-id (`/System/Security/MachineId`, non-secret) and, when present,
//! the log-attestation key (`/System/Security/Keys/LogAttestation`, the secret
//! that seals `audit`/`security` segments) — reads its own attested `Origin`
//! (`self_origin`) and the per-boot `BootId`, and builds a `Journal` over an
//! FS-backed `SegmentStore` that writes each closed segment as its own
//! immutable file under `/System/Logs/<stream>/`. It then binds the well-known
//! `LOG_INGRESS_ENDPOINT` (an unrestricted-sender call endpoint — any process
//! may post a log record, but the id is a reserved rendezvous, so binding it
//! needs `CAP_IPC_BIND_PRIVILEGED`: a squatter could otherwise capture every
//! process's log traffic) and blocks in a serve loop: receive a framed
//! request, read the caller's kernel-attested `Origin` (`call_peer_origin`,
//! never a caller claim), stamp the record with the journal's own ingest lane
//! and the current monotonic + wall time, and hand it to the `serve` dispatch
//! core (`tairix_journald::serve`), which admits it under the attested origin
//! and commits it. It installs a per-stream rate limit so a log flood on the
//! caller-writable `runtime`/`debug` streams is bounded (the system-authority
//! streams are never dropped); dropped records surface as coalesced trusted
//! loss records on the `journal` stream.
//!
//! Every authoritative fact — origin, source, stream, sequence, time — is the
//! kernel's or the journal's, never the caller's; the caller supplies only its
//! message and advisory hints. Missing installation identity fails closed
//! (`audit`/`security` cannot be sealed without the key, and the service
//! refuses to start without a machine-id or boot-id rather than binding logs
//! to a fabricated genesis).
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

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
    extern crate alloc;

    use alloc::vec;
    use alloc::vec::Vec;

    use tairix_abi::log_ingress::{
        encode_reply, LOG_INGRESS_ENDPOINT, LOG_INGRESS_MAX_REQUEST, LOG_INGRESS_REPLY_LEN,
    };
    use tairix_abi::time::{Duration64, WallClockReading};
    use tairix_abi::{Errno, Origin, MACHINE_ID_LEN, ORIGIN_WIRE_LEN};
    use tairix_caps::CapabilitySet;
    use tairix_journald::store::{
        segment_placement_for, LOG_ATTESTATION_KEY_PATH, MACHINE_ID_PATH,
    };
    use tairix_journald::{serve, Clock, Ingest};
    use tairix_log::{
        machine_id_hash, Journal, LogAttestationKey, RateLimit, RateLimiter, SegmentStore,
        LOG_ATTESTATION_KEY_FILE_LEN, MAX_RECORD_PAYLOAD, STREAM_COUNT,
    };

    /// Outstanding-call capacity of the ingress endpoint (a fail-closed memory
    /// bound on queued, not-yet-serviced records).
    const CAPACITY: usize = 16;

    /// Working-buffer size per stream. A stream's open segment is built in its
    /// buffer, so this bounds the segment size (a full buffer rotates the
    /// segment). 64 KiB keeps rotation infrequent while staying a modest
    /// per-process heap cost across the six streams.
    const SEGMENT_BUF_BYTES: usize = 64 * 1024;

    /// The journal's own ingest-lane CPU identity for the records it stamps at
    /// the user-space IPC boundary (records arriving over IPC have no
    /// originating CPU of their own — SYSLOG §5.2). A single serving task, so a
    /// single lane.
    const INGEST_CPU: u32 = 0;

    /// Sustained records per second admitted on the `runtime` stream before
    /// dropping, with the burst that may arrive back-to-back. These bound a
    /// log-driven denial of service (the SYSLOG loss/rate-limit contract); they
    /// are a fixed security budget, not a hardware-scaled capacity, sized to
    /// absorb normal service chatter and a reasonable burst while capping a
    /// runaway emitter.
    const RUNTIME_RATE_PER_SEC: u32 = 2_000;
    const RUNTIME_BURST: u32 = 512;

    /// The same budget for the high-volume `debug` stream — a higher sustained
    /// rate and burst, since diagnostic logs are chattier and short-lived.
    const DEBUG_RATE_PER_SEC: u32 = 8_000;
    const DEBUG_BURST: u32 = 2_048;

    /// Minimum spacing between coalesced rate-limit loss records for a stream,
    /// so a sustained flood yields at most one loss record per interval rather
    /// than a second flood of loss records.
    const RATE_LOSS_REPORT_SECS: i64 = 5;

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// Read the whole of a fixed-size identity/key file at `path` into `buf`,
    /// returning `Ok(())` only when exactly `buf.len()` bytes were read. A
    /// missing file, a short read, or any I/O error fails closed.
    fn read_exact_file(path: &str, buf: &mut [u8]) -> Result<(), i64> {
        let file = tairix_rt::open(path.as_bytes())?;
        let read = file.read_at(0, buf)?;
        if read == buf.len() {
            Ok(())
        } else {
            Err(-i64::from(Errno::BufferTooSmall.as_i32()))
        }
    }

    /// The production [`SegmentStore`]: it writes each closed, immutable
    /// segment as its own file under `/System/Logs/<stream>/`, deriving the
    /// placement from the segment's own header (never a caller claim). It
    /// creates the per-stream directory on first use (an already-existing
    /// directory is not an error) and syncs the file so a power loss cannot
    /// lose an acknowledged segment.
    struct FsSegmentStore;

    impl SegmentStore for FsSegmentStore {
        type Error = Errno;

        fn store_segment(&mut self, bytes: &[u8]) -> Result<(), Errno> {
            // Placement is read from the segment's self-checksummed header, so
            // a corrupt image fails closed rather than landing at a guessed
            // path.
            let (dir, path) = segment_placement_for(bytes).map_err(|_| Errno::OutOfRange)?;

            // Ensure the per-stream directory exists. An `AlreadyExists`
            // return is the steady-state case and is not an error; any other
            // failure is surfaced.
            let mkdir = tairix_rt::fs_mkdir(dir.as_bytes());
            if mkdir < 0 {
                let err = errno_from(mkdir);
                if err != Errno::AlreadyExists {
                    return Err(err);
                }
            }

            // Create the immutable segment file and write the whole image.
            let file = tairix_rt::create(path.as_bytes()).map_err(errno_from)?;
            let written = file.write_at(0, bytes).map_err(errno_from)?;
            if written != bytes.len() {
                return Err(Errno::NoSpace);
            }
            // Flush so an acknowledged segment survives a power loss.
            let synced = tairix_rt::fs_sync(file.fd());
            if synced < 0 {
                return Err(errno_from(synced));
            }
            Ok(())
        }
    }

    /// Read the per-request clock: the mandatory monotonic ordering time and
    /// the wall-clock reading with its trust state. A wall clock that is not
    /// yet set reads as the epoch tagged `Unset`; a failure to read it at all
    /// degrades to `Unset` rather than dropping the record (the monotonic
    /// clock is the ordering authority, never the wall clock — SYSLOG §5.1).
    fn read_clock() -> Clock {
        let monotonic = Duration64::from_nanos(tairix_rt::clock_get());
        let wall = tairix_rt::wall_time().unwrap_or(WallClockReading::UNSET);
        Clock { monotonic, wall }
    }

    /// Answer `ticket` with a framed status reply. A failure to even frame the
    /// reply is dropped: the client's `ipc_call` then observes a truncated
    /// reply and fails closed on decode.
    fn reply(reply_buf: &mut [u8], ticket: u64, result: Result<(), Errno>) {
        if let Ok(total) = encode_reply(result, reply_buf) {
            let _ = tairix_rt::call_reply(LOG_INGRESS_ENDPOINT, ticket, &reply_buf[..total]);
        }
    }

    /// Bind the ingress endpoint and serve log records for the life of the
    /// service.
    fn main() -> i32 {
        // Load the installation identity. The machine-id binds the log
        // genesis to this installation and is mandatory; a system without one
        // is unprovisioned and the service fails closed rather than binding
        // its logs to a fabricated genesis.
        let mut machine_id = [0u8; MACHINE_ID_LEN];
        if read_exact_file(MACHINE_ID_PATH, &mut machine_id).is_err() {
            return 2;
        }
        let machine_hash = machine_id_hash(&machine_id);

        // The per-boot id likewise binds the genesis; it is minted early in
        // boot, so a failure here means the random subsystem never seeded and
        // the service fails closed (PID 1 relaunches).
        let Ok(boot_id) = tairix_rt::boot_id() else {
            return 3;
        };

        // The journal's own attested origin, for the trusted records it authors
        // itself (the `security` spoof-notes). Read from the kernel, never
        // self-claimed.
        let Ok(own_origin) = tairix_rt::self_origin() else {
            return 4;
        };

        // The log-attestation key seals `audit`/`security` segments. It is
        // optional: without it those two streams fail closed at rotation (no
        // unsigned audit segment is ever written), while `boot`/`runtime`/
        // `debug`/`journal` are served normally. A malformed key file is
        // treated as absent rather than trusted.
        let mut key_bytes = [0u8; LOG_ATTESTATION_KEY_FILE_LEN];
        let seal_key: Option<LogAttestationKey> =
            if read_exact_file(LOG_ATTESTATION_KEY_PATH, &mut key_bytes).is_ok() {
                LogAttestationKey::from_file_bytes(&key_bytes).ok()
            } else {
                None
            };

        // One working buffer per stream, held for the life of the journal.
        let mut buffers: [Vec<u8>; STREAM_COUNT] =
            core::array::from_fn(|_| vec![0u8; SEGMENT_BUF_BYTES]);
        let [b0, b1, b2, b3, b4, b5] = &mut buffers;
        let bufs: [&mut [u8]; STREAM_COUNT] = [
            b0.as_mut_slice(),
            b1.as_mut_slice(),
            b2.as_mut_slice(),
            b3.as_mut_slice(),
            b4.as_mut_slice(),
            b5.as_mut_slice(),
        ];

        // Install the ingress rate limit so a log flood on the caller-writable
        // `runtime`/`debug` streams is bounded; the four system-authority
        // streams are never dropped, and dropped records surface as coalesced
        // trusted loss records.
        let limiter = RateLimiter::new(
            RateLimit::per_second(RUNTIME_RATE_PER_SEC, RUNTIME_BURST),
            RateLimit::per_second(DEBUG_RATE_PER_SEC, DEBUG_BURST),
            Duration64::from_secs(RATE_LOSS_REPORT_SECS),
        );
        let mut journal = Journal::new(
            FsSegmentStore,
            machine_hash,
            boot_id,
            seal_key,
            own_origin,
            bufs,
        )
        .with_rate_limit(limiter);
        let mut ingest = Ingest::new(INGEST_CPU);

        // Publish the endpoint. Unrestricted-sender (empty `send_caps`): any
        // process may post a log record. `recv_caps` is empty — endpoint
        // ownership already restricts receive to this task.
        let empty = CapabilitySet::empty();
        let bound = tairix_rt::call_create(
            LOG_INGRESS_ENDPOINT,
            &empty,
            &empty,
            LOG_INGRESS_MAX_REQUEST,
            LOG_INGRESS_REPLY_LEN,
            CAPACITY,
        );
        if bound != 0 {
            // Could not publish (already bound, or no registry): fail closed;
            // PID 1 supervises and relaunches.
            return 1;
        }

        let mut request = vec![0u8; LOG_INGRESS_MAX_REQUEST];
        let mut scratch = vec![0u8; MAX_RECORD_PAYLOAD];
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        let mut reply_buf = [0u8; LOG_INGRESS_REPLY_LEN];
        loop {
            let mut ticket: u64 = 0;
            // A transient recv error (e.g. an oversize request left queued)
            // must not kill the server; drop it and continue.
            let Ok(request_len) =
                tairix_rt::call_recv(LOG_INGRESS_ENDPOINT, &mut request, &mut ticket)
            else {
                continue;
            };

            // Attest the caller. A failure to read the peer origin is
            // fail-closed: reply an error rather than admitting an unattested
            // record.
            let caller =
                match tairix_rt::call_peer_origin(LOG_INGRESS_ENDPOINT, ticket, &mut origin_buf) {
                    Ok(n) => match Origin::from_bytes(&origin_buf[..n]) {
                        Ok(origin) => origin,
                        Err(err) => {
                            reply(&mut reply_buf, ticket, Err(err));
                            continue;
                        }
                    },
                    Err(ret) => {
                        reply(&mut reply_buf, ticket, Err(errno_from(ret)));
                        continue;
                    }
                };

            let clock = read_clock();
            let result = serve(
                &mut journal,
                &caller,
                &mut ingest,
                clock,
                &request[..request_len],
                &mut scratch,
            );
            reply(&mut reply_buf, ticket, result);
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
