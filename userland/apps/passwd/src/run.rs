//! The `Run` entry-point binary of the `passwd` tool — the program an
//! administrator's shell spawns to replace an account's password.
//!
//! A **pure-Rust** program linking the Rust userland runtime `tairix-rt`
//! for `_start`, the per-process stack canary, the panic handler, the
//! `mem_map`-backed global allocator, and the syscall wrappers.
//!
//! `main` binds four production seams: the `users_admin` syscall channel
//! (every decision stays kernel-side under the caller's attested
//! identity), the inherited standard streams with echo switched off
//! around the password prompts — never a console device — a salt drawn
//! from the kernel CSPRNG through the unprivileged `sys:random` resource,
//! and the shared `tairix_help::BundleHelp` over the tool's own bundle
//! `Help/` tree.
//!
//! A caller with no terminal — the desktop, running this under the
//! supervisor's elevated seam with standard input closed — hands over a
//! ready salted record with `--record`, so the prompting seams are never
//! reached and no plaintext exists on either side.
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

    use alloc::format;
    use alloc::vec::Vec;

    use tairix_abi::{Errno, InputMode, OpenFlags};
    use tairix_help::BundleHelp;
    use tairix_passwd::{parse, run, Output, SaltSource, Terminal, USAGE};
    use tairix_rt::io::{write_stderr_line, Read, Stderr, Stdin, Stdout, Write};
    use tairix_useradmin::AdminChannel;
    use tairix_users::{Salt, SALT_LEN};

    /// The production [`AdminChannel`]: the `users_admin` syscall.
    struct RtChannel;

    impl AdminChannel for RtChannel {
        fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::users_admin(req, out).map_err(Errno::from_syscall)
        }
    }

    /// The production [`Terminal`]: prompts on fd 1, the secret read from
    /// fd 0 with echo off and the cooked default restored either way.
    struct RtTerminal;

    impl Terminal for RtTerminal {
        fn read_secret(&self, prompt: &str) -> Option<Vec<u8>> {
            let _ = Stdout.write_all(prompt.as_bytes());
            let _ = tairix_rt::set_input_mode(InputMode::Secret);
            let mut raw = Vec::new();
            let ok = loop {
                let mut byte = [0u8; 1];
                match Stdin.read(&mut byte).unwrap_or(0) {
                    0 => break !raw.is_empty(),
                    _ => match byte[0] {
                        b'\n' => break true,
                        b'\r' => {}
                        other => raw.push(other),
                    },
                }
            };
            let _ = tairix_rt::set_input_mode(InputMode::Cooked);
            // The operator cannot see the newline they typed, so it is
            // supplied explicitly.
            let _ = Stdout.write_all(b"\n");
            if ok {
                Some(raw)
            } else {
                raw.fill(0);
                None
            }
        }
    }

    /// A salt drawn from the kernel CSPRNG through the unprivileged
    /// `sys:random` resource; refuses, never guesses, when the draw fails.
    struct RtSalt;

    impl SaltSource for RtSalt {
        fn salt(&self) -> Option<Salt> {
            let fd =
                u32::try_from(tairix_rt::resource_open(b"sys:random", OpenFlags::READ)).ok()?;
            let mut salt = [0u8; SALT_LEN];
            let outcome = tairix_rt::fs_read(fd, 0, &mut salt);
            let _ = tairix_rt::fs_close(fd);
            match outcome {
                Ok(read) if read == SALT_LEN => Some(salt),
                _ => None,
            }
        }
    }

    /// The production [`Output`] over the inherited standard output.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Program entry point.
    ///
    /// Exit codes: `0` on success, `1` on a refused or failed
    /// replacement, `2` on a usage error.
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            let _ = Stderr.write_all(USAGE.as_bytes());
            return 2;
        };
        let Ok(command) = parse(&arguments) else {
            let _ = Stderr.write_all(USAGE.as_bytes());
            return 2;
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        match run(
            command,
            locale,
            &RtChannel,
            &BundleHelp::new("passwd"),
            &RtTerminal,
            &RtSalt,
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("passwd: {err}"));
                1
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host the freestanding `_start` path is not compiled, so this
// inert `main` keeps the crate building under the host tooling.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
