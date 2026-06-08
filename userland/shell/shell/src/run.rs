//! The `Run` entry-point binary of the `Shell` application bundle
//! (`AGENTS.md` §16.5) — the program PID 1 `init` launches as the user's
//! session through the `spawn` syscall (`plans/SPAWN.md` `SP3b`,
//! `plans/PI.md` P6e).
//!
//! This is a **pure-Rust** program: RustOS is Rust-only (`AGENTS.md` §1), so
//! it links the Rust userland runtime `rustos-rt` — never the C ABI, which
//! exists solely for programs **not** written in Rust (`AGENTS.md` §16.4).
//! `rustos-rt` provides `_start`, the per-process stack canary (`AGENTS.md`
//! §19.2), the panic handler, the `mem_map`-backed global allocator, and the
//! syscall wrappers; `rustos_rt::entry!` names this program's `main`.
//!
//! `main` runs the [`rustos_shell`] interpreter as a read-eval-print loop over
//! its **inherited standard streams** (`AGENTS.md` §20): it reads command
//! lines from standard input (fd 0), writes the prompt and command output to
//! standard output and standard error (fd 1 / fd 2), and emits advisory
//! metadata on the standard information stream (fd 3). It binds to those
//! descriptors only — never a console, UART, or framebuffer — because binding
//! to a device would be ambient authority (`AGENTS.md` §4) and hidden coupling
//! (§17.3 / §17.4); the same binary therefore works whatever the spawner
//! backed the streams with (§20).
//!
//! The interpreter is pure: it decides *what* to run but reaches the outside
//! world only through two injected seams. `RtConsole` carries its output to
//! fd 1 / fd 2, and `RtProcessHost` launches external commands through the
//! `spawn` syscall and reaps them through `wait`. The current `spawn` ABI
//! carries only a program path (no argument vector, environment, pipe, or
//! redirection), so `RtProcessHost` launches a single bare-path command and
//! fails closed (`AGENTS.md` §2.9) on anything it cannot yet express; richer
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

    use rustos_abi::Errno;
    use rustos_shell::{
        Console, LaunchSpec, Pid, ProcessHost, ReplInput, Shell, Signal, WaitOutcome,
    };

    /// The shell's output sink, backed by the inherited standard output (fd 1)
    /// and standard error (fd 2) through `rustos_rt` (`AGENTS.md` §20).
    struct RtConsole;

    /// Write all of `bytes` to standard output, looping over short writes.
    ///
    /// A write that accepts zero bytes means the stream will accept no more
    /// (a closed or full backing); the loop stops rather than spinning
    /// (`AGENTS.md` §2.1). Output is best-effort: a dropped tail does not
    /// abort the session.
    fn write_all_stdout(mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = rustos_rt::stdout(bytes);
            if written == 0 {
                break;
            }
            bytes = &bytes[written.min(bytes.len())..];
        }
    }

    /// Write all of `bytes` to standard error, looping over short writes (see
    /// [`write_all_stdout`]).
    fn write_all_stderr(mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = rustos_rt::stderr(bytes);
            if written == 0 {
                break;
            }
            bytes = &bytes[written.min(bytes.len())..];
        }
    }

    impl Console for RtConsole {
        fn write_stdout(&self, text: &str) {
            write_all_stdout(text.as_bytes());
        }

        fn write_stderr(&self, text: &str) {
            write_all_stderr(text.as_bytes());
        }
    }

    /// The shell's standard-input (fd 0) and standard-information (fd 3) seam,
    /// backed by `rustos_rt` (`AGENTS.md` §20).
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
            // refused rather than silently dropped (`AGENTS.md` §2.9); it
            // awaits an ABI extension, not a shortcut here.
            let [command] = spec.commands else {
                return Err(Errno::NotImplemented);
            };
            if !command.redirections.is_empty() || command.argv.len() != 1 {
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

        fn signal(&self, _pid: Pid, _signal: Signal) -> Result<(), Errno> {
            // There is no signal-delivery syscall yet (`fg`/`bg` resume is
            // future work); fail closed rather than pretend it landed.
            Err(Errno::NotImplemented)
        }

        fn poll(&self) -> Option<(Pid, WaitOutcome)> {
            // No asynchronous background-state notification exists yet; the
            // shell reaps foreground jobs through `wait`.
            None
        }

        fn change_directory(&self, _path: &str) -> Result<String, Errno> {
            // There is no working-directory syscall yet; fail closed so `cd`
            // reports an honest error rather than silently doing nothing.
            Err(Errno::NotImplemented)
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Runs the interpreter as a read-eval-print loop over the inherited
    /// standard streams and returns the session's exit code (the `exit`
    /// builtin's code, or `0` when the input stream ends). The loop binds only
    /// to fd 0/1/2/3, never a device (`AGENTS.md` §20).
    fn main() -> i32 {
        let console = RtConsole;
        let host = RtProcessHost;
        let mut input = RtInput;
        let mut shell = Shell::new(&host, &console);
        rustos_shell::run_repl(&mut shell, &console, &mut input)
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
