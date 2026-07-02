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
use rustos_resref::{KnownNamespace, ResourceRef};

use crate::error::ParseError;
use crate::job::{Pid, Signal, WaitOutcome};
use crate::parser::OpenMode;

/// A single primitive descriptor action, applied to one descriptor, in source
/// order.
///
/// The parser's higher-level [`Redirection`](crate::parser::Redirection) forms
/// are lowered to these primitives before reaching the host: a combined `&>`
/// becomes an open on fd 1 followed by a duplication of fd 1 onto fd 2, exactly
/// as a POSIX shell would apply it. The host therefore never has to re-derive
/// redirection semantics — it just opens, duplicates, or closes descriptors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirAction {
    /// Open `target` with `mode` on the descriptor.
    Open {
        /// How to open the target.
        mode: OpenMode,
        /// The expanded, classified target (a path or a resource reference).
        target: RedirTarget,
    },
    /// Make the descriptor a duplicate of `source`.
    Dup {
        /// The descriptor to alias.
        source: u32,
    },
    /// Close the descriptor.
    Close,
    /// Feed `content` as the descriptor's input (a here-string, `<<< word`).
    /// `content` is the fully-expanded body with its trailing newline already
    /// appended, so the host supplies it verbatim as a read backing without
    /// re-deriving the here-string's shape.
    HereString {
        /// The exact bytes the descriptor reads, trailing newline included.
        content: String,
    },
}

/// A redirection's fully-expanded target, classified into the two worlds a
/// `:`-bearing string can name.
///
/// RustOS has no `/dev`: the byte sinks and sources a redirection can name
/// (`sys:null`, `sys:zero`, …) are resource references, not device files. The
/// shell decides which world a target belongs to *before* any filesystem
/// lookup, so a real on-disk file whose name happens to contain `:` (legal on
/// ext/POSIX volumes, where only `NUL` and `/` are forbidden) always stays
/// reachable as `./name` or when quoted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirTarget {
    /// A filesystem path (an ordinary path or an alias path). The host
    /// resolves it through the VFS.
    Path(String),
    /// A resource reference (`sys:null`, `tty:debug`, …) the host resolves to
    /// a kernel stream backing rather than a path. Only the *spelling* is
    /// decided here; resolving the reference to a backing is a
    /// capability-checked step the host owns, never a filesystem lookup.
    Resource(ResourceRef),
}

/// Classify a fully-expanded redirection `target` into a [`RedirTarget`].
///
/// Applies the shell's target resolution rule: a target is a resource
/// reference only when it is a relative path whose first path component holds
/// a `:` preceded by a registered resource namespace and not immediately
/// followed by `/` (the spelling that tells `sys:null` apart from the
/// `Alias:/path` alias-path form), and whose prefix is neither `.` nor `..`.
/// Every other target — absolute, dot-relative, sub-path, or an unregistered
/// prefix — is a path, so nothing on disk is ever shadowed.
///
/// A target the rule classifies as a resource reference but that is not a
/// well-formed reference fails closed: the shell never falls back to creating
/// a file, so a typo cannot silently produce junk on disk.
///
/// # Errors
///
/// [`ParseError::InvalidResourceTarget`] for a registered-namespace target
/// that is not a valid resource reference.
pub(crate) fn classify_redirect_target(target: String) -> Result<RedirTarget, ParseError> {
    if names_resource_reference(&target) {
        match rustos_resref::parse(&target) {
            Ok(reference) => Ok(RedirTarget::Resource(reference)),
            Err(_) => Err(ParseError::InvalidResourceTarget),
        }
    } else {
        Ok(RedirTarget::Path(target))
    }
}

/// The structural half of the resolution rule: does `target`'s spelling name a
/// resource reference rather than a path? See [`classify_redirect_target`].
fn names_resource_reference(target: &str) -> bool {
    // Absolute paths are always paths.
    if target.starts_with('/') {
        return false;
    }
    // The rule inspects only the first path component (up to the first `/`).
    let first_component_end = target.find('/').unwrap_or(target.len());
    let first_component = &target[..first_component_end];
    let Some(colon) = first_component.find(':') else {
        return false;
    };
    let prefix = &first_component[..colon];
    if prefix == "." || prefix == ".." {
        return false;
    }
    if KnownNamespace::from_name(prefix).is_none() {
        return false;
    }
    // A `:` immediately followed by `/` is the alias-path form (`Home:/x`), a
    // path, not a reference. Tested against the original target so the `/`
    // that ended the first component still counts.
    !target[colon + 1..].starts_with('/')
}

