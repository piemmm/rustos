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
//! user's sessions: one session program per discovered text console
//! (`console_count` / `spawn_at` — the video console and the UART are
//! separate session contexts, `plans/PI.md` P11), reaped with wait-any and
//! relaunched on their own consoles ([`supervisor`]). The runtime routes
//! `main`'s return value through the `exit` syscall.
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
mod supervisor;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use crate::startup::{StartupConfig, BANNER, DEFAULT_CONFIG};
    use crate::supervisor::{supervise_sessions, Outcome, Sessions};

    /// Exit code when the compiled-in startup config does not parse. A
    /// reserved, fail-closed value (`AGENTS.md` §2.9); the default config is
    /// well-formed, so reaching this is a build defect, not a runtime input.
    const EXIT_CONFIG_INVALID: i32 = 70;

    /// Exit code when launching a session program failed — the `spawn`
    /// syscall returned a negative `-errno`. A reserved, fail-closed value
    /// (`AGENTS.md` §2.9) distinct from [`EXIT_CONFIG_INVALID`] so the cause
    /// is unambiguous in the audit transcript.
    const EXIT_SESSION_FAILED: i32 = 71;

    /// Exit code when waiting on the sessions failed — the `wait` syscall
    /// returned a negative `-errno` (the supervisor cannot reap the children
    /// it spawned). A reserved, fail-closed value (`AGENTS.md` §2.9)
    /// distinct from [`EXIT_SESSION_FAILED`] so the cause is unambiguous.
    const EXIT_WAIT_FAILED: i32 = 72;

    /// Exit code when no console's session could stay up: every console
    /// consumed its relaunch budget (`supervisor::SESSION_SPAWN_BUDGET`)
    /// without a session ever blocking, so the supervisor stops rather than
    /// relaunching forever (`AGENTS.md` §2.1 / §2.9).
    const EXIT_SESSION_EXHAUSTED: i32 = 73;

    /// Exit code when the kernel reports no installed console (or refuses
    /// the count): there is nothing a session could attach its standard
    /// streams to, so PID 1 reports the system unusable fail-closed
    /// (`AGENTS.md` §2.9) rather than spawning stream-less sessions.
    const EXIT_NO_CONSOLES: i32 = 74;

    /// The production [`Sessions`] backing: the real `rustos-rt` syscall
    /// wrappers (`console_count`, the console-selecting `spawn_at`, and
    /// wait-any). Zero-sized — PID 1's supervision state lives on `main`'s
    /// stack inside [`supervise_sessions`].
    struct RtSessions;

    impl Sessions for RtSessions {
        fn console_count(&mut self) -> i64 {
            rustos_rt::console_count()
        }
        fn spawn_at(&mut self, path: &[u8], console: u32) -> i64 {
            rustos_rt::spawn_at(path, console)
        }
        fn wait_any(&mut self, status: &mut i32) -> i64 {
            rustos_rt::wait(rustos_abi::WAIT_ANY, status)
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Parses the compiled-in [`DEFAULT_CONFIG`], writes the startup banner to
    /// its inherited standard output (fd 1), then supervises one session per
    /// discovered text console for the lifetime of PID 1
    /// ([`supervise_sessions`] — `plans/PI.md` P11).
    ///
    /// The banner write is *gated*: `stdout` returns the number of
    /// bytes the kernel accepted, so a short count means the write did not
    /// fully land (a missing `CAP_CONSOLE_WRITE`, an unresolved address
    /// space, an unestablished descriptor, or a closed-fail kernel path).
    /// PID 1 cannot usefully proceed without the console it was spawned to
    /// drive, so it parks fail-closed rather than supervising sessions on a
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
        // NOTE: PID 1 does **not** yet launch the device-manager service
        // (`/System/Services/devmgr`). The service blocks reactively in
        // `hw_tree_wait`, which today is a cooperative poll-and-yield (the
        // kernel ships no true park/wake — `RescheduleAction::Park` is
        // unwired); a perpetual waiter would spin and starve a single-CPU
        // system (`AGENTS.md` §2.1). Spawning it from `init` lands together
        // with the true generation-keyed park in the next tranche
        // (`.junie/next-pi-prompt.md` Design D D2b-2b "A"). The foundation
        // (the `hw_tree_read`/`hw_tree_wait` syscalls, the store generation
        // counter, and the signed `devmgr` binary) is proven by
        // `rustos-test-devmgr-hwtree-qemu-aarch64` until then.
        match supervise_sessions(&mut RtSessions, config.session().as_bytes()) {
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
        let _ = (config.session(), startup::BANNER);
    }
    assert_eq!(
        supervisor::supervise_sessions(&mut NoSessions, b"session"),
        supervisor::Outcome::NoConsoles
    );
}
