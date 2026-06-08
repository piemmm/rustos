//! The `Run` entry-point binary of the `init` application bundle
//! (`AGENTS.md` §16.5, `plans/PI.md` P6b).
//!
//! This is the program the kernel spawns as PID 1 once it reaches user mode
//! (`plans/PI.md` P6c). It is a **pure-Rust** program: RustOS is Rust-only
//! (`AGENTS.md` §1), so `init` links the Rust userland runtime
//! `rustos-rt` — never the C ABI (`crt0` + `abi-sys`), which exists solely
//! for programs **not** written in Rust (`AGENTS.md` §16.4). `rustos-rt`
//! provides `_start`, the per-process stack canary (`AGENTS.md` §19.2), the
//! panic handler, and the syscall wrappers; `rustos_rt::entry!` names this
//! program's `main`.
//!
//! `main` parses the compiled-in `startup::DEFAULT_CONFIG`, writes the
//! first banner line to its inherited standard output (fd 1) through
//! `rustos_rt::stdout` (the `abi-v1` `stream_write` syscall — `AGENTS.md`
//! §20, `init` binds to the stream, never a device), then **supervises** the
//! user's session: it launches the session program through `spawn`, blocks on
//! it with `wait`, reaps it, and relaunches it (`plans/PI.md` P6e-3b-ii). The
//! runtime routes `main`'s return value through the `exit` syscall.
//!
//! It links **only** the runtime and its own startup-config parser, never the
//! sibling `rustos-init` orchestrator library, whose `alloc`-and-crypto
//! dependency chain has no place in a banner-printing program (`AGENTS.md`
//! §2.3). That parser therefore lives alongside it in [`startup`] and is
//! host-tested there. The binary is built position-independent and converted
//! to an `rxe` blob by the consuming boot path (`plans/PI.md` P6c). On the
//! host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

