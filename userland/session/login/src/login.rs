//! The [`Login`] state machine: prompt, authenticate, and launch a session.
//!
//! This is the one place a credential is collected, checked, audited, and
//! turned into a running session. The loop **fails closed** (`AGENTS.md`
//! §5.4.5): it launches a session only when a user authenticates *and* their
//! session starts, and it bounds the number of attempts so a stuck console
//! can never spin forever. It always starts in text mode and offers the
//! graphical session only when one is available (`AGENTS.md` §10).

use core::fmt::{self, Write as _};

use rustos_abi::Errno;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::error::LoginError;
use crate::events;
use crate::session::{
    AuthenticatedUser, Authenticator, Credentials, Prompt, SessionKind, SessionLauncher,
    SessionOutcome,
};

/// Construction-time configuration for a [`Login`] instance.
///
/// All seams are borrowed for the login's lifetime; one config per login
/// process, alive for the whole run. This mirrors `init`'s `InitConfig`.
pub struct LoginConfig<'a> {
    /// Maximum number of authentication attempts before login fails closed.
    /// Must be non-zero; a zero budget would refuse every login.
    pub max_attempts: u32,
    /// Whether a graphical session can be offered. Set only when a display
    /// driver loaded **and** the window manager is present; otherwise the
    /// graphical option is hidden, never errored (`AGENTS.md` §10).
    pub graphical_available: bool,
    /// Seam that reads from and writes to the controlling terminal.
    pub prompt: &'a dyn Prompt,
    /// Seam that verifies credentials against `kernel/sec`.
    pub authenticator: &'a dyn Authenticator,
    /// Seam that launches the chosen session.
    pub launcher: &'a dyn SessionLauncher,
    /// Structured audit log sink (`AGENTS.md` §19.4).
    pub sink: &'a dyn Sink,
}

/// The text-login state machine (Stage 6 — `AGENTS.md` §10).
pub struct Login<'a> {
    cfg: LoginConfig<'a>,
}

impl<'a> Login<'a> {
    /// Create a login with the given configuration.
    #[must_use]
    pub fn new(cfg: LoginConfig<'a>) -> Self {
        Self { cfg }
    }

    /// Run the login loop until a session is launched and ends, the attempt
    /// budget is exhausted, or the console fails.
    ///
    /// On success the returned [`SessionOutcome`] is the result of the
    /// session that the authenticated user ran.
    ///
    /// # Errors
    ///
    /// Returns [`LoginError::TooManyAttempts`] if every attempt was rejected,
    /// [`LoginError::Console`] if the terminal could not be read, or
    /// [`LoginError::SessionLaunch`] if an authenticated user's session could
    /// not be started. Each is a fail-closed outcome that launched no session.
    pub fn run(&self) -> Result<SessionOutcome, LoginError> {
        let mut remaining = self.cfg.max_attempts;
        while remaining > 0 {
            remaining -= 1;
            let credentials = self.read_credentials()?;
            if let Ok(user) = self.cfg.authenticator.authenticate(&credentials) {
                return self.start_session(&credentials.username, &user);
            }
            self.audit_auth_failed(&credentials.username);
            self.cfg.prompt.write("Login incorrect\n");
        }
        self.audit_locked_out();
        Err(LoginError::TooManyAttempts)
    }

    /// Collect a username and (un-echoed) password from the terminal.
    fn read_credentials(&self) -> Result<Credentials, LoginError> {
        self.cfg.prompt.write("Username: ");
        let username = self.read("username", |p| p.read_line())?;
        self.cfg.prompt.write("Password: ");
        let password = self.read("password", |p| p.read_secret())?;
        Ok(Credentials { username, password })
    }

    /// Run one terminal read, auditing and surfacing a console failure.
    fn read(
        &self,
        stage: &str,
        op: impl FnOnce(&dyn Prompt) -> Result<alloc::string::String, Errno>,
    ) -> Result<alloc::string::String, LoginError> {
        match op(self.cfg.prompt) {
            Ok(value) => Ok(value),
            Err(err) => {
                self.audit_console_error(stage);
                Err(LoginError::Console(err))
            }
        }
    }

    /// Choose and launch a session for an authenticated user.
    fn start_session(
        &self,
        username: &str,
        user: &AuthenticatedUser,
    ) -> Result<SessionOutcome, LoginError> {
        let kind = self.choose_session()?;
        self.audit_session_started(username, user, kind);
        match self.cfg.launcher.launch(user, kind) {
            Ok(outcome) => {
                self.audit_session_ended(username, user, outcome);
                Ok(outcome)
            }
            Err(err) => {
                self.audit_launch_failed(username, user, kind);
                Err(LoginError::SessionLaunch(err))
            }
        }
    }

