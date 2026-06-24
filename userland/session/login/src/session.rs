//! Login's identity types and the seams through which it reaches the
//! outside world.
//!
//! The [`Login`](crate::Login) state machine is otherwise pure: it drives
//! the prompt/authenticate/launch sequence and decides *what* to do, but it
//! never itself talks to the kernel, the credential store, or the terminal.
//! Three injected traits do that:
//!
//! * [`Prompt`] reads the username and password from, and writes prompts to,
//!   the controlling terminal.
//! * [`Authenticator`] verifies a [`Credentials`] pair against `kernel/sec`
//!   and the credential store, returning the [`AuthenticatedUser`] on success.
//! * [`SessionLauncher`] starts the chosen [`SessionKind`] under the
//!   authenticated user's identity and capability ceiling.
//!
//! On a running kernel these are backed by syscalls and `kernel/sec`; in
//! tests they are in-memory fixtures. This mirrors `init`'s `Spawner`/`Reaper`
//! split and keeps every control-flow and policy decision testable without a
//! kernel.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_caps::CapabilitySet;

pub use rustos_users::{Gid, Uid};

/// Maximum accepted input-line length, in bytes, for every prompt read —
/// a validation bound on untrusted console input,
/// matching the `users-v1` format's own per-line bound. A longer line is
/// refused by the [`Prompt`] implementation, never silently truncated.
pub const INPUT_LINE_MAX: usize = 512;

/// A username and the password offered for it.
///
/// Both fields **borrow** the caller's stack line buffers rather than
/// owning heap strings: the whole prompt → authenticate path runs without
/// an allocator, because the userland heap (the `mem_map` syscall's
/// production producer, `plans/SPAWN.md` `SP5b`) is not required to read a
/// keystroke. Login holds the password only for as long as it takes the
/// [`Authenticator`] to verify it, then zeroes the backing buffer itself
/// (nothing that held a credential survives the attempt);
/// it is never logged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Credentials<'a> {
    /// The offered account name.
    pub username: &'a str,
    /// The offered secret. Never logged.
    pub password: &'a str,
}

/// The identity `kernel/sec` resolves a successful authentication to.
///
/// This is the `(uid, primary gid, supplementary gids, capability grants)`
/// tuple of. The `capabilities` field is the user's grant
/// **ceiling**: when [`SessionLauncher::launch`] execs a binary, the loader
/// intersects this ceiling with the binary's signed manifest request. Login never widens it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedUser {
    /// The authenticated user's id.
    pub uid: Uid,
    /// The user's primary group.
    pub primary_gid: Gid,
    /// The user's supplementary groups.
    pub supplementary_gids: Vec<Gid>,
    /// The maximum capability set this user may exercise this session.
    pub capabilities: CapabilitySet,
    /// Absolute path of the user's shell of choice —
    /// the program [`SessionLauncher::launch`] starts as the text
    /// session. Comes from the account's `/System/Security/Users`
    /// record; login never substitutes its own default.
    pub shell: String,
}

/// Which kind of session to launch after a successful authentication.
///
/// Login always offers **text** first; the graphical option
/// is presented only when a display driver and the window manager are present
/// (see [`LoginConfig::graphical_available`](crate::LoginConfig)).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionKind {
    /// A text shell session.
    Text,
    /// A graphical desktop session.
    Graphical,
}

impl SessionKind {
    /// Interpret the user's free-text session choice.
    ///
    /// The default is always [`SessionKind::Text`]: an empty
    /// line, an unrecognised answer, or any explicit text choice selects it.
    /// Only an explicit graphical answer (`g`, `graphical`, or `2`, in any
    /// case, surrounding whitespace ignored) selects [`SessionKind::Graphical`].
    #[must_use]
    pub fn from_choice(answer: &str) -> Self {
        let trimmed = answer.trim();
        let graphical = trimmed.eq_ignore_ascii_case("g")
            || trimmed.eq_ignore_ascii_case("graphical")
            || trimmed == "2";
        if graphical {
            Self::Graphical
        } else {
            Self::Text
        }
    }

    /// Stable lowercase label used in audit records.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Graphical => "graphical",
        }
    }
}

/// What a finished session reports back to login.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SessionOutcome {
    /// The kind of session that ran.
    pub kind: SessionKind,
    /// The session's exit code.
    pub exit_code: i32,
}

