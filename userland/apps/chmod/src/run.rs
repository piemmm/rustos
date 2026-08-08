//! The `Run` entry-point binary of the `chmod` tool — the program a shell
//! spawns to change file mode bits.
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
//! command against the production seams: `RtFileSystem`, which inspects,
//! walks, and re-modes paths through the kernel-authorised `fs_*` syscalls
//! (the owner-only rule and every per-inode and mount check stay
//! kernel-side), the shared `tairix_help::BundleHelp`, which reads the
//! tool's own bundle's `Help/` tree for the short-help switches, and
//! `RtOutput`, which writes the `-v`/`-c` reports to the inherited standard
//! output. The tool binds only to its inherited descriptors, never a console
//! device, and holds no ambient authority.
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

    use tairix_abi::fs::{DirEntry, OpenFlags, FS_IO_MAX, FS_MODE_MASK};
    use tairix_abi::Errno;
    use tairix_chmod::{
        parse, run, ChmodError, Entry, EntryKind, FileSystem, Metadata, Output, USAGE,
    };
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

    /// Read every entry of the directory at `path` into one snapshot, the
    /// same grow-on-`BufferTooSmall` read `ls` and `rm` use.
    fn read_entries(path: &str) -> Result<Vec<Entry>, Errno> {
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
                    EntryKind::File
                },
            });
        }
        Ok(entries)
    }

    /// The production [`FileSystem`]: the kernel-authorised `fs_*` view. It
    /// adds no authority — every path resolution, the owner-only mode-change
    /// rule, and every per-inode and mount-flag check happen kernel-side
    /// under the caller's attested identity, and a refusal surfaces as the
    /// exact [`Errno`] the kernel chose.
    ///
    /// The engine walks a directory entry-by-entry through the index-based
    /// seam, so the host keeps a one-slot snapshot of the last directory
    /// read, hoisting the per-entry re-read off the walk. A mode change
    /// never renames or removes anything, so a snapshot can never go stale
    /// against this tool's own actions.
    struct RtFileSystem {
        listing: RefCell<Option<(String, Vec<Entry>)>>,
    }

    impl RtFileSystem {
        fn new() -> Self {
            Self {
                listing: RefCell::new(None),
            }
        }
    }

    impl FileSystem for RtFileSystem {
        fn stat(&self, path: &str) -> Result<Metadata, Errno> {
            // A resolve-only open: no read authority is requested, the
            // handle is closed on drop, and only the metadata is learned.
            let file =
                File::open(path.as_bytes(), OpenFlags::empty()).map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(Metadata {
                kind: if stat.kind.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                // Only the permission bits are the mode this tool reasons
                // about; any file-type bits the backing reports are not.
                mode: stat.mode & FS_MODE_MASK,
            })
        }

        fn set_mode(&self, path: &str, mode: u32) -> Result<(), Errno> {
            // The seam contract: the file-type bits are not the caller's to
            // change, so only the permission bits travel.
            let ret = tairix_rt::fs_set_mode(path.as_bytes(), mode & FS_MODE_MASK);
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` on a filesystem or output failure
    /// (silent for `-f`'s suppressed diagnostics — the silence is the very
    /// thing the user asked for; the status still reports the failure), `2`
    /// on a usage error (a malformed argument vector, an unrecognised
    /// option, or an unparsable mode).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            report_usage();
            return 2;
        };
        let Ok(command) = parse(&arguments) else {
            report_usage();
            return 2;
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &RtFileSystem::new(),
            &BundleHelp::new("chmod"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            // `-f` suppressed the diagnostics; the exit status alone
            // reports the failure, exactly as the user requested.
            Err(ChmodError::Silenced) => 1,
            Err(err) => {
                write_stderr_line(&format!("chmod: {err}"));
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
