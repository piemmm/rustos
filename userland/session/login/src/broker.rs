//! The `session-v1` broker: the three requests the session authority
//! answers on its rendezvous, and the answers it gives.
//!
//! The greeter draws and types; the authority verifies and starts. This is
//! the decision half of the seam between them
//! ([`tairix_abi::session_ipc`]) — a pure function over injected seams,
//! exactly like the elevation broker ([`crate::handle_elevate_request`]),
//! so every branch is host-tested and the `Run` binary owns only the IPC
//! serve loop and the syscall-backed seams.
//!
//! No caller is trusted. Every request is checked against the caller's
//! *kernel-attested* identity, never a claim in the message, and the checks
//! come in two layers:
//!
//! * **Placement**, shared by all three requests: the attested console must
//!   be the authority's own.
//! * **Identity**, one rule per request. The two the login screen asks —
//!   [`SessionRequest::Accounts`] and [`SessionRequest::Authenticate`] —
//!   require the attested uid to be the `greeter` service account, which
//!   runs with no authority to read the user database or start a process.
//!   [`SessionRequest::Background`] is not the greeter's at all: it is the
//!   presenting desktop session giving up the screen, so it requires the
//!   attested uid to own the entry the session table records as the
//!   foreground one. The greeter holds no session and is refused it; a
//!   background session cannot use it to take the screen back.
//!
//! Anything else is answered with a frame that discloses nothing.
//!
//! Refusals are **one** answer. An unknown account, a wrong password, a
//! locked account, an authority with no database, and a caller that is not
//! the greeter all produce the identical [`SessionVerdict::Refused`], so a
//! reply can never be used to probe for accounts. Why a request was refused
//! belongs in the audit trail, not on the wire.
//!
//! [`SessionRequest::Authenticate`] starts nothing. It returns a verdict;
//! the authority acts on its own loop, so a compromised greeter can never
//! choose which program runs as the authenticated user.

use alloc::vec::Vec;

use tairix_abi::session_ipc::{
    encode_account_page, SessionAccount, SessionRequest, SessionVerdict, SESSION_ACCOUNTS_PER_PAGE,
    SESSION_DISPLAY_NAME_MAX, SESSION_LOGIN_NAME_MAX,
};
use tairix_abi::time::Duration64;
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use tairix_users::{
    AccountState, UserRecord, UsersDb, GREETER_UID, MAX_DISPLAY_NAME_LEN, MAX_USERNAME_LEN,
};

use crate::budget::AttemptBudget;
use crate::decfmt::DecBuf;
use crate::events;
use crate::session::{Authenticator, Credentials};
use crate::table::LiveSessions;

// A name the account database accepts but the wire cannot carry would make
// an account silently unofferable at the login screen. Only this crate
// depends on both bounds, so this is where the relation is pinned.
const _: () = assert!(MAX_USERNAME_LEN <= SESSION_LOGIN_NAME_MAX);
const _: () = assert!(MAX_DISPLAY_NAME_LEN <= SESSION_DISPLAY_NAME_MAX);

/// The authority's session-and-account view, as the rendezvous may see it.
///
/// An injected seam so the broker is host-testable; the production
/// implementation is [`DbAccounts`]. The account half reports only what a
/// tile shows — a display name, a login name, and whether the account
/// already has a live session — never a uid, a capability ceiling, a home
/// path, or anything derived from a stored password. The session half is the
/// one mutation a request can cause: the presenting session stepping aside.
pub trait SessionDirectory {
    /// How many login-able accounts the machine has.
    ///
    /// The count a client pages against, so it must agree with what
    /// [`SessionDirectory::page`] yields for the same request.
    fn total(&self) -> u32;

    /// The accounts starting at `offset`, at most
    /// [`SESSION_ACCOUNTS_PER_PAGE`] of them.
    fn page(&self, offset: u32) -> Vec<SessionAccount>;

    /// Record the foreground session as background when `peer_uid` owns it,
    /// answering whether it did.
    ///
    /// The lookup and the change are one step, so a caller that does not own
    /// the presenting session changes nothing at all. `peer_uid` is the
    /// kernel's attestation of the caller, never a name it supplied.
    fn background(&mut self, peer_uid: u32) -> bool;
}

