//! The `Run` entry-point binary of the `reset` tool — the program a shell
//! spawns to restore a wedged terminal to a sane state.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the Rust
//! userland runtime `rustos-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `rustos-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `rustos_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector (the reserved `-h`/`-?`
//! short-help switches render the tool's own Help document through the
//! shared engine and exit), restores the **cooked** input discipline (a
//! crashed full-screen program may have left the console raw, with neither
//! echo nor indicator), resolves the terminal's capabilities from the
//! inherited `TERM` (fail-closed: unknown degrades to the dumb baseline),
//! and writes the encoded restoration sequence to standard output (fd 1).
//! The tool binds only to its inherited descriptors, never a console
//! device.
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

    use rustos_abi::InputMode;
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_reset::{parse, reset_bytes, Command, USAGE};
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_termcap::from_term;

    /// Render `reset`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the tool's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("reset"), locale, "reset")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` when the terminal was restored (or short help
    /// served), `1` when the output could not be delivered, `2` on a usage
    /// error.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        match parse(&arguments) {
            Ok(Command::Run) => {}
            Ok(Command::Help) => return short_help(),
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        }

        // Restore the cooked input discipline first: a crashed full-screen
        // program may have left the console raw, and the user typing this
        // very command could not see it. Best-effort — a stream-fed session
        // (a pipe) has no console discipline to restore.
        let _ = rustos_rt::set_input_mode(InputMode::Cooked);

        // The terminal's capabilities come from the inherited `TERM`
        // (fail-closed: unknown or absent degrades to the dumb baseline
        // inside `from_term`), never a hard-coded terminal model. The dumb
        // baseline yields an empty sequence — the discipline restore above
        // is the whole reset for a terminal with no controls.
        let term = rustos_rt::env_var(b"TERM")
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .map_or(rustos_termcap::TermType::Dumb, from_term);
        let bytes = reset_bytes(&term.capabilities());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