/// A resolved redirection: the descriptor it acts on and the action to apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRedirection {
    /// The descriptor this action binds.
    pub fd: u32,
    /// What to do to `fd`.
    pub action: RedirAction,
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
/// `rxe` verification, capability handoff) and the
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
    /// directory, and is permitted: the shell holds no
    /// ambient filesystem authority of its own.
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

/// Reads and imposes the calling process's resource limits, the seam the `ulimit` builtin drives.
///
/// On a running kernel this is backed by the `rlimit_get` / `rlimit_set`
/// syscalls ([`rustos_abi::SyscallNumber::RLIMIT_GET`] /
/// [`rustos_abi::SyscallNumber::RLIMIT_SET`]); in tests it is an in-memory
/// fixture. The shell holds no ambient authority of its own: reading a limit needs no capability, but *raising* a hard bound is
/// gated kernel-side on [`rustos_abi::CapabilityId::RLIMIT_RAISE`],
/// which surfaces here as an [`Errno`] the builtin reports rather than
/// hides.
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
    /// [`rustos_abi::CapabilityId::RLIMIT_RAISE`], or
    /// [`Errno::OutOfRange`] for a malformed pair.
    fn set(&self, kind: LimitKind, value: ResourceLimit) -> Result<(), Errno>;
}

/// A fail-closed [`LimitStore`]: every operation reports
/// [`Errno::NotImplemented`].
///
/// A [`Shell`](crate::Shell) built without a real limit seam uses this, so
/// `ulimit` denies rather than pretending a get or set landed. The real seam is installed with
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
    use super::{
        classify_redirect_target, LaunchSpec, RedirAction, RedirTarget, ResolvedCommand,
        ResolvedRedirection,
    };
    use crate::error::ParseError;
    use crate::parser::OpenMode;
    use alloc::string::ToString;
    use alloc::vec;
    use rustos_resref::KnownNamespace;

    #[test]
    fn launch_spec_borrows_resolved_commands() {
        let commands = vec![ResolvedCommand {
            argv: vec!["echo".to_string(), "hi".to_string()],
            redirections: vec![ResolvedRedirection {
                fd: 1,
                action: RedirAction::Open {
                    mode: OpenMode::Write { clobber: false },
                    target: RedirTarget::Path("out".to_string()),
                },
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

    /// The rule's happy path: a bare registered namespace resolves to a
    /// resource reference, not a file.
    #[test]
    fn registered_namespace_target_is_a_resource_reference() {
        for target in ["sys:null", "sys:zero", "sys:full", "sys:random"] {
            match classify_redirect_target(target.to_string()) {
                Ok(RedirTarget::Resource(reference)) => {
                    assert_eq!(reference.namespace().known(), Some(KnownNamespace::Sys));
                }
                other => panic!("{target} should be a resource reference, got {other:?}"),
            }
        }
    }

    /// Every spelling the rule keeps on the path side: an alias path (`:` then
    /// `/`), an absolute path, a dot-relative path, a sub-path whose first
    /// component has no `:`, and an unregistered prefix.
    #[test]
    fn path_spellings_stay_paths() {
        for target in [
            "Home:/notes",
            "/sys:random",
            "./sys:random",
            "foo/sys:random",
            "foo:bar",
            "sys:/foo",
            "mylisting.txt",
        ] {
            assert_eq!(
                classify_redirect_target(target.to_string()),
                Ok(RedirTarget::Path(target.to_string())),
                "{target} should stay a path",
            );
        }
    }

    /// A registered-namespace prefix with a malformed reference fails closed
    /// rather than falling back to creating a file.
    #[test]
    fn malformed_registered_reference_fails_closed() {
        // A guard delimiter with no fingerprint is a grammar violation the
        // reference parser rejects; the shell must not create a file for it.
        assert_eq!(
            classify_redirect_target("sys:null@".to_string()),
            Err(ParseError::InvalidResourceTarget),
        );
    }
}
