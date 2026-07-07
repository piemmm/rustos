//! The `Run` entry-point binary of the `cat` tool — the program a shell
//! spawns to concatenate files and standard input to the terminal.
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
//! exports it; the tool invents no second source), and runs the parsed
//! command against the production seams: `RtFileSource`, which reads named
//! files through the kernel-authorised `fs_*` syscalls (every per-inode and
//! mount check stays kernel-side) and resource references (`sys:random`)
//! through the kernel's capability-checked resolver, `RtStdin`, which reads the inherited
//! standard input, the shared `rustos_help::BundleHelp`, which reads the
//! tool's own bundle's `Help/` tree for the short-help switches, and
//! `RtOutput`, which writes the stream to the inherited standard output. The
//! tool binds only to its inherited descriptors, never a console device, and
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

    use alloc::format;
    use alloc::string::String;
    use core::cell::RefCell;

    use rustos_abi::fs::OpenFlags;
    use rustos_abi::Errno;
    use rustos_cat::{parse, run, FileSource, Input, Output, USAGE};
    use rustos_help::BundleHelp;
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_rt::File;

    /// The production [`FileSource`]: the kernel-authorised `fs_*` view of
    /// the filesystem. It adds no authority — every path resolution,
    /// per-inode permission, and mount-flag check happens kernel-side under
    /// the caller's attested identity, and a refusal surfaces as the exact
    /// [`Errno`] the kernel chose.
    ///
    /// The client streams one source at a time with an advancing offset, so
    /// the handle of the file currently being streamed is kept open across
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
                // `File::open` applies the one shared spelling rule: a
                // resource reference (`sys:random`) routes to the kernel's
                // capability-checked resolver, a path to the filesystem —
                // the tool carries no routing of its own.
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
    }

    /// The production [`Input`] over the inherited standard input (fd 0):
    /// the tool names only descriptors its spawner chose, never a console
    /// device, so the same binary reads a serial terminal, a pipe, or a
    /// future windowed terminal unchanged.
    struct RtStdin;

    impl Input for RtStdin {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            // The stream backing reports end-of-input as a zero-length read;
            // there is no error channel on the wrapper.
            Ok(rustos_rt::stdin(buf))
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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a read or output failure, `2` on a
    /// usage error (a malformed argument vector or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
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
