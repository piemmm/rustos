//! The `Run` entry-point binary of the `unlink` tool — the program a shell
//! spawns to remove one name.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, parses it, and removes the
//! one operand through `fs_unlink` under the caller's attested identity. The
//! empty flag word asks for a **non-directory** removal, so the kernel
//! refuses a directory in the same locked walk that would have removed the
//! entry — this program never races a stat against a removal, and never
//! follows a link to decide what to remove. The reserved `-?`/`--help`
//! switches render the tool's own Help document through the shared engine.
//! The tool binds only to its inherited descriptors, never a console device.
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

    use tairix_abi::{Errno, UnlinkFlags};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use tairix_unlink::{parse, run, Filesystem, Output, USAGE};

    /// The production [`Filesystem`] over `fs_unlink`.
    ///
    /// The empty flag word is the name-removal posture: a directory is
    /// refused by the kernel, and the final component is never followed, so
    /// a symbolic link is removed itself rather than its target.
    struct RtFilesystem;

    impl Filesystem for RtFilesystem {
        fn unlink(&self, path: &str) -> Result<(), Errno> {
            let ret = tairix_rt::fs_unlink(path.as_bytes(), UnlinkFlags::empty());
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
        }
    }

    /// The production [`Output`] over the inherited standard output (fd 1).
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning. The io layer's error
            // carries no errno, so it collapses onto the same code the
            // kernel uses where abi-v1 has no dedicated one.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Report a usage error: the banner on the standard error stream,
    /// verbatim (it already ends in a newline).
    fn report_usage() {
        let _ = Stderr.write_all(USAGE.as_bytes());
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a filesystem or output failure,
    /// `2` on a usage error (a malformed argument vector, an unrecognised
    /// option, or anything other than exactly one operand).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            report_usage();
            return 2;
        };
        let Ok(command) = parse(&arguments) else {
            report_usage();
            return 2;
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &RtFilesystem,
            &BundleHelp::new("unlink"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("unlink: {err}"));
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
