//! The `Run` entry-point binary of the `Shell` application bundle — the program PID 1 `init` launches as the user's
//! session through the `spawn` syscall (`plans/SPAWN.md` `SP3b`,
//! `plans/PI.md` P6e).
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so
//! it links the Rust userland runtime `rustos-rt` — never the C ABI, which
//! exists solely for programs **not** written in Rust.
//! `rustos-rt` provides `_start`, the per-process stack canary, the panic handler, the `mem_map`-backed global allocator, and the
//! syscall wrappers; `rustos_rt::entry!` names this program's `main`.
//!
//! `main` runs the [`rustos_elsh`] interpreter as a read-eval-print loop over
//! its **inherited standard streams**: it reads command
//! lines from standard input (fd 0), writes the prompt and command output to
//! standard output and standard error (fd 1 / fd 2), and emits advisory
//! metadata on the standard information stream (fd 3). It binds to those
//! descriptors only — never a console, UART, or framebuffer — because binding
//! to a device would be ambient authority and hidden coupling; the same binary therefore works whatever the spawner
//! backed the streams with.
//!
//! The interpreter is pure: it decides *what* to run but reaches the outside
//! world only through two injected seams. `RtConsole` carries its output to
//! fd 1 / fd 2, and `RtProcessHost` launches external commands through the
//! `spawn` syscall and reaps them through `wait`. The current `spawn` ABI
//! carries only a program path (no argument vector, environment, pipe, or
//! redirection), so `RtProcessHost` launches a single bare-path command and
//! fails closed on anything it cannot yet express; richer
//! launches await an ABI extension.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
extern crate alloc;

#[cfg(freestanding)]
mod program {
    use alloc::string::String;

    use rustos_abi::{Errno, LimitKind, ResourceLimit};
    use rustos_elsh::{
        Console, LaunchSpec, LimitStore, Pid, ProcessHost, ReplInput, Shell, Signal, WaitOutcome,
    };
    use rustos_rt::io::{Stderr, Stdout, Write};

    /// The shell's output sink, backed by the inherited standard output (fd 1)
    /// and standard error (fd 2) through the shared `rustos_rt::io` layer — the
    /// one `Write::write_all` short-write loop, never a shell-private copy
    /// (the charter forbids that duplication).
    struct RtConsole;

    impl Console for RtConsole {
        fn write_stdout(&self, text: &str) {
            // Output is best-effort: `write_all` loops over short writes and
            // fails closed if the backing stops accepting bytes, and a dropped
            // tail must not abort the session, so the result is discarded.
            let _ = Stdout.write_all(text.as_bytes());
        }

        fn write_stderr(&self, text: &str) {
            let _ = Stderr.write_all(text.as_bytes());
        }
    }

    /// The shell's standard-input (fd 0) and standard-information (fd 3) seam,
    /// backed by `rustos_rt`.
    struct RtInput;

    impl ReplInput for RtInput {
        fn read(&mut self, buf: &mut [u8]) -> usize {
            rustos_rt::stdin(buf)
        }

