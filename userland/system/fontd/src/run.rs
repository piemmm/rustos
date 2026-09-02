//! The `Run` entry-point binary of the font service, installed at
//! `/System/Services/fontd.app/Run` — the long-running user-space service
//! `login` launches on a display-capable machine to serve glyph coverage
//! (`plans/FONT-SERVICE.md` FS-3, FS-6).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI. `tairix-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `#[global_allocator]`, the filesystem read wrappers used to discover
//! `/System/Fonts`, and the endpoint syscall wrappers (`call_create`/
//! `call_recv`/`call_reply`); `tairix_rt::entry!` names this program's `main`.
//!
//! # What this service does
//!
//! At startup `fontd` lists `/System/Fonts` (a one-shot open authorised by the
//! manifest's `CAP_FS_ACCESS`; `/System` is mounted read-only so it holds no
//! write reach) and, for every subdirectory that carries a readable
//! `FontFamily` manifest, opens its declared face files — without reading
//! their bytes yet — and keeps the open handles for the service's life. A
//! face's bytes are read, once, the first time a request actually needs that
//! face: a session that never draws a script never pays for the face that
//! covers it. Discovery then binds the well-known
//! [`FONT_ENDPOINT`](tairix_abi::font_ipc::FONT_ENDPOINT) (a reserved
//! rendezvous, so binding it needs the manifest's `CAP_IPC_BIND_PRIVILEGED`: a
//! squatter could otherwise feed forged coverage to the compositor and every
//! app), and blocks in a serve loop — receive a request, rasterise/serve the
//! reply, reply. The endpoint is unrestricted-sender: drawing text is not a
//! security boundary, so any process may ask, and the reply path validates
//! every field and fails closed.
//!
//! If the store cannot be listed, no family in it is usable, or the endpoint
//! cannot be bound, the service records `SERVICE_UNAVAILABLE` and exits
//! fail-closed; PID 1 supervises and relaunches. It never serves forged or
//! absent coverage.
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

    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    use tairix_abi::font_ipc::{FontRequest, FONT_ENDPOINT, FONT_MAX_GLYPH_REPLY};
    use tairix_abi::fs::{DirEntries, OpenFlags};
    use tairix_abi::{Errno, WaitSetOp, WaitSourceKind};
    use tairix_caps::CapabilitySet;
    use tairix_fontd::discovery::{discover, FaceLoad, FontStore};
    use tairix_fontd::events::{SERVICE_READY, SERVICE_UNAVAILABLE};
    use tairix_fontd::FontService;
    use tairix_fontface::FAMILY_MANIFEST;
    use tairix_log::{Event, EventId, Level};
    use tairix_procinfo::IpcTransport;
    use tairix_rt::{File, LogSink};

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

    /// Record a startup or runtime outcome. Recorded through the kernel audit
    /// log so an operator can see the font service's state before the
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

    /// The store's root directory. Every family lives one level below it.
    const FONT_STORE_ROOT: &[u8] = b"/System/Fonts/";

    /// The path of `name` inside family directory `dir`.
    fn family_path(dir: &str, name: &str) -> Vec<u8> {
        let mut path = Vec::with_capacity(FONT_STORE_ROOT.len() + dir.len() + 1 + name.len());
        path.extend_from_slice(FONT_STORE_ROOT);
        path.extend_from_slice(dir.as_bytes());
        path.push(b'/');
        path.extend_from_slice(name.as_bytes());
        path
    }

    /// Read the whole of already-open `file` into an owned buffer.
    ///
    /// Stats for the length, then reads at successive offsets until the file
    /// is consumed. Used both for the small manifest text (read once, at
    /// discovery) and for a face's bytes (read once, on first use).
    ///
    /// The buffer is reserved fallibly, so a face larger than the heap can
    /// hold refuses the read rather than aborting the service; the family is
    /// then skipped or the glyph refused, and the rest of the store still
    /// serves.
    fn read_all(file: &File) -> Result<Vec<u8>, Errno> {
        let size = usize::try_from(file.stat().map_err(Errno::from_syscall)?.size)
            .map_err(|_| Errno::LengthOutOfRange)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(size)
            .map_err(|_| Errno::OutOfMemory)?;
        buf.resize(size, 0);
        let mut done = 0usize;
        while done < size {
            let read = file
                .read_at(done as u64, &mut buf[done..])
                .map_err(Errno::from_syscall)?;
            if read == 0 {
                break;
            }
            done += read;
        }
        buf.truncate(done);
        Ok(buf)
    }

    /// A face whose bytes are read once, on first use, from the handle
    /// opened at discovery time — never re-opened, never read early.
    ///
    /// The leaked, boxed byte slice this hands back is retained for the
    /// service's life; the leak is bounded by the store's own size (one
    /// family's declared faces, read at most once each), not by anything a
    /// caller can grow, so it never becomes an unbounded-growth surface.
    struct RealFaceLoad {
        /// The still-open handle, present until the first successful read.
        file: Option<File>,
        /// The leaked bytes, once read.
        bytes: Option<&'static [u8]>,
    }

    impl FaceLoad<'static> for RealFaceLoad {
        fn load(&mut self) -> Result<&'static [u8], Errno> {
            if let Some(bytes) = self.bytes {
                return Ok(bytes);
            }
            let file = self.file.take().ok_or(Errno::NotFound)?;
            let bytes = read_all(&file)?;
            let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            self.bytes = Some(leaked);
            Ok(leaked)
        }
    }

    /// The real `/System/Fonts` store, read through `tairix-rt`.
    struct RealFontStore;

    impl FontStore<'static> for RealFontStore {
        fn family_dirs(&mut self) -> Result<Vec<String>, Errno> {
            let stream = tairix_rt::read_dir_all(&FONT_STORE_ROOT[..FONT_STORE_ROOT.len() - 1])
                .map_err(Errno::from_syscall)?;
            let mut dirs = Vec::new();
            for entry in DirEntries::new(&stream) {
                let entry = entry?;
                if !entry.kind.is_dir() {
                    continue;
                }
                if let Ok(name) = core::str::from_utf8(entry.name) {
                    dirs.push(String::from(name));
                }
            }
            Ok(dirs)
        }

        fn read_manifest(&mut self, dir: &str) -> Option<String> {
            let path = family_path(dir, FAMILY_MANIFEST);
            let file = File::open(&path, OpenFlags::READ).ok()?;
            let bytes = read_all(&file).ok()?;
            String::from_utf8(bytes).ok()
        }

        fn face_loader(&mut self, dir: &str, face: &str) -> Box<dyn FaceLoad<'static> + 'static> {
            let path = family_path(dir, face);
            // A face that fails to open here still yields a loader: the
            // failure surfaces from `load()` on first use, exactly as a face
            // that opened but failed to read or parse would, so discovery
            // never has to special-case "opened" versus "will fail".
            let file = File::open(&path, OpenFlags::READ).ok();
            Box::new(RealFaceLoad { file, bytes: None })
        }
    }

    /// Bind `FONT_ENDPOINT` and return the wait-set watching it alongside the
    /// machine's memory-pressure band, or `None` on any refusal (recorded,
    /// failing closed).
    ///
    /// One wait set covers both things this service must react to: an arriving
    /// request and a memory-pressure change. Serving from a wait set rather
    /// than a blocking receive is what lets the cache shrink while the service
    /// is idle, without ever polling for either.
    fn bind_and_watch() -> Option<u64> {
        // Unrestricted-sender endpoint (empty `send_caps`): any process may
        // ask for coverage. `recv_caps` is empty — endpoint ownership already
        // restricts receive to this task.
        let empty = CapabilitySet::empty();
        if tairix_rt::call_create(
            FONT_ENDPOINT,
            &empty,
            &empty,
            FontRequest::WIRE_LEN,
            FONT_MAX_GLYPH_REPLY,
            CAPACITY,
        ) != 0
        {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: cannot bind FONT_ENDPOINT",
            );
            return None;
        }
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: cannot create the serve wait-set",
            );
            return None;
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
            return None;
        }
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: cannot watch the memory-pressure band",
            );
            return None;
        }
        Some(set)
    }

    /// Serve coverage requests and pressure changes over `set` until the
    /// wait-set itself fails, returning the process exit code.
    fn serve(service: &mut FontService<'_>, set: u64) -> i32 {
        let mut request = [0u8; FontRequest::WIRE_LEN];
        let mut reply = alloc::vec![0u8; FONT_MAX_GLYPH_REPLY];
        let mut token = 0u64;
        loop {
            // The wait stays indefinite: a cache-report change the rate
            // limiter is holding back only ever *tightens* it to the moment
            // it may be sent, and folds back to indefinite once it has gone
            // out. The service never polls for either of its two sources.
            let timeout_ns = tairix_rt::cachereport::fold_wait_deadline_ns(u64::MAX);
            let waited = tairix_rt::waitset_wait(set, timeout_ns, &mut token);
            if waited != 0 {
                if Errno::from_syscall(waited) != Errno::TimedOut {
                    // A dead wait-set would degrade the loop into a busy poll;
                    // exit fail-loud instead and let PID 1 relaunch the service.
                    record(
                        SERVICE_UNAVAILABLE,
                        Level::Warn,
                        "fontd: the serve wait-set failed",
                    );
                    return 1;
                }
                // No member woke, so `token` still names the *previous* wake's
                // source and receiving on it would block with nothing queued.
                // The held-back report is the only bounded wait this loop
                // arms: send it and park again.
                tairix_rt::cachereport::publish_if_due();
                continue;
            }
            if token == PRESSURE_TOKEN {
                if tairix_procinfo::pressure::refresh() {
                    service.trim_cache();
                }
            } else {
                let mut ticket: u64 = 0;
                // A transient recv error must not kill the server; the
                // endpoint's `max_request` bound means an oversize request is
                // refused at post time, never left queued.
                if let Ok(request_len) =
                    tairix_rt::call_recv(FONT_ENDPOINT, &mut request, &mut ticket)
                {
                    let reply_len = service.handle(&request[..request_len], &mut reply);
                    if reply_len > 0 {
                        let _ = tairix_rt::call_reply(FONT_ENDPOINT, ticket, &reply[..reply_len]);
                    }
                }
            }
            tairix_rt::cachereport::publish_if_due();
        }
    }

    /// Discover the store, bind the endpoint, and serve requests for the life
    /// of the service. Returns a non-zero exit code on any fail-closed
    /// startup error.
    fn main() -> i32 {
        // The cache is sized from the machine's own RAM, never a hand-picked
        // ceiling: a reading the System Information service cannot supply is
        // zero, which admits nothing and leaves every glyph rasterised on
        // demand — slower, never wrong.
        let total_ram = tairix_procinfo::memory_total_bytes(&IpcTransport).unwrap_or(0);
        let cache = tairix_fontd::glyph_cache(total_ram, tairix_rt::pressure::gauge(), &LOG_SINK);
        // From here on the registry may hold this process's glyph-cache row,
        // so every return path — startup failure or the serve loop's own
        // fail-loud exit — must withdraw it; a dropped guard does that once,
        // unconditionally.
        let _cache_report_guard = tairix_rt::cachereport::ReportGuard;
        let mut store = RealFontStore;
        let Ok(mut service) = discover(&mut store, cache, &LOG_SINK) else {
            record(
                SERVICE_UNAVAILABLE,
                Level::Warn,
                "fontd: the /System/Fonts store has no usable family",
            );
            return 1;
        };
        // Arms the pressure wake and primes the gauge with the band in force,
        // so the cache never runs on the fail-closed unknown band.
        let Some(set) = bind_and_watch() else {
            return 1;
        };
        record(SERVICE_READY, Level::Info, "fontd: serving FONT_ENDPOINT");
        serve(&mut service, set)
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
