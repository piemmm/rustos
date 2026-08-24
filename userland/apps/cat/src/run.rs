//! The `Run` entry-point binary of the `cat` tool — the program a shell
//! spawns to concatenate files and standard input to the terminal.
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
//! command against the production seams: `RtFileSource`, which reads every
//! named source through the one shared `tairix_procinfo::NamedSource` — paths
//! and stream references through the kernel, `info:`/`state:`/`stats:` values
//! through the `sysinfod` broker, each checked under the caller's attested
//! identity; `RtStdin`, which reads the inherited standard input; the shared
//! `tairix_help::BundleHelp` for the short-help switches; and `RtOutput`,
//! which writes the stream to the inherited standard output. The tool binds
//! only to its inherited descriptors, never a console device, and holds no
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
    use alloc::string::String;
    use core::cell::RefCell;

    use tairix_abi::Errno;
    use tairix_cat::{parse, run, FileSource, Input, Output, USAGE};
    use tairix_help::BundleHelp;
    use tairix_procinfo::{NamedSource, OpenError};
    use tairix_rt::io::{self, write_stderr_line, Read, Stdin, Stdout, Write};

    /// The production [`FileSource`]: the shared open-by-name view of a
    /// readable source. It adds no authority — every path check stays
    /// kernel-side and every value read is gated at the broker, both under
    /// the caller's attested identity.
    ///
    /// The client streams one source at a time with an advancing offset, so
    /// the open source is kept across calls — opened once, not once per
    /// chunk — and replaced when the client moves on.
    struct RtFileSource {
        open: RefCell<Option<(String, NamedSource)>>,
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
                let source = NamedSource::open(path.as_bytes()).map_err(|err| {
                    report_open_refusal(path, err);
                    err.errno()
                })?;
                *open = Some((String::from(path), source));
            }
            match &*open {
                Some((_, source)) => source.read_at(offset, buf),
                // Unreachable by construction (the source was just installed),
                // but fail closed rather than panic.
                None => Err(Errno::NotFound),
            }
        }
    }

    /// State why an `info:`/`state:`/`stats:` operand could not be read.
    ///
    /// The `cat: <errno>` line printed afterwards cannot say which resource was
    /// refused or which grant it wanted; the shared wording can. A stream or
    /// path refusal needs no extra line — the errno says it.
    fn report_open_refusal(path: &str, err: OpenError) {
        if let OpenError::Value(value) = err {
            write_stderr_line(&format!("cat: {path}: {value}"));
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a read or output failure, `2` on a
    /// usage error (a malformed argument vector or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let Ok(command) = parse(&arguments) else {
            write_stderr_line(USAGE);
            return 2;
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &RtFileSource::new(),
            &RtStdin,
            &BundleHelp::new("cat"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("cat: {err}"));
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
