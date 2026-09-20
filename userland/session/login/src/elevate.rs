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
//! launched one on its own loop rather than leaving it a zombie, handing an
//! abnormal exit to [`audit_launch_ended_abnormally`] — such a child's
//! `stderr` is login's console, invisible behind a desktop, so the reaper is
//! the only component that can state how it ended.
//!
//! A caller that must *show* what an elevated run printed posts
//! [`ElevateRequest::Capture`]. It takes the identical re-authentication
//! and runs the program on the identical terms, but its standard output is
//! bound to a pipe the `Run` binary owns instead of login's console, and
//! the reply carries the drained bytes. The relay is bounded
//! ([`tairix_abi::elevate::ELEVATE_MAX_OUTPUT`]) and a run that prints more
//! is answered [`ElevateReply::Overran`] with no bytes at all, so a caller
//! never reads a prefix as the whole. The audit records how much was
//! relayed and never what it was.
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

use tairix_abi::elevate::{ElevateArgv, ElevateReply, ElevateRequest};
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
    /// Spawn `program` as `uid` with the argument vector `argv`, block until
    /// it exits, and return its exit code.
    ///
    /// The arguments are data the implementation hands the child verbatim;
    /// they confer nothing, and the child's capability set is still its
    /// manifest intersected with the target account's ceiling.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim when the program
    /// cannot be spawned or reaped (unknown path, spawn refused, …); the
    /// broker reports it to the requester and audits it.
    fn run_as(&self, program: &str, argv: ElevateArgv<'_>, uid: u32) -> Result<i32, Errno>;

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
    fn launch_as(&self, program: &str, uid: u32) -> Result<i64, Errno>;

    /// Spawn `program` as `uid` with its standard output bound to a
    /// collector the implementation owns, drain that output into `out`,
    /// block until the program exits, and report what it produced.
    ///
    /// The same authority and the same terms as [`Self::run_as`]; only the
    /// destination of the child's standard output differs. The
    /// implementation drains to end-of-stream **before** reaping, because a
    /// child that fills the collector blocks until it is emptied and a
    /// reaper that waited first would never empty it.
    ///
    /// # Errors
    ///
    /// Returns the implementation's [`Errno`] verbatim when the program
    /// cannot be spawned, collected, or reaped.
    fn capture_as(
        &self,
        program: &str,
        argv: ElevateArgv<'_>,
        uid: u32,
        out: &mut [u8],
    ) -> Result<Captured, Errno>;
}

/// What one [`ElevateLauncher::capture_as`] run produced.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Captured {
    /// The run's exit code, and how many leading bytes of the caller's
    /// buffer its whole standard output filled.
    Output {
        /// The program's exit status, exactly as `wait` reported it.
        exit_code: i32,
        /// Bytes of the caller's buffer the output filled.
        len: usize,
    },
    /// The run's exit code; it printed more than the caller's buffer holds,
    /// so none of it is offered.
    ///
    /// The run still happened — whatever it did, it did — which is why this
    /// is an outcome rather than an error, and why no prefix is handed back
    /// to be mistaken for the whole.
    Overran {
        /// The program's exit status.
        exit_code: i32,
    },
}

/// Read a captured run's output to end of stream, keeping the first
/// `out.len()` bytes.
///
/// `read` fills a buffer and answers how many bytes it took, `0` at end of
/// stream — the `Run` binary's pipe read; tests script it. The policy is
/// here rather than beside that syscall because its two edges are exactly
/// the ones a reader gets wrong: a stream that *exactly* fills `out` is
/// not an overrun (the buffer is only known to be too small once a further
/// read yields a byte), and the read continues past the bound regardless,
/// because a writer blocked on a full pipe never exits and a drain that
/// stopped early would hang the reap that follows it.
///
/// Answers how many bytes filled `out`, or [`None`] when the stream was
/// longer — in which case nothing is offered, because a prefix a caller
/// could mistake for the whole is worse than no answer.
///
/// # Errors
///
/// Whatever `read` raises, verbatim; nothing partial is reported.
pub fn drain_bounded(
    out: &mut [u8],
    mut read: impl FnMut(&mut [u8]) -> Result<usize, Errno>,
) -> Result<Option<usize>, Errno> {
    let mut filled = 0;
    while filled < out.len() {
        let Some(room) = out.get_mut(filled..) else {
            break;
        };
        let taken = read(room)?;
        if taken == 0 {
            return Ok(Some(filled));
        }
        filled = filled.saturating_add(taken);
    }
    let mut discard = [0u8; DRAIN_CHUNK];
    let mut overran = false;
    loop {
        if read(&mut discard)? == 0 {
            return Ok(if overran { None } else { Some(filled) });
        }
        overran = true;
    }
}

