//! The `Run` entry-point binary of the `flock` tool — the program a shell
//! spawns to serialise a command against other runs of itself.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` opens the lock file, takes a whole-file advisory lock on it, and
//! runs the command while holding it. The lock lives on the *open file
//! description* the handle owns, so it is released when this process ends —
//! by returning, by a signal, or by a fault. Nothing has to unwind it, and no
//! run can leave a stale lock behind for the next one to time out on.
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
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::{Errno, LockMode, LockRange, OpenFlags, WaitFlags, WaitStatus};
    use tairix_cmdres::{resolution_candidates, CommandEnv};
    use tairix_flock::{parse, run, Mode, Output, Ran, Session, Wait, USAGE};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};
    use tairix_rt::File;

    /// The production [`Session`]: a real lock file and a real child.
    ///
    /// The handle is kept for the whole run, because that — not an explicit
    /// unlock — is what holds the lock: dropping it would release the lock
    /// while the command was still running.
    struct RtSession {
        held: RefCell<Option<File>>,
    }

    impl RtSession {
        fn new() -> Self {
            Self {
                held: RefCell::new(None),
            }
        }
    }

    impl Session for RtSession {
        fn lock(&self, path: &str, mode: Mode, wait: Wait) -> Result<(), Errno> {
            // Read *and* write: an exclusive lock asserts a writer's right
            // and the kernel checks the descriptor was opened for it, and a
            // lock file is created on first use rather than being something
            // the user has to lay down first.
            let flags = OpenFlags::READ
                .union(OpenFlags::WRITE)
                .union(OpenFlags::CREATE);
            let file = File::open(path.as_bytes(), flags).map_err(Errno::from_syscall)?;
            let requested = match mode {
                Mode::Shared => LockMode::Shared,
                Mode::Exclusive => LockMode::Exclusive,
            };
            let whole = LockRange::WHOLE;
            let outcome = match wait {
                Wait::Forever => file.lock(requested, whole),
                Wait::Never => file.try_lock(requested, whole),
                Wait::Until(timeout_ns) => file.lock_timeout(requested, whole, timeout_ns),
            };
            outcome.map_err(Errno::from_syscall)?;
            // Held from here until the process ends.
            *self.held.borrow_mut() = Some(file);
            Ok(())
        }

        fn run(&self, word: &str, args: &[&str]) -> Result<Ran, Errno> {
            // Resolve the command word through the one shared policy every
            // other consumer reads (the fixed system-store prefix, then the
            // user's own stores, then `PATH`), never a private path walk.
            let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
            let path_var =
                tairix_rt::env_var(b"PATH").and_then(|raw| core::str::from_utf8(raw).ok());
            let candidates = resolution_candidates(word, CommandEnv { home, path_var });
            // The child's argument vector is the command word followed by
            // its own arguments, exactly as a shell would hand them over.
            let mut child: Vec<&[u8]> = Vec::with_capacity(args.len() + 1);
            child.push(word.as_bytes());
            for arg in args {
                child.push(arg.as_bytes());
            }
            let env: Vec<&[u8]> = Vec::new();

            let mut last = Errno::NotFound;
            for candidate in &candidates {
                let ret = tairix_rt::spawn_with(
                    candidate.as_bytes(),
                    tairix_abi::CONSOLE_INHERIT,
                    tairix_abi::SPAWN_UID_INHERIT,
                    &child,
                    &env,
                );
                if ret < 0 {
                    last = Errno::from_syscall(ret);
                    // Only "no such program" moves on to the next
                    // candidate; any other refusal is this candidate's own
                    // answer and stands, so a permission problem is never
                    // reported as a missing command.
                    if matches!(last, Errno::NotFound) {
                        continue;
                    }
                    return Ok(Ran::NotExecutable);
                }
                return collect(ret);
            }
            if matches!(last, Errno::NotFound) {
                return Ok(Ran::NotFound);
            }
            Err(last)
        }
    }

    /// Wait for the child `pid` and report the status it left.
    ///
    /// A child that `spawn` admitted may still refuse to load its own image;
    /// that arrives as one of the reserved load-failure statuses, which is
    /// reported as an unrunnable command rather than as the command's own
    /// exit code.
    fn collect(pid: i64) -> Result<Ran, Errno> {
        let mut status = WaitStatus::Exited(0);
        let ret = tairix_rt::wait(pid, &mut status, WaitFlags::empty());
        if ret < 0 {
            return Err(Errno::from_syscall(ret));
        }
        // A stop is not opted into (no `STOPPED` flag), so the only reading
        // the reap can produce is an exit.
        let WaitStatus::Exited(code) = status else {
            return Err(Errno::NotSupported);
        };
        if let Some(reason) = tairix_abi::load_failure_reason(code) {
            write_stderr_line(&format!("flock: {reason}"));
            return Ok(if code == tairix_abi::LOAD_NOT_FOUND {
                Ran::NotFound
            } else {
                Ran::NotExecutable
            });
        }
        Ok(Ran::Exited(code))
    }

    /// The production [`Output`] over the inherited standard output (fd 1).
    struct RtStdout;

    impl Output for RtStdout {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning. The io layer's error
            // carries no errno, so it collapses onto the same code the
            // kernel uses where abi-v1 has no dedicated one.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production [`Output`] for the `-v` notes: standard error, so a
    /// note never contaminates the command's own output stream.
    struct RtStderr;

    impl Output for RtStderr {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Report a usage error: the banner on the standard error stream,
    /// verbatim (it already ends in a newline).
    fn report_usage() {
        let _ = Stderr.write_all(USAGE.as_bytes());
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// The status is the command's own on success, the `-E` conflict code
    /// when the lock could not be taken under `-n`/`-w`, `2` on a usage
    /// error, `127`/`126` for a command word that resolved to nothing or
    /// could not be executed, and `1` on any other failure.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            report_usage();
            return tairix_flock::EXIT_USAGE;
        };
        let Ok(command) = parse(&arguments) else {
            report_usage();
            return tairix_flock::EXIT_USAGE;
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let session = RtSession::new();
        match run(
            &command,
            locale,
            &session,
            &BundleHelp::new("flock"),
            &RtStdout,
            &RtStderr,
        ) {
            Ok(status) => status,
            Err(err) => {
                // Every abnormal exit states its reason, so a script's `$?`
                // is never the only evidence of what went wrong.
                write_stderr_line(&format!("flock: {err}"));
                err.exit_code()
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
