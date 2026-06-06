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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Parses the compiled-in [`DEFAULT_CONFIG`], writes the startup banner to
    /// the system console, and returns [`EXIT_OK`].
    ///
    /// The config also names the session program `init` will launch; spawning
    /// it needs the process-spawn syscall (`plans/PI.md` P6d) and a shell
    /// (P6e), neither of which exists yet. Until then `init`'s P6b job is to
    /// reach user mode and prove the console write path, so the session path
    /// is only validated as parsed, not launched.
    fn main() -> i32 {
        let config = match StartupConfig::parse(DEFAULT_CONFIG) {
            Ok(config) => config,
            Err(_) => return EXIT_CONFIG_INVALID,
        };
        let _ = rustos_rt::console_write(BANNER.as_bytes());
        // P6d/P6e will spawn this program as the user's session; for now its
        // presence is what the parse guarantees.
        let _session = config.session();
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
