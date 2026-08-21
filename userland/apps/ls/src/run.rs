//! The `Run` entry-point binary of the `ls` tool — the program a shell
//! spawns to list directory contents.
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
//! command against the production seams: `RtListing`, which stats paths and
//! reads directories through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side), the shared
//! `tairix_help::BundleHelp`, which reads the tool's own bundle's `Help/`
//! tree for the short-help switches, and
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

    use tairix_abi::fs::{DirEntries, OpenFlags, FS_SYMLINK_MAX};
    use tairix_abi::time::Time64;
    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_ls::{parse, run, Entry, FinalLink, Listing, Metadata, Output, USAGE};
    use tairix_rt::io::{write_stderr_line, StdInfo, Stdout, Write};
    use tairix_rt::File;

    /// The production [`Listing`]: the kernel-authorised `fs_*` view of the
    /// filesystem. It adds no authority — every path resolution, per-inode
    /// permission, and mount-flag check happens kernel-side under the
    /// caller's attested identity, and a refusal surfaces as the exact
    /// [`Errno`] the kernel chose.
    struct RtListing;

    impl Listing for RtListing {
        fn stat(&self, path: &str, links: FinalLink) -> Result<Metadata, Errno> {
            // A resolve-only open: no read authority is requested, the
            // handle is closed on drop, and only the metadata is learned.
            // The follow posture is fixed once, here, by the open's flags,
            // and every operation served for that handle re-derives it — so
            // a `NO_FOLLOW` handle is the `lstat` reading and can describe
            // even a dangling link.
            let flags = match links {
                FinalLink::Keep => OpenFlags::NO_FOLLOW,
                FinalLink::Follow => OpenFlags::empty(),
            };
            let file = File::open(path.as_bytes(), flags).map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(Metadata {
                kind: stat.kind,
                nlink: stat.nlink,
                mode: stat.mode,
                size: stat.size,
                allocated: stat.allocated,
                uid: stat.uid,
                gid: stat.gid,
                id: stat.id,
                times: stat.times,
            })
        }

        fn read_link(&self, path: &str) -> Result<String, Errno> {
            // One call, one buffer: `fs_readlink` refuses an undersized
            // buffer rather than truncating a target — a truncated one would
            // name somewhere else entirely — and a target is bounded by
            // `FS_SYMLINK_MAX`, so a buffer of that size always suffices.
            let mut buf = alloc::vec![0u8; FS_SYMLINK_MAX];
            let ret = tairix_rt::fs_readlink(path.as_bytes(), &mut buf);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            let len = usize::try_from(ret).map_err(|_| Errno::OutOfRange)?;
            let target = buf.get(..len).ok_or(Errno::OutOfRange)?;
            // A stored target is UTF-8 by the grammar the kernel checked
            // before storing it; anything else is a corrupt or hostile
            // volume, refused rather than lossily shown.
            core::str::from_utf8(target)
                .map(String::from)
                .map_err(|_| Errno::OutOfRange)
        }

        fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno> {
            // The one shared listing call and stream walker (`lib/rt`'s
            // grow-to-`FS_IO_MAX` read, `lib/abi`'s `DirEntries`), so the
            // transfer policy and the record bookkeeping are never
            // re-derived here.
            let stream = tairix_rt::read_dir_all(path.as_bytes()).map_err(Errno::from_syscall)?;
            let mut entries = Vec::new();
            for item in DirEntries::new(&stream) {
                let entry = item?;
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

        fn error(&self, message: &str) {
            // The reason a path was skipped goes to fd 2, where a reader and
            // a script both see it while the listing itself still reaches
            // fd 1. Best-effort: a console that will not take the diagnostic
            // is not a reason to abandon the listing.
            write_stderr_line(message);
        }

        fn info(&self, record: &[u8]) {
            // fd 3 is ignorable by contract: unattached is a no-op and a
            // short write is never an error a listing depends on.
            let _ = StdInfo.write_all(record);
        }

        fn terminal_width(&self) -> Option<usize> {
            // The kernel attests a width only for a console whose grid it
            // owns (a framebuffer text console); a pipe, a file, or a
            // byte-stream (UART) console fails the probe closed, so the
            // listing degrades to plain one-per-line output rather than
            // guessing a width — the GNU rule, and no ambient authority.
            tairix_rt::terminal_size(tairix_abi::STDOUT)
                .ok()
                .map(|size| usize::from(size.cols()))
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes are the GNU `ls` grades: `0` when everything listed, `1`
    /// for a minor problem (an entry or subdirectory inside a listing that
    /// could not be reached — each one already reported on standard error),
    /// and `2` for serious trouble (a usage error, a command-line operand
    /// that could not be reached, or a failure to write the listing at all).
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
        // The `TERM` preference decides the colour depth of `--color` output.
        // An unset or non-UTF-8 value reads as `None`, so `auto` renders plain
        // — colour is never guessed at.
        let term = tairix_rt::env_var(b"TERM").and_then(|raw| core::str::from_utf8(raw).ok());
        // The current wall-clock instant decides the default (`locale`) and
        // `iso` date styles' recent/old window. An unset or unreadable clock
        // reads as the epoch, so every stamp renders in the "old" long form
        // rather than a guessed-at "recent" time — never a fabricated now.
        let now = tairix_rt::wall_time().map_or(Time64::UNIX_EPOCH, |reading| reading.time());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            now,
            term,
            &RtListing,
            &BundleHelp::new("ls"),
            &RtOutput,
        ) {
            Ok(outcome) => outcome.exit_status(),
            Err(err) => {
                write_stderr_line(&format!("ls: {err}"));
                2
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