/// The production [`SessionDirectory`]: the parsed user database, filtered
/// to the accounts a login could actually succeed for, with the live flag
/// taken from the authority's own session table.
///
/// Offering an account the login screen cannot log into is a defect, not a
/// cosmetic one: it invites someone to type a secret at a tile that can
/// never accept it. So a record is offered only when it is
/// [`AccountState::Active`] **and** carries both a home and a shell — the
/// same conditions the authenticator itself requires. Service and locked
/// accounts are therefore absent from the chooser entirely.
pub struct DbAccounts<'a> {
    db: Option<&'a UsersDb>,
    live: &'a mut LiveSessions,
}

impl<'a> DbAccounts<'a> {
    /// Build the directory over `db` and the authority's session table.
    ///
    /// `db` is [`None`] before the encrypted root that carries the
    /// database is unlocked, and on an installer image. The login screen
    /// then offers no accounts at all, which is the honest answer: nothing
    /// can authenticate either.
    ///
    /// The table is borrowed mutably because a served request can move the
    /// presenting session into the background; the borrow lasts only as long
    /// as the round serves the rendezvous.
    pub fn new(db: Option<&'a UsersDb>, live: &'a mut LiveSessions) -> Self {
        Self { db, live }
    }

    /// Every record a login could succeed for, in database order.
    fn login_able(&self) -> impl Iterator<Item = &UserRecord> {
        self.db
            .into_iter()
            .flat_map(|db| db.records().iter())
            .filter(|record| {
                record.state() == AccountState::Active
                    && record.home().is_some()
                    && record.shell().is_some()
            })
    }
}

impl SessionDirectory for DbAccounts<'_> {
    fn total(&self) -> u32 {
        u32::try_from(self.login_able().count()).unwrap_or(u32::MAX)
    }

    fn page(&self, offset: u32) -> Vec<SessionAccount> {
        self.login_able()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(SESSION_ACCOUNTS_PER_PAGE)
            .filter_map(|record| {
                // An account whose recorded names the wire cannot carry is
                // omitted rather than refusing the whole page; it stays
                // reachable by typing its name.
                SessionAccount::new(
                    record.shown_name(),
                    record.username(),
                    self.live.is_live(record.username()),
                )
                .ok()
            })
            .collect()
    }

    fn background(&mut self, peer_uid: u32) -> bool {
        self.live.background(peer_uid)
    }
}

/// What one served `session-v1` request produced.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SessionReply {
    /// Encoded reply length. Zero means nothing was written because the
    /// buffer could not hold even the shortest frame, and the caller posts
    /// no reply.
    pub len: usize,
    /// The caller was the presenting session and is now recorded as a
    /// background one. The authority stops supervising it — it keeps its
    /// processes and its table entry — and puts the login screen back up.
    pub stepped_aside: bool,
}

impl SessionReply {
    /// A reply of `len` bytes that leaves the caller's session as it was.
    const fn of(len: usize) -> Self {
        Self {
            len,
            stepped_aside: false,
        }
    }
}

