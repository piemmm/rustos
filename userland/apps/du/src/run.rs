//! The `Run` entry-point binary of the `du` tool — the program a shell
//! spawns to estimate file space usage.
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
//! command against the production seams: `RtWalk`, which stats paths and
//! reads directories through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side), the shared
//! `tairix_help::BundleHelp`, which reads the tool's own bundle's `Help/`
//! tree for the short-help switches, and `RtOutput`/`RtErrors`, which write
//! the usage rows to the inherited standard output and the diagnostics to
//! standard error. The tool binds only to its inherited descriptors, never
//! a console device, and holds no ambient authority.
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
    use alloc::vec::Vec;

    use tairix_abi::fs::{DirEntry, OpenFlags, FS_IO_MAX};
    use tairix_abi::Errno;
    use tairix_du::{parse, run, Entry, Metadata, Output, Walk, USAGE};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use tairix_rt::File;

    /// Initial byte size of the directory-listing buffer: one page covers a
    /// typical directory; `BufferTooSmall` grows it (below).
    const DIR_BUF_INITIAL: usize = 4096;

    /// Ceiling for the directory-listing buffer: the kernel's own per-call
    /// staging cap ([`FS_IO_MAX`]), so the buffer grows exactly as far as
    /// one `fs_readdir` transfer can ever fill and no further.
    const DIR_BUF_MAX: usize = FS_IO_MAX;

    /// The production [`Walk`]: the kernel-authorised `fs_*` view of the
    /// filesystem. It adds no authority — every path resolution, per-inode
    /// permission, and mount-flag check happens kernel-side under the
    /// caller's attested identity, and a refusal surfaces as the exact
    /// [`Errno`] the kernel chose.
    struct RtWalk;

    impl Walk for RtWalk {
        fn stat(&self, path: &str) -> Result<Metadata, Errno> {
            // A resolve-only open: no read authority is requested, the
            // handle is closed on drop, and only the metadata is learned.
            let file =
                File::open(path.as_bytes(), OpenFlags::empty()).map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(Metadata {
                kind: stat.kind,
                size: stat.size,
                allocated: stat.allocated,
                id: stat.id,
                nlink: stat.nlink,
            })
        }

        fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno> {
            let dir = tairix_rt::open_dir(path.as_bytes()).map_err(Errno::from_syscall)?;
            let mut buf = alloc::vec![0u8; DIR_BUF_INITIAL];
            let used = loop {
                match dir.read(&mut buf) {
                    Ok(used) => break used,
                    Err(ret) => match Errno::from_syscall(ret) {
                        Errno::BufferTooSmall if buf.len() < DIR_BUF_MAX => {
                            buf.resize((buf.len() * 2).min(DIR_BUF_MAX), 0);
                        }
                        other => return Err(other),
                    },
                }
            };
            let mut entries = Vec::new();
            let mut rest = &buf[..used];
            while !rest.is_empty() {
                let (entry, consumed) = DirEntry::decode(rest)?;
                rest = &rest[consumed..];
                // The ABI contract makes every entry name UTF-8; a name
                // that is not is a corrupt or hostile stream, refused whole
                // rather than silently dropped from the walk — `OutOfRange`,
                // the same errno the entry decoder itself uses for a field
                // outside its permitted domain.
                let name = core::str::from_utf8(entry.name).map_err(|_| Errno::OutOfRange)?;
                entries.push(Entry {
                    name: String::from(name),
                    meta: Metadata {
                        kind: entry.kind,
                        size: entry.size,
                        allocated: entry.allocated,
                        id: entry.id,
                        nlink: entry.nlink,
                    },
                });
            }
            Ok(entries)
        }
    }

    /// The production standard-output stream: the usage rows go to fd 1.
    /// The tool names only descriptors its spawner chose, so the same
    /// binary drives a serial terminal, a framebuffer console, or a future
    /// windowed terminal unchanged.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production standard-error stream: per-path diagnostics go to
    /// fd 2, keeping the usage rows on fd 1 clean for pipes.
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` when a path was diagnosed or the
    /// output failed, `2` on a usage error (a malformed argument vector or
    /// an unrecognised option).
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
                write_stderr_line(&format!("du: {err}"));
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
            &RtWalk,
            &BundleHelp::new("du"),
            &RtOutput,
            &RtErrors,
        ) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(err) => {
                write_stderr_line(&format!("du: {err}"));
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
