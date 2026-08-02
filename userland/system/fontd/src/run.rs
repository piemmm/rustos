//! The `Run` entry-point binary of the font service, installed at
//! `/System/Services/fontd.app/Run` — the long-running user-space service
//! `login` launches on a display-capable machine to serve glyph coverage
//! (`plans/FONT-SERVICE.md` FS-3, FS-6).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI. `tairix-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `#[global_allocator]`, the filesystem read wrappers used to load
//! `/System/Fonts`, and the endpoint syscall wrappers (`call_create`/
//! `call_recv`/`call_reply`); `tairix_rt::entry!` names this program's `main`.
//!
//! # What this service does
//!
//! At startup `fontd` reads the four committed TrueType faces from
//! `/System/Fonts/` (a one-shot open authorised by the manifest's
//! `CAP_FS_ACCESS`; `/System` is mounted read-only so it holds no write reach
//! and keeps no open fd afterwards), parses them into
//! the sandboxed [`FontService`](tairix_fontd::FontService) rasteriser, binds
//! the well-known [`FONT_ENDPOINT`](tairix_abi::font_ipc::FONT_ENDPOINT) (a
//! reserved rendezvous, so binding it needs the manifest's
//! `CAP_IPC_BIND_PRIVILEGED`: a squatter could otherwise feed forged coverage
//! to the compositor and every app), and blocks in a serve loop — receive a
//! request, rasterise/serve the reply, reply. The endpoint is
//! unrestricted-sender: drawing text is not a security boundary, so any
//! process may ask, and the reply path validates every field and fails
//! closed.
//!
//! If a face cannot be loaded or parsed, or the endpoint cannot be bound, the
//! service records `SERVICE_UNAVAILABLE` and exits fail-closed; PID 1
//! supervises and relaunches. It never serves forged or absent coverage.
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

    use alloc::vec::Vec;

    use tairix_abi::font_ipc::{FontRequest, FONT_ENDPOINT, FONT_MAX_GLYPH_REPLY};
    use tairix_abi::{WaitSetOp, WaitSourceKind};
    use tairix_caps::CapabilitySet;
    use tairix_fontd::events::{SERVICE_READY, SERVICE_UNAVAILABLE};
    use tairix_fontd::{glyph_cache, FontService, FACE_REPERTOIRES};
    use tairix_fontface::Repertoire;
    use tairix_log::{Event, EventId, Level};
    use tairix_procinfo::IpcTransport;
    use tairix_reclaim::PressureBand;
    use tairix_rt::LogSink;

    /// Outstanding-call capacity of the endpoint (a fail-closed memory bound).
    const CAPACITY: usize = 8;

    /// Wait-set token for "a glyph request is waiting on the endpoint".
    const REQUEST_TOKEN: u64 = 0;

    /// Wait-set token for "the machine's memory-pressure band moved".
    ///
    /// The service parks on this alongside the endpoint rather than polling:
    /// the kernel wakes it only when the band actually changes, and it then
    /// gives back whatever the new band says its rasterised glyphs may no
    /// longer occupy.
    const PRESSURE_TOKEN: u64 = 1;

    /// The audit sink every record — startup, and the reclaim model's own
    /// classification/defect events — is written through.
    static LOG_SINK: LogSink = LogSink;

    /// Read the machine's current memory-pressure band and publish it to the
    /// process gauge the glyph cache consults, returning whether the band
    /// actually moved.
    ///
    /// A refused or failed read publishes nothing: the gauge keeps the band it
    /// already had rather than assuming the machine is comfortable, which
    /// costs cache hits and never correctness.
    fn refresh_pressure_band() -> bool {
        let Ok(reported) = tairix_procinfo::memory_pressure_band(&IpcTransport) else {
            return false;
        };
        tairix_rt::pressure::report(PressureBand::from_depth(reported.band))
    }

    /// The four committed faces the service loads, in the family's resolution
    /// order — the same order and scoping [`FACE_REPERTOIRES`] declares.
    const FACE_PATHS: [&[u8]; 4] = [
        b"/System/Fonts/Inconsolata-EX.ttf",
        b"/System/Fonts/MPLUS1Code-Regular.ttf",
        b"/System/Fonts/D2Coding-Regular.ttf",
        b"/System/Fonts/NotoSansHebrew-ExtraCondensed.ttf",
    ];

    /// Read the whole regular file at `path` into an owned buffer.
    ///
    /// Stats for the length, then reads at successive offsets until the file
    /// is consumed. Returns the raw negative kernel result (`-errno`) of the
    /// failing syscall.
    fn read_file(path: &[u8]) -> Result<Vec<u8>, i64> {
        let file = tairix_rt::open(path)?;
        let size = usize::try_from(file.stat()?.size).unwrap_or(usize::MAX);
        let mut buf = alloc::vec![0u8; size];
        let mut done = 0usize;
        while done < size {
            let read = file.read_at(done as u64, &mut buf[done..])?;
            if read == 0 {
                break;
            }
            done += read;
        }
        buf.truncate(done);
        Ok(buf)
    }

    /// Record a startup outcome. Recorded through the kernel audit log so an
    /// operator can see the font service came up (or failed closed) before the
    /// desktop.
    fn record(id: EventId, level: Level, message: &str) {
        let _ = tairix_log::log(
            &LOG_SINK,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// Load the faces, bind the endpoint, and serve requests for the life of
    /// the service. Returns a non-zero exit code on any fail-closed startup
    /// error.
    fn main() -> i32 {
        let mut faces: Vec<Vec<u8>> = Vec::with_capacity(FACE_PATHS.len());
        for path in FACE_PATHS {
            match read_file(path) {
                Ok(bytes) => faces.push(bytes),
                Err(_) => {
                    record(
                        SERVICE_UNAVAILABLE,
                        Level::Warn,
                        "fontd: cannot read a /System/Fonts face",
                    );
                    return 1;
                }
            }
        }

        let sources: Vec<(&[u8], Repertoire)> = faces
            .iter()
            .map(Vec::as_slice)
            .zip(FACE_REPERTOIRES)
            .collect();
        // The cache is sized from the machine's own RAM, never a hand-picked
        // ceiling: a reading the System Information service cannot supply is
        // zero, which admits nothing and leaves every glyph rasterised on
        // demand — slower, never wrong.
        let total_ram = tairix_procinfo::memory_total_bytes(&IpcTransport).unwrap_or(0);
        let cache = glyph_cache(total_ram, tairix_rt::pressure::gauge(), &LOG_SINK);
        let mut service = match FontService::new(&sources, cache) {
            Ok(service) => service,
            Err(_) => {
                record(
                    SERVICE_UNAVAILABLE,
                    Level::Warn,
                    "fontd: a /System/Fonts face failed to parse",
                );
                return 1;
            }
        };

        // Unrestricted-sender endpoint (empty `send_caps`): any process may
        // ask for coverage. `recv_caps` is empty — endpoint ownership already
        // restricts receive to this task.
        let empty = CapabilitySet::empty();
        let bound = tairix_rt::call_create(
            FONT_ENDPOINT,
            &empty,
            &empty,
            FontRequest::WIRE_LEN,
            FONT_MAX_GLYPH_REPLY,
            CAPACITY,
        );
        if bound != 0 {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: cannot bind FONT_ENDPOINT",
            );
            return 1;
        }
        // One wait set covers both things this service must react to: an
        // arriving request and a memory-pressure change. Serving from a wait
        // set rather than a blocking receive is what lets the cache shrink
        // while the service is idle, without ever polling for either.
        let set = tairix_rt::waitset_create();
        let Ok(set) = u64::try_from(set) else {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: cannot create the serve wait-set",
            );
            return 1;
        };
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            FONT_ENDPOINT,
            REQUEST_TOKEN,
        ) != 0
        {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: cannot watch FONT_ENDPOINT",
            );
            return 1;
        }
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::MemoryPressure,
            0,
            PRESSURE_TOKEN,
        ) != 0
        {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: cannot watch the memory-pressure band",
            );
            return 1;
        }
        // Start from the band in force now: the member reports only *changes*,
        // so without this the cache would run on the gauge's fail-closed
        // unknown state — retaining nothing — until the machine happened to
        // move band.
        refresh_pressure_band();
        record(SERVICE_READY, Level::Info, "fontd: serving FONT_ENDPOINT");

        let mut request = [0u8; FontRequest::WIRE_LEN];
        let mut reply = alloc::vec![0u8; FONT_MAX_GLYPH_REPLY];
        let mut token = 0u64;
        loop {
            if tairix_rt::waitset_wait(set, u64::MAX, &mut token) != 0 {
                // A dead wait-set would degrade the loop into a busy poll;
                // exit fail-loud instead and let PID 1 relaunch the service.
                record(
                    SERVICE_UNAVAILABLE,
                    Level::Warn,
                    "fontd: the serve wait-set failed",
                );
                return 1;
            }
            if token == PRESSURE_TOKEN {
                if refresh_pressure_band() {
                    service.trim_cache();
                }
                continue;
            }
            let mut ticket: u64 = 0;
            let request_len = match tairix_rt::call_recv(FONT_ENDPOINT, &mut request, &mut ticket) {
                Ok(len) => len,
                // A transient recv error must not kill the server; drop it and
                // continue. The endpoint's `max_request` bound means an
                // oversize request is refused at post time, never left queued.
                Err(_) => continue,
            };
            let reply_len = service.handle(&request[..request_len], &mut reply);
            if reply_len > 0 {
                let _ = tairix_rt::call_reply(FONT_ENDPOINT, ticket, &reply[..reply_len]);
            }
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
