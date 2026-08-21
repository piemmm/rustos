//! The per-invocation elevation broker (`plans/CAPABILITY_USE.md` CU5).
//!
//! While a session runs, login serves its console's elevation call endpoint
//! ([`tairix_abi::elevate`]): the session's shell forwards an
//! `elevate <user> <program>` request, and this module decides it —
//! re-authenticating the target account through the **same**
//! [`Authenticator`] the login prompt uses (timing-equalised, refusals
//! indistinguishable), then running the program as that account through the
//! injected [`ElevateLauncher`] and reporting its exit code. The requesting
//! shell's own identity and capability set are never touched: the elevated
//! child's set is derived kernel-side as
//! `its manifest ∩ the target account's ceiling`, exactly as at login.
//!
//! A graphical caller cannot use that exchange — its reply arrives only
//! once the elevated program has exited, so a desktop session posting it
//! would stop serving windows to the very program it is waiting for, and
//! the two would deadlock. Such a caller posts
//! [`ElevateRequest::Launch`], which takes the identical
//! re-authentication path and answers the started program's pid instead.
//! The program is *login's* child either way, so the `Run` binary reaps a
//! launched one on its own loop rather than leaving it a zombie.
//!
//! The same endpoint also answers a narrower [`ElevateRequest::Verify`]
//! request: re-authenticate the **caller's own** kernel-attested account and
//! run nothing. This is the primitive a graphical session's screen lock
//! needs, reusing every part of the broker — the same timing-equalised
//! authenticator, the same indistinguishable refusal, the same per-attempt
//! audit — rather than a second authenticator existing anywhere in the tree.
//! It is strictly weaker than a `Run` request (it never spawns a program)
//! and narrower still (it can only ever check the caller's *own* account,
//! never one it names), so it grants no authority a `Run` request did not
//! already carry.
//!
//! Like the [`Login`](crate::Login) state machine this is pure decision
//! logic over injected seams, so every branch is host-tested; the `Run`
//! binary owns the IPC serve loop (`call_recv` → this → `call_reply`) and
//! the syscall-backed launcher.

use tairix_abi::elevate::{ElevateReply, ElevateRequest};
use tairix_abi::Errno;
use tairix_log::{log, Event, EventId, Field, Level, Sink};

use crate::decfmt::DecBuf;
use crate::events;
use crate::session::{Authenticator, Credentials};

/// Runs a re-authenticated elevation command: spawns `program` as `uid` on
/// login's own console and blocks until it exits.
///
/// The `Run` binary backs this with the `spawn` syscall's spawn-as-user
/// switch (login's `CAP_SPAWN_AS_USER`) plus `wait`; tests use in-memory
/// fixtures. Separate from [`crate::SessionLauncher`], which starts a
/// *session* from an [`crate::AuthenticatedUser`] record's shell of choice —
/// an elevation runs one explicit program and must never consult the
/// account's shell.
pub trait ElevateLauncher {
    /// Spawn `program` as `uid`, block until it exits, and return its exit
    /// code.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim when the program
    /// cannot be spawned or reaped (unknown path, spawn refused, …); the
    /// broker reports it to the requester and audits it.
    fn run_as(&self, program: &str, uid: u32) -> Result<i32, Errno>;

    /// Spawn `program` as `uid` and return its pid **without waiting for
    /// it**.
    ///
    /// Serves a caller that must keep running while the elevated program
    /// does — a graphical session, which owns the compositor that program
    /// draws through, and would deadlock on a reply that waits for its
    /// exit. The started process is the implementation's own child, so the
    /// implementation owns reaping it; the requester never can.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim when the program
    /// cannot be spawned.
    fn launch_as(&self, program: &str, uid: u32) -> Result<i32, Errno>;
}

