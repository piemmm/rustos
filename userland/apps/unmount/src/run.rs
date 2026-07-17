//! The `Run` entry-point binary of the `unmount` tool — the program a
//! shell spawns to detach a runtime-attached volume.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the
//! Rust userland runtime `tairix-rt` — never the C ABI, which exists
//! solely for programs *not* written in Rust. `tairix-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `mem_map`-backed global allocator, and the syscall wrappers;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5 — the
//! shell exports it; the tool invents no second source), and runs the
//! parsed command against the production seams: the shared
//! `tairix_procinfo::IpcTransport` for the `sysinfo-v1` `MOUNT_LIST`
//! query, `RtDetacher`, which encodes the `VolumeDetachRequest` and
//! issues the `volume_detach` syscall (the kernel checks `CAP_FS_MOUNT`
//! and audits every decision — no authority lives here), the shared
//! `tairix_help::BundleHelp` for the short-help switches, and
//! `RtOutput`/`RtErrors`, which write to the inherited standard streams
//! (with the `--force` advisory on fd 3, best-effort). The tool binds
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

    use tairix_abi::volume::{VolumeDetachRequest, VOLUME_DETACH_LEN, VOLUME_ID_LEN};
    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_procinfo::IpcTransport;
    use tairix_rt::io::{write_stderr_line, StdInfo, Stderr, Stdout, Write};
    use tairix_unmount::{parse, run, Detacher, Output, UnmountError, USAGE};

    /// The production [`Detacher`]: encode the detach frame and issue the
    /// `volume_detach` syscall. It adds no authority — the kernel
    /// verifies `CAP_FS_MOUNT`, re-validates the identity against the
    /// attached volumes, and audits the decision; a refusal surfaces as
    /// the exact [`Errno`] the kernel chose.
    struct RtDetacher;

    impl Detacher for RtDetacher {
        fn detach(&self, volume_id: [u8; VOLUME_ID_LEN], force: bool) -> Result<(), Errno> {
            let request = VolumeDetachRequest { volume_id, force };
            let mut frame = [0u8; VOLUME_DETACH_LEN];
            let len = request.encode(&mut frame)?;
            let ret = tairix_rt::volume_detach(&frame[..len]);
            if ret == 0 {
                Ok(())
            } else {
                Err(Errno::from_syscall(ret))
            }
        }
    }

    /// The production standard-output stream (the short help).
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production standard-error stream: diagnostics go to fd 2 and
    /// the `--force` advisory to fd 3 (best-effort).
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }

        fn info(&self, record: &[u8]) {
            // fd 3 is ignorable by contract: unattached is a no-op and a
            // short write is never an error the outcome depends on.
            let _ = StdInfo.write_all(record);
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    ///
    /// Exit codes: `0` on success (or the short help), `1` when the
    /// volume could not be resolved or the kernel refused the detach,
    /// `2` on a usage error.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error,
        // reported rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("unmount: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &IpcTransport,
            &RtDetacher,
            &BundleHelp::new("unmount"),
            &RtOutput,
            &RtErrors,
        ) {
            Ok(()) => 0,
            Err(err @ UnmountError::Usage) => {
                write_stderr_line(&format!("unmount: {err}"));
                write_stderr_line(USAGE);
                2
            }
            Err(err) => {
                write_stderr_line(&format!("unmount: {err}"));
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
