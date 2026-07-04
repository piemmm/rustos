//! The `Run` entry-point binary of the `users` tool — the interactive
//! account-administration client an administrator's shell spawns
//! (`plans/CAPABILITY_USE.md` CU4).
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the
//! Rust userland runtime `rustos-rt` — never the C ABI, which exists
//! solely for programs *not* written in Rust. `rustos-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `mem_map`-backed global allocator, and the syscall wrappers;
//! `rustos_rt::entry!` names this program's `main`.
//!
//! `main` collects the inherited argument vector (the reserved `-h`/`-?`
//! short-help switches render the tool's own Help document through the
//! shared engine and exit, plans/APPS.md §4; anything else on the command
//! line is a usage error — accounts are administered with commands typed
//! inside the session). It then binds the session library's three seams to
//! production: the
//! inherited standard streams (echo toggled off for password prompts,
//! never a console device), the `users_admin` syscall wrapper (every
//! decision stays kernel-side under the caller's attested identity), and
//! a salt drawn from the kernel CSPRNG through the unprivileged
//! `sys:random` resource. The tool holds no ambient authority: without
//! `CAP_USER_ADMIN` in the account's ceiling every operation is refused
//! at dispatch.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::string::String;
    use alloc::vec::Vec;

    use rustos_abi::OpenFlags;
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_users::{Salt, SALT_LEN};
    use rustos_users_cli::{
        parse, run_session, AdminChannel, Command, SaltSource, SessionConfig, ToolIo, USAGE,
    };

    /// The inherited-standard-stream terminal: prompts on fd 1, lines
    /// from fd 0, errors on fd 2; echo switched off around secrets.
    struct RtIo;

    impl RtIo {
        /// Read one newline-terminated line from fd 0 into `out`,
        /// returning `false` on end-of-input. Carriage returns are
        /// stripped; the newline is not stored.
        fn read_raw_line(out: &mut Vec<u8>) -> bool {
            loop {
                let mut byte = [0u8; 1];
                let read = rustos_rt::stdin(&mut byte);
                if read == 0 {
                    return !out.is_empty();
                }
                match byte[0] {
                    b'\n' => return true,
                    b'\r' => {}
                    other => out.push(other),
                }
            }
        }
    }

    impl ToolIo for RtIo {
        fn write_line(&mut self, line: &str) {
            // Terminal output is best-effort: a dropped tail must not abort
            // the session, so the accepted counts are discarded.
            let _ = rustos_rt::stdout(line.as_bytes());
            let _ = rustos_rt::stdout(b"\n");
        }

        fn error_line(&mut self, line: &str) {
            let _ = rustos_rt::stderr(line.as_bytes());
            let _ = rustos_rt::stderr(b"\n");
        }

        fn read_line(&mut self, prompt: &str) -> Option<String> {
            let _ = rustos_rt::stdout(prompt.as_bytes());
            let mut raw = Vec::new();
            if !Self::read_raw_line(&mut raw) {
                return None;
            }
            String::from_utf8(raw).ok()
        }

        fn read_secret(&mut self, prompt: &str) -> Option<Vec<u8>> {
            let _ = rustos_rt::stdout(prompt.as_bytes());
            // Echo off for the secret; restored regardless of outcome,
            // and the terminal newline the operator cannot see is
            // supplied explicitly.
            let _ = rustos_rt::set_echo(false);
            let mut raw = Vec::new();
            let ok = Self::read_raw_line(&mut raw);
            let _ = rustos_rt::set_echo(true);
            let _ = rustos_rt::stdout(b"\n");
            if ok {
                Some(raw)
            } else {
                raw.fill(0);
                None
            }
        }
    }

    /// The production `users_admin` syscall channel.
    struct RtChannel;

    impl AdminChannel for RtChannel {
        fn call(&mut self, req: &[u8], out: &mut [u8]) -> Result<usize, i64> {
            rustos_rt::users_admin(req, out)
        }
    }

    /// A salt drawn from the kernel CSPRNG through the unprivileged
    /// `sys:random` resource; refuses (never guesses) when the draw
    /// fails.
    struct RtSalt;

    impl SaltSource for RtSalt {
        fn salt(&mut self) -> Option<Salt> {
            let fd = rustos_rt::resource_open(b"sys:random", OpenFlags::READ);
            let fd = u32::try_from(fd).ok()?;
            let mut salt = [0u8; SALT_LEN];
            let outcome = rustos_rt::fs_read(fd, 0, &mut salt);
            let _ = rustos_rt::fs_close(fd);
            match outcome {
                Ok(read) if read == SALT_LEN => Some(salt),
                _ => None,
            }
        }
    }

    /// Render `users`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the tool's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("users"), locale, "users")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    ///
    /// Exit codes: the session's own code on a normal run (`0` for a clean
    /// exit, or the served short help), `2` on a usage error (a malformed
    /// argument vector or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error,
        // reported rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        match parse(&arguments) {
            Ok(Command::Session) => {}
            Ok(Command::Help) => return short_help(),
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        }
        run_session(
            &mut RtIo,
            &mut RtChannel,
            &mut RtSalt,
            SessionConfig::default(),
        )
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