/// Decide one elevation request, returning the reply to post.
///
/// The checks run strictly in trust order, each failing closed:
///
/// 1. **Placement** — the caller's kernel-attested console (`peer_console`,
///    read from `call_peer_origin`, never claimed) must be login's own
///    console (`own_console`): a caller on another console — or on none —
///    is refused before its bytes are even parsed.
/// 2. **Shape** — the request must decode ([`ElevateRequest::decode`],
///    fail-closed).
/// 3. **Re-authentication** — for [`ElevateRequest::Run`], the offered
///    `(username, password)` must verify through `authenticator`; for
///    [`ElevateRequest::Verify`], `password` must verify against the
///    account owned by the caller's kernel-attested `peer_uid` — never a
///    name the request supplies. Either way a wrong password, an unknown
///    account, and a locked account are refused indistinguishably
///    ([`Errno::PermissionDenied`] — the cause is audited, never
///    disclosed).
/// 4. **Run** — for [`ElevateRequest::Run`] the program is spawned as the
///    target account and waited for; for [`ElevateRequest::Launch`] it is
///    spawned and its pid answered at once. Either way a spawn refusal is
///    reported verbatim. A [`ElevateRequest::Verify`] request never reaches
///    the launcher: a successful re-authentication answers
///    [`ElevateReply::Verified`] directly.
///
/// Every grant and every refusal emits its audit event
/// ([`events::ELEVATE_GRANTED`] / [`events::ELEVATE_REFUSED`] for a `Run`
/// request, [`events::LAUNCH_GRANTED`] / [`events::LAUNCH_REFUSED`] for a
/// `Launch` request, [`events::VERIFY_GRANTED`] / [`events::VERIFY_REFUSED`]
/// for a `Verify` request). The caller owns the request buffer and zeroises it
/// (it carries the offered password) as soon as this returns.
pub fn handle_elevate_request(
    bytes: &[u8],
    peer_console: u64,
    peer_uid: Option<u32>,
    own_console: u64,
    authenticator: &dyn Authenticator,
    launcher: &dyn ElevateLauncher,
    sink: &dyn Sink,
) -> ElevateReply {
    if peer_console != own_console {
        audit_refused(sink, "foreign console", None, Errno::PermissionDenied);
        return ElevateReply::Refused(Errno::PermissionDenied);
    }
    let request = match ElevateRequest::decode(bytes) {
        Ok(request) => request,
        Err(err) => {
            audit_refused(sink, "malformed request", None, err);
            return ElevateReply::Refused(err);
        }
    };
    match request {
        ElevateRequest::Run {
            username,
            password,
            program,
        } => handle_run(username, password, program, authenticator, launcher, sink),
        ElevateRequest::Verify { password } => {
            handle_verify(peer_uid, password, authenticator, sink)
        }
        ElevateRequest::Launch {
            username,
            password,
            program,
        } => handle_launch(username, password, program, authenticator, launcher, sink),
    }
}

/// Decide a [`ElevateRequest::Run`] request: re-authenticate `username`
/// and, on success, spawn `program` as that account.
fn handle_run(
    username: &str,
    password: &str,
    program: &str,
    authenticator: &dyn Authenticator,
    launcher: &dyn ElevateLauncher,
    sink: &dyn Sink,
) -> ElevateReply {
    let credentials = Credentials { username, password };
    let Ok(user) = authenticator.authenticate(&credentials) else {
        // The cause (wrong password / unknown / locked) is deliberately not
        // recorded beyond the offered username: refusals stay
        // indistinguishable even to an audit-log reader comparing entries.
        audit_refused(
            sink,
            "authentication failed",
            Some(username),
            Errno::PermissionDenied,
        );
        return ElevateReply::Refused(Errno::PermissionDenied);
    };
    match launcher.run_as(program, user.uid.0) {
        Ok(exit_code) => {
            audit_granted(sink, username, program, user.uid.0, exit_code);
            ElevateReply::Completed { exit_code }
        }
        Err(err) => {
            audit_refused(sink, "launch failed", Some(username), err);
            ElevateReply::Refused(err)
        }
    }
}

