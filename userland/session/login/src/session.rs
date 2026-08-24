//! Login's identity types and the seams through which it reaches the
//! outside world.
//!
//! The [`Login`](crate::Login) state machine is otherwise pure: it drives
//! the prompt/authenticate/launch sequence and decides *what* to do, but it
//! never itself talks to the kernel, the credential store, or the terminal.
//! Three injected traits do that:
//!
//! * [`LoginView`] presents the login screen and reads the username and password from
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

use tairix_abi::{BootSession, Errno};
use tairix_caps::CapabilitySet;
use tairix_sysconfig::{LoginType, SystemConfig};

pub use tairix_users::{Gid, Uid};

/// Maximum accepted input-line length, in bytes, for every prompt read —
/// a validation bound on untrusted console input,
/// matching the `users-v1` format's own per-line bound. A longer line is
/// refused by the [`LoginView`] implementation, never silently truncated.
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
    /// The authenticated account's login name (from the account record, not
    /// the raw typed line). Exported to the session as `USER`/`LOGNAME` and
    /// shown in the shell prompt.
    pub username: String,
    /// The authenticated user's id.
    pub uid: Uid,
    /// The user's primary group.
    pub primary_gid: Gid,
    /// The user's supplementary groups.
    pub supplementary_gids: Vec<Gid>,
    /// The maximum capability set this user may exercise this session.
    pub capabilities: CapabilitySet,
    /// Absolute path of the user's home directory (from the account record).
    /// Exported to the session as `HOME` and used as the initial working
    /// directory and the `~` abbreviation in the shell prompt.
    pub home: String,
    /// Absolute path of the user's shell of choice —
    /// the program [`SessionLauncher::launch`] starts as the text
    /// session. Comes from the account's `/System/Security/Users`
    /// record; login never substitutes its own default.
    pub shell: String,
}

/// The default search path exported to a session: the machine-wide installed
/// application store (`/Apps`). The two system stores and the user's own two
/// stores are the fixed, non-overridable prefix the resolution policy itself
/// searches first (`lib/cmdres`), so they must never be repeated here — doing
/// so would be redundant and would misleadingly imply they are just another
/// configurable `PATH` entry. TAIRiX has no `/usr/bin`, so this is the analogue
/// of a POSIX login's default `PATH`.
pub const DEFAULT_PATH: &str = "/Apps";

/// The default `TERM` exported to a session. The system text console
/// (`lib/fbcon`) renders the xterm 256-colour repertoire, so a session
/// starts assuming that terminal type.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// The default `LANG` exported to a session: TAIRiX's default locale (`en-US`),
/// the mandatory canonical help locale.
pub const DEFAULT_LANG: &str = "en-US";

/// Build the environment login hands the session shell, as
/// `NAME=value` byte-spelled entries (the [`crate::session`] launch form
/// consumed by `spawn_with`).
///
/// These are the identity- and locale-carrying variables a POSIX login sets
/// (`USER`, `LOGNAME`, `HOME`, `SHELL`, `PWD`, `PATH`, `TERM`, `LANG`),
/// filled from the authenticated account so the shell's prompt and
/// `$USER`/`$HOME`/… reflect the real user rather than a guess. Shell-owned
/// variables (`OLDPWD`, `ELSH_PROMPT`) and the hostname default are the
/// shell's to fill; login sets only what it authoritatively knows.
#[must_use]
pub fn session_environment(user: &AuthenticatedUser) -> Vec<String> {
    use alloc::format;
    alloc::vec![
        format!("USER={}", user.username),
        format!("LOGNAME={}", user.username),
        format!("HOME={}", user.home),
        format!("SHELL={}", user.shell),
        format!("PWD={}", user.home),
        format!("PATH={DEFAULT_PATH}"),
        format!("TERM={DEFAULT_TERM}"),
        format!("LANG={DEFAULT_LANG}"),
    ]
}

/// Absolute path of the `desktop` application's `Run` binary — the
/// program a [`SessionKind::Graphical`] session launches
/// (`plans/DISPLAY.md` D7c/D7d). The desktop is a system application
/// (`.app` bundle in the system application store, `/System/Applications/`),
/// so the same bundle a graphical login starts is what a shell user runs by
/// typing `desktop`. This is the one OS-wide spelling of its entry point:
/// the launcher spawns it and login's availability probe checks the same
/// path, so the two can never name different bundles.
pub const DESKTOP_SESSION_PATH: &str = "/System/Applications/desktop.app/Run";

