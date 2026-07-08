//! The `Run` entry-point binary of the `printf` tool — the program a
//! shell spawns to format and print data.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the Rust
//! userland runtime `rustos-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `rustos-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `rustos_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5 — the shell
//! exports it; the tool invents no second source), and runs the parsed
//! command against the production seams: the inherited standard output
//! (fd 1) for the formatted data, standard error (fd 2) for diagnostics,
//! and the shared `rustos_help::BundleHelp` for the short-help switches.
//! The tool binds only to its inherited descriptors, never a console
//! device, and holds no ambient authority.
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

    use alloc::format;

    use rustos_help::BundleHelp;
    use rustos_printf::{parse, run, Output, PrintfError, USAGE};
    use rustos_rt::io::{write_stderr_line, Stderr, Stdout, Write};

    /// The production [`Output`] over the inherited standard output (fd 1).
    struct RtStdout;

    impl Output for RtStdout {
        fn write_all(&self, bytes: &[u8]) -> Result<(), PrintfError> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| PrintfError::Output)
        }
    }

    /// The production [`Output`] over the inherited standard error (fd 2).
    struct RtStderr;

    impl Output for RtStderr {
        fn write_all(&self, bytes: &[u8]) -> Result<(), PrintfError> {
            Stderr.write_all(bytes).map_err(|_| PrintfError::Output)
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes follow GNU `printf`: `0` when everything printed; `1`
    /// for a conversion diagnostic, an invalid conversion specification,
    /// a malformed escape, a missing FORMAT, or a dead output stream.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 1;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("printf: {err}"));
                write_stderr_line(USAGE);
                return 1;
            }
        };
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &BundleHelp::new("printf"),
            &RtStdout,
            &RtStderr,
        ) {
            Ok(status) => i32::from(status),
            Err(PrintfError::Output) => {
                write_stderr_line("printf: write error");
                1
            }
            Err(err) => {
                // A fatal template failure (invalid conversion
                // specification, malformed escape), GNU's way: diagnose
                // and exit 1; anything already rendered was written.
                write_stderr_line(&format!("printf: {err}"));
                1
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
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