/// Bytes one discarding read takes once a captured run has already printed
/// past what the seam carries. Only the syscall count depends on it, so it
/// is sized to keep a runaway writer cheap to drain without a buffer worth
/// reserving.
const DRAIN_CHUNK: usize = 512;

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
///    A malformed or over-long argument vector is refused here, at the
///    shape check, so a request that could never be run costs no
///    authentication attempt against the named account.
/// 3. **Re-authentication** — for [`ElevateRequest::Run`], the offered
///    `(username, password)` must verify through `authenticator`; for
///    [`ElevateRequest::Verify`], `password` must verify against the
///    account owned by the caller's kernel-attested `peer_uid` — never a
///    name the request supplies. Either way a wrong password, an unknown
///    account, and a locked account are refused indistinguishably
///    ([`Errno::PermissionDenied`] — the cause is audited, never
///    disclosed).
/// 4. **Run** — for [`ElevateRequest::Run`] the program is spawned as the
///    target account with the request's argument vector and waited for; for
///    [`ElevateRequest::Launch`] it is spawned and its pid answered at once. Either way a spawn refusal is
///    reported verbatim. A [`ElevateRequest::Verify`] request never reaches
///    the launcher: a successful re-authentication answers
///    [`ElevateReply::Verified`] directly.
///
/// Every grant and every refusal emits its audit event
/// ([`events::ELEVATE_GRANTED`] / [`events::ELEVATE_REFUSED`] for a `Run`
/// request, [`events::LAUNCH_GRANTED`] / [`events::LAUNCH_REFUSED`] for a
/// `Launch` request, [`events::CAPTURE_GRANTED`] /
/// [`events::CAPTURE_REFUSED`] for a `Capture` request,
/// [`events::VERIFY_GRANTED`] / [`events::VERIFY_REFUSED`] for a `Verify`
/// request). The caller owns the request buffer and zeroises it (it carries
/// the offered password) as soon as this returns.
///
/// `out` is the scratch a captured run's standard output is drained into,
/// and the returned reply borrows the prefix of it that was filled; every
/// other request form leaves it untouched. It must hold
/// [`tairix_abi::elevate::ELEVATE_MAX_OUTPUT`] bytes, since a run that
/// prints more than the scratch holds is reported as having overrun the
/// seam rather than truncated to fit it.
#[allow(clippy::too_many_arguments)] // Each seam is injected separately so every branch stays host-testable.
pub fn handle_elevate_request<'o>(
    bytes: &[u8],
    peer_console: u64,
    peer_uid: Option<u32>,
    own_console: u64,
    authenticator: &dyn Authenticator,
    launcher: &dyn ElevateLauncher,
    sink: &dyn Sink,
    out: &'o mut [u8],
) -> ElevateReply<'o> {
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
            argv,
        } => handle_run(
            username,
            password,
            Started { program, argv },
            authenticator,
            launcher,
            sink,
        ),
        ElevateRequest::Verify { password } => {
            handle_verify(peer_uid, password, authenticator, sink)
        }
        ElevateRequest::Launch {
            username,
            password,
            program,
        } => handle_launch(username, password, program, authenticator, launcher, sink),
        ElevateRequest::Capture {
            username,
            password,
            program,
            argv,
        } => handle_capture(
            username,
            password,
            Started { program, argv },
            authenticator,
            launcher,
            sink,
            out,
        ),
    }
}

/// What a [`ElevateRequest::Run`] asks to be started: the program and the
/// arguments to hand it, carried together because neither is meaningful
/// without the other.
#[derive(Copy, Clone)]
struct Started<'a> {
    program: &'a str,
    argv: ElevateArgv<'a>,
}