        fn write_info(&mut self, bytes: &[u8]) {
            // fd 3 is best-effort and ignorable: discard the accepted count.
            let _ = rustos_rt::stdinfo(bytes);
        }
    }

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`, the standard `abi-v1` convention). An unrecognised code
    /// fails closed as [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// Launches and reaps external commands through the `spawn` and `wait`
    /// syscalls (`plans/SPAWN.md` SP3 / SP6).
    struct RtProcessHost;

    impl ProcessHost for RtProcessHost {
        fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Pid, Errno> {
            // The current `spawn` ABI carries only a program path: no argument
            // vector, environment, pipe, or redirection. Anything richer is
            // refused rather than silently dropped; it
            // awaits an ABI extension, not a shortcut here.
            let [command] = spec.commands else {
                return Err(Errno::NotImplemented);
            };
            if !command.redirections.is_empty()
                || !command.env_overrides.is_empty()
                || command.argv.len() != 1
            {
                return Err(Errno::NotImplemented);
            }
            let Some(path) = command.argv.first() else {
                return Err(Errno::NotImplemented);
            };
            let ret = rustos_rt::spawn(path.as_bytes());
            if ret < 0 {
                return Err(errno_from(ret));
            }
            // `ret >= 0` here, so the cast preserves the PID value.
            #[allow(clippy::cast_sign_loss)]
            Ok(Pid::new(ret as u64))
        }

        fn wait(&self, pid: Pid) -> Result<WaitOutcome, Errno> {
            let mut status = 0i32;
            // PIDs fit an `i32` on this ABI; `wait` takes a signed PID.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let ret = rustos_rt::wait(pid.as_u64() as i32, &mut status);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(WaitOutcome::Exited(status))
        }

        fn signal(&self, pid: Pid, signal: Signal) -> Result<(), Errno> {
            // Map the shell's own job-control signal vocabulary onto the
            // `abi-v1` signal set (one definition, no shell-private numbering)
            // and deliver it through the `signal` syscall. The kernel
            // validates that `pid` is a child the shell spawned and fails
            // closed; until its signal producer is installed the call surfaces
            // `NotImplemented` honestly rather than pretending it landed.
            let abi_signal = match signal {
                Signal::Continue => rustos_abi::Signal::Continue,
                Signal::Terminate => rustos_abi::Signal::Terminate,
                Signal::Kill => rustos_abi::Signal::Kill,
            };
            // PIDs fit an `i32` on this ABI; `signal` takes a signed PID.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let ret = rustos_rt::signal(pid.as_u64() as i32, abi_signal);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(())
        }

        fn poll(&self) -> Option<(Pid, WaitOutcome)> {
            // No asynchronous background-state notification exists yet; the
            // shell reaps foreground jobs through `wait`.
            None
        }

        fn change_directory(&self, path: &str) -> Result<String, Errno> {
            // The kernel — not the shell — resolves the path (relative to the
            // process's current working directory), re-authorises it as a
            // searchable directory, and only then moves the process. A refusal
            // surfaces as its `Errno`; the shell holds no ambient filesystem
            // authority of its own.
            let ret = rustos_rt::fs_chdir(path.as_bytes());
            if ret < 0 {
                return Err(errno_from(ret));
            }
            // Report the resolved absolute directory the kernel settled on
            // (for the prompt and `cd`'s echo). A normalised absolute path
            // never exceeds `FS_PATH_MAX`, so this buffer always holds it.
            let mut buf = alloc::vec![0u8; rustos_abi::FS_PATH_MAX];
            let n = rustos_rt::fs_getcwd(&mut buf).map_err(errno_from)?;
            core::str::from_utf8(&buf[..n])
                .map(String::from)
                .map_err(|_| Errno::OutOfRange)
        }
    }

    /// Reads and imposes resource limits through the `rlimit_get` /
    /// `rlimit_set` syscalls, backing the `ulimit`
    /// builtin.
    struct RtLimitStore;

    impl LimitStore for RtLimitStore {
        fn get(&self, kind: LimitKind) -> Result<ResourceLimit, Errno> {
            rustos_rt::rlimit_get(kind).map_err(errno_from)
        }

        fn set(&self, kind: LimitKind, value: ResourceLimit) -> Result<(), Errno> {
            let ret = rustos_rt::rlimit_set(kind, value);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            Ok(())
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Runs the interpreter as a read-eval-print loop over the inherited
    /// standard streams and returns the session's exit code (the `exit`
    /// builtin's code, or `0` when the input stream ends). The loop binds only
    /// to fd 0/1/2/3, never a device.
    fn main() -> i32 {
        let console = RtConsole;
        let host = RtProcessHost;
        let limits = RtLimitStore;
        let mut input = RtInput;
        let mut shell = Shell::new(&host, &console).with_limits(&limits);
        rustos_elsh::run_repl(&mut shell, &console, &mut input)
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
