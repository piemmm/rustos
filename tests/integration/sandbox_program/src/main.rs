//! EL0 fixture program for the `lib/sandbox` parser-sandbox seam (the
//! fstree S8b increment — `plans/APPS.md`).
//!
//! One binary, six roles, selected by the **registry path** it is spawned
//! under (`arg(0)`) plus the seam's role marker (`arg(1)`), because the
//! production launchers (`tairix_sandbox::rt::RtLauncher`,
//! `tairix_sandbox::rt::RtSessionChannel`) always pass `[path, marker]`:
//!
//! * **parent** (`/bin/sbx`, no marker) — drives the whole seam over the
//!   real syscalls and exits 0 only when every check passed;
//! * **decode worker** (`/bin/sbx` + one-shot marker) — serves the
//!   `tairix_sandbox::decode::DecodeService` over its wired fd 0/1 inside
//!   the kernel sandbox spawn mode;
//! * **dying worker** (`/bin/sbx-die` + one-shot marker) — exits
//!   immediately without serving: the real-process stand-in for a crashed
//!   parser;
//! * **probe worker** (`/bin/sbx-probe` + one-shot marker) — attempts
//!   syscalls the sandbox allow-list forbids (`fs_open`, `spawn`) *from
//!   inside the sandbox* and reports the denials over its reply pipe;
//! * **session worker** (`/bin/sbx-session` + session marker) — serves the
//!   duplex seam, silently counting every frame it is sent and answering
//!   only the final report, then closing the session itself;
//! * **stream worker** (`/bin/sbx-stream` + session marker) — echoes every
//!   frame, and dies through its panic path on the crash frame: the
//!   real-process stand-in for a streaming decoder a crafted input kills.
//!
//! The parent proves, end to end over the production spawn/pipe/wait path:
//! decode of valid and malformed inputs through a genuinely sandboxed
//! worker; typed crash containment with a logged crash event and a
//! surviving caller; the syscall wall holding from the inside; the duplex
//! session driven entirely from a wait-set, pushing far more than one
//! pipe's worth of frames at a worker that answers nothing until the end —
//! which can only complete if the write-room wake fires; and the supervised
//! session replacing a crashed stream worker only once its paced delay has
//! elapsed on a real one-shot wait, the replacement serving on fresh
//! descriptors. Each failure site exits with a distinct diagnostic code the
//! chassis folds into its failure finisher.
//!
//! It is a **pure-Rust** program: it links `tairix-rt` (which supplies
//! `_start` and the global allocator), never the C ABI. It is built
//! position-independent and converted to an `rxe` blob by the consuming
//! test's build script. On the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(freestanding)]
extern crate alloc;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use tairix_abi::{Errno, OpenFlags};
    use tairix_log::{Event, Sink};
    use tairix_sandbox::decode::{
        container_summary, disassemble, ContainerFormat, DecodeFailure, DecodeRefusal,
        DecodeService, Isa,
    };
    use tairix_sandbox::host::{
        log_unavailable, ParserSandbox, SandboxError, EVENT_WORKER_CRASHED,
    };
    use tairix_sandbox::rt::{
        serve_session_stdio, serve_stdio, session_worker_role, worker_role, RtLauncher,
        RtSessionChannel, RtSessionLauncher, SessionMembers,
    };
    use tairix_sandbox::session::{
        FrameOut, SandboxSession, SessionBounds, SessionError, SessionService, SessionStep,
    };
    use tairix_sandbox::supervise::SupervisedSession;
    use tairix_sandbox::worker::{ServeEnd, Service};

    /// Registry path of the parent role — and of the decode worker the
    /// seam spawns from it.
    const SBX_PATH: &[u8] = b"/bin/sbx";
    /// Registry path whose worker exits without serving (the simulated
    /// parser crash).
    const DIE_PATH: &[u8] = b"/bin/sbx-die";
    /// Registry path whose worker probes the sandbox syscall wall.
    const PROBE_PATH: &[u8] = b"/bin/sbx-probe";
    /// Registry path whose worker serves the duplex session.
    const SESSION_PATH: &[u8] = b"/bin/sbx-session";
    /// Registry path whose worker echoes a stream and dies on the crash
    /// frame.
    const STREAM_PATH: &[u8] = b"/bin/sbx-stream";
    /// The frame the stream worker dies on.
    const STREAM_CRASH: &[u8] = b"crash";

    /// Bytes each direction of the session may hold queued. Deliberately
    /// far below the burst, so the parent must drain and refill.
    const SESSION_QUEUE: usize = 8 * 1024;
    /// Payload of one burst frame.
    const SESSION_PAYLOAD: usize = 512;
    /// Frames pushed at a worker that answers none of them. The framed
    /// total (about 129 KiB) is twice a pipe's capacity, so the burst
    /// cannot finish unless the write-room wake fires.
    const SESSION_FRAMES: u32 = 256;
    /// The one frame the session worker answers.
    const SESSION_REPORT: &[u8] = b"report";
    /// Wait-set tokens for the session's two directions.
    const TOKEN_READ: u64 = 1;
    const TOKEN_WRITE: u64 = 2;
    /// Per-wait deadline: a wedged session fails loudly with its
    /// diagnostic code rather than hanging out the harness budget.
    const SESSION_TIMEOUT_NS: u64 = 30_000_000_000;

    /// A minimal valid wasm module with two empty function bodies: one
    /// `code` section region plus `func[0]` / `func[1]` code regions.
    const WASM_FIXTURE: &[u8] = &[
        0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, // magic + version
        10, 9, // code section id + payload length
        2, // two bodies
        3, 0, 0x01, 0x0B, // body: locals 0, nop, end
        3, 0, 0x01, 0x0B, // body: locals 0, nop, end
    ];

    /// Two A64 `nop`s (0xD503201F little-endian).
    const NOPS: &[u8] = &[0x1F, 0x20, 0x03, 0xD5, 0x1F, 0x20, 0x03, 0xD5];

    /// Diagnostic exit code of the dying worker (asserted nowhere — any
    /// non-clean death is contained identically; distinct for a human
    /// reading the transcript).
    const DIE_EXIT: i32 = 3;
    /// A worker's serve loop failed on its transport.
    const FAIL_SERVE: i32 = 7;

    /// Crash-containment events the parent's sink observed.
    static CRASH_EVENTS: AtomicUsize = AtomicUsize::new(0);

    /// Counts [`EVENT_WORKER_CRASHED`] emissions; everything else is
    /// irrelevant to this fixture.
    #[derive(Clone, Copy)]
    struct CountingSink;

    impl Sink for CountingSink {
        fn write_event(&self, event: &Event<'_>) {
            if event.id == EVENT_WORKER_CRASHED {
                CRASH_EVENTS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Whether a raw syscall result is the sandbox wall's refusal.
    fn is_denied(ret: i64) -> bool {
        Errno::try_from_syscall(ret) == Some(Errno::PermissionDenied)
    }

    /// The probe worker's service: attempt syscalls outside the sandbox
    /// allow-list *from inside the sandbox* and report each denial as a
    /// `1` byte. The reply crossing the pipe proves the allowed surface
    /// (stream I/O) works while the wall holds.
    struct ProbeService;

    impl Service for ProbeService {
        fn handle(&mut self, _request: &[u8]) -> Vec<u8> {
            let open_denied = is_denied(tairix_rt::fs_open(SBX_PATH, OpenFlags::READ));
            let spawn_denied = is_denied(tairix_rt::spawn(SBX_PATH));
            vec![u8::from(open_denied), u8::from(spawn_denied)]
        }
    }

    /// Serve `service` over the wired standard streams; the exit code is
    /// the worker's whole observable outcome.
    fn run_worker<S: Service>(service: &mut S) -> i32 {
        match serve_stdio(service) {
            ServeEnd::Finished | ServeEnd::Ended => 0,
            ServeEnd::Failed(_) => FAIL_SERVE,
        }
    }

    /// The session worker's service: swallow every burst frame, counting
    /// it, and answer only the report — then close the session, so the
    /// parent also observes a real worker ending cleanly.
    struct CountingService {
        seen: u32,
    }

    impl SessionService for CountingService {
        fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
            if request == SESSION_REPORT {
                let _ = out.frame(&self.seen.to_le_bytes());
                return SessionStep::Finished;
            }
            self.seen = self.seen.saturating_add(1);
            SessionStep::Continue
        }
    }

    /// Serve the session over the wired standard streams.
    fn run_session_worker() -> i32 {
        match serve_session_stdio(&mut CountingService { seen: 0 }) {
            ServeEnd::Finished | ServeEnd::Ended => 0,
            ServeEnd::Failed(_) => FAIL_SERVE,
        }
    }

    /// The stream worker's service: echo every frame until the crash frame,
    /// which kills the process through its panic path mid-conversation.
    struct StreamService;

    impl SessionService for StreamService {
        fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
            assert!(request != STREAM_CRASH, "simulated parser crash");
            let _ = out.frame(request);
            SessionStep::Continue
        }
    }

    /// Serve the stream over the wired standard streams, until it crashes.
    fn run_stream_worker() -> i32 {
        match serve_session_stdio(&mut StreamService) {
            ServeEnd::Finished | ServeEnd::Ended => 0,
            ServeEnd::Failed(_) => FAIL_SERVE,
        }
    }

    /// Drive a duplex session over a real sandboxed worker, entirely from
    /// a wait-set: push far more frames than one pipe holds at a worker
    /// that answers nothing, then ask it how many it saw.
    fn session_leg() -> i32 {
        // A session has no launcher of its own, so its owner logs a failed
        // transport construction through the seam's one emitter of that id.
        let transport = match RtSessionChannel::launch(SESSION_PATH) {
            Ok(transport) => transport,
            Err(errno) => {
                log_unavailable(&CountingSink, errno);
                return 40;
            }
        };
        let Ok(bounds) = SessionBounds::new(SESSION_QUEUE, SESSION_QUEUE) else {
            return 41;
        };
        let Ok(mut session) = SandboxSession::new(transport, bounds, CountingSink) else {
            return 42;
        };
        if session.descriptors().is_none() {
            return 43;
        }
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            return 44;
        };
        let mut members = SessionMembers::new(set, TOKEN_READ, TOKEN_WRITE);

        let burst = vec![0xC3u8; SESSION_PAYLOAD];
        let mut queued: u32 = 0;
        let mut asked = false;
        let mut reported: Option<u32> = None;

        while reported.is_none() {
            // Refill the outbound queue: a full queue refuses transiently,
            // and the room wake below is the only thing that lets the rest
            // through.
            while queued < SESSION_FRAMES {
                match session.send(&burst) {
                    Ok(()) => queued += 1,
                    Err(SessionError::OutboundFull) => break,
                    Err(_) => return 45,
                }
            }
            if queued == SESSION_FRAMES && !asked {
                match session.send(SESSION_REPORT) {
                    Ok(()) => asked = true,
                    Err(SessionError::OutboundFull) => {}
                    Err(_) => return 45,
                }
            }
            // Disarming what the session does not want is what keeps
            // write room, ready whenever the pipe is not full, from waking a
            // parent that has nothing to write.
            if members
                .sync(
                    session.descriptors(),
                    session.wants_read(),
                    session.wants_write(),
                )
                .is_err()
            {
                return 46;
            }
            if !session.wants_read() && !session.wants_write() {
                // Nothing left to wait on and no answer: the session ended
                // without reporting.
                return 48;
            }
            let mut token = 0u64;
            if tairix_rt::waitset_wait(set, SESSION_TIMEOUT_NS, &mut token) < 0 {
                return 49;
            }
            let stepped = match token {
                TOKEN_WRITE => session.on_writable(),
                TOKEN_READ => session.on_readable(),
                _ => return 50,
            };
            if stepped.is_err() {
                return 51;
            }
            loop {
                match session.recv(|frame| <[u8; 4]>::try_from(frame).map(u32::from_le_bytes)) {
                    Ok(Some(Ok(count))) => reported = Some(count),
                    Ok(Some(Err(_))) => return 52,
                    Ok(None) => break,
                    Err(_) => return 53,
                }
            }
        }

        if reported != Some(SESSION_FRAMES) {
            return 54;
        }
        // The worker closed the session itself and exits cleanly.
        if session.end() != Some(0) {
            return 55;
        }
        0
    }

    /// The supervised session the stream leg drives.
    type Stream = SupervisedSession<RtSessionLauncher, CountingSink>;

    /// Frames echoed through each stream worker.
    const STREAM_PINGS: u8 = 4;

    /// One wait-set turn: arm what the session wants, wait, take the woken
    /// direction, and drain every frame that completed.
    fn stream_turn(
        session: &mut Stream,
        members: &mut SessionMembers,
        set: u64,
    ) -> Result<Vec<Vec<u8>>, SessionError> {
        members
            .sync(
                session.descriptors(),
                session.wants_read(),
                session.wants_write(),
            )
            .map_err(|_| SessionError::WorkerFailed)?;
        let mut token = 0u64;
        if tairix_rt::waitset_wait(set, SESSION_TIMEOUT_NS, &mut token) != 0 {
            return Err(SessionError::WorkerFailed);
        }
        let now = tairix_rt::clock_get();
        match token {
            TOKEN_WRITE => session.on_writable(now)?,
            TOKEN_READ => session.on_readable(now)?,
            _ => return Err(SessionError::WorkerFailed),
        }
        let mut frames = Vec::new();
        while let Some(frame) = session.recv(now, <[u8]>::to_vec)? {
            frames.push(frame);
        }
        Ok(frames)
    }

    /// Stream [`STREAM_PINGS`] frames tagged `tag` through the live worker
    /// and require each echoed back, in order.
    fn stream_echoes(
        session: &mut Stream,
        members: &mut SessionMembers,
        set: u64,
        tag: u8,
    ) -> Result<(), i32> {
        for index in 0..STREAM_PINGS {
            session.send(&[tag, index]).map_err(|_| 70)?;
        }
        let mut next = 0u8;
        while next < STREAM_PINGS {
            for frame in stream_turn(session, members, set).map_err(|_| 71)? {
                if frame.as_slice() != [tag, next] {
                    return Err(72);
                }
                next += 1;
            }
        }
        Ok(())
    }

    /// Drive a supervised session over a real stream worker: stream through
    /// it, kill it with the crash frame, and require the replacement to be
    /// started only once its paced delay has elapsed on a real one-shot
    /// wait, then to serve on a fresh pipe pair.
    fn supervised_leg() -> i32 {
        let Ok(bounds) = SessionBounds::new(SESSION_QUEUE, SESSION_QUEUE) else {
            return 60;
        };
        let mut session = Stream::new(RtSessionLauncher::new(STREAM_PATH), bounds, CountingSink);
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            return 61;
        };
        let mut members = SessionMembers::new(set, TOKEN_READ, TOKEN_WRITE);
        if session.start(tairix_rt::clock_get()) != Some(1) {
            return 62;
        }
        if let Err(code) = stream_echoes(&mut session, &mut members, set, 1) {
            return code;
        }

        let crashes = CRASH_EVENTS.load(Ordering::Relaxed);
        if session.send(STREAM_CRASH).is_err() {
            return 63;
        }
        let mut observed = false;
        for _ in 0..16 {
            if stream_turn(&mut session, &mut members, set) == Err(SessionError::WorkerFailed) {
                observed = true;
                break;
            }
        }
        if !observed || session.is_live() {
            return 64;
        }
        if CRASH_EVENTS.load(Ordering::Relaxed) != crashes + 1 {
            return 65;
        }

        let Some(due) = session.restart_deadline() else {
            return 66;
        };
        let now = tairix_rt::clock_get();
        if now < due && session.start(now).is_some() {
            return 67;
        }
        // Park on the emptied wait-set until the replacement is due.
        if members.sync(None, false, false).is_err() {
            return 68;
        }
        let mut token = 0u64;
        let waited = tairix_rt::waitset_wait(set, due.saturating_sub(now), &mut token);
        if waited != 0 && Errno::try_from_syscall(waited) != Some(Errno::TimedOut) {
            return 69;
        }
        if session.start(tairix_rt::clock_get()) != Some(2) {
            return 73;
        }
        if session.descriptors().is_none() {
            return 74;
        }
        if let Err(code) = stream_echoes(&mut session, &mut members, set, 2) {
            return code;
        }
        0
    }

    /// The parent role: every check distinct, fail-closed, in seam order.
    fn parent() -> i32 {
        // A healthy sandboxed decode worker spawned from this binary.
        let mut good = ParserSandbox::new(RtLauncher::new(SBX_PATH), CountingSink);

        // 1. A valid container decodes through the real sandbox path.
        match container_summary(&mut good, WASM_FIXTURE) {
            Ok(summary) => {
                if summary.format != ContainerFormat::Wasm {
                    return 12;
                }
                // The code section plus the two function-body regions.
                if summary.regions.len() != 3 {
                    return 13;
                }
            }
            Err(_) => return 11,
        }

        // 2. A malformed input is a typed refusal, not a crash.
        match container_summary(&mut good, b"not an executable image") {
            Err(DecodeFailure::Refused(DecodeRefusal::UnrecognisedContainer)) => {}
            _ => return 14,
        }

        // 3. Instruction decode through the same worker.
        match disassemble(&mut good, Isa::Aarch64, 0x1000, 0, 16, NOPS) {
            Ok(window) => {
                if window.insns.len() != 2 {
                    return 16;
                }
                if window.insns[0].mnemonic != "nop" {
                    return 17;
                }
                if window.next_address != 0x1008 {
                    return 18;
                }
            }
            Err(_) => return 15,
        }

        // 4. Real crash containment: the dying worker is a genuine spawned
        //    process that exits without serving. The request must fail
        //    typed, the crash must be logged, and this caller must survive.
        let mut dying = ParserSandbox::new(RtLauncher::new(DIE_PATH), CountingSink);
        match dying.request(b"anything") {
            Err(SandboxError::WorkerFailed) => {}
            Ok(_) => return 20,
            Err(_) => return 21,
        }
        if CRASH_EVENTS.load(Ordering::Relaxed) == 0 {
            return 22;
        }
        // Reap the dying seam's replacement worker eagerly.
        drop(dying);
        // The caller survived: the healthy worker still answers.
        if container_summary(&mut good, WASM_FIXTURE).is_err() {
            return 23;
        }

        // 5. The syscall wall, probed from inside a live sandbox.
        let mut probe = ParserSandbox::new(RtLauncher::new(PROBE_PATH), CountingSink);
        match probe.request(b"go") {
            Ok(reply) => {
                if reply.as_slice() != [1u8, 1] {
                    return 31;
                }
            }
            Err(_) => return 30,
        }

        // 6. The duplex session, driven from a wait-set over both
        //    directions of a real sandboxed worker's pipe pair.
        let session = session_leg();
        if session != 0 {
            return session;
        }

        // 7. The supervised session: a crashed stream worker replaced after
        //    its paced delay, the caller surviving throughout.
        supervised_leg()
    }

    /// Program entry point: the role marker (`arg(1)`) selects a worker
    /// role, the registry path (`arg(0)`) selects which; otherwise this is
    /// the parent. An unknown shape cannot occur — the launchers and the
    /// registry rows are the only spawners.
    fn main() -> i32 {
        if session_worker_role() {
            return match tairix_rt::arg(0) {
                Some(path) if path == STREAM_PATH => run_stream_worker(),
                _ => run_session_worker(),
            };
        }
        if worker_role() {
            return match tairix_rt::arg(0) {
                Some(path) if path == DIE_PATH => DIE_EXIT,
                Some(path) if path == PROBE_PATH => run_worker(&mut ProbeService),
                _ => run_worker(&mut DecodeService),
            };
        }
        parent()
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the
// crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