mod startup;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use crate::startup::{StartupConfig, BANNER, DEFAULT_CONFIG};

    /// Exit code when the compiled-in startup config does not parse. A
    /// reserved, fail-closed value (`AGENTS.md` §2.9); the default config is
    /// well-formed, so reaching this is a build defect, not a runtime input.
    const EXIT_CONFIG_INVALID: i32 = 70;

    /// Exit code when launching the session program failed — the `spawn`
    /// syscall returned a negative `-errno`. A reserved, fail-closed value
    /// (`AGENTS.md` §2.9) distinct from [`EXIT_CONFIG_INVALID`] so the cause
    /// is unambiguous in the audit transcript.
    const EXIT_SESSION_FAILED: i32 = 71;

    /// Exit code when waiting on the session failed — the `wait` syscall
    /// returned a negative `-errno` (the supervisor cannot reap the child it
    /// just spawned). A reserved, fail-closed value (`AGENTS.md` §2.9)
    /// distinct from [`EXIT_SESSION_FAILED`] so the cause is unambiguous.
    const EXIT_WAIT_FAILED: i32 = 72;

    /// Exit code when the session could not stay up: it exited and was
    /// relaunched [`SESSION_SPAWN_BUDGET`] times without ever blocking, so the
    /// supervisor stops rather than relaunching it forever. A reserved,
    /// fail-closed value (`AGENTS.md` §2.9): a session that immediately exits
    /// every time means the system cannot come up, and PID 1 declares that
    /// honestly instead of busy-looping on `spawn` (`AGENTS.md` §2.1).
    const EXIT_SESSION_EXHAUSTED: i32 = 73;

    /// How many times PID 1 will (re)launch the session before concluding it
    /// cannot stay up and failing closed ([`EXIT_SESSION_EXHAUSTED`]).
    ///
    /// On a system with a working input stream the session blocks on `stdin`
    /// rather than exiting, so the supervisor blocks in `wait` for the
    /// session's whole lifetime and never approaches this bound — it
    /// supervises one long-lived session, exactly as intended. The bound
    /// exists only as a **crash-loop guard**: if the session exits the instant
    /// it starts (e.g. no input backing is attached), relaunching it without
    /// limit would be a busy loop on `spawn`, which `AGENTS.md` §2.1 forbids.
    /// A small budget proves the supervisor genuinely relaunches a dead
    /// session while keeping the loop bounded.
    const SESSION_SPAWN_BUDGET: u32 = 3;

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Parses the compiled-in [`DEFAULT_CONFIG`], writes the startup banner to
    /// its inherited standard output (fd 1), then supervises the user's
    /// session for the lifetime of PID 1 (see [`supervise_session`]).
    ///
    /// The banner write is *gated*: `stdout` returns the number of
    /// bytes the kernel accepted, so a short count means the write did not
    /// fully land (a missing `CAP_CONSOLE_WRITE`, an unresolved address
    /// space, an unestablished descriptor, or a closed-fail kernel path).
    /// PID 1 cannot usefully proceed without the console it was spawned to
    /// drive, so it parks fail-closed rather than supervising a session on a
    /// console it never reached (`AGENTS.md` §2.9). This is a terminal park,
    /// not a retry loop (`AGENTS.md` §2.1).
    fn main() -> i32 {
        let Ok(config) = StartupConfig::parse(DEFAULT_CONFIG) else {
            return EXIT_CONFIG_INVALID;
        };
        let banner = BANNER.as_bytes();
        if rustos_rt::stdout(banner) != banner.len() {
            loop {
                core::hint::spin_loop();
            }
        }
        supervise_session(config.session().as_bytes())
    }

    /// Supervise the user's `session` program across the lifetime of PID 1:
    /// launch it, block until it exits, reap it, and relaunch it.
    ///
    /// This is the standing init duty (`plans/PI.md` P6e-3b-ii): the session is
    /// a separate, hardware-isolated process (a true `spawn`, not an
    /// `exec`-style hand-off, `AGENTS.md` §4), so PID 1 keeps running and
    /// *owns* its lifecycle rather than spawning-and-forgetting it. Each cycle:
    ///
    /// 1. `spawn` the session. A negative result is a failed launch (an unknown
    ///    path, an unwired spawn subsystem, a build failure); it is fail-loud
    ///    ([`EXIT_SESSION_FAILED`], `AGENTS.md` §2.9), never ignored.
    /// 2. `wait` on exactly that child, blocking until it exits and reaping it
    ///    (`plans/SPAWN.md` SP6). A negative result means the supervisor cannot
    ///    reap its own child — a kernel-state inconsistency it surfaces as
    ///    [`EXIT_WAIT_FAILED`] rather than continuing blindly.
    /// 3. Relaunch, up to [`SESSION_SPAWN_BUDGET`] launches total. The bound is
    ///    a crash-loop guard (see its docs): a session that blocks on input
    ///    never reaches it; one that exits instantly stops the loop at
    ///    [`EXIT_SESSION_EXHAUSTED`] instead of busy-looping on `spawn`
    ///    (`AGENTS.md` §2.1).
    ///
    /// The reaped child's exit status is read but not yet acted on; a policy
    /// that distinguishes a clean logout from a crash (and resets the budget
    /// on a session that ran long enough) awaits a clock/session-state ABI.
    fn supervise_session(session: &[u8]) -> i32 {
        let mut launches: u32 = 0;
        loop {
            let pid = rustos_rt::spawn(session);
            if pid < 0 {
                return EXIT_SESSION_FAILED;
            }
            launches += 1;

            // Block until this session exits, reaping it so it does not linger
            // as a zombie. `wait` is given the specific child PID, so the
            // supervisor reaps the session it launched and nothing else.
            let mut status = 0i32;
            // PIDs fit an `i32` on this ABI, and `spawn` returned a
            // non-negative value, so the cast preserves the PID.
            #[allow(clippy::cast_possible_truncation)]
            let reaped = rustos_rt::wait(pid as i32, &mut status);
            if reaped < 0 {
                return EXIT_WAIT_FAILED;
            }

            if launches >= SESSION_SPAWN_BUDGET {
                return EXIT_SESSION_EXHAUSTED;
            }
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// parses the compiled-in default config (and touches the parser's accessors)
// so a malformed `DEFAULT_CONFIG` is caught by an ordinary `cargo build` and
// the parser is exercised, not dead code, on the host. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {
    if let Ok(config) = startup::StartupConfig::parse(startup::DEFAULT_CONFIG) {
        let _ = (config.session(), startup::BANNER);
    }
}
