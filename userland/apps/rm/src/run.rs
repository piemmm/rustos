//! The `Run` entry-point binary of the `rm` tool — the program a shell
//! spawns to remove files and directories.
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
//! command against the production seams: `RtRemoval`, which inspects, walks,
//! and unlinks paths through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side), `RtPrompt`, which asks the
//! `-i`/`-I` confirmations on standard error and reads the reply from
//! standard input (consent only on a leading `y`/`Y`; an unreadable reply is
//! never consent), the shared `rustos_help::BundleHelp`, which reads the
//! tool's own bundle's `Help/` tree for the short-help switches, and
//! `RtOutput`, which writes `-v` reports to the inherited standard output.
//! The tool binds only to its inherited descriptors, never a console device,
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
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use rustos_abi::fs::{DirEntry, OpenFlags, FS_IO_MAX};
    use rustos_abi::Errno;
    use rustos_help::BundleHelp;
    use rustos_rm::{parse, run, Entry, EntryKind, Output, Prompt, Removal, USAGE};
    use rustos_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use rustos_rt::File;

    /// Initial byte size of the directory-listing buffer: one page covers a
    /// typical directory; `BufferTooSmall` grows it (below).
    const DIR_BUF_INITIAL: usize = 4096;

    /// Ceiling for the directory-listing buffer: the kernel's own per-call
    /// staging cap ([`FS_IO_MAX`]), so the buffer grows exactly as far as
    /// one `fs_readdir` transfer can ever fill and no further.
    const DIR_BUF_MAX: usize = FS_IO_MAX;

    /// Longest interactive reply chunk read per syscall; the reply is only
    /// ever judged by its first byte, so the bound caps work, not meaning.
    const REPLY_MAX: usize = 64;

    /// Read every entry of the directory at `path` into one snapshot, the
    /// same grow-on-`BufferTooSmall` read `ls` uses.
    fn read_entries(path: &str) -> Result<Vec<Entry>, Errno> {
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
            // The ABI contract makes every entry name UTF-8; a name that is
            // not is a corrupt or hostile stream, refused whole rather than
            // silently dropped — `OutOfRange`, the same errno the entry
            // decoder itself uses for a field outside its permitted domain.
            let name = core::str::from_utf8(entry.name).map_err(|_| Errno::OutOfRange)?;
            entries.push(Entry {
                name: String::from(name),
                kind: if entry.kind.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::Other
                },
            });
        }
        Ok(entries)
    }

    /// The production [`Removal`]: the kernel-authorised `fs_*` view. It
    /// adds no authority — every path resolution, per-inode permission, and
    /// mount-flag check happens kernel-side under the caller's attested
    /// identity, and a refusal surfaces as the exact [`Errno`] the kernel
    /// chose.
    ///
    /// The engine walks a directory entry-by-entry through the index-based
    /// seam, so the host keeps a one-slot snapshot of the last directory
    /// read, hoisting the per-entry re-read off the walk. The engine
    /// snapshots a directory's children before removing any of them, and a
    /// removal under (or of) the snapshotted directory drops the snapshot,
    /// so a stale listing is never served.
    struct RtRemoval {
        listing: RefCell<Option<(String, Vec<Entry>)>>,
    }

    impl RtRemoval {
        fn new() -> Self {
            Self {
                listing: RefCell::new(None),
            }
        }

        /// Drop the directory snapshot a removal of `path` could have
        /// staled: the snapshot of the path itself (a removed directory)
        /// or of any directory the path lives under.
        fn forget(&self, path: &str) {
            let mut listing = self.listing.borrow_mut();
            let trimmed = path.trim_end_matches('/');
            if matches!(&*listing, Some((dir, _)) if trimmed.starts_with(dir.trim_end_matches('/')))
            {
                *listing = None;
            }
        }
    }

    impl Removal for RtRemoval {
        fn kind(&self, path: &str) -> Result<EntryKind, Errno> {
            // A resolve-only open: no read authority is requested, the
            // handle is closed on drop, and only the metadata is learned.
            let file =
                File::open(path.as_bytes(), OpenFlags::empty()).map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(if stat.kind.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::Other
            })
        }

        fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno> {
            let mut listing = self.listing.borrow_mut();
            let cached = matches!(&*listing, Some((dir, _)) if dir == path);
            if !cached {
                *listing = Some((String::from(path), read_entries(path)?));
            }
            let Some((_, entries)) = &*listing else {
                // Unreachable by construction (the snapshot was just
                // installed), but fail closed rather than panic.
                return Err(Errno::NotFound);
            };
            let index = usize::try_from(index).map_err(|_| Errno::LengthOutOfRange)?;
            Ok(entries.get(index).cloned())
        }

        fn remove_file(&self, path: &str) -> Result<(), Errno> {
            let ret = rustos_rt::fs_unlink(path.as_bytes(), rustos_abi::UnlinkFlags::empty());
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            self.forget(path);
            Ok(())
        }

        fn remove_dir(&self, path: &str) -> Result<(), Errno> {
            // The DIRECTORY flag makes the kernel remove the name only when
            // it is an (empty) directory, decided atomically under the
            // filesystem's own lock — a concurrent swap of the directory for
            // a file between this tool's listing and the removal fails
            // closed instead of unlinking the file. A non-empty directory
            // fails closed with the kernel's own errno.
            let ret = rustos_rt::fs_unlink(path.as_bytes(), rustos_abi::UnlinkFlags::DIRECTORY);
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            self.forget(path);
            Ok(())
        }
    }

    /// The production [`Prompt`] over the inherited standard streams: the
    /// question goes to fd 2 (so it is seen even when fd 1 is redirected)
    /// and the reply is read from fd 0, the GNU shape. Only a reply whose
    /// first byte is `y`/`Y` consents; end-of-input or an unreadable stream
    /// is never consent.
    struct RtPrompt;

    impl Prompt for RtPrompt {
        fn confirm(&self, question: &str) -> Result<bool, Errno> {
            Stderr
                .write_all(format!("rm: {question} ").as_bytes())
                .map_err(|_| Errno::NotImplemented)?;
            let mut first: Option<u8> = None;
            let mut buf = [0u8; REPLY_MAX];
            loop {
                let n = rustos_rt::stdin(&mut buf);
                if n == 0 {
                    // End of input (or an unreadable stream): no consent was
                    // given, so the answer is a decline — never an assumed yes.
                    break;
                }
                for &byte in &buf[..n] {
                    if byte == b'\n' {
                        return Ok(matches!(first, Some(b'y' | b'Y')));
                    }
                    if first.is_none() {
                        first = Some(byte);
                    }
                }
            }
            Ok(matches!(first, Some(b'y' | b'Y')))
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

    /// Write the multi-line usage banner to fd 2 byte-exact (it carries its
    /// own trailing newline), best-effort on the already-failing path.
    fn report_usage() {
        let _ = Stderr.write_all(USAGE.as_bytes());
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a filesystem, prompt, or output
    /// failure, `2` on a usage error (a malformed argument vector or an
    /// unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            report_usage();
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(_) => {
                report_usage();
                return 2;
            }
        };
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &RtRemoval::new(),
            &RtPrompt,
            &BundleHelp::new("rm"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("rm: {err}"));
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