/// Decide one `session-v1` request, writing the reply into `reply`.
///
/// The checks run strictly in trust order, each failing closed:
///
/// 1. **Shape** — the request must decode. Decoding is bounded, total, and
///    touches no state, so it may precede the identity checks; all it
///    decides is which *shape* of refusal an unauthorised caller receives.
/// 2. **Placement** — `peer_console` (kernel-attested, never claimed) must
///    be `own_console`. This one check guards every request.
/// 3. **Identity** — then each request's own rule: the greeter's uid for
///    `Accounts` and `Authenticate`, ownership of the presenting session
///    for `Background`. An unattested caller arrives as [`None`] and fails
///    both.
/// 4. **Adjudication** — only then is state touched: the directory read,
///    the presenting session stepped aside, or the budget and the
///    authenticator consulted.
///
/// A refused or undecodable request is answered in the shape it asked for —
/// an empty account page, or the one reasonless
/// [`SessionVerdict::Refused`] — so a malformed frame reaches a client as
/// the protocol fault it is rather than as a wrong password, and no reply
/// tells one refusal from another. An accepted `Background` additionally
/// reports [`SessionReply::stepped_aside`].
///
/// `now` is the caller's monotonic reading, all the attempt budget runs on.
/// Every decision is audited with the account name and the attested uid,
/// never the offered secret; the caller owns the request buffer that
/// carries that secret and zeroises it as soon as this returns.
#[allow(clippy::too_many_arguments)] // Each seam is injected separately so every branch stays host-testable.
pub fn handle_session_request(
    request: &[u8],
    peer_uid: Option<u32>,
    peer_console: u64,
    own_console: u64,
    directory: &mut dyn SessionDirectory,
    authenticator: &dyn Authenticator,
    budget: &mut AttemptBudget,
    now: Duration64,
    sink: &dyn Sink,
    reply: &mut [u8],
) -> SessionReply {
    let Ok(decoded) = SessionRequest::decode(request) else {
        audit_request_refused(sink, "malformed request", peer_uid);
        return SessionReply::of(empty_page(reply));
    };
    if peer_console != own_console {
        audit_request_refused(sink, "another console", peer_uid);
        // Shaped to the request that was sent — an empty page for the list,
        // one reasonless verdict for the other two — so a stranger cannot
        // tell a placement refusal from any other.
        return SessionReply::of(match decoded {
            SessionRequest::Accounts { .. } => empty_page(reply),
            SessionRequest::Authenticate { .. } | SessionRequest::Background => {
                encode_verdict(&refusal(Duration64::ZERO), reply)
            }
        });
    }
    let from_greeter = peer_uid == Some(GREETER_UID.0);
    match decoded {
        // The login screen's two questions: only the greeter service account
        // may ask them.
        SessionRequest::Accounts { offset } => {
            if !from_greeter {
                audit_request_refused(sink, "not the greeter", peer_uid);
                return SessionReply::of(empty_page(reply));
            }
            SessionReply::of(send_page(directory, offset, sink, reply))
        }
        SessionRequest::Authenticate { username, password } => {
            if !from_greeter {
                audit_request_refused(sink, "not the greeter", peer_uid);
                return SessionReply::of(encode_verdict(&refusal(Duration64::ZERO), reply));
            }
            SessionReply::of(adjudicate(
                username,
                password,
                authenticator,
                budget,
                now,
                peer_uid,
                sink,
                reply,
            ))
        }
        // The desktop session's own request, and never the greeter's: only
        // the session holding the screen may give it up.
        SessionRequest::Background => step_aside(directory, peer_uid, sink, reply),
    }
}

/// Answer a [`SessionRequest::Background`] from the presenting session.
///
/// The directory's own check is the identity check: it moves the foreground
/// entry to the background only when the attested uid owns it, so an
/// unattested caller, the greeter, a background session, and a stranger all
/// change nothing and receive the same reasonless refusal. `retry_after` is
/// zero — there is no cooldown to wait out, the caller simply does not hold
/// the screen.
fn step_aside(
    directory: &mut dyn SessionDirectory,
    peer_uid: Option<u32>,
    sink: &dyn Sink,
    reply: &mut [u8],
) -> SessionReply {
    if peer_uid.is_some_and(|uid| directory.background(uid)) {
        audit_backgrounded(sink, peer_uid);
        return SessionReply {
            len: encode_verdict(&SessionVerdict::Accepted, reply),
            stepped_aside: true,
        };
    }
    audit_request_refused(sink, "not the presenting session", peer_uid);
    SessionReply::of(encode_verdict(&refusal(Duration64::ZERO), reply))
}

/// Answer an authorised [`SessionRequest::Accounts`] request.
fn send_page(
    accounts: &dyn SessionDirectory,
    offset: u32,
    sink: &dyn Sink,
    reply: &mut [u8],
) -> usize {
    let total = accounts.total();
    let page = if offset < total {
        accounts.page(offset)
    } else {
        Vec::new()
    };
    let page = &page[..page.len().min(SESSION_ACCOUNTS_PER_PAGE)];
    let count = u32::try_from(page.len()).unwrap_or(0);
    // A directory that disagreed with its own total would encode a frame a
    // client must distrust; send the empty one instead of a partial truth.
    let (offset, page) = if u64::from(offset) + u64::from(count) > u64::from(total) {
        (offset.min(total), &[][..])
    } else {
        (offset, page)
    };
    audit_accounts_sent(sink, total, offset, page.len());
    encode_account_page(reply, total, offset, page).unwrap_or(0)
}