    /// Decide which session to launch.
    ///
    /// Text is the default and the only option when no graphical session is
    /// available (`AGENTS.md` §10); otherwise the user is offered the choice.
    fn choose_session(&self) -> Result<SessionKind, LoginError> {
        if !self.cfg.graphical_available {
            return Ok(SessionKind::Text);
        }
        self.cfg
            .prompt
            .write("Session type — text (default) or graphical? ");
        let answer = self.read("session choice", |p| p.read_line())?;
        Ok(SessionKind::from_choice(&answer))
    }

    fn emit(&self, level: Level, id: EventId, fields: &[Field<'_>]) {
        log(
            self.cfg.sink,
            &Event {
                level,
                id,
                message: event_message(id),
                fields,
            },
        );
    }

    fn audit_auth_failed(&self, username: &str) {
        self.emit(
            Level::Warn,
            events::AUTH_FAILED,
            &[Field {
                key: "user",
                value: username,
            }],
        );
    }

    fn audit_locked_out(&self) {
        let mut attempts = DecBuf::new();
        self.emit(
            Level::Error,
            events::LOCKED_OUT,
            &[Field {
                key: "attempts",
                value: attempts.format(i128::from(self.cfg.max_attempts)),
            }],
        );
    }

    fn audit_session_started(&self, username: &str, user: &AuthenticatedUser, kind: SessionKind) {
        let mut uid = DecBuf::new();
        let mut caps = DecBuf::new();
        self.emit(
            Level::Info,
            events::SESSION_STARTED,
            &[
                Field {
                    key: "user",
                    value: username,
                },
                Field {
                    key: "uid",
                    value: uid.format(i128::from(user.uid.0)),
                },
                Field {
                    key: "session",
                    value: kind.label(),
                },
                Field {
                    key: "granted_caps",
                    value: caps.format(i128::from(user.capabilities.len())),
                },
            ],
        );
    }

    fn audit_session_ended(
        &self,
        username: &str,
        user: &AuthenticatedUser,
        outcome: SessionOutcome,
    ) {
        let mut uid = DecBuf::new();
        let mut code = DecBuf::new();
        self.emit(
            Level::Info,
            events::SESSION_ENDED,
            &[
                Field {
                    key: "user",
                    value: username,
                },
                Field {
                    key: "uid",
                    value: uid.format(i128::from(user.uid.0)),
                },
                Field {
                    key: "session",
                    value: outcome.kind.label(),
                },
                Field {
                    key: "exit_code",
                    value: code.format(i128::from(outcome.exit_code)),
                },
            ],
        );
    }

    fn audit_launch_failed(&self, username: &str, user: &AuthenticatedUser, kind: SessionKind) {
        let mut uid = DecBuf::new();
        self.emit(
            Level::Error,
            events::SESSION_LAUNCH_FAILED,
            &[
                Field {
                    key: "user",
                    value: username,
                },
                Field {
                    key: "uid",
                    value: uid.format(i128::from(user.uid.0)),
                },
                Field {
                    key: "session",
                    value: kind.label(),
                },
            ],
        );
    }

    fn audit_console_error(&self, stage: &str) {
        self.emit(
            Level::Error,
            events::CONSOLE_ERROR,
            &[Field {
                key: "stage",
                value: stage,
            }],
        );
    }
}

fn event_message(id: EventId) -> &'static str {
    match id {
        events::SESSION_STARTED => "session started",
        events::AUTH_FAILED => "authentication failed",
        events::LOCKED_OUT => "login locked out: too many attempts",
        events::SESSION_ENDED => "session ended",
        events::SESSION_LAUNCH_FAILED => "session launch failed",
        events::CONSOLE_ERROR => "console error",
        _ => "login event",
    }
}

/// Fixed-capacity decimal formatter for an `i128`, used to render numeric
/// audit fields without an allocator. 40 bytes hold the widest `i128`
/// (39 digits plus a sign). Mirrors the helper in `init`.
struct DecBuf {
    bytes: [u8; Self::CAP],
    len: usize,
}

impl DecBuf {
    const CAP: usize = 40;

    fn new() -> Self {
        Self {
            bytes: [0; Self::CAP],
            len: 0,
        }
    }

    fn format(&mut self, value: i128) -> &str {
        self.len = 0;
        let _ = write!(DecWriter(self), "{value}");
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("?")
    }
}

struct DecWriter<'a>(&'a mut DecBuf);

