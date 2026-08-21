//! The `Run` entry-point binary of the `readlink` tool — the program a
//! shell spawns to print a symbolic link's target.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, parses it, and reads each
//! operand's stored target through `fs_readlink` — or, under `-f`/`-e`/`-m`,
//! its canonical path through `fs_realpath` — under the caller's attested
//! identity. Without a canonicalisation switch the final component is never
//! followed, so the answer is the link's own target rather than a resolution
//! of it; with one, the kernel's own resolution answers. The reserved
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
    use alloc::string::String;

    use tairix_abi::fs::{FS_PATH_MAX, FS_SYMLINK_MAX};
    use tairix_abi::{Errno, RealpathMode};
    use tairix_help::BundleHelp;
    use tairix_readlink::{parse, run, Filesystem, Output, USAGE};
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};

    /// The production [`Filesystem`] over `fs_readlink` and `fs_realpath`.
    ///
    /// Each buffer is the ABI's own bound for what it receives, so one call
    /// always suffices and no growth loop is needed: neither a stored target
    /// nor a canonical path can be longer.
    struct RtFilesystem;

    impl Filesystem for RtFilesystem {
        fn read_link(&self, path: &str) -> Result<String, Errno> {
            let mut buf = alloc::vec![0u8; FS_SYMLINK_MAX];
            let ret = tairix_rt::fs_readlink(path.as_bytes(), &mut buf);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            let len = usize::try_from(ret).map_err(|_| Errno::OutOfRange)?;
            let bytes = buf.get(..len).ok_or(Errno::OutOfRange)?;
            // A target is stored as bytes; the ABI contract makes it UTF-8,
            // and a target that is not is a corrupt or hostile record —
            // refused rather than printed as replacement characters.
            let target = core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)?;
            Ok(String::from(target))
        }

        fn realpath(&self, path: &str, mode: RealpathMode) -> Result<String, Errno> {
            let mut buf = alloc::vec![0u8; FS_PATH_MAX];
            let ret = tairix_rt::fs_realpath(path.as_bytes(), &mut buf, mode);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            let len = usize::try_from(ret).map_err(|_| Errno::OutOfRange)?;
            let bytes = buf.get(..len).ok_or(Errno::OutOfRange)?;
            let canonical = core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)?;
            Ok(String::from(canonical))
        }
    }

    /// The production [`Output`] over the inherited standard output (fd 1):
    /// the printed targets.
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

    /// The production diagnostics stream (fd 2), keeping the targets on fd 1
    /// clean for pipes.
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` when every target was printed, `1` when a read was
    /// refused or the output failed, `2` on a usage error (a malformed
    /// argument vector, or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            let _ = Stderr.write_all(USAGE.as_bytes());
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("readlink: {err}"));
                let _ = Stderr.write_all(USAGE.as_bytes());
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &RtFilesystem,
            &BundleHelp::new("readlink"),
            &RtOutput,
            &RtErrors,
        ) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(err) => {
                write_stderr_line(&format!("readlink: {err}"));
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