/// Decide a [`ElevateRequest::Run`] request: re-authenticate `username`
/// and, on success, spawn the named program as that account.
fn handle_run(
    username: &str,
    password: &str,
    started: Started<'_>,
    authenticator: &dyn Authenticator,
    launcher: &dyn ElevateLauncher,
    sink: &dyn Sink,
) -> ElevateReply<'static> {
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
    match launcher.run_as(started.program, started.argv, user.uid.0) {
        Ok(exit_code) => {
            audit_granted(sink, username, started, user.uid.0, exit_code);
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
) -> ElevateReply<'static> {
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

/// Decide a [`ElevateRequest::Capture`] request: re-authenticate
/// `username` and, on success, run the named program as that account with
/// its standard output drained into `out`.
///
/// The identical trust order and the identical indistinguishable refusal as
/// [`handle_run`]; what differs is where the child's output goes and that
/// the reply carries it. A run that printed more than `out` holds is
/// answered [`ElevateReply::Overran`] — the run happened and its code is
/// real, but no prefix is handed back to be read as the whole.
#[allow(clippy::too_many_arguments)] // Each seam is injected separately so every branch stays host-testable.
fn handle_capture<'o>(
    username: &str,
    password: &str,
    started: Started<'_>,
    authenticator: &dyn Authenticator,
    launcher: &dyn ElevateLauncher,
    sink: &dyn Sink,
    out: &'o mut [u8],
) -> ElevateReply<'o> {
    let credentials = Credentials { username, password };
    let Ok(user) = authenticator.authenticate(&credentials) else {
        audit_capture_refused(
            sink,
            "authentication failed",
            username,
            Errno::PermissionDenied,
        );
        return ElevateReply::Refused(Errno::PermissionDenied);
    };
    let outcome = match launcher.capture_as(started.program, started.argv, user.uid.0, out) {
        Ok(outcome) => outcome,
        Err(err) => {
            audit_capture_refused(sink, "run failed", username, err);
            return ElevateReply::Refused(err);
        }
    };
    match outcome {
        Captured::Output { exit_code, len } => {
            let Some(output) = out.get(..len) else {
                // A launcher reporting more bytes than the scratch holds
                // has answered nothing this end can stand behind.
                audit_capture_refused(sink, "collector overran", username, Errno::OutOfRange);
                return ElevateReply::Refused(Errno::OutOfRange);
            };
            audit_capture_granted(sink, username, started, user.uid.0, outcome);
            ElevateReply::Captured { exit_code, output }
        }
        Captured::Overran { exit_code } => {
            audit_capture_granted(sink, username, started, user.uid.0, outcome);
            ElevateReply::Overran { exit_code }
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
) -> ElevateReply<'static> {
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

/// Audit a granted run.
///
/// The argument vector is recorded as a **count**, never as content: the
/// broker hands the arguments over without interpreting them, so it cannot
/// know which of them is a secret — a tool that sets an account's password
/// would take one on its command line — and a log that might carry one is
/// worse than a log that carries none. What the elevated run changed is the
/// elevated tool's own to audit, where the authority and the meaning both
/// are.
fn audit_granted(sink: &dyn Sink, username: &str, started: Started<'_>, uid: u32, exit_code: i32) {
    let mut uid_buf = DecBuf::new();
    let mut code_buf = DecBuf::new();
    let mut args_buf = DecBuf::new();
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
                value: tairix_log::FieldValue::Str(started.program),
            },
            Field {
                key: "args",
                value: tairix_log::FieldValue::Str(
                    args_buf.format(i128::try_from(started.argv.len()).unwrap_or(i128::MAX)),
                ),
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

fn audit_launch_granted(sink: &dyn Sink, username: &str, program: &str, uid: u32, pid: i64) {
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

/// Audit a granted capture: what ran, and **how much** it printed back to
/// the caller — never a byte of what that was.
///
/// The bytes are the elevated program's output, which the broker relays
/// without reading, so it cannot know which of them is a secret; a log that
/// might carry one is worse than a log that carries none. Recording the
/// volume is what makes an unusual relay visible, exactly as the granted
/// run records the argument count.
fn audit_capture_granted(
    sink: &dyn Sink,
    username: &str,
    started: Started<'_>,
    uid: u32,
    outcome: Captured,
) {
    let (exit_code, relayed, disposition) = match outcome {
        Captured::Output { exit_code, len } => (exit_code, len, "returned"),
        // Nothing was relayed: the run printed past the seam's bound and
        // the caller was handed no bytes at all.
        Captured::Overran { exit_code } => (exit_code, 0, "overran"),
    };
    let mut uid_buf = DecBuf::new();
    let mut code_buf = DecBuf::new();
    let mut args_buf = DecBuf::new();
    let mut bytes_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::CAPTURE_GRANTED,
        "elevated run returned its output",
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
                value: tairix_log::FieldValue::Str(started.program),
            },
            Field {
                key: "args",
                value: tairix_log::FieldValue::Str(
                    args_buf.format(i128::try_from(started.argv.len()).unwrap_or(i128::MAX)),
                ),
            },
            Field {
                key: "output",
                value: tairix_log::FieldValue::Str(disposition),
            },
            Field {
                key: "bytes",
                value: tairix_log::FieldValue::Str(
                    bytes_buf.format(i128::try_from(relayed).unwrap_or(i128::MAX)),
                ),
            },
            Field {
                key: "exit_code",
                value: tairix_log::FieldValue::Str(code_buf.format(i128::from(exit_code))),
            },
        ],
    );
}

