//! The `Run` entry-point binary of the `useradd` tool — the program an
//! administrator's shell spawns to create a user account.
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
//! command against the production seams: the `users_admin` syscall channel
//! (every capability and record decision stays kernel-side under the
//! caller's attested identity — without `CAP_USER_ADMIN` the creation is
//! refused at dispatch), the kernel CSPRNG through the unprivileged
//! `sys:random` resource (the salt and throwaway secret behind the created
//! account's unusable password record), the shared
//! `tairix_help::BundleHelp`, which reads the tool's own bundle's `Help/`
//! tree for the short-help switches, and the inherited standard output.
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

    use tairix_abi::{Errno, OpenFlags};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use tairix_useradd::db::{AdminChannel, Entropy, UsersAdminDb};
    use tairix_useradd::{parse, run, Output, USAGE};

    /// The production [`AdminChannel`]: the `users_admin` syscall. It adds
    /// no authority — the capability check and every record rule stay
    /// kernel-side, and a refusal surfaces as the exact [`Errno`] the
    /// kernel chose.
    struct RtChannel;

    impl AdminChannel for RtChannel {
        fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::users_admin(req, out).map_err(Errno::from_syscall)
        }
    }

    /// The production [`Entropy`]: the kernel CSPRNG through the
    /// unprivileged `sys:random` resource; refuses (never guesses) when
    /// the draw comes up short.
    struct RtEntropy;

    impl Entropy for RtEntropy {
        fn fill(&self, buf: &mut [u8]) -> Result<(), Errno> {
            let fd = tairix_rt::resource_open(b"sys:random", OpenFlags::READ);
            let fd = u32::try_from(fd).map_err(|_| Errno::NotImplemented)?;
            let outcome = tairix_rt::fs_read(fd, 0, buf);
            let _ = tairix_rt::fs_close(fd);
            match outcome {
                Ok(read) if read == buf.len() => Ok(()),
                Ok(_) => Err(Errno::EntropyNotReady),
                Err(ret) => Err(Errno::from_syscall(ret)),
            }
        }
    }

    /// The production [`Output`] over the inherited standard output (fd 1).
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Write the multi-line usage banner to fd 2 byte-exact (it carries its
    /// own trailing newline), best-effort on the already-failing path.
    fn report_usage() {
        let _ = Stderr.write_all(USAGE.as_bytes());
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a database, entropy, or output
    /// failure (including a refused creation), `2` on a usage error (a
    /// malformed argument vector, an unrecognised option, a bad id, or a
    /// bad name).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            report_usage();
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(_) => {
                report_usage();
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let db = UsersAdminDb::new(&RtChannel, &RtEntropy);
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(command, locale, &db, &BundleHelp::new("useradd"), &RtOutput) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("useradd: {err}"));
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
