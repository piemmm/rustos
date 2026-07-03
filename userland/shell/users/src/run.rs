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
//! `main` binds the session library's three seams to production: the
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
    use rustos_users::{Salt, SALT_LEN};
    use rustos_users_cli::{run_session, AdminChannel, SaltSource, SessionConfig, ToolIo};

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
            rustos_rt::stdout(line.as_bytes());
            rustos_rt::stdout(b"\n");
        }

        fn error_line(&mut self, line: &str) {
            rustos_rt::stderr(line.as_bytes());
            rustos_rt::stderr(b"\n");
        }

        fn read_line(&mut self, prompt: &str) -> Option<String> {
            rustos_rt::stdout(prompt.as_bytes());
            let mut raw = Vec::new();
            if !Self::read_raw_line(&mut raw) {
                return None;
            }
            String::from_utf8(raw).ok()
        }

        fn read_secret(&mut self, prompt: &str) -> Option<Vec<u8>> {
            rustos_rt::stdout(prompt.as_bytes());
            // Echo off for the secret; restored regardless of outcome,
            // and the terminal newline the operator cannot see is
            // supplied explicitly.
            let _ = rustos_rt::set_echo(false);
            let mut raw = Vec::new();
            let ok = Self::read_raw_line(&mut raw);
            let _ = rustos_rt::set_echo(true);
            rustos_rt::stdout(b"\n");
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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    fn main() -> i32 {
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
