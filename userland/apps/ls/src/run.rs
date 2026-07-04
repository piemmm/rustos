//! The `Run` entry-point binary of the `ls` tool — the program a shell
//! spawns to list directory contents.
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
//! command against the production seams: `RtListing`, which stats paths and
//! reads directories through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side), `RtHelp`, which reads the
//! tool's own bundle's `Help/` tree for the short-help switches, and
//! `RtOutput`, which writes the listing to the inherited standard output and
//! the hidden-entries advisory to fd 3, best-effort. The tool binds only to
//! its inherited descriptors, never a console device, and holds no ambient
//! authority.
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

    use rustos_abi::fs::{DirEntry, FileKind, OpenFlags, FS_IO_MAX};
    use rustos_abi::{Errno, BUNDLE_SUFFIX, SYSTEM_APP_STORE};
    use rustos_help::{HelpSource, SourceError, MAX_DOC_LEN};
    use rustos_ls::{parse, run, Entry, Listing, Metadata, Output, USAGE};
    use rustos_rt::io::{write_stderr_line, StdInfo, Stdout, Write};
    use rustos_rt::File;

    /// Initial byte size of the directory-listing buffer: one page covers a
    /// typical directory; `BufferTooSmall` grows it (below).
    const DIR_BUF_INITIAL: usize = 4096;

    /// Ceiling for the directory-listing buffer: the kernel's own per-call
    /// staging cap ([`FS_IO_MAX`]), so the buffer grows exactly as far as
    /// one `fs_readdir` transfer can ever fill and no further.
    const DIR_BUF_MAX: usize = FS_IO_MAX;

    /// The production [`Listing`]: the kernel-authorised `fs_*` view of the
    /// filesystem. It adds no authority — every path resolution, per-inode
    /// permission, and mount-flag check happens kernel-side under the
    /// caller's attested identity, and a refusal surfaces as the exact
    /// [`Errno`] the kernel chose.
    struct RtListing;

    impl Listing for RtListing {
        fn stat(&self, path: &str) -> Result<Metadata, Errno> {
            // A resolve-only open: no read authority is requested, the
            // handle is closed on drop, and only the metadata is learned.
            let file =
                File::open(path.as_bytes(), OpenFlags::empty()).map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(Metadata {
                kind: stat.kind,
                mode: stat.mode,
                size: stat.size,
            })
        }

        fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno> {
            let dir = rustos_rt::open_dir(path.as_bytes()).map_err(Errno::from_syscall)?;
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
                // rather than silently dropped from the listing —
                // `OutOfRange`, the same errno the entry decoder itself uses
                // for a field outside its permitted domain.
                let name = core::str::from_utf8(entry.name).map_err(|_| Errno::OutOfRange)?;
                entries.push(Entry {
                    name: String::from(name),
                    kind: entry.kind,
                });
            }
            Ok(entries)
        }
    }

    /// The tool's own bundle's `Help/` tree, read through the same
    /// kernel-authorised `fs_*` view for the short-help switches. The bundle
    /// directory is the system app store's command-named spelling — the one
    /// shared `lib/abi` definition, so it cannot drift from where the image
    /// builder plants the documents.
    struct RtHelp;

    impl RtHelp {
        /// `/System/Apps/ls.app/Help/<suffix>`.
        fn help_path(suffix: &str) -> String {
            format!("{SYSTEM_APP_STORE}/ls{BUNDLE_SUFFIX}/Help/{suffix}")
        }
    }

    impl HelpSource for RtHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            let path = format!("{SYSTEM_APP_STORE}/ls{BUNDLE_SUFFIX}/Help");
            let dir = match rustos_rt::open_dir(path.as_bytes()) {
                Ok(dir) => dir,
                // A build without the bundle's documents simply has no
                // locales; the engine then reports "not found" and the
                // caller falls back to the usage banner.
                Err(_) => return Ok(Vec::new()),
            };
            let mut buf = alloc::vec![0u8; DIR_BUF_INITIAL];
            let used = loop {
                match dir.read(&mut buf) {
                    Ok(used) => break used,
                    Err(ret) => match Errno::from_syscall(ret) {
                        Errno::BufferTooSmall if buf.len() < DIR_BUF_MAX => {
                            buf.resize((buf.len() * 2).min(DIR_BUF_MAX), 0);
                        }
                        _ => return Err(SourceError),
                    },
                }
            };
            let mut dirs = Vec::new();
            let mut rest = &buf[..used];
            while !rest.is_empty() {
                let (entry, consumed) = DirEntry::decode(rest).map_err(|_| SourceError)?;
                rest = &rest[consumed..];
                if entry.kind != FileKind::Directory {
                    continue;
                }
                // A non-UTF-8 name can never be a locale directory the
                // engine validated a spelling for; skipping it loses nothing
                // and fabricates nothing.
                if let Ok(name) = core::str::from_utf8(entry.name) {
                    dirs.push(String::from(name));
                }
            }
            Ok(dirs)
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            let path = Self::help_path(&format!("{locale_dir}/{file_name}"));
            let file = match rustos_rt::open(path.as_bytes()) {
                Ok(file) => file,
                Err(ret) => {
                    return match Errno::from_syscall(ret) {
                        Errno::NotFound => Ok(None),
                        _ => Err(SourceError),
                    };
                }
            };
            // Read at most one byte past the engine's limit: the engine's
            // own document bound then rejects the oversized file, and a
            // hostile huge file cannot exhaust memory here first.
            let cap = MAX_DOC_LEN.saturating_add(1);
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            while bytes.len() < cap {
                let want = chunk.len().min(cap - bytes.len());
                let read = file
                    .read_at(bytes.len() as u64, &mut chunk[..want])
                    .map_err(|_| SourceError)?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Ok(Some(bytes))
        }
    }

    /// The production [`Output`] over the inherited standard streams: the
    /// listing goes to fd 1 and the advisory record to fd 3 (best-effort).
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

        fn info(&self, record: &[u8]) {
            // fd 3 is ignorable by contract: unattached is a no-op and a
            // short write is never an error a listing depends on.
            let _ = StdInfo.write_all(record);
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a filesystem or output failure,
    /// `2` on a usage error (a malformed argument vector or an unrecognised
    /// option).
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
        match run(command, locale, &RtListing, &RtHelp, &RtOutput) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("ls: {err}"));
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