/// Absolute path of the graphical login screen's `Run` binary — the
/// service the authority starts as the `greeter` account when a graphical
/// login round begins (`plans/NEW-DESKTOP-LOGIN.md` G3). It draws the login
/// screen and asks the authority's `session-v1` endpoint; it can neither
/// read the user database nor start a process.
///
/// One OS-wide spelling, matching the bundle the image plants under
/// `/System/Services`: the launcher spawns this path and the availability
/// probe checks the same one, so the two can never name different bundles.
pub const GREETER_SERVICE_PATH: &str = "/System/Services/greeter.app/Run";

/// Absolute path of the sandboxed OS font service (`fontd`) `Run` binary — the
/// service the graphical desktop draws text through (`FONT_ENDPOINT`,
/// `plans/FONT-SERVICE.md`). login starts it (as the `fontd` service account)
/// when launching a [`SessionKind::Graphical`] session, so text rendering is a
/// single sandboxed OS resource that runs only when a graphical session needs
/// it — never on a text-only or headless boot. One OS-wide spelling, matching
/// the bundle the image plants under `/System/Services`.
pub const FONTD_SERVICE_PATH: &str = "/System/Services/fontd.app/Run";

/// The program a session of `kind` runs for `user`: the authenticated
/// account's own shell of choice for a text session, the OS desktop
/// session service ([`DESKTOP_SESSION_PATH`]) for a graphical one.
///
/// One definition, so the launcher and every test agree on the mapping.
/// The desktop session is deliberately *not* per-account data: which
/// desktop runs is OS policy (the bundle installed on the volume), while
/// the shell is the account's recorded choice.
#[must_use]
pub fn session_program(user: &AuthenticatedUser, kind: SessionKind) -> &str {
    match kind {
        SessionKind::Text => &user.shell,
        SessionKind::Graphical => DESKTOP_SESSION_PATH,
    }
}

/// Which kind of session to launch after a successful authentication.
///
/// There is no per-login prompt: the kind is system policy, decided by
/// the operator's one-boot Supervisor choice where they made one and
/// otherwise by the administrator-configured `os.loginType` setting (see
/// [`effective_session_kind`]), and a graphical result takes effect only
/// when a graphical session is actually available (see
/// [`LoginConfig::graphical_available`](crate::LoginConfig)) — otherwise
/// the text shell runs. A shell user starts the desktop on demand with
/// the `desktop` command instead.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionKind {
    /// A text shell session.
    Text,
    /// A graphical desktop session.
    Graphical,
}

impl SessionKind {
    /// Stable lowercase label used in audit records.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Graphical => "graphical",
        }
    }
}

/// What a round learned about the system-configuration store.
///
/// The two absences are different facts and must not be collapsed: a
/// machine that simply carries no configuration has told us to use the
/// engine's default, whereas a store whose volume is not up yet has told us
/// nothing at all.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfigStore<'a> {
    /// The store's own location was reachable. `Some` carries the document
    /// found there; `None` means the location exists but holds no
    /// configuration.
    Reachable(Option<&'a [u8]>),
    /// The store's location could not be reached, so nothing was read.
    Unreachable,
}

impl<'a> ConfigStore<'a> {
    /// Classify one attempt to read the store's document.
    ///
    /// Only [`Errno::NotImplemented`] means the store could not answer: a
    /// mount whose backing volume is not online refuses with it and never
    /// falls back to another volume, so it is the one refusal that says
    /// "ask again once the root is up". Every other refusal came from a
    /// live volume — an absent document, a denied read, a fault — leaving a
    /// store that is reachable and simply teaches nothing.
    ///
    /// Absence is deliberately **not** inferred from the document's
    /// directory: `configure` creates that directory on its first write, so
    /// a machine nobody has configured has none, and reading its absence as
    /// an offline volume would withhold the compiled default forever.
    #[must_use]
    pub fn from_read(read: Result<&'a [u8], Errno>) -> Self {
        match read {
            Ok(document) => Self::Reachable(Some(document)),
            Err(Errno::NotImplemented) => Self::Unreachable,
            Err(_) => Self::Reachable(None),
        }
    }
}

