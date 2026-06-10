//! The seams through which the shell reaches the outside world.
//!
//! The [`Shell`](crate::Shell) interpreter is otherwise pure: it parses,
//! expands, and decides *what* to run, but it never itself talks to the
//! kernel or a terminal. Two injected traits do that:
//!
//! * [`ProcessHost`] launches a resolved pipeline, waits on a foreground job,
//!   signals a job (for `fg`/`bg`), and polls for background state changes.
//! * [`Console`] carries the shell's and its builtins' text output.
//!
//! On a running kernel these are backed by syscalls (`spawn`/`wait`/`kill`
//! and the standard-stream writes); in tests they are in-memory fixtures.
//! This mirrors `init`'s `Spawner`/`Reaper` split and keeps every
//! security-relevant and control-flow decision testable without a kernel.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::{Errno, LimitKind, ResourceLimit};

use crate::job::{Pid, Signal, WaitOutcome};
use crate::parser::RedirectionKind;

/// A redirection with its target path already expanded to a string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRedirection {
    /// Which stream is redirected, and how.
    pub kind: RedirectionKind,
    /// The expanded target path.
    pub target: String,
}

/// One command of a pipeline, with every word expanded to its final string.
///
/// `argv` is non-empty: the shell never asks the host to launch a command
/// with no program name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCommand {
    /// The program name (`argv[0]`) and its arguments.
    pub argv: Vec<String>,
    /// Redirections to apply, in source order.
    pub redirections: Vec<ResolvedRedirection>,
}

/// A fully-resolved pipeline ready for the [`ProcessHost`] to launch.
#[derive(Clone, Copy, Debug)]
pub struct LaunchSpec<'a> {
    /// The pipeline's commands, left to right. Length ≥ 1.
    pub commands: &'a [ResolvedCommand],
    /// The exported environment the children inherit, as `(name, value)`.
    pub env: &'a [(&'a str, &'a str)],
    /// `true` if the pipeline is launched in the background (`&`).
    pub background: bool,
}

/// Launches and controls the child processes a pipeline becomes.
///
/// The implementation owns the trusted load pipeline (path resolution,
/// `rxe` verification, capability handoff — `AGENTS.md` §8, §16.4) and the
/// plumbing of pipes and redirections. The shell hands it a fully-resolved
/// [`LaunchSpec`] and never performs ambient I/O itself.
pub trait ProcessHost {
    /// Launch a pipeline, returning the process-group leader's [`Pid`].
    ///
    /// # Errors
    ///
    /// Returns the host's [`Errno`] verbatim if the pipeline cannot be
    /// launched (program not found, permission denied, a redirection target
    /// that cannot be opened, …).
    fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Pid, Errno>;

    /// Block until the foreground job led by `pid` exits or stops.
    ///
    /// # Errors
    ///
    /// Returns the host's [`Errno`] if the job cannot be awaited.
    fn wait(&self, pid: Pid) -> Result<WaitOutcome, Errno>;

    /// Deliver `signal` to the job led by `pid` (used by `fg`/`bg`).
    ///
    /// # Errors
    ///
    /// Returns the host's [`Errno`] if the signal cannot be delivered.
    fn signal(&self, pid: Pid, signal: Signal) -> Result<(), Errno>;

    /// Report the next background job that changed state since the last
    /// call, or `None` if none have. Must not block.
    fn poll(&self) -> Option<(Pid, WaitOutcome)>;

    /// Change the working directory to `path`, returning the resolved
    /// absolute directory on success (used by the `cd` builtin).
    ///
    /// The host — not the shell — validates that the target exists, is a
    /// directory, and is permitted (`AGENTS.md` §5.3): the shell holds no
    /// ambient filesystem authority of its own (`AGENTS.md` §4).
    ///
    /// # Errors
    ///
    /// Returns the host's [`Errno`] if the directory cannot be entered.
    fn change_directory(&self, path: &str) -> Result<String, Errno>;
}

/// The shell's text output sink: standard output and standard error.
pub trait Console {
    /// Write to standard output.
    fn write_stdout(&self, text: &str);
    /// Write to standard error.
    fn write_stderr(&self, text: &str);
}

/// Reads and imposes the calling process's resource limits (`AGENTS.md`
/// §24.3), the seam the `ulimit` builtin drives.
///
/// On a running kernel this is backed by the `rlimit_get` / `rlimit_set`
/// syscalls ([`rustos_abi::SyscallNumber::RLIMIT_GET`] /
/// [`rustos_abi::SyscallNumber::RLIMIT_SET`]); in tests it is an in-memory
/// fixture. The shell holds no ambient authority of its own (`AGENTS.md`
/// §4): reading a limit needs no capability, but *raising* a hard bound is
/// gated kernel-side on [`rustos_abi::CapabilityId::RLIMIT_RAISE`] (§24.3),
/// which surfaces here as an [`Errno`] the builtin reports rather than
/// hides (`AGENTS.md` §2.9).
pub trait LimitStore {
    /// Read the effective [`ResourceLimit`] for resource `kind`.
    ///
    /// # Errors
    ///
    /// Returns the host's [`Errno`] if the limit cannot be read (e.g. the
    /// kernel returned a malformed pair, which fails closed).
    fn get(&self, kind: LimitKind) -> Result<ResourceLimit, Errno>;

    /// Impose `value` as the limit for resource `kind`.
    ///
    /// # Errors
    ///
    /// Returns the host's [`Errno`] if the limit cannot be set —
    /// [`Errno::PermissionDenied`] when raising a hard bound without
    /// [`rustos_abi::CapabilityId::RLIMIT_RAISE`] (§24.3), or
    /// [`Errno::OutOfRange`] for a malformed pair.
    fn set(&self, kind: LimitKind, value: ResourceLimit) -> Result<(), Errno>;
}

/// A fail-closed [`LimitStore`]: every operation reports
/// [`Errno::NotImplemented`].
///
/// A [`Shell`](crate::Shell) built without a real limit seam uses this, so
/// `ulimit` denies rather than pretending a get or set landed (`AGENTS.md`
/// §2.9, §5.4). The real seam is installed with
/// [`Shell::with_limits`](crate::Shell::with_limits).
pub(crate) struct NullLimitStore;

impl LimitStore for NullLimitStore {
    fn get(&self, _kind: LimitKind) -> Result<ResourceLimit, Errno> {
        Err(Errno::NotImplemented)
    }

    fn set(&self, _kind: LimitKind, _value: ResourceLimit) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared fail-closed default limit seam (see [`NullLimitStore`]).
pub(crate) static NULL_LIMIT_STORE: NullLimitStore = NullLimitStore;

#[cfg(test)]
mod tests {
    use super::{LaunchSpec, ResolvedCommand, ResolvedRedirection};
    use crate::parser::RedirectionKind;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn launch_spec_borrows_resolved_commands() {
        let commands = vec![ResolvedCommand {
            argv: vec!["echo".to_string(), "hi".to_string()],
            redirections: vec![ResolvedRedirection {
                kind: RedirectionKind::OutputTruncate,
                target: "out".to_string(),
            }],
        }];
        let env = vec![("PATH", "/Apps")];
        let spec = LaunchSpec {
            commands: &commands,
            env: &env,
            background: true,
        };
        assert_eq!(spec.commands.len(), 1);
        assert_eq!(spec.commands[0].argv, ["echo", "hi"]);
        assert_eq!(spec.env, [("PATH", "/Apps")]);
        assert!(spec.background);
    }
}