/// Decide a [`ElevateRequest::Launch`] request: re-authenticate `username`
/// and, on success, start `program` as that account without waiting for it.
///
/// The identical trust order and the identical indistinguishable refusal as
/// [`handle_run`]; only the reply differs, carrying the started pid instead
/// of an exit code the broker would have had to block for.
fn handle_launch(
    username: &str,
    password: &str,
    program: &str,
    authenticator: &dyn Authenticator,
    launcher: &dyn ElevateLauncher,
    sink: &dyn Sink,
) -> ElevateReply {
    let credentials = Credentials { username, password };
    let Ok(user) = authenticator.authenticate(&credentials) else {
        audit_launch_refused(
            sink,
            "authentication failed",
            username,
            Errno::PermissionDenied,
        );
        return ElevateReply::Refused(Errno::PermissionDenied);
    };
    match launcher.launch_as(program, user.uid.0) {
        Ok(pid) => {
            audit_launch_granted(sink, username, program, user.uid.0, pid);
            ElevateReply::Launched { pid }
        }
        Err(err) => {
            audit_launch_refused(sink, "launch failed", username, err);
            ElevateReply::Refused(err)
        }
    }
}

/// Decide a [`ElevateRequest::Verify`] request: re-authenticate the
/// account owned by `peer_uid` against `password`; run nothing.
///
/// `peer_uid` is `None` when the caller's identity could not be attested
/// (the `call_peer_origin` read failed) — this is refused before any
/// password comparison is attempted, exactly like the console-placement
/// check above, since there is no account to check against. An attested
/// uid that resolves to no account is a different case and is refused
/// through the identical `authenticate_uid` call every other uid takes, so
/// it costs the same derivation and cannot be timed apart from a wrong
/// password on a real account.
fn handle_verify(
    peer_uid: Option<u32>,
    password: &str,
    authenticator: &dyn Authenticator,
    sink: &dyn Sink,
) -> ElevateReply {
    let Some(uid) = peer_uid else {
        audit_verify_refused(sink, "no attested uid", Errno::PermissionDenied);
        return ElevateReply::Refused(Errno::PermissionDenied);
    };
    if authenticator.authenticate_uid(uid, password).is_ok() {
        audit_verify_granted(sink, uid);
        return ElevateReply::Verified;
    }
    // As with `handle_run`, the cause is not recorded beyond the attested
    // uid: an unresolvable uid and a wrong password on a real account both
    // land here, indistinguishably.
    audit_verify_refused(sink, "authentication failed", Errno::PermissionDenied);
    ElevateReply::Refused(Errno::PermissionDenied)
}

fn emit(sink: &dyn Sink, level: Level, id: EventId, message: &str, fields: &[Field<'_>]) {
    log(
        sink,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}

fn audit_granted(sink: &dyn Sink, username: &str, program: &str, uid: u32, exit_code: i32) {
    let mut uid_buf = DecBuf::new();
    let mut code_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::ELEVATE_GRANTED,
        "elevation granted",
        &[
            Field {
                key: "user",
                value: tairix_log::FieldValue::Str(username),
            },
            Field {
                key: "uid",
                value: tairix_log::FieldValue::Str(uid_buf.format(i128::from(uid))),
            },
            Field {
                key: "program",
                value: tairix_log::FieldValue::Str(program),
            },
            Field {
                key: "exit_code",
                value: tairix_log::FieldValue::Str(code_buf.format(i128::from(exit_code))),
            },
        ],
    );
}

fn audit_refused(sink: &dyn Sink, cause: &str, username: Option<&str>, err: Errno) {
    let mut errno_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::ELEVATE_REFUSED,
        "elevation refused",
        &[
            Field {
                key: "cause",
                value: tairix_log::FieldValue::Str(cause),
            },
            Field {
                key: "user",
                value: tairix_log::FieldValue::Str(username.unwrap_or("-")),
            },
            Field {
                key: "errno",
                value: tairix_log::FieldValue::Str(errno_buf.format(i128::from(err.as_i32()))),
            },
        ],
    );
}

