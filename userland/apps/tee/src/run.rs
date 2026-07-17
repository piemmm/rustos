//! The `Run` entry-point binary of the `tee` tool — the program a shell
//! spawns to copy standard input to standard output and files.
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
//! command against the production seams: `RtStdin`, which reads the
//! inherited standard input, `RtFileSink`, which creates and writes the
//! file operands through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side), the shared
//! `tairix_help::BundleHelp`, which reads the tool's own bundle's `Help/`
//! tree for the short-help switches, and `RtOutput`/`RtErrors`, which
//! write to the inherited standard output and standard error. The tool
//! binds only to its inherited descriptors, never a console device, and
//! holds no ambient authority.
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

    use alloc::collections::BTreeMap;
    use alloc::format;
    use core::cell::RefCell;

    use tairix_abi::fs::OpenFlags;
    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use tairix_rt::File;
    use tairix_tee::{parse, run, FileSink, Input, Output, USAGE};

    /// The production [`Input`] over the inherited standard input (fd 0):
    /// the tool names only descriptors its spawner chose, never a console
    /// device, so the same binary reads a serial terminal, a pipe, or a
    /// future windowed terminal unchanged.
    struct RtStdin;

    impl Input for RtStdin {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            // The stream backing reports end-of-input as a zero-length read;
            // there is no error channel on the wrapper.
            Ok(tairix_rt::stdin(buf))
        }
    }

    /// The production [`FileSink`]: the kernel-authorised `fs_*` view of
    /// the filesystem. It adds no authority — every path resolution,
    /// per-inode permission, and mount-flag check happens kernel-side under
    /// the caller's attested identity, and a refusal surfaces as the exact
    /// [`Errno`] the kernel chose.
    ///
    /// Each operand's handle stays open for the whole run — a file is
    /// opened once, not once per chunk — keyed by its command-line
    /// position, so a file named twice gets two independent handles exactly
    /// as two GNU file descriptors would. The tracked offset advances past
    /// each write; under the `APPEND` posture the kernel positions every
    /// write at end-of-file regardless.
    struct RtFileSink {
        open: RefCell<BTreeMap<usize, (File, u64)>>,
    }

    impl RtFileSink {
        fn new() -> Self {
            Self {
                open: RefCell::new(BTreeMap::new()),
            }
        }
    }

    impl FileSink for RtFileSink {
        fn open(&self, id: usize, path: &str, append: bool) -> Result<(), Errno> {
            let mode = if append {
                OpenFlags::APPEND
            } else {
                OpenFlags::TRUNCATE
            };
            let flags = OpenFlags::WRITE.union(OpenFlags::CREATE).union(mode);
            let file = File::open(path.as_bytes(), flags).map_err(Errno::from_syscall)?;
            self.open.borrow_mut().insert(id, (file, 0));
            Ok(())
        }

        fn write(&self, id: usize, bytes: &[u8]) -> Result<(), Errno> {
            let mut open = self.open.borrow_mut();
            let Some((file, offset)) = open.get_mut(&id) else {
                // Unreachable by construction (the client writes only
                // outputs it opened), but fail closed rather than panic.
                return Err(Errno::NotFound);
            };
            // The kernel may accept a short write; every byte is the seam's
            // contract, so loop until the chunk is on disk or refused.
            let mut written = 0usize;
            while written < bytes.len() {
                let n = file
                    .write_at(*offset + written as u64, &bytes[written..])
                    .map_err(Errno::from_syscall)?;
                if n == 0 {
                    // A zero-byte accept would spin forever; fail closed.
                    return Err(Errno::LengthOutOfRange);
                }
                written += n;
            }
            *offset += bytes.len() as u64;
            Ok(())
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

    /// The production diagnostic [`Output`] over the inherited standard
    /// error (fd 2).
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` when every output was served to end-of-input (or the
    /// short help was written), `1` when an output failed in a way the
    /// selected mode counts or a diagnostic could not be delivered, `2` on
    /// a usage error (a malformed argument vector, an unrecognised option,
    /// or an invalid `--output-error` mode).
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
                write_stderr_line(&format!("tee: {err}"));
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
            &RtStdin,
            &RtFileSink::new(),
            &BundleHelp::new("tee"),
            &RtOutput,
            &RtErrors,
        ) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(err) => {
                write_stderr_line(&format!("tee: {err}"));
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
