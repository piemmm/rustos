//! The `Run` entry-point binary of the `link` tool — the program a shell
//! spawns to give a file a second name.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, parses it, and creates the
//! one hard link through `fs_link` under the caller's attested identity. The
//! empty flag word is POSIX `link()`: neither final component is followed,
//! so the node that gains a name is the one the caller spelled and an
//! occupied new name is refused rather than replaced. The reserved
//! `-?`/`--help` switches render the tool's own Help document through the
//! shared engine. The tool binds only to its inherited descriptors, never a
//! console device.
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

    use tairix_abi::{Errno, LinkFlags};
    use tairix_help::BundleHelp;
    use tairix_link::{parse, run, Filesystem, Output, USAGE};
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};

    /// The production [`Filesystem`] over `fs_link`.
    ///
    /// The empty flag word is POSIX `link()`: the existing name's own final
    /// symbolic link is not resolved, so the node that gains a name is the
    /// one the caller spelled. `ln -L` is the tool for the other posture.
    struct RtFilesystem;

    impl Filesystem for RtFilesystem {
        fn link(&self, existing: &str, new: &str) -> Result<(), Errno> {
            let ret = tairix_rt::fs_link(existing.as_bytes(), new.as_bytes(), LinkFlags::empty());
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
    /// option, or anything other than exactly two operands).
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
            &BundleHelp::new("link"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("link: {err}"));
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
