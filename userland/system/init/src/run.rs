//! The `Run` entry-point binary of the `init` application bundle
//! (`plans/PI.md` P6b).
//!
//! This is the program the kernel spawns as PID 1 once it reaches user mode
//! (`plans/PI.md` P6c). It is a **pure-Rust** program: RustOS is Rust-only, so `init` links the Rust userland runtime
//! `rustos-rt` — never the C ABI (`crt0` + `abi-sys`), which exists solely
//! for programs **not** written in Rust. `rustos-rt`
//! provides `_start`, the per-process stack canary, the
//! panic handler, and the syscall wrappers; `rustos_rt::entry!` names this
//! program's `main`.
//!
//! `main` parses the compiled-in `startup::DEFAULT_CONFIG`, writes the
//! first banner line to its inherited standard output (fd 1) through the
//! shared `rustos_rt::io` layer over the `abi-v1` `stream_write` syscall
//! (`init` binds to the stream, never a device), then **supervises** the
//! user's sessions: one session program per discovered text console
//! (`console_count` / `spawn_at` — the video console and the UART are
//! separate session contexts, `plans/PI.md` P11), reaped with wait-any and
//! relaunched on their own consoles ([`supervisor`]). The runtime routes
//! `main`'s return value through the `exit` syscall.
//!
//! It links **only** the runtime and its own startup-config parser, never the
//! sibling `rustos-init` orchestrator library, whose `alloc`-and-crypto
//! dependency chain has no place in a banner-printing program. That parser therefore lives alongside it in [`startup`] and is
//! host-tested there. The binary is built position-independent and converted
//! to an `rxe` blob by the consuming boot path (`plans/PI.md` P6c). On the
//! host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

