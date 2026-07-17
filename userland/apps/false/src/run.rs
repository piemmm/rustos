//! The `Run` entry-point binary of the `false` tool — the program a shell
//! spawns to do nothing, unsuccessfully.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector and exits `1`, ignoring
//! every argument exactly as GNU `false` does. The one exception is a first
//! argument of `-h`/`-?`/`--help` (the reserved short-help switches), which
//! renders the tool's own Help document through the shared engine and — a
//! documented divergence from GNU `false --help` — exits `0`, per the
//! TAIRiX short-help convention. The tool binds only to its inherited
//! descriptors, never a console device.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use tairix_false::{parse, Command, USAGE};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_rt::io::{Stdout, Write};

    /// Render `false`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the tool's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("false"), locale, "false")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `1` always (the tool's whole purpose), except `0` when a
    /// requested short help was served (the TAIRiX short-help convention; a
    /// documented divergence from GNU `false --help`).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is ignored like any other
        // argument: GNU `false` has no failure mode beyond its own status.
        let Some(arguments) = tairix_rt::args() else {
            return 1;
        };
        match parse(&arguments) {
            Command::Fail => 1,
            Command::Help => short_help(),
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
