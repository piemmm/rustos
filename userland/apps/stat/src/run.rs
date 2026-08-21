//! The `Run` entry-point binary of the `stat` tool — the program a shell
//! spawns to report a file's or a filesystem's status.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, parses it, and reports each
//! operand through the kernel-authorised `fs_*` syscalls under the caller's
//! attested identity. The mount snapshot behind `%m`, `%o`, and the `-f`
//! reading, and the account name behind `%U`, are the ungated System
//! Information queries `df` and `whoami` read, through the one shared client.
//! The tool binds only to its inherited descriptors, never a console device.
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

    use tairix_abi::fs::{OpenFlags, FS_PATH_MAX, FS_SYMLINK_MAX};
    use tairix_abi::{Errno, FileStat, RealpathMode};
    use tairix_help::BundleHelp;
    use tairix_procinfo::{for_each_mount, IpcTransport, WalkStep};
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use tairix_rt::File;
    use tairix_stat::{parse, run, Filesystem, Mount, Mounts, Names, Output, Reporter, USAGE};

    /// The production [`Filesystem`]: the kernel-authorised `fs_*` view. It
    /// adds no authority — every path resolution, per-inode permission, and
    /// mount-flag check happens kernel-side under the caller's attested
    /// identity, and a refusal surfaces as the exact [`Errno`] the kernel
    /// chose.
    struct RtFilesystem;

    impl Filesystem for RtFilesystem {
        fn stat(&self, path: &str, dereference: bool) -> Result<FileStat, Errno> {
            // A resolve-only open, `NO_FOLLOW` unless `-L`: the descriptor's
            // flags are what fix the follow posture, so the stat served for
            // it cannot contradict the open.
            let flags = if dereference {
                OpenFlags::empty()
            } else {
                OpenFlags::NO_FOLLOW
            };
            let file = File::open(path.as_bytes(), flags).map_err(Errno::from_syscall)?;
            file.stat().map_err(Errno::from_syscall)
        }

        fn read_link(&self, path: &str) -> Result<String, Errno> {
            let mut buf = alloc::vec![0u8; FS_SYMLINK_MAX];
            let ret = tairix_rt::fs_readlink(path.as_bytes(), &mut buf);
            copied(ret, &buf)
        }

        fn canonicalize(&self, path: &str) -> Result<String, Errno> {
            // The kernel's own resolution. `%m` names the mount holding the
            // *canonical* path, so a link into another volume reports the
            // volume it lands on; a path whose tail does not exist yet is
            // still nameable, which is the reading `RealpathMode::Missing`
            // asks for.
            let mut buf = alloc::vec![0u8; FS_PATH_MAX];
            let ret = tairix_rt::fs_realpath(path.as_bytes(), &mut buf, RealpathMode::Missing);
            copied(ret, &buf)
        }
    }

    /// The text a `(length, else -errno)` syscall wrote into `buf`.
    ///
    /// A path the ABI hands back is UTF-8 by contract; bytes that are not are
    /// a corrupt or hostile record, refused rather than rendered as
    /// replacement characters.
    fn copied(ret: i64, buf: &[u8]) -> Result<String, Errno> {
        if ret < 0 {
            return Err(Errno::from_syscall(ret));
        }
        let len = usize::try_from(ret).map_err(|_| Errno::OutOfRange)?;
        let bytes = buf.get(..len).ok_or(Errno::OutOfRange)?;
        let text = core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)?;
        Ok(String::from(text))
    }

    /// The production [`Mounts`]: the `sysinfo-v1` `MOUNT_LIST` query, read
    /// through the one shared client `df` uses.
    struct RtMounts;

    impl Mounts for RtMounts {
        fn list(&self) -> Result<Vec<Mount>, Errno> {
            let mut mounts = Vec::new();
            for_each_mount(&IpcTransport, |record| {
                mounts.push(Mount {
                    target: String::from_utf8_lossy(record.target_bytes()).into_owned(),
                    fstype: String::from_utf8_lossy(record.fstype_bytes()).into_owned(),
                    usage: record.usage(),
                });
                Ok(WalkStep::Continue)
            })
            // A snapshot the caller cannot read leaves the mount-derived
            // fields unknown rather than failing the whole report, so the
                // walk's reason collapses onto one refusal here.
            .map_err(|_| Errno::NotImplemented)?;
            Ok(mounts)
        }
    }

    /// The production [`Names`]: the ungated `USER_DIRECTORY` query, the same
    /// one `whoami` reads.
    struct RtNames;

    impl Names for RtNames {
        fn user(&self, uid: u32) -> Option<String> {
            tairix_procinfo::user_name(&IpcTransport, uid)
                .ok()
                .flatten()
        }
    }

    /// The production [`Output`] over the inherited standard output (fd 1):
    /// the report.
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning. The io layer's error
            // carries no errno, so it collapses onto the same code the
            // kernel uses where abi-v1 has no dedicated one.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production diagnostics stream (fd 2), keeping the report on fd 1
    /// clean for pipes.
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` when every operand was described, `1` when one was
    /// refused or the output failed, `2` on a usage error (a malformed
    /// argument vector, an unrecognised option, or a format naming a
    /// directive this platform cannot serve).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            let _ = Stderr.write_all(USAGE.as_bytes());
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("stat: {err}"));
                let _ = Stderr.write_all(USAGE.as_bytes());
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let out = RtOutput;
        let err = RtErrors;
        let reporter = Reporter {
            fs: &RtFilesystem,
            mounts: &RtMounts,
            names: &RtNames,
            out: &out,
            err: &err,
        };
        match run(command, locale, &reporter, &BundleHelp::new("stat")) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(err) => {
                write_stderr_line(&format!("stat: {err}"));
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
