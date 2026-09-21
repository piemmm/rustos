//! The `Run` entry-point binary of the `groupdel` tool — the program an
//! administrator's shell spawns to delete a group.
//!
//! A **pure-Rust** program linking the Rust userland runtime `tairix-rt`
//! for `_start`, the per-process stack canary, the panic handler, the
//! `mem_map`-backed global allocator, and the syscall wrappers.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment, and runs the parsed command
//! against the production seams: the `users_admin` syscall channel (every
//! capability and record decision stays kernel-side under the caller's
//! attested identity — without `CAP_USER_ADMIN` the operation is refused
//! at dispatch), the shared `tairix_help::BundleHelp` over the tool's own
//! bundle `Help/` tree, and the inherited standard output. The tool binds
//! only to its inherited descriptors, never a console device, and holds
//! no ambient authority.
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

    use tairix_abi::Errno;
    use tairix_groupdel::{parse, run, Output, USAGE};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use tairix_useradmin::AdminChannel;

    /// The production [`AdminChannel`]: the `users_admin` syscall. It adds
    /// no authority — every rule stays kernel-side, and a refusal surfaces
    /// as the exact [`Errno`] the kernel chose.
    struct RtChannel;

    impl AdminChannel for RtChannel {
        fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::users_admin(req, out).map_err(Errno::from_syscall)
        }
    }

    /// The production [`Output`] over the inherited standard output.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Write the usage banner to fd 2 byte-exact (it carries its own
    /// trailing newline), best-effort on the already-failing path.
    fn report_usage() {
        let _ = Stderr.write_all(USAGE.as_bytes());
    }

    /// Program entry point.
    ///
    /// Exit codes: `0` on success, `1` on a refused or failed operation,
    /// `2` on a usage error.
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            report_usage();
            return 2;
        };
        let Ok(command) = parse(&arguments) else {
            report_usage();
            return 2;
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        match run(
            command,
            locale,
            &RtChannel,
            &BundleHelp::new("groupdel"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("groupdel: {err}"));
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
