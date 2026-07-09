//! The `Run` entry-point binary of the `lspci` tool — the program a shell
//! spawns to list the discovered PCI/PCIe devices.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the Rust
//! userland runtime `rustos-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `rustos-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `rustos_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5 — the shell
//! exports it; the tool invents no second source), loads the bundle's own
//! `Resources/pci.ids.bin` lookup table through the kernel-authorised `fs_*`
//! syscalls (a missing or corrupt table degrades the listing to numeric ids
//! with the reason on standard error — never a withheld inventory, never a
//! fabricated name), and runs the parsed command against the production
//! seams: the shared `rustos_procinfo::IpcTransport` for the `sysinfo-v1`
//! `HARDWARE_TREE` query, the shared `rustos_help::BundleHelp` for the
//! short-help switches, and `RtOutput`/`RtErrors`, which write the listing
//! to the inherited standard output (with the naming advisory on fd 3,
//! best-effort) and the diagnostics to standard error. The tool binds only
//! to its inherited descriptors, never a console device, and holds no
//! ambient authority.
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

    use rustos_abi::fs::OpenFlags;
    use rustos_abi::{Errno, BUNDLE_SUFFIX, SYSTEM_APP_STORE};
    use rustos_devids::{DbKind, DevIds, MAX_SOURCE_BYTES};
    use rustos_help::BundleHelp;
    use rustos_lspci::{parse, run, Output, USAGE};
    use rustos_procinfo::IpcTransport;
    use rustos_rt::io::{write_stderr_line, StdInfo, Stdout, Write};
    use rustos_rt::File;

    /// The command word this bundle is named by.
    const OWN_WORD: &str = "lspci";

    /// Read chunk size for the table load: one comfortable transfer per
    /// `read_at` call over the bounded growing buffer below.
    const READ_CHUNK: usize = 64 * 1024;

    /// Load the bundle's own compiled `pci.ids` table,
    /// `/System/Apps/lspci.app/Resources/pci.ids.bin`, through the
    /// kernel-authorised `fs_*` syscalls. The path is spelled from the one
    /// shared `lib/abi` store definition, so it cannot drift from where the
    /// image builder plants the resource. The read is bounded: a "table"
    /// larger than the import pipeline could ever emit is refused whole.
    ///
    /// # Errors
    ///
    /// A short, human-readable reason (open failure, read failure, or an
    /// over-long file); the caller reports it and degrades to numeric ids.
    fn load_table() -> Result<Vec<u8>, alloc::string::String> {
        let path = format!("{SYSTEM_APP_STORE}/{OWN_WORD}{BUNDLE_SUFFIX}/Resources/pci.ids.bin");
        let file = File::open(path.as_bytes(), OpenFlags::READ)
            .map_err(|ret| format!("cannot open {path}: {}", Errno::from_syscall(ret)))?;
        let mut bytes = Vec::new();
        loop {
            if bytes.len() > MAX_SOURCE_BYTES {
                return Err(format!("{path} exceeds the table size bound"));
            }
            let offset = bytes.len();
            bytes.resize(offset + READ_CHUNK, 0);
            let got = file
                .read_at(offset as u64, &mut bytes[offset..])
                .map_err(|ret| format!("cannot read {path}: {}", Errno::from_syscall(ret)))?;
            bytes.truncate(offset + got);
            if got < READ_CHUNK {
                return Ok(bytes);
            }
        }
    }

    /// The production standard-output stream: the listing goes to fd 1 and
    /// the naming advisory to fd 3 (best-effort). The tool names only
    /// descriptors its spawner chose, so the same binary drives a serial
    /// terminal, a framebuffer console, or a future windowed terminal
    /// unchanged.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }

        fn info(&self, record: &[u8]) {
            // fd 3 is ignorable by contract: unattached is a no-op and a
            // short write is never an error a listing depends on.
            let _ = StdInfo.write_all(record);
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` when the query or output failed
    /// (including a refused `CAP_SYSINFO_HW` — the tool's whole purpose, so
    /// the reason lands on standard error and nothing is fabricated), `2`
    /// on a usage error.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("lspci: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The bundle's own lookup table. A load or validation failure is a
        // lost naming aid, not a lost inventory: the reason goes to standard
        // error and the listing renders numeric ids.
        let table_bytes = match load_table() {
            Ok(bytes) => Some(bytes),
            Err(why) => {
                write_stderr_line(&format!(
                    "lspci: device names unavailable ({why}); listing numeric ids"
                ));
                None
            }
        };
        let database = table_bytes.as_deref().and_then(|bytes| {
            match DevIds::parse(DbKind::Pci, bytes) {
                Ok(db) => Some(db),
                Err(err) => {
                    write_stderr_line(&format!(
                        "lspci: device names unavailable (invalid table: {err:?}); listing numeric ids"
                    ));
                    None
                }
            }
        });
        match run(
            command,
            locale,
            &IpcTransport,
            database.as_ref(),
            &BundleHelp::new(OWN_WORD),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("lspci: {err}"));
                1
            }
        }
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