/// The administrator-configured boot-default session kind for one round.
///
/// A reachable store decides: its document if it parses, otherwise the
/// configuration engine's own default ([`SystemConfig::default`]), so the
/// default has exactly one definition and login cannot drift from
/// `configure`. The store is untrusted input — the bounded, fail-closed
/// engine parse is the only decoder, and a malformed document never breaks
/// a login.
///
/// An **unreachable** store decides nothing, and the round must not pretend
/// otherwise. Assuming the compiled default there would silently override
/// an administrator who had configured the opposite, purely because their
/// volume had not finished mounting — so the round runs the text prompt,
/// which is always available and never contradicts a stored choice, and
/// re-reads on the next round once the volume is up.
///
/// The result is only a *request*: the caller still intersects it with what
/// this round can actually start, which degrades a graphical answer to
/// [`SessionKind::Text`] rather than erroring.
#[must_use]
pub fn configured_session_kind(store: ConfigStore<'_>) -> SessionKind {
    let ConfigStore::Reachable(document) = store else {
        return SessionKind::Text;
    };
    let config = document
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .and_then(|text| SystemConfig::parse(text).ok())
        .unwrap_or_default();
    match config.login_type {
        LoginType::Text => SessionKind::Text,
        LoginType::Graphical => SessionKind::Graphical,
    }
}