fn audit_capture_refused(sink: &dyn Sink, cause: &str, username: &str, err: Errno) {
    let mut errno_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::CAPTURE_REFUSED,
        "elevated capture refused",
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

/// Audit a program started for an
/// [`ElevateRequest::Launch`] request that ended abnormally.
///
/// Login is the only component that can state this. Such a child inherits
/// login's console — under a graphical session the framebuffer text console
/// behind the desktop — so whatever it wrote to `stderr` before exiting
/// reaches nobody; the observer of the exit records it where a user can find
/// it. A clean exit (`status` 0) is a normal end and records nothing.
///
/// A status inside the reserved load-failure band is named in words through
/// the one shared reverse map ([`tairix_abi::load_failure_reason`]); any
/// other code is stated as the number alone, the most login can honestly say
/// about a program it did not write.
pub fn audit_launch_ended_abnormally(sink: &dyn Sink, pid: i64, status: i32) {
    if status == 0 {
        return;
    }
    let mut pid_buf = DecBuf::new();
    let mut status_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::LAUNCH_ENDED_ABNORMALLY,
        "elevated program ended abnormally",
        &[
            Field {
                key: "pid",
                value: tairix_log::FieldValue::Str(pid_buf.format(i128::from(pid))),
            },
            Field {
                key: "status",
                value: tairix_log::FieldValue::Str(status_buf.format(i128::from(status))),
            },
            Field {
                key: "reason",
                value: tairix_log::FieldValue::Str(
                    tairix_abi::load_failure_reason(status).unwrap_or("-"),
                ),
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

    use super::{
        audit_launch_ended_abnormally, drain_bounded, handle_elevate_request, Captured,
        ElevateLauncher,
    };
    use crate::events;
    use crate::session::{AuthenticatedUser, Authenticator, Credentials, Gid, Uid};
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::elevate::{
        ElevateArgv, ElevateReply, ElevateRequest, ELEVATE_MAX_ARGS, ELEVATE_MAX_OUTPUT,
        ELEVATE_MAX_REPLY, ELEVATE_MAX_REQUEST,
    };
    use tairix_abi::{Errno, LOAD_UNVERIFIED};
    use tairix_caps::CapabilitySet;
    use tairix_log::{Event, EventId, Sink};

    /// One decided request, kept as the frame the supervisor would post.
    ///
    /// The broker's answer borrows the scratch it drained a captured run
    /// into, so a test that holds two verdicts at once cannot hold two
    /// borrows of one scratch; keeping the encoded frame instead owns the
    /// answer, and round-trips every broker decision through the wire
    /// encoding on the way.
    #[derive(Clone)]
    struct Decided(Vec<u8>);

    impl Decided {
        fn reply(&self) -> ElevateReply<'_> {
            ElevateReply::decode(&self.0).expect("the supervisor's own frame decodes")
        }
    }

    impl core::fmt::Debug for Decided {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            self.reply().fmt(f)
        }
    }

    impl PartialEq for Decided {
        fn eq(&self, other: &Self) -> bool {
            self.reply() == other.reply()
        }
    }

    impl PartialEq<ElevateReply<'_>> for Decided {
        fn eq(&self, other: &ElevateReply<'_>) -> bool {
            self.reply() == *other
        }
    }

    /// Decide one request against a fresh output scratch.
    #[allow(clippy::too_many_arguments)] // Mirrors the seam under test, one injected dependency each.
    fn decide(
        bytes: &[u8],
        peer_console: u64,
        peer_uid: Option<u32>,
        own_console: u64,
        authenticator: &dyn Authenticator,
        launcher: &dyn ElevateLauncher,
        sink: &dyn Sink,
    ) -> Decided {
        let mut out = [0u8; ELEVATE_MAX_OUTPUT];
        let mut frame = [0u8; ELEVATE_MAX_REPLY];
        let reply = handle_elevate_request(
            bytes,
            peer_console,
            peer_uid,
            own_console,
            authenticator,
            launcher,
            sink,
            &mut out,
        );
        let len = reply
            .encode(&mut frame)
            .expect("the supervisor's answer encodes");
        Decided(frame[..len].to_vec())
    }

    /// Authenticator accepting exactly `root`/`correct` as uid 0, by name or
    /// by uid.
    struct FixedAuth;

    impl FixedAuth {
        fn root() -> AuthenticatedUser {
            AuthenticatedUser {
                username: "root".to_string(),
                shown_name: "System Administrator".to_string(),
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

    /// One recorded run: the program, the arguments it was handed, and the
    /// account it ran as.
    type RecordedRun = (alloc::string::String, Vec<alloc::string::String>, u32);

    /// Launcher recording each run, launch, and capture, and returning a
    /// scripted outcome for all three.
    struct MockLauncher {
        outcome: Result<i32, Errno>,
        /// What a captured run prints; longer than the caller's scratch
        /// models a program that overruns the seam.
        printed: Vec<u8>,
        runs: RefCell<Vec<RecordedRun>>,
        launches: RefCell<Vec<(alloc::string::String, u32)>>,
        captures: RefCell<Vec<RecordedRun>>,
    }

    impl MockLauncher {
        fn new(outcome: Result<i32, Errno>) -> Self {
            Self {
                outcome,
                printed: Vec::new(),
                runs: RefCell::new(Vec::new()),
                launches: RefCell::new(Vec::new()),
                captures: RefCell::new(Vec::new()),
            }
        }

        /// The same launcher, with a captured run printing `printed`.
        fn printing(outcome: Result<i32, Errno>, printed: &[u8]) -> Self {
            Self {
                printed: printed.to_vec(),
                ..Self::new(outcome)
            }
        }
    }

    impl ElevateLauncher for MockLauncher {
        fn run_as(&self, program: &str, argv: ElevateArgv<'_>, uid: u32) -> Result<i32, Errno> {
            self.runs.borrow_mut().push((
                program.to_string(),
                argv.iter().map(ToString::to_string).collect(),
                uid,
            ));
            self.outcome
        }

        fn launch_as(&self, program: &str, uid: u32) -> Result<i64, Errno> {
            self.launches.borrow_mut().push((program.to_string(), uid));
            self.outcome.map(i64::from)
        }

        fn capture_as(
            &self,
            program: &str,
            argv: ElevateArgv<'_>,
            uid: u32,
            out: &mut [u8],
        ) -> Result<Captured, Errno> {
            self.captures.borrow_mut().push((
                program.to_string(),
                argv.iter().map(ToString::to_string).collect(),
                uid,
            ));
            let exit_code = self.outcome?;
            let Some(room) = out.get_mut(..self.printed.len()) else {
                return Ok(Captured::Overran { exit_code });
            };
            room.copy_from_slice(&self.printed);
            Ok(Captured::Output {
                exit_code,
                len: self.printed.len(),
            })
        }
    }

    /// An authenticator that refuses everything and records that it was
    /// asked at all, so a test can prove a refusal happened *before* any
    /// attempt was spent against the named account.
    #[derive(Default)]
    struct CountingAuth {
        attempts: core::cell::Cell<usize>,
    }

    impl Authenticator for CountingAuth {
        fn authenticate(&self, _credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
            self.attempts.set(self.attempts.get() + 1);
            Err(Errno::PermissionDenied)
        }

        fn authenticate_uid(&self, _uid: u32, _password: &str) -> Result<AuthenticatedUser, Errno> {
            self.attempts.set(self.attempts.get() + 1);
            Err(Errno::PermissionDenied)
        }
    }

    /// Sink counting events by id and keeping each record's textual fields.
    #[derive(Default)]
    struct CountSink {
        seen: RefCell<Vec<EventId>>,
        fields: RefCell<Vec<(alloc::string::String, alloc::string::String)>>,
    }

    impl CountSink {
        fn count(&self, id: EventId) -> usize {
            self.seen.borrow().iter().filter(|e| **e == id).count()
        }

        /// The value the most recent record carried under `key`.
        fn field(&self, key: &str) -> Option<alloc::string::String> {
            self.fields
                .borrow()
                .iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.clone())
        }
    }

    impl Sink for CountSink {
        fn write_event(&self, event: &Event<'_>) {
            self.seen.borrow_mut().push(event.id);
            for field in event.fields {
                if let tairix_log::FieldValue::Str(value) = field.value {
                    self.fields
                        .borrow_mut()
                        .push((field.key.to_string(), value.to_string()));
                }
            }
        }
    }

    fn encoded_run(
        username: &str,
        password: &str,
        program: &str,
    ) -> ([u8; ELEVATE_MAX_REQUEST], usize) {
        encoded_run_with(username, password, program, &[])
    }

    fn encoded_run_with(
        username: &str,
        password: &str,
        program: &str,
        args: &[&str],
    ) -> ([u8; ELEVATE_MAX_REQUEST], usize) {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = ElevateRequest::Run {
            username,
            password,
            program,
            argv: ElevateArgv::new(args).expect("within bounds"),
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

    fn encoded_capture(
        username: &str,
        password: &str,
        program: &str,
        args: &[&str],
    ) -> ([u8; ELEVATE_MAX_REQUEST], usize) {
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let len = ElevateRequest::Capture {
            username,
            password,
            program,
            argv: ElevateArgv::new(args).expect("within bounds"),
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
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Completed { exit_code: 7 });
        assert_eq!(
            launcher.runs.borrow().as_slice(),
            &[("/System/Commands/users.app/Run".to_string(), Vec::new(), 0)]
        );
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 1);
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 0);
    }

    #[test]
    fn a_run_hands_its_argument_vector_to_the_launcher_verbatim() {
        let (buf, len) = encoded_run_with(
            "root",
            "correct",
            "/System/Commands/configure.app/Run",
            &["os.loginType", "text"],
        );
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Completed { exit_code: 0 });
        assert_eq!(
            launcher.runs.borrow().as_slice(),
            &[(
                "/System/Commands/configure.app/Run".to_string(),
                alloc::vec!["os.loginType".to_string(), "text".to_string()],
                0,
            )]
        );
    }

    #[test]
    fn the_audit_records_the_argument_count_and_never_the_arguments() {
        // A tool that sets an account's password would take one on its
        // command line, and the broker cannot tell which argument that is —
        // so no argument reaches the log, only how many there were.
        let (buf, len) = encoded_run_with(
            "root",
            "correct",
            "/System/Commands/configure.app/Run",
            &["os.loginType", "hunter2"],
        );
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let _ = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 1);
        assert_eq!(sink.field("args").as_deref(), Some("2"));
        assert!(!sink
            .fields
            .borrow()
            .iter()
            .any(|(_, value)| value.contains("hunter2") || value == "os.loginType"));
    }

    #[test]
    fn an_over_long_argument_vector_is_refused_before_any_authentication() {
        // Hand-built past the bound the encoder enforces, so the refusal
        // proved here is the *decoder's* — the shape check the broker runs
        // before it spends an attempt against the named account.
        let mut buf = [0u8; ELEVATE_MAX_REQUEST];
        let mut at = 0;
        buf[at..at + 2].copy_from_slice(&1u16.to_le_bytes()); // ELEVATE_VERSION
        at += 2;
        buf[at] = 0; // the run opcode
        at += 1;
        for field in ["root", "correct", "/System/Commands/configure.app/Run"] {
            let len = u16::try_from(field.len()).expect("short");
            buf[at..at + 2].copy_from_slice(&len.to_le_bytes());
            at += 2;
            buf[at..at + field.len()].copy_from_slice(field.as_bytes());
            at += field.len();
        }
        let count = u16::try_from(ELEVATE_MAX_ARGS + 1).expect("small");
        buf[at..at + 2].copy_from_slice(&count.to_le_bytes());
        at += 2;

        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let auth = CountingAuth::default();
        let reply = decide(&buf[..at], 1, Some(0), 1, &auth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::LengthOutOfRange));
        assert_eq!(auth.attempts.get(), 0);
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.field("cause").as_deref(), Some("malformed request"));
    }

    #[test]
    fn wrong_password_and_unknown_account_are_refused_indistinguishably() {
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let (wrong, wrong_len) = encoded_run("root", "wrong", "/System/Commands/users.app/Run");
        let (unknown, unknown_len) =
            encoded_run("mallory", "correct", "/System/Commands/users.app/Run");
        let refused_wrong = decide(
            &wrong[..wrong_len],
            1,
            Some(0),
            1,
            &FixedAuth,
            &launcher,
            &sink,
        );
        let refused_unknown = decide(
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
        let reply = decide(&buf[..len], 2, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
    }

    #[test]
    fn a_malformed_request_is_refused_without_authentication() {
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply = decide(&[0xFF; 10], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert!(matches!(reply.reply(), ElevateReply::Refused(_)));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
    }

    #[test]
    fn a_spawn_refusal_is_reported_verbatim_and_audited() {
        let (buf, len) = encoded_run("root", "correct", "/missing");
        let launcher = MockLauncher::new(Err(Errno::NotFound));
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::NotFound));
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 0);
    }

    #[test]
    fn correct_password_launches_the_program_without_waiting_and_audits() {
        let (buf, len) = encoded_launch("root", "correct", "/System/Applications/datetime.app/Run");
        let launcher = MockLauncher::new(Ok(4210));
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
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
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
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
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::NotFound));
        assert_eq!(sink.count(events::LAUNCH_REFUSED), 1);
        assert_eq!(sink.count(events::LAUNCH_GRANTED), 0);
    }

    #[test]
    fn a_launch_caller_on_another_console_is_refused_before_parsing() {
        let (buf, len) = encoded_launch("root", "correct", "/System/Applications/datetime.app/Run");
        let launcher = MockLauncher::new(Ok(4210));
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 2, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.launches.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::LAUNCH_REFUSED), 0);
    }

    #[test]
    fn a_launched_program_that_ended_abnormally_is_audited_once() {
        let sink = CountSink::default();
        audit_launch_ended_abnormally(&sink, 4210, 83);
        assert_eq!(sink.count(events::LAUNCH_ENDED_ABNORMALLY), 1);
        assert_eq!(sink.field("pid").as_deref(), Some("4210"));
        assert_eq!(sink.field("status").as_deref(), Some("83"));
    }

    #[test]
    fn a_launched_program_that_exited_cleanly_is_not_audited() {
        let sink = CountSink::default();
        audit_launch_ended_abnormally(&sink, 4210, 0);
        assert_eq!(sink.count(events::LAUNCH_ENDED_ABNORMALLY), 0);
    }

    #[test]
    fn a_reserved_load_failure_status_is_named_in_words() {
        let sink = CountSink::default();
        audit_launch_ended_abnormally(&sink, 4210, LOAD_UNVERIFIED);
        assert_eq!(sink.count(events::LAUNCH_ENDED_ABNORMALLY), 1);
        assert_eq!(
            sink.field("reason").as_deref(),
            tairix_abi::load_failure_reason(LOAD_UNVERIFIED)
        );
    }

    #[test]
    fn a_status_outside_the_load_band_states_the_code_and_invents_no_reason() {
        let sink = CountSink::default();
        audit_launch_ended_abnormally(&sink, 4210, 7);
        assert!(tairix_abi::load_failure_reason(7).is_none());
        assert_eq!(sink.field("status").as_deref(), Some("7"));
        assert_eq!(sink.field("reason").as_deref(), Some("-"));
    }

    #[test]
    fn a_capture_returns_what_the_run_printed_and_audits_the_volume_only() {
        let (buf, len) = encoded_capture(
            "root",
            "correct",
            "/System/Commands/configure.app/Run",
            &["wan.ipv4.address"],
        );
        let launcher = MockLauncher::printing(Ok(0), b"10.0.0.7/24\n");
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(
            reply,
            ElevateReply::Captured {
                exit_code: 0,
                output: b"10.0.0.7/24\n",
            }
        );
        assert_eq!(
            launcher.captures.borrow().as_slice(),
            &[(
                "/System/Commands/configure.app/Run".to_string(),
                alloc::vec!["wan.ipv4.address".to_string()],
                0,
            )]
        );
        assert!(
            launcher.runs.borrow().is_empty(),
            "a Capture request must never take the console-inheriting run path"
        );
        assert_eq!(sink.count(events::CAPTURE_GRANTED), 1);
        assert_eq!(sink.count(events::ELEVATE_GRANTED), 0);
        assert_eq!(sink.field("bytes").as_deref(), Some("12"));
        assert_eq!(sink.field("output").as_deref(), Some("returned"));
        // The relayed text itself never reaches the log: the broker hands
        // it over without reading it, so it cannot know what is a secret.
        assert!(!sink
            .fields
            .borrow()
            .iter()
            .any(|(_, value)| value.contains("10.0.0.7")));
    }

    #[test]
    fn a_run_that_prints_past_the_bound_returns_no_bytes_at_all() {
        let (buf, len) = encoded_capture("root", "correct", "/System/Commands/cat.app/Run", &[]);
        let flood = alloc::vec![b'x'; ELEVATE_MAX_OUTPUT + 1];
        let launcher = MockLauncher::printing(Ok(3), &flood);
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        // The run happened and its code is real; a prefix a caller could
        // mistake for the whole is not offered.
        assert_eq!(reply, ElevateReply::Overran { exit_code: 3 });
        assert_eq!(sink.count(events::CAPTURE_GRANTED), 1);
        assert_eq!(sink.field("output").as_deref(), Some("overran"));
        assert_eq!(sink.field("bytes").as_deref(), Some("0"));
    }

    #[test]
    fn a_capture_with_the_wrong_password_is_refused_and_runs_nothing() {
        let (buf, len) =
            encoded_capture("root", "wrong", "/System/Commands/configure.app/Run", &[]);
        let launcher = MockLauncher::printing(Ok(0), b"secret\n");
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.captures.borrow().is_empty());
        assert_eq!(sink.count(events::CAPTURE_REFUSED), 1);
        assert_eq!(sink.count(events::CAPTURE_GRANTED), 0);
    }

    #[test]
    fn a_capture_spawn_refusal_is_reported_verbatim_and_audited() {
        let (buf, len) = encoded_capture("root", "correct", "/missing", &[]);
        let launcher = MockLauncher::new(Err(Errno::NotFound));
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::NotFound));
        assert_eq!(sink.count(events::CAPTURE_REFUSED), 1);
        assert_eq!(sink.count(events::CAPTURE_GRANTED), 0);
    }

    #[test]
    fn a_capture_caller_on_another_console_is_refused_before_parsing() {
        let (buf, len) =
            encoded_capture("root", "correct", "/System/Commands/configure.app/Run", &[]);
        let launcher = MockLauncher::printing(Ok(0), b"secret\n");
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 2, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.captures.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::CAPTURE_REFUSED), 0);
    }

    #[test]
    fn a_capture_that_printed_nothing_answers_an_empty_output_not_a_refusal() {
        let (buf, len) =
            encoded_capture("root", "correct", "/System/Commands/configure.app/Run", &[]);
        let launcher = MockLauncher::printing(Ok(0), b"");
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(
            reply,
            ElevateReply::Captured {
                exit_code: 0,
                output: b"",
            }
        );
        assert_eq!(sink.field("bytes").as_deref(), Some("0"));
        assert_eq!(sink.field("output").as_deref(), Some("returned"));
    }

    /// A reader answering `chunks` in order, then end of stream.
    fn scripted<'a>(
        chunks: &'a [&'a [u8]],
    ) -> impl FnMut(&mut [u8]) -> Result<usize, Errno> + use<'a> {
        let mut left: Vec<&[u8]> = chunks.to_vec();
        left.reverse();
        move |buf: &mut [u8]| {
            let Some(chunk) = left.pop() else {
                return Ok(0);
            };
            let take = chunk.len().min(buf.len());
            buf[..take].copy_from_slice(&chunk[..take]);
            // A short read leaves the rest for the next call, exactly as a
            // pipe whose buffer holds less than the reader asked for does.
            if take < chunk.len() {
                left.push(&chunk[take..]);
            }
            Ok(take)
        }
    }

    #[test]
    fn a_drain_keeps_the_whole_stream_when_it_fits() {
        let mut out = [0u8; 8];
        assert_eq!(
            drain_bounded(&mut out, scripted(&[b"ab", b"cd", b"e"])),
            Ok(Some(5))
        );
        assert_eq!(&out[..5], b"abcde");
    }

    #[test]
    fn a_stream_that_exactly_fills_the_buffer_is_not_an_overrun() {
        // The edge the bound turns on: `out` being full says nothing about
        // whether more is coming, so the verdict waits on a further read.
        let mut out = [0u8; 4];
        assert_eq!(drain_bounded(&mut out, scripted(&[b"abcd"])), Ok(Some(4)));
        assert_eq!(&out, b"abcd");
    }

    #[test]
    fn one_byte_past_the_buffer_returns_nothing_at_all() {
        let mut out = [0u8; 4];
        assert_eq!(drain_bounded(&mut out, scripted(&[b"abcde"])), Ok(None));
    }

    #[test]
    fn a_drain_reads_to_end_of_stream_even_after_it_has_overrun() {
        // A writer blocked on a full pipe never exits, so the reap that
        // follows would hang if the drain stopped at the bound.
        let mut reads = 0usize;
        let mut out = [0u8; 2];
        let drained = drain_bounded(&mut out, |buf| {
            reads += 1;
            if reads > 6 {
                return Ok(0);
            }
            let take = buf.len().min(4);
            buf[..take].fill(b'x');
            Ok(take)
        });
        assert_eq!(drained, Ok(None));
        assert_eq!(reads, 7, "every read to end of stream, then the zero");
    }

    #[test]
    fn an_empty_stream_and_an_empty_buffer_both_answer_nothing_kept() {
        let mut out = [0u8; 4];
        assert_eq!(drain_bounded(&mut out, scripted(&[])), Ok(Some(0)));
        let mut none = [0u8; 0];
        assert_eq!(drain_bounded(&mut none, scripted(&[])), Ok(Some(0)));
        // A buffer with no room at all still cannot keep a byte.
        let mut none = [0u8; 0];
        assert_eq!(drain_bounded(&mut none, scripted(&[b"a"])), Ok(None));
    }

    #[test]
    fn a_read_failure_surfaces_verbatim_and_reports_nothing_partial() {
        let mut out = [0u8; 4];
        assert_eq!(
            drain_bounded(&mut out, |_| Err(Errno::BrokenPipe)),
            Err(Errno::BrokenPipe)
        );
    }

    #[test]
    fn verify_with_the_right_password_answers_verified_and_runs_nothing() {
        let (buf, len) = encoded_verify("correct");
        let launcher = MockLauncher::new(Ok(0));
        let sink = CountSink::default();
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
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
        let reply = decide(&buf[..len], 1, Some(0), 1, &FixedAuth, &launcher, &sink);
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
        let no_account = decide(
            &unowned[..unowned_len],
            1,
            Some(9999),
            1,
            &FixedAuth,
            &launcher,
            &sink,
        );
        let wrong_password = decide(
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
        let reply = decide(&buf[..len], 1, None, 1, &FixedAuth, &launcher, &sink);
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
        let reply = decide(&buf[..len], 2, Some(0), 1, &FixedAuth, &launcher, &sink);
        assert_eq!(reply, ElevateReply::Refused(Errno::PermissionDenied));
        assert!(launcher.runs.borrow().is_empty());
        assert_eq!(sink.count(events::ELEVATE_REFUSED), 1);
        assert_eq!(sink.count(events::VERIFY_REFUSED), 0);
    }
}
