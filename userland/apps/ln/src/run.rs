//! The `Run` entry-point binary of the `ln` tool — the program a shell
//! spawns to create a symbolic link.
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
//! command against the production seams: `RtFileSystem`, which inspects
//! names and creates links through the kernel-authorised `fs_*` syscalls
//! (every per-inode and mount check stays kernel-side), `RtPrompt` for the
//! `-i` question, the shared `tairix_help::BundleHelp` for the short-help
//! switches, and `RtOutput` for the `-v` reports. The tool binds only to its
//! inherited descriptors, never a console device, and holds no ambient
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

    use tairix_abi::fs::{OpenFlags, FS_PATH_MAX};
    use tairix_abi::{Errno, FileKind, LinkFlags, RealpathMode, UnlinkFlags};
    use tairix_help::BundleHelp;
    use tairix_ln::{parse, run, FileSystem, Occupant, Output, Prompt, USAGE};
    use tairix_rt::io::{self, write_stderr_line, Read, Stderr, Stdin, Stdout, Write};
    use tairix_rt::File;

    /// Longest interactive reply chunk read per syscall; the reply is only
    /// ever judged by its first byte, so the bound caps work, not meaning.
    const REPLY_MAX: usize = 64;

    /// The production [`FileSystem`]: the kernel-authorised `fs_*` view. It
    /// adds no authority — every path resolution, per-inode permission, and
    /// mount-flag check happens kernel-side under the caller's attested
    /// identity, and a refusal surfaces as the exact [`Errno`] the kernel
    /// chose.
    struct RtFileSystem;

    impl FileSystem for RtFileSystem {
        fn occupant(&self, path: &str) -> Result<Occupant, Errno> {
            // The name as typed: a resolve-only, `NO_FOLLOW` open, so a
            // final link describes *itself* and a dangling one is still
            // visible. Following here would let a link already at the
            // destination decide what a later replacement acts on.
            let kind = match File::open(path.as_bytes(), OpenFlags::NO_FOLLOW) {
                Ok(file) => file.stat().map_err(Errno::from_syscall)?.kind,
                Err(ret) => {
                    return match Errno::from_syscall(ret) {
                        Errno::NotFound => Ok(Occupant::Vacant),
                        other => Err(other),
                    };
                }
            };
            match kind {
                FileKind::Directory => Ok(Occupant::Directory),
                FileKind::Regular => Ok(Occupant::File),
                // A link is resolved just far enough to say whether it names
                // a directory — the one thing the destination reading and
                // `-n` turn on. A dangling link, or one the caller may not
                // follow, is simply not a directory.
                FileKind::Symlink => {
                    let resolved = File::open(path.as_bytes(), OpenFlags::empty())
                        .ok()
                        .and_then(|file| file.stat().ok())
                        .map(|stat| stat.kind);
                    Ok(if resolved == Some(FileKind::Directory) {
                        Occupant::LinkToDirectory
                    } else {
                        Occupant::Link
                    })
                }
            }
        }

        fn canonicalize(&self, path: &str, mode: RealpathMode) -> Result<String, Errno> {
            // The kernel's own resolution, never a second walk here: `-r`
            // spells the difference between two canonical paths, and a
            // difference computed against a different reading of the tree
            // would name a different node. The buffer is the ABI's own path
            // bound, so one call always suffices.
            let mut buf = alloc::vec![0u8; FS_PATH_MAX];
            let ret = tairix_rt::fs_realpath(path.as_bytes(), &mut buf, mode);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            let len = usize::try_from(ret).map_err(|_| Errno::OutOfRange)?;
            let bytes = buf.get(..len).ok_or(Errno::OutOfRange)?;
            let canonical = core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)?;
            Ok(String::from(canonical))
        }

        fn symlink(&self, target: &str, link: &str) -> Result<(), Errno> {
            // Target first, then the link — the `symlink(2)` argument order.
            // The kernel checks the target's grammar and the caller's right
            // to create a name in the link's own parent; the target itself
            // is stored verbatim and never resolved.
            let ret = tairix_rt::fs_symlink(target.as_bytes(), link.as_bytes());
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
        }

        fn link(&self, target: &str, link: &str, dereference: bool) -> Result<(), Errno> {
            // Existing name first, then the new one — the `link(2)` argument
            // order. `-L` selects the `linkat(AT_SYMLINK_FOLLOW)` posture;
            // without it neither final component is followed, so the inode
            // that gains a name is the one the operand spelled.
            let flags = if dereference {
                LinkFlags::FOLLOW
            } else {
                LinkFlags::empty()
            };
            let ret = tairix_rt::fs_link(target.as_bytes(), link.as_bytes(), flags);
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
        }

        fn remove(&self, path: &str) -> Result<(), Errno> {
            // `fs_unlink` keeps the name as typed, so removing a link
            // removes the link and never what it names.
            let ret = tairix_rt::fs_unlink(path.as_bytes(), UnlinkFlags::empty());
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
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
                .write_all(format!("ln: {question} ").as_bytes())
                .map_err(io::Error::as_errno)?;
            let mut first: Option<u8> = None;
            let mut buf = [0u8; REPLY_MAX];
            loop {
                let n = Stdin.read(&mut buf).map_err(io::Error::as_errno)?;
                if n == 0 {
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
            Stdout.write_all(bytes).map_err(io::Error::as_errno)
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
    /// Exit codes: `0` when every link was made (or the short help was
    /// written), `1` otherwise — the GNU `ln` shape, which has no separate
    /// usage status. Every failure states its reason on fd 2, and a usage
    /// failure adds the banner.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            report_usage();
            return 1;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("ln: {err}"));
                report_usage();
                return 1;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &RtFileSystem,
            &RtPrompt,
            &BundleHelp::new("ln"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err) => {
                write_stderr_line(&format!("ln: {err}"));
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