/// The login this round runs, from the operator's one-boot Supervisor
/// choice and the administrator's stored `os.loginType` default.
///
/// The operator is at the machine and their `continue text` / `continue gui` is
/// the more recent instruction, so it wins for this boot only;
/// [`BootSession::Unset`] — a boot the operator never diverted — leaves the
/// stored default in charge. Neither input can force a desktop onto a machine
/// that cannot start one: the caller still intersects the result with what is
/// available this round, which degrades to [`SessionKind::Text`] rather than
/// erroring.
///
/// One definition, so the binary and every test agree on the precedence.
#[must_use]
pub fn effective_session_kind(boot: BootSession, configured: SessionKind) -> SessionKind {
    match boot {
        BootSession::Text => SessionKind::Text,
        BootSession::Graphical => SessionKind::Graphical,
        BootSession::Unset => configured,
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

/// The login screen: login's only channel to the user.
///
/// The implementation owns the real presentation — the full-screen curses
/// view ([`crate::view::CursesView`]) on a running system, an in-memory
/// fixture in tests. The [`Login`](crate::Login) state machine drives it
/// through *semantic* operations (begin a round, read a credential, note a
/// failure), never raw terminal writes, so the same machine runs unchanged
/// over any presentation. In particular [`read_password`] must collect the
/// secret **without displaying it** (secrets are not rendered). Login never
/// performs ambient terminal I/O itself.
///
/// Every read fills a **caller-provided** buffer instead of returning an
/// owned string, so the credential path takes no allocator (the buffer is
/// the caller's stack, zeroed by the caller after the attempt). Login
/// validates the filled bytes as UTF-8 itself (every input validated, in
/// one place).
///
/// [`read_password`]: LoginView::read_password
pub trait LoginView {
    /// Begin one authentication round: draw (or redraw) the whole view and
    /// present the username prompt.
    fn round_begin(&self);

    /// Read the username into `buf`, returning the number of bytes filled.
    /// The line terminator is not stored.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] if the terminal cannot be
    /// read (closed, timed out, …) — login treats this as fatal and fails
    /// closed — or [`Errno::LengthOutOfRange`] for a line longer than
    /// `buf`, which is refused rather than truncated.
    fn read_username(&self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Read the password into `buf` **without displaying it**, returning
    /// the number of bytes filled. The line terminator is not stored.
    ///
    /// # Errors
    ///
    /// As [`LoginView::read_username`]: the implementation's [`Errno`] when
    /// the terminal cannot be read, [`Errno::LengthOutOfRange`] for an
    /// over-long line.
    fn read_password(&self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Record one rejected authentication attempt and show the running
    /// failed-attempt count. The count accumulates across rounds until a
    /// session launches, so whoever is at the console sees every failure
    /// since the last successful login.
    fn note_failure(&self);

    /// Hand the terminal over to the launched session: restore the normal
    /// screen and input discipline. The next [`round_begin`]
    /// (after the session ends) re-enters the view.
    ///
    /// [`round_begin`]: LoginView::round_begin
    fn session_handoff(&self);

    /// Take the terminal back from a session that has ended, discarding
    /// everything it left there — the screen it drew, the screen it was not
    /// showing, a remote emulator's saved scrollback, and the keystrokes it
    /// typed ahead but never read.
    ///
    /// The counterpart of [`session_handoff`], called once per session
    /// boundary however the session ended — cleanly, refused at load, or
    /// never launched at all — because in each case the terminal is about to
    /// be offered to whoever is at it next, and none of one user's session is
    /// theirs to see.
    ///
    /// [`session_handoff`]: LoginView::session_handoff
    fn session_ended(&self);
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

    /// Verify `password` against the account already identified by `uid`,
    /// returning the authenticated identity on success.
    ///
    /// The uid-keyed counterpart of [`Self::authenticate`], used to decide a
    /// [`tairix_abi::elevate::ElevateRequest::Verify`] request: the caller
    /// is re-authenticated against its own **kernel-attested** account,
    /// never a name it supplies, so this method takes no username at all.
    ///
    /// # Errors
    ///
    /// Returns an [`Errno`] (typically [`Errno::PermissionDenied`]) when the
    /// credentials are rejected. The implementation must return the
    /// **same** error whether `uid` owns no account or the password is
    /// wrong, so a caller cannot probe for which uids exist (fail closed,
    /// no information leak). Login does not inspect the cause.
    fn authenticate_uid(&self, uid: u32, password: &str) -> Result<AuthenticatedUser, Errno>;
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
    use super::{
        configured_session_kind, effective_session_kind, session_environment, AuthenticatedUser,
        ConfigStore, Gid, SessionKind, Uid, DEFAULT_LANG, DEFAULT_PATH, DEFAULT_TERM,
    };
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use tairix_abi::{BootSession, Errno};
    use tairix_caps::CapabilitySet;

    #[test]
    fn id_newtypes_round_trip() {
        assert_eq!(Uid(0).0, 0);
        assert_eq!(Gid(1000).0, 1000);
    }

    #[test]
    fn session_environment_carries_the_authenticated_identity() {
        let user = AuthenticatedUser {
            username: "ada".to_string(),
            uid: Uid(1000),
            primary_gid: Gid(1000),
            supplementary_gids: Vec::new(),
            capabilities: CapabilitySet::empty(),
            home: "/Users/ada".to_string(),
            shell: "/System/Commands/elsh.app/Run".to_string(),
        };
        let env = session_environment(&user);
        // The identity is drawn from the account, and PWD starts at home.
        assert!(env.contains(&"USER=ada".to_string()));
        assert!(env.contains(&"LOGNAME=ada".to_string()));
        assert!(env.contains(&"HOME=/Users/ada".to_string()));
        assert!(env.contains(&"SHELL=/System/Commands/elsh.app/Run".to_string()));
        assert!(env.contains(&"PWD=/Users/ada".to_string()));
        // The locale/path/term defaults are the documented OS values.
        assert!(env.contains(&alloc::format!("PATH={DEFAULT_PATH}")));
        assert!(env.contains(&alloc::format!("TERM={DEFAULT_TERM}")));
        assert!(env.contains(&alloc::format!("LANG={DEFAULT_LANG}")));
        // Every entry is a well-formed NAME=value pair (no bare names).
        for entry in &env {
            assert!(entry.contains('='), "malformed entry: {entry:?}");
        }
    }

    #[test]
    fn session_program_maps_text_to_the_shell_and_graphical_to_the_desktop() {
        let user = AuthenticatedUser {
            username: "ada".to_string(),
            uid: Uid(1000),
            primary_gid: Gid(1000),
            supplementary_gids: Vec::new(),
            capabilities: CapabilitySet::empty(),
            home: "/Users/ada".to_string(),
            shell: "/System/Commands/elsh.app/Run".to_string(),
        };
        assert_eq!(
            super::session_program(&user, SessionKind::Text),
            "/System/Commands/elsh.app/Run"
        );
        assert_eq!(
            super::session_program(&user, SessionKind::Graphical),
            super::DESKTOP_SESSION_PATH
        );
        // The desktop path is the system-application-store bundle spelling:
        // the same bundle the shell's `desktop` command word resolves to.
        assert_eq!(
            super::DESKTOP_SESSION_PATH,
            "/System/Applications/desktop.app/Run"
        );
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(SessionKind::Text.label(), "text");
        assert_eq!(SessionKind::Graphical.label(), "graphical");
    }

    #[test]
    fn a_supervisor_choice_overrides_the_stored_default_both_ways() {
        for configured in [SessionKind::Text, SessionKind::Graphical] {
            assert_eq!(
                effective_session_kind(BootSession::Text, configured),
                SessionKind::Text
            );
            assert_eq!(
                effective_session_kind(BootSession::Graphical, configured),
                SessionKind::Graphical
            );
        }
    }

    #[test]
    fn a_boot_the_operator_never_diverted_keeps_the_stored_default() {
        for configured in [SessionKind::Text, SessionKind::Graphical] {
            assert_eq!(
                effective_session_kind(BootSession::Unset, configured),
                configured
            );
        }
    }

    #[test]
    fn a_stored_login_type_decides_the_configured_kind() {
        assert_eq!(
            configured_session_kind(ConfigStore::Reachable(Some(b"os.loginType text\n"))),
            SessionKind::Text
        );
        assert_eq!(
            configured_session_kind(ConfigStore::Reachable(Some(b"os.loginType graphical\n"))),
            SessionKind::Graphical
        );
    }

    /// A store login could not learn a type from must resolve to the
    /// configuration engine's own default, never to a second idea of what
    /// the default is: rendering that default and parsing it back must give
    /// the same answer as having no store at all.
    #[test]
    fn an_unlearnable_store_resolves_to_the_engine_default() {
        let rendered = tairix_sysconfig::SystemConfig::default().render();
        let engine_default =
            configured_session_kind(ConfigStore::Reachable(Some(rendered.as_bytes())));
        for document in [
            None,
            Some(&b""[..]),
            Some(&[0xff, 0xfe][..]),
            Some(&b"os.loginType desktop\n"[..]),
            Some(&b"os.notAKey graphical\n"[..]),
            Some(&b"os.loginType text\nos.loginType graphical\n"[..]),
        ] {
            assert_eq!(
                configured_session_kind(ConfigStore::Reachable(document)),
                engine_default
            );
        }
    }

    /// The boot default a machine with no store gets today. Flipping it is
    /// a deliberate change to how every fresh installation boots, so it is
    /// pinned here as well as in the configuration engine.
    #[test]
    fn a_machine_with_no_store_boots_graphical() {
        assert_eq!(
            configured_session_kind(ConfigStore::Reachable(None)),
            SessionKind::Graphical
        );
    }

    /// A round that could not reach the store must not act as though the
    /// machine had no configuration. Before this distinction existed, a
    /// volume that had not finished mounting read as "no store", which
    /// silently promoted the round to the compiled default and overrode an
    /// administrator who had configured the text prompt.
    #[test]
    fn an_unreachable_store_never_asserts_the_compiled_default() {
        assert_eq!(
            configured_session_kind(ConfigStore::Unreachable),
            SessionKind::Text
        );
        // The whole point of the two states: they must not agree while the
        // compiled default is graphical.
        assert_ne!(
            configured_session_kind(ConfigStore::Unreachable),
            configured_session_kind(ConfigStore::Reachable(None))
        );
    }

    /// An unreachable store withholds a *default*, never an administrator's
    /// explicit choice: the operator's one-boot Supervisor override still
    /// wins, so `continue gui` reaches a desktop even on the very first
    /// round after boot.
    #[test]
    fn a_supervisor_choice_survives_an_unreachable_store() {
        let withheld = configured_session_kind(ConfigStore::Unreachable);
        assert_eq!(
            effective_session_kind(BootSession::Graphical, withheld),
            SessionKind::Graphical
        );
    }

    #[test]
    fn a_document_that_was_read_is_a_reachable_store() {
        let document = b"os.loginType text\n";
        assert_eq!(
            ConfigStore::from_read(Ok(document)),
            ConfigStore::Reachable(Some(document))
        );
    }

    /// The refusal a mount with no registered backing gives is the only one
    /// that means "not yet": nothing else may withhold the default.
    #[test]
    fn only_an_offline_volume_makes_the_store_unreachable() {
        assert_eq!(
            ConfigStore::from_read(Err(Errno::NotImplemented)),
            ConfigStore::Unreachable
        );
        for refusal in [
            Errno::NotFound,
            Errno::PermissionDenied,
            Errno::NotADirectory,
            Errno::DeviceFault,
            Errno::OutOfRange,
            Errno::LengthOutOfRange,
        ] {
            assert_eq!(
                ConfigStore::from_read(Err(refusal)),
                ConfigStore::Reachable(None),
                "{refusal:?} must not read as an offline volume"
            );
        }
    }

    /// The regression this classification exists for. `configure` creates
    /// the store's directory on its first write, so an unconfigured machine
    /// has neither the directory nor the document and the read refuses
    /// `NotFound`. Reading that as an offline volume pinned every fresh
    /// installation to the text prompt, however capable of a graphical login
    /// it was.
    #[test]
    fn a_never_configured_machine_still_boots_graphical() {
        assert_eq!(
            configured_session_kind(ConfigStore::from_read(Err(Errno::NotFound))),
            SessionKind::Graphical
        );
    }

    /// The other half of the same distinction: a round that runs before the
    /// root is mounted reads `NotImplemented` and must still withhold the
    /// default rather than override a stored `text`.
    #[test]
    fn a_round_before_the_root_is_mounted_withholds_the_default() {
        assert_eq!(
            configured_session_kind(ConfigStore::from_read(Err(Errno::NotImplemented))),
            SessionKind::Text
        );
    }
}
