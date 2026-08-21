//! EL0 fixture program for the `lib/sandbox` parser-sandbox seam (the
//! fstree S8b increment — `plans/APPS.md`).
//!
//! One binary, four roles, selected by the **registry path** it is spawned
//! under (`arg(0)`) plus the seam's worker marker (`arg(1)`), because the
//! production launcher (`tairix_sandbox::rt::RtLauncher`) always passes
//! `[path, WORKER_ROLE_ARG]`:
//!
//! * **parent** (`/bin/sbx`, no worker marker) — drives the whole seam over
//!   the real syscalls and exits 0 only when every check passed;
//! * **decode worker** (`/bin/sbx` + marker) — serves the
//!   `tairix_sandbox::decode::DecodeService` over its wired fd 0/1 inside
//!   the kernel sandbox spawn mode;
//! * **dying worker** (`/bin/sbx-die` + marker) — exits immediately without
//!   serving: the real-process stand-in for a crashed parser;
//! * **probe worker** (`/bin/sbx-probe` + marker) — attempts syscalls the
//!   sandbox allow-list forbids (`fs_open`, `spawn`) *from inside the
//!   sandbox* and reports the denials over its reply pipe.
//!
//! The parent proves, end to end over the production spawn/pipe/wait path:
//! decode of valid and malformed inputs through a genuinely sandboxed
//! worker; typed crash containment with a logged crash event and a
//! surviving caller; and the syscall wall holding from the inside. Each
//! failure site exits with a distinct diagnostic code the chassis folds
//! into its failure finisher.
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
    use tairix_sandbox::host::{ParserSandbox, SandboxError, EVENT_WORKER_CRASHED};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::worker::{ServeEnd, Service};

    /// Registry path of the parent role — and of the decode worker the
    /// seam spawns from it.
    const SBX_PATH: &[u8] = b"/bin/sbx";
    /// Registry path whose worker exits without serving (the simulated
    /// parser crash).
    const DIE_PATH: &[u8] = b"/bin/sbx-die";
    /// Registry path whose worker probes the sandbox syscall wall.
    const PROBE_PATH: &[u8] = b"/bin/sbx-probe";

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
        ret < 0
            && ret
                .checked_neg()
                .and_then(|positive| i32::try_from(positive).ok())
                .and_then(Errno::from_i32)
                == Some(Errno::PermissionDenied)
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
            ServeEnd::Finished => 0,
            ServeEnd::Failed(_) => FAIL_SERVE,
        }
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
        0
    }

    /// Program entry point: the worker marker (`arg(1)`) selects a worker
    /// role, the registry path (`arg(0)`) selects which; otherwise this is
    /// the parent. An unknown shape cannot occur — the launcher and the
    /// registry rows are the only spawners.
    fn main() -> i32 {
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