/// Decide an authorised [`SessionRequest::Authenticate`] request.
///
/// A cooling-down account is refused before the authenticator is called:
/// the point of the cooldown is that the guess is not adjudicated at all.
/// That leaks nothing about which accounts exist — the budget meters
/// invented names exactly as it meters real ones.
#[allow(clippy::too_many_arguments)] // Mirrors the injected seams of the entry point above.
fn adjudicate(
    username: &str,
    password: &str,
    authenticator: &dyn Authenticator,
    budget: &mut AttemptBudget,
    now: Duration64,
    peer_uid: Option<u32>,
    sink: &dyn Sink,
    reply: &mut [u8],
) -> usize {
    let cooling = budget.retry_after(username, now);
    if cooling > Duration64::ZERO {
        audit_auth_refused(sink, "cooling down", username, peer_uid, cooling);
        return encode_verdict(&refusal(cooling), reply);
    }
    if authenticator
        .authenticate(&Credentials { username, password })
        .is_ok()
    {
        budget.note_success(username);
        audit_auth_granted(sink, username, peer_uid);
        return encode_verdict(&SessionVerdict::Accepted, reply);
    }
    // The cause (wrong password / unknown / locked / no database) is
    // deliberately not recorded beyond the offered name: refusals stay
    // indistinguishable even to an audit-log reader comparing entries.
    let retry_after = budget.note_failure(username, now);
    audit_auth_refused(
        sink,
        "authentication failed",
        username,
        peer_uid,
        retry_after,
    );
    encode_verdict(&refusal(retry_after), reply)
}

/// The one refusal every failure answers with.
const fn refusal(retry_after: Duration64) -> SessionVerdict {
    SessionVerdict::Refused { retry_after }
}

/// The frame that discloses nothing: no accounts, and none to come.
fn empty_page(reply: &mut [u8]) -> usize {
    encode_account_page(reply, 0, 0, &[]).unwrap_or(0)
}

fn encode_verdict(verdict: &SessionVerdict, reply: &mut [u8]) -> usize {
    verdict.encode(reply).unwrap_or(0)
}

/// The attested uid as an audit field, or `-` when the caller could not be
/// attested at all.
fn uid_field(buf: &mut DecBuf, peer_uid: Option<u32>) -> &str {
    match peer_uid {
        Some(uid) => buf.format(i128::from(uid)),
        None => "-",
    }
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

fn audit_request_refused(sink: &dyn Sink, cause: &str, peer_uid: Option<u32>) {
    let mut uid_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::SESSION_REQUEST_REFUSED,
        "session request refused",
        &[
            Field {
                key: "cause",
                value: FieldValue::Str(cause),
            },
            Field {
                key: "uid",
                value: FieldValue::Str(uid_field(&mut uid_buf, peer_uid)),
            },
        ],
    );
}

fn audit_auth_granted(sink: &dyn Sink, username: &str, peer_uid: Option<u32>) {
    let mut uid_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::SESSION_AUTH_GRANTED,
        "graphical login authenticated",
        &[
            Field {
                key: "user",
                value: FieldValue::Str(username),
            },
            Field {
                key: "uid",
                value: FieldValue::Str(uid_field(&mut uid_buf, peer_uid)),
            },
        ],
    );
}

fn audit_auth_refused(
    sink: &dyn Sink,
    cause: &str,
    username: &str,
    peer_uid: Option<u32>,
    retry_after: Duration64,
) {
    let mut uid_buf = DecBuf::new();
    let mut retry_buf = DecBuf::new();
    emit(
        sink,
        Level::Warn,
        events::SESSION_AUTH_REFUSED,
        "graphical login refused",
        &[
            Field {
                key: "cause",
                value: FieldValue::Str(cause),
            },
            Field {
                key: "user",
                value: FieldValue::Str(username),
            },
            Field {
                key: "uid",
                value: FieldValue::Str(uid_field(&mut uid_buf, peer_uid)),
            },
            Field {
                key: "retry_after_s",
                value: FieldValue::Str(retry_buf.format(i128::from(retry_after.secs()))),
            },
        ],
    );
}

fn audit_backgrounded(sink: &dyn Sink, peer_uid: Option<u32>) {
    let mut uid_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::SESSION_BACKGROUNDED,
        "desktop session stepped aside from the screen",
        &[Field {
            key: "uid",
            value: FieldValue::Str(uid_field(&mut uid_buf, peer_uid)),
        }],
    );
}

fn audit_accounts_sent(sink: &dyn Sink, total: u32, offset: u32, count: usize) {
    let mut total_buf = DecBuf::new();
    let mut offset_buf = DecBuf::new();
    let mut count_buf = DecBuf::new();
    emit(
        sink,
        Level::Info,
        events::SESSION_ACCOUNTS_SENT,
        "account list disclosed to the login screen",
        &[
            Field {
                key: "total",
                value: FieldValue::Str(total_buf.format(i128::from(total))),
            },
            Field {
                key: "offset",
                value: FieldValue::Str(offset_buf.format(i128::from(offset))),
            },
            Field {
                key: "count",
                value: FieldValue::Str(count_buf.format(count as i128)),
            },
        ],
    );
}

#[cfg(test)]
#[path = "broker_tests.rs"]
mod tests;
