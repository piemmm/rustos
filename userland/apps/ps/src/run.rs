//! The `Run` entry-point binary of the `ps` tool — the program a shell spawns
//! to list processes through the System Information API.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5), parses the
//! arguments with the pure [`tairix_ps`] grammar, and runs the resulting
//! command against the production seams: `IpcTransport` (shared through
//! `lib/procinfo`), which carries the framed `sysinfo-v1` request to
//! `/System/Services/sysinfod.app/Run` over the well-known IPC call
//! endpoint, the shared `tairix_help::BundleHelp`, which reads the tool's
//! own bundle's `Help/` tree for the short-help switches, and `RtOutput`,
//! which writes each rendered row to the inherited standard output (fd 1).
//! The tool binds only to its inherited descriptors, never a console
//! device, and holds no ambient authority: `sysinfod` gates every query
//! against the caller's kernel-attested origin.
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

    use tairix_help::BundleHelp;
    use tairix_procinfo::{IpcTransport, RtOutput};
    use tairix_ps::{parse, run, USAGE};
    use tairix_rt::args;
    use tairix_rt::io::write_stderr_line;

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a service or output failure, `2` on
    /// a usage error (a malformed argument vector or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let transport = IpcTransport;
        let out = RtOutput;
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(command, locale, &transport, &BundleHelp::new("ps"), &out) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("ps: {err}"));
                1
            }
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