fn audit_launch_granted(sink: &dyn Sink, username: &str, program: &str, uid: u32, pid: i32) {
    let mut uid_buf = DecBuf::new();
    let mut pid_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::LAUNCH_GRANTED,
        "elevated program started",
        &[
            Field {
                key: "user",
                value: tairix_log::FieldValue::Str(username),
            },
            Field {
                key: "uid",
                value: tairix_log::FieldValue::Str(uid_buf.format(i128::from(uid))),
            },
            Field {
                key: "program",
                value: tairix_log::FieldValue::Str(program),
            },
            Field {
                key: "pid",
                value: tairix_log::FieldValue::Str(pid_buf.format(i128::from(pid))),
            },
        ],
    );
}

fn audit_launch_refused(sink: &dyn Sink, cause: &str, username: &str, err: Errno) {
    let mut errno_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::LAUNCH_REFUSED,
        "elevated launch refused",
        &[
            Field {
                key: "cause",
                value: tairix_log::FieldValue::Str(cause),
            },
            Field {
                key: "user",
                value: tairix_log::FieldValue::Str(username),
            },
            Field {
                key: "errno",
                value: tairix_log::FieldValue::Str(errno_buf.format(i128::from(err.as_i32()))),
            },
        ],
    );
}

fn audit_verify_granted(sink: &dyn Sink, uid: u32) {
    let mut uid_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::VERIFY_GRANTED,
        "verify-only elevation granted",
        &[Field {
            key: "uid",
            value: tairix_log::FieldValue::Str(uid_buf.format(i128::from(uid))),
        }],
    );
}