/// The controlling terminal: login's only channel to the user.
///
/// The implementation owns the real device I/O; in particular [`read_secret`]
/// must read the password **without echoing it** (secrets
/// are not displayed). Login never performs ambient terminal I/O itself.
///
/// Both reads fill a **caller-provided** buffer instead of returning an
/// owned string, so the prompt path takes no allocator (
/// — the userland heap is not required to read a keystroke; the buffer is
/// the caller's stack). Login validates the filled bytes as UTF-8 itself
/// (every input validated, in one place).
///
/// [`read_secret`]: Prompt::read_secret
pub trait Prompt {
    /// Write a prompt or message to the terminal.
    fn write(&self, text: &str);

    /// Read one echoed line of input (the username, the session choice)
    /// into `buf`, returning the number of bytes filled. The line
    /// terminator is not stored.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] if the terminal cannot be
    /// read (closed, timed out, …) — login treats this as fatal and fails
    /// closed — or [`Errno::LengthOutOfRange`] for a line longer than
    /// `buf`, which is refused rather than truncated.
    fn read_line(&self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Read one line of input **without echoing it** (the password) into
    /// `buf`, returning the number of bytes filled. The line terminator is
    /// not stored.
    ///
    /// # Errors
    ///
    /// As [`Prompt::read_line`]: the implementation's [`Errno`] when the
    /// terminal cannot be read, [`Errno::LengthOutOfRange`] for an
    /// over-long line.
    fn read_secret(&self, buf: &mut [u8]) -> Result<usize, Errno>;
}

/// Verifies credentials against `kernel/sec` and the credential store.
///
/// The implementation reads `/System/Security/Users` and
/// checks the offered password against the stored hash using `lib/crypto`'s
/// constant-time primitives; login itself never
/// sees the stored hash. On success it returns the user's full identity and
/// capability ceiling.
pub trait Authenticator {
    /// Verify `credentials`, returning the authenticated identity on success.
    ///
    /// # Errors
    ///
    /// Returns an [`Errno`] (typically [`Errno::PermissionDenied`]) when the
    /// credentials are rejected. The implementation must return the **same**
    /// error whether the account is unknown or the password is wrong, so a
    /// caller cannot probe for valid usernames (fail closed,
    /// no information leak). Login does not inspect the cause.
    fn authenticate(&self, credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno>;
}

/// Starts a session under an authenticated user's identity.
///
/// The implementation drops to the user's `(uid, gid, supplementary gids)`
/// and the capability ceiling carried by [`AuthenticatedUser`], then execs
/// the shell (text) or the window manager (graphical). It blocks until the
/// session ends and returns the [`SessionOutcome`].
pub trait SessionLauncher {
    /// Launch `kind` for `user`, returning when the session ends.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim if the session cannot
    /// be started (binary missing, exec refused, …). Login records it and
    /// fails closed rather than retry.
    fn launch(&self, user: &AuthenticatedUser, kind: SessionKind) -> Result<SessionOutcome, Errno>;
}

#[cfg(test)]
mod tests {
    use super::{Gid, SessionKind, Uid};

    #[test]
    fn id_newtypes_round_trip() {
        assert_eq!(Uid(0).0, 0);
        assert_eq!(Gid(1000).0, 1000);
    }

    #[test]
    fn session_choice_defaults_to_text() {
        assert_eq!(SessionKind::from_choice(""), SessionKind::Text);
        assert_eq!(SessionKind::from_choice("  "), SessionKind::Text);
        assert_eq!(SessionKind::from_choice("t"), SessionKind::Text);
        assert_eq!(SessionKind::from_choice("text"), SessionKind::Text);
        assert_eq!(SessionKind::from_choice("yes please"), SessionKind::Text);
    }

    #[test]
    fn session_choice_recognises_graphical() {
        assert_eq!(SessionKind::from_choice("g"), SessionKind::Graphical);
        assert_eq!(SessionKind::from_choice("G"), SessionKind::Graphical);
        assert_eq!(
            SessionKind::from_choice("  Graphical  "),
            SessionKind::Graphical
        );
        assert_eq!(SessionKind::from_choice("2"), SessionKind::Graphical);
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(SessionKind::Text.label(), "text");
        assert_eq!(SessionKind::Graphical.label(), "graphical");
    }
}
