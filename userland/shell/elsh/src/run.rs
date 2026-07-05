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
//! `spawn` syscall and reaps them through `wait`. A command word is resolved
//! to a runnable bundle through the shared candidate policy
//! ([`rustos_cmdres::resolution_candidates`]): the system app store first,
//! then the user's `PATH`, attempted in order. The command's words travel
//! to the child as its argument vector and the shell's exported variables
//! (with any `NAME=v cmd` prefix overrides) as its environment, through
//! the `spawn` startup-strings block. Pipes and redirections need
//! descriptor plumbing the ABI does not yet express, so `RtProcessHost`
//! fails them closed rather than silently dropping them.
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
    use alloc::vec::Vec;

    use rustos_abi::elevate::{
        elevate_endpoint, ElevateReply, ElevateRequest, ELEVATE_MAX_REQUEST, ELEVATE_REPLY_LEN,
    };
    use rustos_abi::{Errno, InputMode, LimitKind, ResourceLimit};
    use rustos_elsh::{
        parse_invocation, Console, Elevator, Environment, Invocation, LaunchSpec, LimitStore, Pid,
        ProcessHost, ReplInput, Shell, Signal, WaitOutcome, USAGE,
    };
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_rt::io::{write_stderr_line, Stderr, Stdout, Write};

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
    /// syscalls (`plans/SPAWN.md` SP3 / SP6), resolving the command word to
    /// a bundle `Run` path through the shared candidate policy
    /// (`plans/APPS.md` §8: the system app store first, then `PATH`).
    struct RtProcessHost;

    impl ProcessHost for RtProcessHost {
        fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Pid, Errno> {
            // The `spawn` ABI carries a program path plus the child's
            // argument vector and environment. Pipes and redirections need
            // descriptor plumbing the ABI does not yet express; they are
            // refused rather than silently dropped — an ABI extension, not
            // a shortcut here.
            let [command] = spec.commands else {
                return Err(Errno::NotImplemented);
            };
            if !command.redirections.is_empty() {
                return Err(Errno::NotImplemented);
            }
            let Some(word) = command.argv.first() else {
                return Err(Errno::NotImplemented);
            };
            // The child's environment: the shell's exported variables with
            // this command's `NAME=v cmd` prefix assignments layered on top
            // (an override replaces the export of the same name), each
            // encoded in the conventional `NAME=value` spelling the child's
            // runtime splits at the first `=`.
            let mut env: Vec<(&str, &str)> = spec.env.to_vec();
            for (name, value) in &command.env_overrides {
                match env.iter_mut().find(|(seen, _)| *seen == name.as_str()) {
                    Some(entry) => entry.1 = value.as_str(),
                    None => env.push((name.as_str(), value.as_str())),
                }
            }
            let env_entries: Vec<String> = env
                .iter()
                .map(|(name, value)| alloc::format!("{name}={value}"))
                .collect();
            let env_bytes: Vec<&[u8]> = env_entries.iter().map(|entry| entry.as_bytes()).collect();
            // The child's argument vector is the command's words verbatim
            // (`argv[0]` is the typed word) — data for the child's own
            // parser, never authority.
            let arg_bytes: Vec<&[u8]> = command.argv.iter().map(|word| word.as_bytes()).collect();
            // Resolve the word to its candidate program paths (the system
            // app store, then the exported `PATH`) and attempt each in
            // order. `spawn`'s `NotFound` is a definitive "no program is
            // registered at this path, nothing ran", so moving to the next
            // candidate is a deterministic first-match search, never a
            // retry; any other refusal (a permission or capability denial,
            // a malformed image) is final and reported verbatim. The
            // kernel authorises every attempt — a candidate spelling grants
            // nothing.
            let path_var = spec
                .env
                .iter()
                .find(|(name, _)| *name == "PATH")
                .map(|(_, value)| *value);
            for candidate in rustos_cmdres::resolution_candidates(word, path_var) {
                let ret = rustos_rt::spawn_with(
                    candidate.as_bytes(),
                    rustos_abi::CONSOLE_INHERIT,
                    rustos_abi::SPAWN_UID_INHERIT,
                    &arg_bytes,
                    &env_bytes,
                );
                if ret >= 0 {
                    // `ret >= 0` here, so the cast preserves the PID value.
                    #[allow(clippy::cast_sign_loss)]
                    return Ok(Pid::new(ret as u64));
                }
                let err = errno_from(ret);
                if err != Errno::NotFound {
                    return Err(err);
                }
            }
            Err(Errno::NotFound)
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

    /// The `elevate` builtin's production seam (`plans/CAPABILITY_USE.md`
    /// CU5): posts one synchronous request to this console's login
    /// supervisor and blocks until the re-authenticated command has run.
    ///
    /// The shell holds no elevation authority — it derives the rendezvous
    /// from its **own** kernel-attested console (`self_origin`, never a
    /// claim), and the supervisor re-authenticates the offered credentials
    /// before anything runs. A process with no console-backed streams has no
    /// rendezvous and fails closed before posting.
    struct RtElevator;

    impl RtElevator {
        /// Read one edited input line (without its terminator) from standard
        /// input — the read line discipline's **buffer** half
        /// ([`rustos_vt::line::LineEditor`]) over `rustos_rt::stdin`, exactly
        /// as the REPL reads a command line. A zero-length read means the
        /// stream closed and fails closed; a line longer than `buf` is
        /// refused, never truncated.
        fn read_line_raw(buf: &mut [u8]) -> Result<usize, Errno> {
            let mut editor = rustos_vt::line::LineEditor::new();
            let mut len = 0;
            let mut byte = [0u8; 1];
            loop {
                if rustos_rt::stdin(&mut byte) == 0 {
                    return Err(Errno::NotFound);
                }
                match editor.push(buf, &mut len, byte[0]) {
                    rustos_vt::line::LineFeed::Pending => {}
                    rustos_vt::line::LineFeed::Complete => return Ok(len),
                    rustos_vt::line::LineFeed::TooLong => return Err(Errno::LengthOutOfRange),
                }
            }
        }
    }

    impl Elevator for RtElevator {
        fn read_secret(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            // A credential must never render: select the secret discipline
            // for the read and fail closed if it cannot be selected.
            let toggled = rustos_rt::set_input_mode(InputMode::Secret);
            if toggled < 0 {
                return Err(errno_from(toggled));
            }
            let result = Self::read_line_raw(buf);
            // Restoring the cooked default is best-effort — it cannot
            // compromise the secret already read. The un-echoed Return key
            // advanced no line, so advance one ourselves with a plain line
            // feed, which the console line discipline cooks to CR-LF.
            let _ = rustos_rt::set_input_mode(InputMode::Cooked);
            let _ = Stdout.write_all(b"\n");
            result
        }

        fn elevate(&self, username: &str, password: &str, program: &str) -> Result<i32, Errno> {
            let console = rustos_rt::self_origin().map_err(errno_from)?.console();
            // `elevate_endpoint` refuses the "no console" sentinel, so a
            // stream-fed shell (a pipe, a network session) cannot name a
            // rendezvous it is not sitting on.
            let endpoint = elevate_endpoint(console)?;
            let request = ElevateRequest {
                username,
                password,
                program,
            };
            let mut request_buf = [0u8; ELEVATE_MAX_REQUEST];
            let encoded = match request.encode(&mut request_buf) {
                Ok(len) => len,
                Err(err) => {
                    request_buf.fill(0);
                    return Err(err);
                }
            };
            let mut reply_buf = [0u8; ELEVATE_REPLY_LEN];
            let posted = rustos_rt::ipc_call(endpoint, &request_buf[..encoded], &mut reply_buf);
            // The request carries the offered password: zero it as soon as
            // the exchange resolves, before the reply is even decoded.
            request_buf.fill(0);
            let reply_len = posted.map_err(errno_from)?;
            match ElevateReply::decode(&reply_buf[..reply_len])? {
                ElevateReply::Completed { exit_code } => Ok(exit_code),
                ElevateReply::Refused(err) => Err(err),
            }
        }
    }

    /// Render `elsh`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the shell's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("elsh"), locale, "elsh")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Runs the interpreter as a read-eval-print loop over the inherited
    /// standard streams and returns the session's exit code (the `exit`
    /// builtin's code, or `0` when the input stream ends). The reserved
    /// `-h`/`-?` short-help switches render the shell's own Help document
    /// and exit `0`; any other argument is a usage error and exits `2`.
    /// The loop binds only to fd 0/1/2/3, never a device.
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        match parse_invocation(&arguments) {
            Ok(Invocation::Repl) => {}
            Ok(Invocation::Help) => return short_help(),
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        }
        let console = RtConsole;
        let host = RtProcessHost;
        let limits = RtLimitStore;
        let elevator = RtElevator;
        let mut input = RtInput;
        // Seed the interactive session from the environment login exported
        // (USER, HOME, SHELL, PATH, TERM, LANG, …), filling the shell-owned
        // defaults (HOSTNAME, PWD/OLDPWD, ELSH_PROMPT) so the prompt shows
        // `user@host cwd% ` and `$USER`/`$HOME`/… are present from the first
        // line.
        let mut env = Environment::new();
        env.seed_interactive(|name| {
            rustos_rt::env_var(name.as_bytes())
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        });
        let mut shell = Shell::with_environment(&host, &console, env)
            .with_limits(&limits)
            .with_elevator(&elevator);
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