mod startup;
mod supervisor;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_rt::io::{Stdout, Write};

    use crate::startup::{StartupConfig, BANNER, DEFAULT_CONFIG, MAX_SERVICES};
    use crate::supervisor::{supervise, Outcome, Sessions};

    /// Exit code when the compiled-in startup config does not parse. A
    /// reserved, fail-closed value; the default config is
    /// well-formed, so reaching this is a build defect, not a runtime input.
    const EXIT_CONFIG_INVALID: i32 = 70;

    /// Exit code when launching a session program failed — the `spawn`
    /// syscall returned a negative `-errno`. A reserved, fail-closed value distinct from [`EXIT_CONFIG_INVALID`] so the cause
    /// is unambiguous in the audit transcript.
    const EXIT_SESSION_FAILED: i32 = 71;

    /// Exit code when waiting on the sessions failed — the `wait` syscall
    /// returned a negative `-errno` (the supervisor cannot reap the children
    /// it spawned). A reserved, fail-closed value
    /// distinct from [`EXIT_SESSION_FAILED`] so the cause is unambiguous.
    const EXIT_WAIT_FAILED: i32 = 72;

    /// Exit code when no console's session could stay up: every console
    /// consumed its relaunch budget (`supervisor::SESSION_SPAWN_BUDGET`)
    /// without a session ever blocking, so the supervisor stops rather than
    /// relaunching forever.
    const EXIT_SESSION_EXHAUSTED: i32 = 73;

    /// Exit code when the kernel reports no installed console (or refuses
    /// the count): there is nothing a session could attach its standard
    /// streams to, so PID 1 reports the system unusable fail-closed rather than spawning stream-less sessions.
    const EXIT_NO_CONSOLES: i32 = 74;

    /// The production [`Sessions`] backing: the real `rustos-rt` syscall
    /// wrappers (`console_count`, the console-selecting `spawn_at`, and
    /// wait-any). Zero-sized — PID 1's supervision state lives on `main`'s
    /// stack inside [`supervise`].
    struct RtSessions;

    impl Sessions for RtSessions {
        fn console_count(&mut self) -> i64 {
            rustos_rt::console_count()
        }
        fn spawn_at(&mut self, path: &[u8], console: u32) -> i64 {
            rustos_rt::spawn_at(path, console)
        }
        fn wait_any(&mut self, status: &mut i32) -> i64 {
            rustos_rt::wait_exit(rustos_abi::WAIT_PID_ANY, status)
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Parses the compiled-in [`DEFAULT_CONFIG`], writes the startup banner to
    /// its inherited standard output (fd 1), then supervises one session per
    /// discovered text console for the lifetime of PID 1
    /// ([`supervise`] — `plans/PI.md` P11).
    ///
    /// The banner write is *gated*: `write_all` loops over benign short writes
    /// and fails closed only when the backing accepts nothing more (a missing
    /// `CAP_CONSOLE_WRITE`, an unresolved address space, an unestablished
    /// descriptor, or a closed-fail kernel path). PID 1 cannot usefully proceed
    /// without the console it was spawned to drive, so it parks fail-closed
    /// off the run queue (`rustos_rt::park_forever`) rather than supervising
    /// sessions on a console it never reached — a terminal park consuming no
    /// CPU, not a retry loop. Only when even that park is refused does it
    /// fall to the last-resort halt spin: with no console and no wait-set
    /// there is nothing left to park on or report to.
    fn main() -> i32 {
        let Ok(config) = StartupConfig::parse(DEFAULT_CONFIG) else {
            return EXIT_CONFIG_INVALID;
        };
        // The shared `rustos_rt::io` short-write loop, never an init-private
        // copy (the charter forbids that duplication).
        if Stdout.write_all(BANNER.as_bytes()).is_err() {
            // Terminal park off the run queue — a spinning halt would peg a
            // core for the life of the system. The spin below runs only when
            // even the park is refused (a doubly-failed boot: no console, no
            // wait-set), where nothing better remains.
            let _ = rustos_rt::park_forever();
            loop {
                core::hint::spin_loop();
            }
        }
        // Launch the configured long-running services (the device manager,
        // `/System/Services/devmgr.app/Run`, today) once each, then supervise them
        // alongside one login session per console for the life of PID 1
        // ([`supervise`]). `devmgr` observes the discovered hardware tree
        // and blocks reactively in `hw_tree_wait` — a **true**
        // generation-keyed park, not a busy poll: the kernel ships the
        // blocking wait-queue + scheduler wake-pending token + explicit
        // wake (Design D P-2 — `kernel/core::waitq`,
        // `RescheduleAction::Park`), and the tickless one-shot arms a
        // finite-timeout wakeup per-port (`SchedulerArch::set_wakeup`) with
        // the idle drive-loop parking on `KernelArch::wait_for_interrupt`
        // and re-stepping a woken sole waiter rather than halting, so the
        // perpetual `devmgr` parks off the run queue without starving a
        // single-CPU system.
        //
        // The service paths arrive as `&str` from the parsed config; PID 1
        // re-borrows them as bytes into a fixed stack array (no heap,
        // `plans/SPAWN.md` SP5b) bounded by `MAX_SERVICES`, which the parser
        // already enforces, so the slice is never truncated.
        let services = config.services();
        let mut service_bytes: [&[u8]; MAX_SERVICES] = [b""; MAX_SERVICES];
        for (dst, path) in service_bytes.iter_mut().zip(services) {
            *dst = path.as_bytes();
        }
        match supervise(
            &mut RtSessions,
            config.session().as_bytes(),
            &service_bytes[..services.len()],
        ) {
            Outcome::NoConsoles => EXIT_NO_CONSOLES,
            Outcome::SpawnFailed => EXIT_SESSION_FAILED,
            Outcome::WaitFailed => EXIT_WAIT_FAILED,
            Outcome::Exhausted => EXIT_SESSION_EXHAUSTED,
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
// so a malformed `DEFAULT_CONFIG` is caught by an ordinary `cargo build`,
// and drives the session supervisor against an inert zero-console seam so
// neither is dead code on the host. It performs no I/O.
/// An inert host-stub seam: zero consoles, so the supervisor returns
/// [`supervisor::Outcome::NoConsoles`] without spawning or waiting.
#[cfg(not(freestanding))]
struct NoSessions;

#[cfg(not(freestanding))]
impl supervisor::Sessions for NoSessions {
    fn console_count(&mut self) -> i64 {
        0
    }
    fn spawn_at(&mut self, _path: &[u8], _console: u32) -> i64 {
        -1
    }
    fn wait_any(&mut self, _status: &mut i32) -> i64 {
        -1
    }
}

#[cfg(not(freestanding))]
fn main() {
    if let Ok(config) = startup::StartupConfig::parse(startup::DEFAULT_CONFIG) {
        let _ = (config.session(), config.services(), startup::BANNER);
    }
    assert_eq!(
        supervisor::supervise(&mut NoSessions, b"session", &[]),
        supervisor::Outcome::NoConsoles
    );
}