impl fmt::Write for DecWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let end = self.0.len.checked_add(bytes.len()).ok_or(fmt::Error)?;
        if end > DecBuf::CAP {
            return Err(fmt::Error);
        }
        self.0.bytes[self.0.len..end].copy_from_slice(bytes);
        self.0.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{event_message, DecBuf, Login, LoginConfig};
    use crate::error::LoginError;
    use crate::events;
    use crate::session::{
        AuthenticatedUser, Authenticator, Credentials, Gid, Prompt, SessionKind, SessionLauncher,
        SessionOutcome, Uid,
    };
    use alloc::collections::VecDeque;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::{CapabilityId, Errno};
    use rustos_caps::CapabilitySet;
    use rustos_log::{Event, EventId, Level, Sink};

    /// Prompt that replays scripted input lines and records what was written.
    struct MockPrompt {
        lines: RefCell<VecDeque<Result<String, Errno>>>,
        secrets: RefCell<VecDeque<Result<String, Errno>>>,
        written: RefCell<Vec<String>>,
    }
    impl MockPrompt {
        fn new(lines: &[&str], secrets: &[&str]) -> Self {
            Self {
                lines: RefCell::new(lines.iter().map(|s| Ok((*s).to_string())).collect()),
                secrets: RefCell::new(secrets.iter().map(|s| Ok((*s).to_string())).collect()),
                written: RefCell::new(Vec::new()),
            }
        }
        fn with_results(
            lines: Vec<Result<String, Errno>>,
            secrets: Vec<Result<String, Errno>>,
        ) -> Self {
            Self {
                lines: RefCell::new(lines.into_iter().collect()),
                secrets: RefCell::new(secrets.into_iter().collect()),
                written: RefCell::new(Vec::new()),
            }
        }
        fn output(&self) -> String {
            self.written.borrow().concat()
        }
    }
    impl Prompt for MockPrompt {
        fn write(&self, text: &str) {
            self.written.borrow_mut().push(text.to_string());
        }
        fn read_line(&self) -> Result<String, Errno> {
            self.lines
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(Errno::NotFound))
        }
        fn read_secret(&self) -> Result<String, Errno> {
            self.secrets
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(Errno::NotFound))
        }
    }

    /// Authenticator that accepts exactly one (username, password) pair.
    struct FixedAuth {
        username: &'static str,
        password: &'static str,
        caps: CapabilitySet,
    }
    impl Authenticator for FixedAuth {
        fn authenticate(&self, c: &Credentials) -> Result<AuthenticatedUser, Errno> {
            if c.username == self.username && c.password == self.password {
                Ok(AuthenticatedUser {
                    uid: Uid(1000),
                    primary_gid: Gid(1000),
                    supplementary_gids: Vec::new(),
                    capabilities: self.caps,
                })
            } else {
                Err(Errno::PermissionDenied)
            }
        }
    }

    /// Launcher that records the launch and returns a scripted result.
    struct MockLauncher {
        result: Result<SessionOutcome, Errno>,
        launched: RefCell<Vec<(Uid, SessionKind)>>,
    }
    impl MockLauncher {
        fn ok(kind: SessionKind) -> Self {
            Self {
                result: Ok(SessionOutcome { kind, exit_code: 0 }),
                launched: RefCell::new(Vec::new()),
            }
        }
        fn failing() -> Self {
            Self {
                result: Err(Errno::NotFound),
                launched: RefCell::new(Vec::new()),
            }
        }
    }
    impl SessionLauncher for MockLauncher {
        fn launch(
            &self,
            user: &AuthenticatedUser,
            kind: SessionKind,
        ) -> Result<SessionOutcome, Errno> {
            self.launched.borrow_mut().push((user.uid, kind));
            self.result
                .as_ref()
                .map(|o| SessionOutcome {
                    kind,
                    exit_code: o.exit_code,
                })
                .map_err(|e| *e)
        }
    }

    struct RecordingSink {
        events: RefCell<Vec<(Level, EventId)>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
            }
        }
        fn count(&self, id: EventId) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|(_, e)| *e == id)
                .count()
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push((event.level, event.id));
        }
    }

    fn caps(list: &[CapabilityId]) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for c in list {
            set.insert(*c);
        }
        set
    }

    fn auth() -> FixedAuth {
        FixedAuth {
            username: "ada",
            password: "byron",
            caps: caps(&[CapabilityId::FS_MOUNT]),
        }
    }

    #[test]
    fn successful_text_login_launches_session() {
        let prompt = MockPrompt::new(&["ada"], &["byron"]);
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Text);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 3,
            graphical_available: false,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        let outcome = login.run().unwrap();
        assert_eq!(outcome.kind, SessionKind::Text);
        assert_eq!(launcher.launched.borrow().len(), 1);
        assert_eq!(launcher.launched.borrow()[0].0, Uid(1000));
        assert_eq!(sink.count(events::SESSION_STARTED), 1);
        assert_eq!(sink.count(events::SESSION_ENDED), 1);
        assert_eq!(sink.count(events::AUTH_FAILED), 0);
    }

    #[test]
    fn graphical_option_hidden_when_unavailable() {
        // Only one input line (the username); no session-choice line is read.
        let prompt = MockPrompt::new(&["ada"], &["byron"]);
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Text);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 3,
            graphical_available: false,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        let outcome = login.run().unwrap();
        assert_eq!(outcome.kind, SessionKind::Text);
        assert!(!prompt.output().contains("graphical"));
    }

    #[test]
    fn graphical_session_chosen_when_offered() {
        // Username, then the session choice "g".
        let prompt = MockPrompt::new(&["ada", "g"], &["byron"]);
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Graphical);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 3,
            graphical_available: true,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        let outcome = login.run().unwrap();
        assert_eq!(outcome.kind, SessionKind::Graphical);
        assert_eq!(launcher.launched.borrow()[0].1, SessionKind::Graphical);
        assert!(prompt.output().contains("graphical"));
    }

    #[test]
    fn offered_but_text_chosen_defaults_to_text() {
        let prompt = MockPrompt::new(&["ada", ""], &["byron"]);
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Text);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 3,
            graphical_available: true,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        let outcome = login.run().unwrap();
        assert_eq!(outcome.kind, SessionKind::Text);
    }

    #[test]
    fn wrong_password_retries_then_succeeds() {
        // First attempt: bad password; second: correct.
        let prompt = MockPrompt::new(&["ada", "ada"], &["wrong", "byron"]);
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Text);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 3,
            graphical_available: false,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        let outcome = login.run().unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(sink.count(events::AUTH_FAILED), 1);
        assert_eq!(sink.count(events::SESSION_STARTED), 1);
        assert!(prompt.output().contains("Login incorrect"));
    }

    #[test]
    fn attempts_exhausted_fails_closed() {
        let prompt = MockPrompt::new(&["x", "y"], &["a", "b"]);
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Text);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 2,
            graphical_available: false,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        assert_eq!(login.run(), Err(LoginError::TooManyAttempts));
        assert_eq!(sink.count(events::AUTH_FAILED), 2);
        assert_eq!(sink.count(events::LOCKED_OUT), 1);
        assert!(launcher.launched.borrow().is_empty());
    }

    #[test]
    fn console_read_failure_fails_closed() {
        // Username read returns an error immediately.
        let prompt = MockPrompt::with_results(alloc::vec![Err(Errno::TimedOut)], Vec::new());
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Text);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 3,
            graphical_available: false,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        assert_eq!(login.run(), Err(LoginError::Console(Errno::TimedOut)));
        assert_eq!(sink.count(events::CONSOLE_ERROR), 1);
        assert!(launcher.launched.borrow().is_empty());
    }

    #[test]
    fn session_launch_failure_is_reported() {
        let prompt = MockPrompt::new(&["ada"], &["byron"]);
        let auth = auth();
        let launcher = MockLauncher::failing();
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 3,
            graphical_available: false,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        assert_eq!(login.run(), Err(LoginError::SessionLaunch(Errno::NotFound)));
        assert_eq!(sink.count(events::SESSION_STARTED), 1);
        assert_eq!(sink.count(events::SESSION_LAUNCH_FAILED), 1);
        assert_eq!(sink.count(events::SESSION_ENDED), 0);
    }

    #[test]
    fn zero_budget_launches_nothing() {
        let prompt = MockPrompt::new(&[], &[]);
        let auth = auth();
        let launcher = MockLauncher::ok(SessionKind::Text);
        let sink = RecordingSink::new();
        let login = Login::new(LoginConfig {
            max_attempts: 0,
            graphical_available: false,
            prompt: &prompt,
            authenticator: &auth,
            launcher: &launcher,
            sink: &sink,
        });

        assert_eq!(login.run(), Err(LoginError::TooManyAttempts));
        assert_eq!(sink.count(events::LOCKED_OUT), 1);
        assert!(launcher.launched.borrow().is_empty());
    }

    #[test]
    fn dec_buf_and_event_messages() {
        let mut buf = DecBuf::new();
        assert_eq!(buf.format(0), "0");
        assert_eq!(buf.format(-7), "-7");
        assert_eq!(buf.format(i128::from(u32::MAX)), "4294967295");
        assert_eq!(event_message(events::SESSION_STARTED), "session started");
        assert_eq!(event_message(EventId(1)), "login event");
    }
}
