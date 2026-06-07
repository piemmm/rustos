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
//! `main` parses the compiled-in `startup::DEFAULT_CONFIG` and, when it asks
//! for the console, writes the first banner line through the `abi-v1`
//! `console_write` syscall (`rustos_rt::console_write`, the P6a syscall).
//! The runtime routes `main`'s return value through the `exit` syscall.
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

    /// Exit code for a clean run: the config parsed and the banner was written.
    const EXIT_OK: i32 = 0;

    /// Exit code when the compiled-in startup config does not parse. A
    /// reserved, fail-closed value (`AGENTS.md` §2.9); the default config is
    /// well-formed, so reaching this is a build defect, not a runtime input.
    const EXIT_CONFIG_INVALID: i32 = 70;

    /// Exit code when launching the session program failed — the `spawn`
    /// syscall returned a negative `-errno`. A reserved, fail-closed value
    /// (`AGENTS.md` §2.9) distinct from [`EXIT_CONFIG_INVALID`] so the cause
    /// is unambiguous in the audit transcript.
    const EXIT_SESSION_FAILED: i32 = 71;

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Parses the compiled-in [`DEFAULT_CONFIG`], writes the startup banner to
    /// the system console, and returns [`EXIT_OK`].
    ///
    /// The banner write is *gated*: `console_write` returns the number of
    /// bytes the kernel accepted, so a short count means the privileged
    /// console write did not fully land (a missing `CAP_CONSOLE_WRITE`, an
    /// unresolved address space, or a closed-fail kernel path). PID 1 cannot
    /// usefully proceed without the console it was spawned to drive and has
    /// no session path to launch yet (P6d/P6e), so it parks fail-closed
    /// rather than exiting "successfully" on a console it never reached
    /// (`AGENTS.md` §2.9). This is a terminal park, not a retry loop
    /// (`AGENTS.md` §2.1).
    ///
    /// The config also names the session program `init` launches: after the
    /// banner lands, PID 1 spawns it through the `spawn` syscall
    /// (`plans/SPAWN.md` SP3) as a separate, hardware-isolated process that
    /// runs concurrently — a true spawn, not an `exec`-style hand-off, so
    /// PID 1 keeps running. `spawn` returns the child's PID (`≥ 0`) or a
    /// negative `-errno`; a failed spawn is fail-loud, not ignored
    /// ([`EXIT_SESSION_FAILED`], `AGENTS.md` §2.9). Supervising the session
    /// across its lifetime (restart, reap) is `plans/PI.md` P6e.
    fn main() -> i32 {
        let Ok(config) = StartupConfig::parse(DEFAULT_CONFIG) else {
            return EXIT_CONFIG_INVALID;
        };
        let banner = BANNER.as_bytes();
        if rustos_rt::console_write(banner) != banner.len() {
            loop {
                core::hint::spin_loop();
            }
        }
        // Launch the user's session as a concurrent, isolated process. A
        // negative result is a failed spawn (an unknown path, an unwired
        // spawn subsystem, a build failure); surface it as a distinct,
        // fail-closed exit code rather than pretending the system came up.
        if rustos_rt::spawn(config.session().as_bytes()) < 0 {
            return EXIT_SESSION_FAILED;
        }
        EXIT_OK
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
