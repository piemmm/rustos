//! The `Run` entry-point binary of the `ss` tool — the program a shell
//! spawns to list the system's open sockets.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5 — the shell
//! exports it; the tool invents no second source), and runs the parsed
//! command against the production seams: the shared
//! `tairix_procinfo::IpcTransport` for the `sysinfo-v1` `NET_SOCKETS` query,
//! the shared `tairix_help::BundleHelp` for the short-help switches, and
//! `RtOutput`/`RtErrors`, which write the table to the inherited standard
//! output (with the omission advisory on fd 3, best-effort) and the
//! diagnostics to standard error. The tool binds only to its inherited
//! descriptors, never a console device, and holds no ambient authority.
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

    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_procinfo::IpcTransport;
    use tairix_rt::io::{write_stderr_line, StdInfo, Stderr, Stdout, Write};
    use tairix_ss::{parse, run, Output, USAGE};

    /// The production standard-output stream: the table goes to fd 1 and
    /// the omission advisory to fd 3 (best-effort). The tool names only
    /// descriptors its spawner chose, so the same binary drives a serial
    /// terminal, a framebuffer console, or a future windowed terminal
    /// unchanged.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }

        fn info(&self, record: &[u8]) {
            // fd 3 is ignorable by contract: unattached is a no-op and a
            // short write is never an error a report depends on.
            let _ = StdInfo.write_all(record);
        }
    }

    /// The production standard-error stream: diagnostics go to fd 2,
    /// keeping the table on fd 1 clean for pipes.
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` when the query or output failed, `2`
    /// on a usage error (a malformed argument vector or an unrecognised
    /// option).
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("ss: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        match run(
            command,
            locale,
            &IpcTransport,
            &BundleHelp::new("ss"),
            &RtOutput,
            &RtErrors,
        ) {
            Ok(_) => 0,
            Err(err) => {
                write_stderr_line(&format!("ss: {err}"));
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
