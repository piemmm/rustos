//! The `Run` entry-point binary of the `servicectl` tool — the control
//! client an administrator's shell spawns (`plans/NEW-SERVICEMANAGER.md`
//! SVC-8).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` parses the inherited argument vector and binds the behaviour
//! library's two seams to production: the service-control endpoint through
//! `ipc_call`, and the inherited standard streams. The tool holds no ambient
//! authority — reaching the endpoint *is* the authority, and the kernel
//! refuses the call outright for an account whose ceiling does not carry
//! `CAP_SERVICE_CONTROL`.
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

    use tairix_abi::service_control::SERVICE_CONTROL_ENDPOINT;
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_servicectl::{dispatch, parse, report_usage, ControlChannel, Exit, ToolIo, USAGE};

    /// The production control channel: one synchronous call to the service
    /// manager's reserved endpoint.
    struct RtChannel;

    impl ControlChannel for RtChannel {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, i64> {
            tairix_rt::ipc_call(SERVICE_CONTROL_ENDPOINT, request, reply)
        }
    }

    /// The inherited standard streams: the outcome on fd 1, every diagnosis
    /// on fd 2.
    struct RtIo;

    impl ToolIo for RtIo {
        fn write_line(&mut self, line: &str) {
            // Best-effort: a dropped tail must not change the exit status the
            // manager's answer already decided.
            let _ = Stdout.write_all(line.as_bytes());
            let _ = Stdout.write_all(b"\n");
        }
        fn write_error(&mut self, line: &str) {
            write_stderr_line(line);
        }
    }

    /// Render `servicectl`'s own short help from its own bundle's `Help/`
    /// tree through the one shared engine; when no document can be served the
    /// usage banner stands in — the tool's own text, never fabricated help
    /// content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("servicectl"), locale, "servicectl")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => Exit::Ok.code(),
            Err(_) => Exit::Failed.code(),
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes are the coreutils shape: `0` applied (or the short help
    /// shown), `1` refused, `2` a command line that was not understood.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line("servicectl: the argument vector is not valid UTF-8");
            write_stderr_line(USAGE);
            return Exit::Usage.code();
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => return report_usage(&mut RtIo, err).code(),
        };
        // `dispatch` returns `None` for the help switch, whose text is this
        // bundle's own to read.
        match dispatch(&mut RtChannel, &mut RtIo, command) {
            Some(exit) => exit.code(),
            None => short_help(),
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