fn audit_verify_refused(sink: &dyn Sink, cause: &str, err: Errno) {
    let mut errno_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::VERIFY_REFUSED,
        "verify-only elevation refused",
        &[
            Field {
                key: "cause",
                value: tairix_log::FieldValue::Str(cause),
            },
            Field {
                key: "errno",
                value: tairix_log::FieldValue::Str(errno_buf.format(i128::from(err.as_i32()))),
            },
        ],
    );
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{handle_elevate_request, ElevateLauncher};
    use crate::events;
    use crate::session::{AuthenticatedUser, Authenticator, Credentials, Gid, Uid};
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::elevate::{ElevateReply, ElevateRequest, ELEVATE_MAX_REQUEST};
    use tairix_abi::Errno;
    use tairix_caps::CapabilitySet;
    use tairix_log::{Event, EventId, Sink};

    /// Authenticator accepting exactly `root`/`correct` as uid 0, by name or
    /// by uid.
    struct FixedAuth;

    impl FixedAuth {
        fn root() -> AuthenticatedUser {
            AuthenticatedUser {
                username: "root".to_string(),
                uid: Uid(0),
                primary_gid: Gid(0),
                supplementary_gids: Vec::new(),
                capabilities: CapabilitySet::empty(),
                home: "/Users/root".to_string(),
                shell: "/System/Services/shell".to_string(),
            }
        }
    }

    impl Authenticator for FixedAuth {
        fn authenticate(&self, credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
            // Unknown accounts, wrong passwords, and locked accounts are one
            // indistinguishable refusal, exactly as the production
            // `UsersAuthenticator` behaves.
            if credentials.username == "root" && credentials.password == "correct" {
                Ok(Self::root())
            } else {
                Err(Errno::PermissionDenied)
            }
        }

        fn authenticate_uid(&self, uid: u32, password: &str) -> Result<AuthenticatedUser, Errno> {
            // Mirrors `authenticate`: an unresolvable uid and a wrong
            // password on the real one are the identical refusal.
            if uid == 0 && password == "correct" {
                Ok(Self::root())
            } else {
                Err(Errno::PermissionDenied)
            }
        }
    }

    /// Launcher recording each run and each launch, and returning a
    /// scripted outcome for both.
    struct MockLauncher {
        outcome: Result<i32, Errno>,
        runs: RefCell<Vec<(alloc::string::String, u32)>>,
        launches: RefCell<Vec<(alloc::string::String, u32)>>,
    }

    impl MockLauncher {
        fn new(outcome: Result<i32, Errno>) -> Self {
            Self {
                outcome,
                runs: RefCell::new(Vec::new()),
                launches: RefCell::new(Vec::new()),
            }
        }
    }

    impl ElevateLauncher for MockLauncher {
        fn run_as(&self, program: &str, uid: u32) -> Result<i32, Errno> {
            self.runs.borrow_mut().push((program.to_string(), uid));
            self.outcome
        }

        fn launch_as(&self, program: &str, uid: u32) -> Result<i32, Errno> {
            self.launches.borrow_mut().push((program.to_string(), uid));
            self.outcome
        }
    }

    /// Sink counting events by id.
    #[derive(Default)]
    struct CountSink {
        seen: RefCell<Vec<EventId>>,
    }

    impl CountSink {
        fn count(&self, id: EventId) -> usize {
            self.seen.borrow().iter().filter(|e| **e == id).count()
        }
    }

    impl Sink for CountSink {
        fn write_event(&self, event: &Event<'_>) {
            self.seen.borrow_mut().push(event.id);
        }
    }

    fn encoded_run(
        username: &str,
        password: &str,
        program: &str,
    ) -> ([u8; ELEVATE_MAX_REQUEST], usize) {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = ElevateRequest::Run {
            username,
            password,
            program,
        }
        .encode(&mut buf)
        .expect("encodes");
        (buf, len)
    }

    fn encoded_launch(
        username: &str,
        password: &str,
        program: &str,
    ) -> ([u8; ELEVATE_MAX_REQUEST], usize) {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = ElevateRequest::Launch {
            username,
            password,
            program,
        }
        .encode(&mut buf)
        .expect("encodes");
        (buf, len)
    }

    fn encoded_verify(password: &str) -> ([u8; ELEVATE_MAX_REQUEST], usize) {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = ElevateRequest::Verify { password }
            .encode(&mut buf)
            .expect("encodes");
        (buf, len)
    }

    #[test]
    fn correct_password_runs_the_program_as_the_account_and_audits() {
        let (buf, len) = encoded_run("root", "correct", "/System/Commands/users.app/Run");
        let launcher = MockLauncher::new(Ok(7));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Completed { exit_code: 7 });
        assert_eq!(
            launcher.runs.borrow().as_slice(),
            &[("/System/Commands/users.app/Run".to_string(), 0)]
        );
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 1);
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 0);
    }

    #[test]
    fn wrong_password_and_unknown_account_are_refused_indistinguishably() {
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let (wrong, wrong_len) = encoded_run("root", "wrong", "/System/Commands/users.app/Run");
        let (unknown, unknown_len) =
            encoded_run("mallory", "correct", "/System/Commands/users.app/Run");
        let refused_wrong = handle_elevate_request(
            &wrong[..wrong_len],
            1,
            Some(0),
            1,
            &FixedAuth,
            &launcher,
            &sink,
        );
        let refused_unknown = handle_elevate_request(
            &unknown[..unknown_len],
            1,
            Some(0),
            1,
            &FixedAuth,
            &launcher,
            &sink,
        );
        // One reply for both causes: the requester learns nothing about
        // which part of the credentials failed.
        assert_eq!(
            refused_wrong,
            ElevateReply::Refused(Errno::PermissionDenied)
        );
        assert_eq!(refused_wrong, refused_unknown);
        assert!(launcher.runs.borrow().is_empty(), "nothing was spawned");
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 2);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 0);
    }

    #[test]
    fn a_caller_on_another_console_is_refused_before_parsing() {
        let (buf, len) = encoded_run("root", "correct", "/System/Commands/users.app/Run");
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 2, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
    }

    #[test]
    fn a_malformed_request_is_refused_without_authentication() {
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&[0xFF; 10], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert!(matches!(reply, ElevateReply::Refused(_)));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
    }

    #[test]
    fn a_spawn_refusal_is_reported_verbatim_and_audited() {
        let (buf, len) = encoded_run("root", "correct", "/missing");
        let launcher = MockLauncher::new(Err(Errno::NotFound));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::NotFound));
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 0);
    }

    #[test]
    fn correct_password_launches_the_program_without_waiting_and_audits() {
        let (buf, len) = encoded_launch("root", "correct", "/System/Applications/datetime.app/Run");
        let launcher = MockLauncher::new(Ok(4210));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Launched { pid: 4210 });
        assert_eq!(
            launcher.launches.borrow().as_slice(),
            &[("/System/Applications/datetime.app/Run".to_string(), 0)]
        );
        assert!(
            launcher.runs.borrow().is_empty(),
            "a Launch request must never take the blocking run path"
        );
        assert_eq!(sink.count(events::LAUNCH_GRANTED), 1);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 0);
    }

    #[test]
    fn a_launch_with_the_wrong_password_is_refused_and_starts_nothing() {
        let (buf, len) = encoded_launch("root", "wrong", "/System/Applications/datetime.app/Run");
        let launcher = MockLauncher::new(Ok(4210));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.launches.borrow().is_empty());
        assert_eq!(sink.count(events::LAUNCH_REFUSED), 1);
        assert_eq!(sink.count(events::LAUNCH_GRANTED), 0);
    }

    #[test]
    fn a_launch_spawn_refusal_is_reported_verbatim_and_audited() {
        let (buf, len) = encoded_launch("root", "correct", "/missing");
        let launcher = MockLauncher::new(Err(Errno::NotFound));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::NotFound));
        assert_eq!(sink.count(events::LAUNCH_REFUSED), 1);
        assert_eq!(sink.count(events::LAUNCH_GRANTED), 0);
    }

    #[test]
    fn a_launch_caller_on_another_console_is_refused_before_parsing() {
        let (buf, len) = encoded_launch("root", "correct", "/System/Applications/datetime.app/Run");
        let launcher = MockLauncher::new(Ok(4210));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 2, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.launches.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::LAUNCH_REFUSED), 0);
    }

    #[test]
    fn verify_with_the_right_password_answers_verified_and_runs_nothing() {
        let (buf, len) = encoded_verify("correct");
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Verified);
        assert!(
            launcher.runs.borrow().is_empty(),
            "a Verify request must never invoke the launcher"
        );
        assert_eq!(sink.count(events::VERIFY_GRANTED), 1);
        assert_eq!(sink.count(events::VERIFY_REFUSED), 0);
    }

    #[test]
    fn verify_with_the_wrong_password_is_refused_and_runs_nothing() {
        let (buf, len) = encoded_verify("wrong");
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::VERIFY_REFUSED), 1);
        assert_eq!(sink.count(events::VERIFY_GRANTED), 0);
    }

    #[test]
    fn verify_with_an_attested_uid_owning_no_account_is_indistinguishable_from_wrong_password() {
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let (unowned, unowned_len) = encoded_verify("correct");
        let (wrong, wrong_len) = encoded_verify("wrong");
        let no_account = handle_elevate_request(
            &unowned[..unowned_len],
            1,
            Some(9999),
            1,
            &FixedAuth,
            &launcher,
            &sink,
        );
        let wrong_password = handle_elevate_request(
            &wrong[..wrong_len],
            1,
            Some(0),
            1,
            &FixedAuth,
            &launcher,
            &sink,
        );
        // Both take the identical `authenticate_uid` path and answer the
        // identical refusal — an unresolvable uid never runs any faster or
        // differently than a wrong password on a real one.
        assert_eq!(no_account, ElevateReply::Refused(Errno::PermissionDenied));
        assert_eq!(no_account, wrong_password);
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::VERIFY_REFUSED), 2);
    }

    #[test]
    fn verify_without_an_attested_uid_is_refused_before_authenticating() {
        let (buf, len) = encoded_verify("correct");
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply = handle_elevate_request(&buf[..len], 1, None, 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::VERIFY_REFUSED), 1);
        assert_eq!(sink.count(events::VERIFY_GRANTED), 0);
    }

    #[test]
    fn a_verify_caller_on_another_console_is_refused_before_parsing() {
        let (buf, len) = encoded_verify("correct");
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply =
            handle_elevate_request(&buf[..len], 2, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::VERIFY_REFUSED), 0);
    }
}
