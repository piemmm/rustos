//! The `Run` entry-point binary of the `mdadm` tool — the program a shell
//! spawns to inspect and administer RAID arrays.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (the shell exports it; the tool
//! invents no second source), and runs the parsed command against the
//! production seams: the shared System Information client
//! (`tairix_procinfo::raid_arrays` / `raid_members`) for the reads, a single
//! `ipc_call` to the composer's control endpoint for the mutations, the shared
//! `tairix_help::BundleHelp` for the short-help switches, and `RtOutput`, which
//! writes the report to the inherited standard output (with the advisory
//! records on fd 3, best-effort) and diagnostics to standard error. The tool
//! binds only to its inherited descriptors, never a console device, and holds
//! no ambient authority.
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
    use alloc::vec::Vec;

    use tairix_abi::raid_admin::{RaidArrayRecord, RaidMemberRecord, RAID_CONTROL_ENDPOINT};
    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_mdadm::{parse, run, Controller, Output, Reader, USAGE};
    use tairix_rt::io::{write_stderr_line, StdInfo, Stdout, Write};

    /// The command word this bundle is named by.
    const OWN_WORD: &str = "mdadm";

    /// The production reads: the shared System Information client, gated by
    /// `CAP_SYSINFO_HW` (a refusal surfaces as `PermissionDenied`, which the
    /// engine reports; nothing is fabricated).
    struct RtReader;

    impl Reader for RtReader {
        fn arrays(&self) -> Result<Vec<RaidArrayRecord>, Errno> {
            tairix_procinfo::raid_arrays()
        }

        fn members(&self) -> Result<Vec<RaidMemberRecord>, Errno> {
            tairix_procinfo::raid_members()
        }
    }

    /// The production mutation transport: one `ipc_call` to the composer's
    /// reserved control endpoint. The composer authorises `CAP_STORAGE_ADMIN`
    /// against the caller's kernel-attested origin; this carries no authority.
    struct RtController;

    impl Controller for RtController {
        fn call(&self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(RAID_CONTROL_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The production standard-output stream: the report goes to fd 1 and the
    /// advisory records to fd 3 (best-effort). The tool names only descriptors
    /// its spawner chose, so the same binary drives a serial terminal, a
    /// framebuffer console, or a future windowed terminal unchanged.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting bytes
            // fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }

        fn info(&self, record: &[u8]) {
            // fd 3 is ignorable by contract: unattached is a no-op and a short
            // write is never an error the report depends on.
            let _ = StdInfo.write_all(record);
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a runtime failure (a denied
    /// capability, a name that did not resolve, a service or composer refusal,
    /// or an output failure — the reason lands on standard error), `2` on a
    /// usage error.
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("mdadm: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        match run(
            command,
            locale,
            &RtReader,
            &RtController,
            &BundleHelp::new(OWN_WORD),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("mdadm: {err}"));
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
