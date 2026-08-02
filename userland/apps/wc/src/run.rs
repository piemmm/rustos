//! The `Run` entry-point binary of the `wc` tool — the program a shell
//! spawns to print newline, word, and byte counts for files.
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
//! command against the production seams: `RtFileSource`, which reads named
//! files (and probes their size for the column-width rule) through the
//! kernel-authorised `fs_*` syscalls (every per-inode and mount check stays
//! kernel-side), `RtStdin`, which reads the inherited standard input, the
//! shared `tairix_help::BundleHelp`, which reads the tool's own bundle's
//! `Help/` tree for the short-help switches, and `RtOutput`/`RtErrors`,
//! which write to the inherited standard output and standard error. The
//! tool binds only to its inherited descriptors, never a console device,
//! and holds no ambient authority.
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
    use core::cell::RefCell;

    use tairix_abi::fs::{FileKind, OpenFlags};
    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_rt::io::{self, write_stderr_line, Read, Stderr, Stdin, Stdout, Write};
    use tairix_rt::File;
    use tairix_wc::{parse, run, FileSource, Input, Output, SizeProbe, USAGE};

    /// The production [`FileSource`]: the kernel-authorised `fs_*` view of
    /// the filesystem. It adds no authority — every path resolution,
    /// per-inode permission, and mount-flag check happens kernel-side under
    /// the caller's attested identity, and a refusal surfaces as the exact
    /// [`Errno`] the kernel chose.
    ///
    /// The client streams one input at a time with an advancing offset, so
    /// the handle of the file currently being counted is kept open across
    /// calls — a file is opened once, not once per chunk — and replaced when
    /// the client moves to the next path.
    struct RtFileSource {
        open: RefCell<Option<(String, File)>>,
    }

    impl RtFileSource {
        fn new() -> Self {
            Self {
                open: RefCell::new(None),
            }
        }
    }

    impl FileSource for RtFileSource {
        fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let mut open = self.open.borrow_mut();
            let cached = matches!(&*open, Some((name, _)) if name == path);
            if !cached {
                let file =
                    File::open(path.as_bytes(), OpenFlags::READ).map_err(Errno::from_syscall)?;
                *open = Some((String::from(path), file));
            }
            match &*open {
                Some((_, file)) => file.read_at(offset, buf).map_err(Errno::from_syscall),
                // Unreachable by construction (the handle was just installed),
                // but fail closed rather than panic.
                None => Err(Errno::NotFound),
            }
        }

        fn size(&self, path: &str) -> SizeProbe {
            let Ok(file) = File::open(path.as_bytes(), OpenFlags::READ) else {
                return SizeProbe::Unavailable;
            };
            match file.stat() {
                Ok(stat) if stat.kind == FileKind::Regular => SizeProbe::Regular(stat.size),
                Ok(_) => SizeProbe::NotRegular,
                Err(_) => SizeProbe::Unavailable,
            }
        }
    }

    /// The production [`Input`] over the inherited standard input (fd 0):
    /// the tool names only descriptors its spawner chose, never a console
    /// device, so the same binary reads a serial terminal, a pipe, or a
    /// future windowed terminal unchanged.
    struct RtStdin;

    impl Input for RtStdin {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            Stdin.read(buf).map_err(io::Error::as_errno)
        }
    }

    /// The production [`Output`] over the inherited standard output (fd 1).
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(io::Error::as_errno)
        }
    }

    /// The production diagnostic [`Output`] over the inherited standard
    /// error (fd 2).
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(io::Error::as_errno)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` when every input was counted (or the short help was
    /// written), `1` when an input failed or output could not be delivered,
    /// `2` on a usage error (a malformed argument vector, an unrecognised
    /// option, or an invalid option value).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("wc: {err}"));
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
            &RtFileSource::new(),
            &RtStdin,
            &BundleHelp::new("wc"),
            &RtOutput,
            &RtErrors,
        ) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(err) => {
                write_stderr_line(&format!("wc: {err}"));
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
