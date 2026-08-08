//! Decision-table tests for the `session-v1` broker.
//!
//! Split from `broker.rs` to keep either file inside the 500-line bound.

extern crate std;

use super::{handle_session_request, DbAccounts, SessionDirectory};
use crate::budget::{AttemptBudget, FREE_ATTEMPTS};
use crate::events;
use crate::session::{AuthenticatedUser, Authenticator, Credentials, Gid, Uid};
use crate::table::LiveSessions;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::Write as _;
use tairix_abi::session_ipc::{
    decode_account_page, SessionAccount, SessionRequest, SessionVerdict, SESSION_ACCOUNTS_PER_PAGE,
    SESSION_MAX_REPLY, SESSION_MAX_REQUEST,
};
use tairix_abi::time::Duration64;
use tairix_abi::Errno;
use tairix_caps::CapabilitySet;
use tairix_log::{Event, EventId, FieldValue, Sink};
use tairix_users::{
    AccountState, Identity, StoredPassword, UserRecord, UsersDb, GREETER_UID, MIN_ITERATIONS,
};

/// Login's own console; the only placement the broker serves.
const OWN_CONSOLE: u64 = 1;

/// The password the fixture account authenticates with. No reply, log
/// field, or audit message may ever contain it.
const SECRET: &str = "correct-horse";

/// Authenticator accepting exactly `ada`/[`SECRET`], as uid 1000.
struct FixedAuth;

impl FixedAuth {
    fn ada() -> AuthenticatedUser {
        AuthenticatedUser {
            username: "ada".to_string(),
            uid: Uid(1000),
            primary_gid: Gid(1000),
            supplementary_gids: Vec::new(),
            capabilities: CapabilitySet::empty(),
            home: "/Users/ada".to_string(),
            shell: "/System/Commands/elsh.app/Run".to_string(),
        }
    }
}

impl Authenticator for FixedAuth {
    fn authenticate(&self, credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
        if credentials.username == "ada" && credentials.password == SECRET {
            Ok(Self::ada())
        } else {
            Err(Errno::PermissionDenied)
        }
    }

    fn authenticate_uid(&self, uid: u32, password: &str) -> Result<AuthenticatedUser, Errno> {
        if uid == 1000 && password == SECRET {
            Ok(Self::ada())
        } else {
            Err(Errno::PermissionDenied)
        }
    }
}

/// The authenticator wired when no database is held: everything refused.
struct NoDatabase;

impl Authenticator for NoDatabase {
    fn authenticate(&self, _credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
        Err(Errno::PermissionDenied)
    }

    fn authenticate_uid(&self, _uid: u32, _password: &str) -> Result<AuthenticatedUser, Errno> {
        Err(Errno::PermissionDenied)
    }
}

/// A directory over a fixed list of accounts, paged the same way the
/// production one is, recording which uid (if any) holds the screen.
struct MockDirectory {
    accounts: Vec<SessionAccount>,
    presenting: Option<u32>,
}

impl MockDirectory {
    fn of(count: usize) -> Self {
        let accounts = (0..count)
            .map(|index| {
                let name = std::format!("user{index}");
                SessionAccount::new(&name, &name, false).expect("a well-formed name")
            })
            .collect();
        Self {
            accounts,
            presenting: None,
        }
    }

    /// A directory whose `uid` holds the screen.
    fn presented_by(uid: u32) -> Self {
        Self {
            presenting: Some(uid),
            ..Self::of(0)
        }
    }
}

impl SessionDirectory for MockDirectory {
    fn total(&self) -> u32 {
        u32::try_from(self.accounts.len()).expect("the fixture is small")
    }

    fn page(&self, offset: u32) -> Vec<SessionAccount> {
        self.accounts
            .iter()
            .skip(usize::try_from(offset).expect("the fixture is small"))
            .take(SESSION_ACCOUNTS_PER_PAGE)
            .copied()
            .collect()
    }

    fn background(&mut self, peer_uid: u32) -> bool {
        if self.presenting == Some(peer_uid) {
            self.presenting = None;
            return true;
        }
        false
    }
}

/// Sink recording each event's id, message, and rendered field values, so
/// a test can assert both what was audited and what was *not*.
#[derive(Default)]
struct RecordingSink {
    seen: RefCell<Vec<(EventId, String)>>,
}

impl RecordingSink {
    fn count(&self, id: EventId) -> usize {
        self.seen.borrow().iter().filter(|(e, _)| *e == id).count()
    }

    /// Every audited byte, concatenated — what an audit-log reader sees.
    fn transcript(&self) -> String {
        self.seen
            .borrow()
            .iter()
            .map(|(_, text)| text.clone())
            .collect()
    }
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        let mut text = String::from(event.message);
        for field in event.fields {
            text.push(' ');
            text.push_str(field.key);
            text.push('=');
            match field.value {
                FieldValue::Str(value) => text.push_str(value),
                other => {
                    let _ = write!(text, "{other:?}");
                }
            }
        }
        self.seen.borrow_mut().push((event.id, text));
    }
}

/// One served request, returning the encoded reply.
struct Harness {
    budget: AttemptBudget,
    sink: RecordingSink,
    /// Whether the last served request stepped the caller's session aside.
    stepped_aside: bool,
}

impl Harness {
    fn new() -> Self {
        Self {
            budget: AttemptBudget::new(),
            sink: RecordingSink::default(),
            stepped_aside: false,
        }
    }

    fn serve(
        &mut self,
        request: &SessionRequest<'_>,
        peer_uid: Option<u32>,
        peer_console: u64,
        directory: &mut dyn SessionDirectory,
        authenticator: &dyn Authenticator,
        now: Duration64,
    ) -> Vec<u8> {
        let mut encoded = [0u8; SESSION_MAX_REQUEST];
        let len = request.encode(&mut encoded).expect("the fixture encodes");
        let mut reply = [0u8; SESSION_MAX_REPLY];
        let answer = handle_session_request(
            &encoded[..len],
            peer_uid,
            peer_console,
            OWN_CONSOLE,
            directory,
            authenticator,
            &mut self.budget,
            now,
            &self.sink,
            &mut reply,
        );
        self.stepped_aside = answer.stepped_aside;
        reply[..answer.len].to_vec()
    }

    /// The common case: the greeter, on login's own console, at `t=0`.
    fn serve_as_greeter(
        &mut self,
        request: &SessionRequest<'_>,
        directory: &mut dyn SessionDirectory,
        authenticator: &dyn Authenticator,
    ) -> Vec<u8> {
        self.serve(
            request,
            Some(GREETER_UID.0),
            OWN_CONSOLE,
            directory,
            authenticator,
            Duration64::ZERO,
        )
    }
}

fn authenticate<'a>(username: &'a str, password: &'a str) -> SessionRequest<'a> {
    SessionRequest::Authenticate { username, password }
}

// --- Caller attestation -------------------------------------------------

#[test]
fn a_caller_whose_attested_uid_is_not_the_greeter_is_refused() {
    let mut harness = Harness::new();
    let reply = harness.serve(
        &authenticate("ada", SECRET),
        Some(GREETER_UID.0 + 1),
        OWN_CONSOLE,
        &mut MockDirectory::of(0),
        &FixedAuth,
        Duration64::ZERO,
    );
    assert_eq!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused {
            retry_after: Duration64::ZERO
        })
    );
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 1);
    assert_eq!(harness.sink.count(events::SESSION_AUTH_GRANTED), 0);
}

#[test]
fn a_caller_on_another_console_is_refused() {
    let mut harness = Harness::new();
    let reply = harness.serve(
        &authenticate("ada", SECRET),
        Some(GREETER_UID.0),
        OWN_CONSOLE + 1,
        &mut MockDirectory::of(0),
        &FixedAuth,
        Duration64::ZERO,
    );
    assert_eq!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused {
            retry_after: Duration64::ZERO
        })
    );
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 1);
}

#[test]
fn a_caller_that_could_not_be_attested_at_all_is_refused() {
    let mut harness = Harness::new();
    let reply = harness.serve(
        &authenticate("ada", SECRET),
        None,
        OWN_CONSOLE,
        &mut MockDirectory::of(0),
        &FixedAuth,
        Duration64::ZERO,
    );
    assert!(matches!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused { .. })
    ));
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 1);
}

#[test]
fn an_unauthorised_accounts_request_yields_an_empty_page() {
    let mut harness = Harness::new();
    let reply = harness.serve(
        &SessionRequest::Accounts { offset: 0 },
        Some(GREETER_UID.0 + 1),
        OWN_CONSOLE,
        &mut MockDirectory::of(4),
        &FixedAuth,
        Duration64::ZERO,
    );
    let page = decode_account_page(&reply).expect("a well-formed frame");
    assert_eq!(page.total(), 0);
    assert!(page.accounts().is_empty());
    // The refusal disclosed nothing about the four accounts that exist.
    assert_eq!(harness.sink.count(events::SESSION_ACCOUNTS_SENT), 0);
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 1);
}

#[test]
fn a_malformed_request_is_answered_without_touching_any_state() {
    let mut harness = Harness::new();
    let mut reply = [0u8; SESSION_MAX_REPLY];
    let answer = handle_session_request(
        &[0xFF; 24],
        Some(GREETER_UID.0),
        OWN_CONSOLE,
        OWN_CONSOLE,
        &mut MockDirectory::of(4),
        &FixedAuth,
        &mut harness.budget,
        Duration64::ZERO,
        &harness.sink,
        &mut reply,
    );
    assert!(!answer.stepped_aside);
    // A client fault reaches the greeter as a protocol fault, never as a
    // refusal it would show the user as a wrong password.
    let page = decode_account_page(&reply[..answer.len]).expect("a well-formed frame");
    assert_eq!(page.total(), 0);
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 1);
    assert_eq!(harness.sink.count(events::SESSION_ACCOUNTS_SENT), 0);
    assert_eq!(harness.sink.count(events::SESSION_AUTH_REFUSED), 0);
}

// --- Authentication -----------------------------------------------------

#[test]
fn the_right_secret_is_accepted_and_audited_without_starting_anything() {
    let mut harness = Harness::new();
    let reply = harness.serve_as_greeter(
        &authenticate("ada", SECRET),
        &mut MockDirectory::of(0),
        &FixedAuth,
    );
    assert_eq!(SessionVerdict::decode(&reply), Ok(SessionVerdict::Accepted));
    assert_eq!(harness.sink.count(events::SESSION_AUTH_GRANTED), 1);
    assert_eq!(harness.sink.count(events::SESSION_AUTH_REFUSED), 0);
}

/// Every way an authentication can fail must produce the identical bytes:
/// a caller comparing replies learns nothing about which accounts exist.
#[test]
fn every_failure_mode_produces_a_byte_identical_refusal() {
    let mut directory = MockDirectory::of(0);
    let replies: Vec<Vec<u8>> = vec![
        // A wrong password on a real account.
        Harness::new().serve_as_greeter(&authenticate("ada", "wrong"), &mut directory, &FixedAuth),
        // An account that does not exist.
        Harness::new().serve_as_greeter(
            &authenticate("mallory", SECRET),
            &mut directory,
            &FixedAuth,
        ),
        // A locked or no-login account: the authenticator refuses it
        // exactly as it refuses an unknown one.
        Harness::new().serve_as_greeter(
            &authenticate("devmgr", SECRET),
            &mut directory,
            &FixedAuth,
        ),
        // No database at all.
        Harness::new().serve_as_greeter(&authenticate("ada", SECRET), &mut directory, &NoDatabase),
        // A caller that is not the greeter.
        Harness::new().serve(
            &authenticate("ada", SECRET),
            Some(GREETER_UID.0 + 1),
            OWN_CONSOLE,
            &mut directory,
            &FixedAuth,
            Duration64::ZERO,
        ),
        // A caller with no attested identity.
        Harness::new().serve(
            &authenticate("ada", SECRET),
            None,
            OWN_CONSOLE,
            &mut directory,
            &FixedAuth,
            Duration64::ZERO,
        ),
    ];
    for reply in &replies {
        assert_eq!(
            reply, &replies[0],
            "one refusal, whatever the cause: {replies:?}"
        );
    }
    assert_eq!(
        SessionVerdict::decode(&replies[0]),
        Ok(SessionVerdict::Refused {
            retry_after: Duration64::ZERO
        })
    );
}

#[test]
fn repeated_failures_start_reporting_a_cooldown_and_a_success_clears_it() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::of(0);
    for _ in 0..FREE_ATTEMPTS {
        let reply =
            harness.serve_as_greeter(&authenticate("ada", "wrong"), &mut directory, &FixedAuth);
        assert_eq!(
            SessionVerdict::decode(&reply),
            Ok(SessionVerdict::Refused {
                retry_after: Duration64::ZERO
            })
        );
    }
    let reply = harness.serve_as_greeter(&authenticate("ada", "wrong"), &mut directory, &FixedAuth);
    let Ok(SessionVerdict::Refused { retry_after }) = SessionVerdict::decode(&reply) else {
        panic!("a refusal");
    };
    assert!(retry_after > Duration64::ZERO);
    // The cooldown is reported, and while it runs the secret is not even
    // adjudicated: the right password still waits.
    let reply = harness.serve_as_greeter(&authenticate("ada", SECRET), &mut directory, &FixedAuth);
    assert!(matches!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused { .. })
    ));
    // Once it lapses the right password is accepted and the account is
    // cleared.
    let later = Duration64::from_secs(retry_after.secs() + 1);
    let mut encoded = [0u8; SESSION_MAX_REQUEST];
    let request = authenticate("ada", SECRET);
    let len = request.encode(&mut encoded).expect("encodes");
    let mut reply = [0u8; SESSION_MAX_REPLY];
    let answer = handle_session_request(
        &encoded[..len],
        Some(GREETER_UID.0),
        OWN_CONSOLE,
        OWN_CONSOLE,
        &mut directory,
        &FixedAuth,
        &mut harness.budget,
        later,
        &harness.sink,
        &mut reply,
    );
    assert_eq!(
        SessionVerdict::decode(&reply[..answer.len]),
        Ok(SessionVerdict::Accepted)
    );
    assert_eq!(
        harness.budget.retry_after("ada", later),
        Duration64::ZERO,
        "a success clears the account"
    );
}

#[test]
fn one_accounts_cooldown_does_not_delay_another() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::of(0);
    for _ in 0..=FREE_ATTEMPTS {
        let _ = harness.serve_as_greeter(&authenticate("ada", "wrong"), &mut directory, &FixedAuth);
    }
    let reply =
        harness.serve_as_greeter(&authenticate("grace", "wrong"), &mut directory, &FixedAuth);
    assert_eq!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused {
            retry_after: Duration64::ZERO
        })
    );
}

// --- Stepping aside from the screen -------------------------------------

/// The uid the step-aside fixtures record as holding the screen.
const PRESENTING_UID: u32 = 1000;

/// Serve one `Background` request from `peer_uid` on `peer_console`.
fn serve_step_aside(
    harness: &mut Harness,
    directory: &mut dyn SessionDirectory,
    peer_uid: Option<u32>,
    peer_console: u64,
) -> Vec<u8> {
    harness.serve(
        &SessionRequest::Background,
        peer_uid,
        peer_console,
        directory,
        &FixedAuth,
        Duration64::ZERO,
    )
}

#[test]
fn the_presenting_session_may_step_aside() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::presented_by(PRESENTING_UID);
    let reply = serve_step_aside(
        &mut harness,
        &mut directory,
        Some(PRESENTING_UID),
        OWN_CONSOLE,
    );
    assert_eq!(SessionVerdict::decode(&reply), Ok(SessionVerdict::Accepted));
    assert!(harness.stepped_aside, "the round must stop supervising it");
    assert_eq!(directory.presenting, None, "the seat is free");
    assert_eq!(harness.sink.count(events::SESSION_BACKGROUNDED), 1);
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 0);
    // The record names the attested uid and nothing else identifying.
    assert!(harness
        .sink
        .transcript()
        .contains(&std::format!("uid={PRESENTING_UID}")));
}

#[test]
fn the_greeter_may_not_step_a_session_aside() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::presented_by(PRESENTING_UID);
    let reply = serve_step_aside(
        &mut harness,
        &mut directory,
        Some(GREETER_UID.0),
        OWN_CONSOLE,
    );
    assert!(matches!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused { .. })
    ));
    assert!(!harness.stepped_aside);
    assert_eq!(
        directory.presenting,
        Some(PRESENTING_UID),
        "the login screen cannot take the screen from the person using it"
    );
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 1);
    assert_eq!(harness.sink.count(events::SESSION_BACKGROUNDED), 0);
}

#[test]
fn a_background_session_may_not_step_the_presenting_one_aside() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::presented_by(PRESENTING_UID);
    // Another logged-in account, live but not presenting.
    let reply = serve_step_aside(
        &mut harness,
        &mut directory,
        Some(PRESENTING_UID + 1),
        OWN_CONSOLE,
    );
    assert!(matches!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused { .. })
    ));
    assert!(!harness.stepped_aside);
    assert_eq!(directory.presenting, Some(PRESENTING_UID));
}

#[test]
fn an_unattested_caller_may_not_step_a_session_aside() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::presented_by(PRESENTING_UID);
    let reply = serve_step_aside(&mut harness, &mut directory, None, OWN_CONSOLE);
    assert!(matches!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused { .. })
    ));
    assert!(!harness.stepped_aside);
    assert_eq!(directory.presenting, Some(PRESENTING_UID));
}

#[test]
fn a_step_aside_from_another_console_is_refused() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::presented_by(PRESENTING_UID);
    let reply = serve_step_aside(
        &mut harness,
        &mut directory,
        Some(PRESENTING_UID),
        OWN_CONSOLE + 1,
    );
    assert!(matches!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused { .. })
    ));
    assert!(!harness.stepped_aside);
    assert_eq!(
        directory.presenting,
        Some(PRESENTING_UID),
        "the placement check runs before the identity one"
    );
}

#[test]
fn a_step_aside_with_nothing_presenting_is_refused() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::of(0);
    let reply = serve_step_aside(
        &mut harness,
        &mut directory,
        Some(PRESENTING_UID),
        OWN_CONSOLE,
    );
    assert!(matches!(
        SessionVerdict::decode(&reply),
        Ok(SessionVerdict::Refused { .. })
    ));
    assert!(!harness.stepped_aside);
    assert_eq!(harness.sink.count(events::SESSION_REQUEST_REFUSED), 1);
}

/// However a step-aside is refused, the caller gets the same bytes: it
/// cannot learn who holds the screen by comparing replies.
#[test]
fn every_step_aside_refusal_is_byte_identical() {
    let mut presented = MockDirectory::presented_by(PRESENTING_UID);
    let mut vacant = MockDirectory::of(0);
    let replies: Vec<Vec<u8>> = vec![
        serve_step_aside(
            &mut Harness::new(),
            &mut presented,
            Some(GREETER_UID.0),
            OWN_CONSOLE,
        ),
        serve_step_aside(
            &mut Harness::new(),
            &mut presented,
            Some(PRESENTING_UID + 1),
            OWN_CONSOLE,
        ),
        serve_step_aside(&mut Harness::new(), &mut presented, None, OWN_CONSOLE),
        serve_step_aside(
            &mut Harness::new(),
            &mut presented,
            Some(PRESENTING_UID),
            OWN_CONSOLE + 1,
        ),
        serve_step_aside(
            &mut Harness::new(),
            &mut vacant,
            Some(PRESENTING_UID),
            OWN_CONSOLE,
        ),
    ];
    for reply in &replies {
        assert_eq!(
            reply, &replies[0],
            "one refusal, whatever the cause: {replies:?}"
        );
    }
    assert_eq!(
        SessionVerdict::decode(&replies[0]),
        Ok(SessionVerdict::Refused {
            retry_after: Duration64::ZERO
        })
    );
}

#[test]
fn a_session_that_stepped_aside_stays_live_and_resumable() {
    let db = mixed_db();
    let mut live = LiveSessions::new();
    live.insert("ada", 1000, 42).expect("a fresh account");
    let mut harness = Harness::new();
    {
        let mut directory = DbAccounts::new(Some(&db), &mut live);
        let reply = serve_step_aside(&mut harness, &mut directory, Some(1000), OWN_CONSOLE);
        assert_eq!(SessionVerdict::decode(&reply), Ok(SessionVerdict::Accepted));
        assert!(harness.stepped_aside);
        // Still offered as live, so the chooser shows it as returnable.
        assert!(directory
            .page(0)
            .iter()
            .find(|account| account.login_name() == "ada")
            .expect("offered")
            .has_live_session());
        // It no longer holds the screen, so a second request changes nothing.
        let again = serve_step_aside(&mut harness, &mut directory, Some(1000), OWN_CONSOLE);
        assert!(matches!(
            SessionVerdict::decode(&again),
            Ok(SessionVerdict::Refused { .. })
        ));
        assert!(!harness.stepped_aside);
    }
    assert!(live.is_live("ada"), "stepping aside never ends a session");
    assert!(live.set_foreground("ada"), "and it can be brought back");
    // Removing the entry is the *other* outcome — the session exited — and is
    // the only one that takes the account off the table.
    assert!(live.remove("ada").is_some());
    assert!(!live.is_live("ada"));
}

// --- Secret hygiene -----------------------------------------------------

#[test]
fn the_secret_never_reaches_a_reply_or_an_audit_record() {
    let mut harness = Harness::new();
    let mut directory = MockDirectory::of(0);
    let accepted =
        harness.serve_as_greeter(&authenticate("ada", SECRET), &mut directory, &FixedAuth);
    let refused = harness.serve_as_greeter(
        &authenticate("ada", "another-secret"),
        &mut directory,
        &FixedAuth,
    );
    for reply in [&accepted, &refused] {
        assert!(
            !std::string::String::from_utf8_lossy(reply).contains(SECRET),
            "a reply carried the offered secret"
        );
    }
    let transcript = harness.sink.transcript();
    assert!(!transcript.contains(SECRET), "an audit field carried it");
    assert!(!transcript.contains("another-secret"));
    // The account name and the attested uid are audited, though.
    assert!(transcript.contains("ada"));
    assert!(transcript.contains(&std::format!("uid={}", GREETER_UID.0)));
}

#[test]
fn the_request_buffer_is_wiped_after_the_decision() {
    let mut budget = AttemptBudget::new();
    let sink = RecordingSink::default();
    let mut request = tairix_util::secret::Wiped::<SESSION_MAX_REQUEST>::new();
    let len = authenticate("ada", SECRET)
        .encode(&mut request[..])
        .expect("encodes");
    let mut reply = [0u8; SESSION_MAX_REPLY];
    let _ = handle_session_request(
        &request[..len],
        Some(GREETER_UID.0),
        OWN_CONSOLE,
        OWN_CONSOLE,
        &mut MockDirectory::of(0),
        &FixedAuth,
        &mut budget,
        Duration64::ZERO,
        &sink,
        &mut reply,
    );
    // The `Run` binary's serve step, in the same order it performs it.
    tairix_util::secret::wipe(&mut request[..]);
    assert_eq!(*request, [0u8; SESSION_MAX_REQUEST]);
}

// --- Paging -------------------------------------------------------------

#[test]
fn accounts_are_paged_and_the_whole_list_is_walkable() {
    let count = SESSION_ACCOUNTS_PER_PAGE * 2 + 3;
    let mut directory = MockDirectory::of(count);
    let mut harness = Harness::new();
    let mut walked = Vec::new();
    let mut offset = 0u32;
    loop {
        let reply = harness.serve_as_greeter(
            &SessionRequest::Accounts { offset },
            &mut directory,
            &FixedAuth,
        );
        let page = decode_account_page(&reply).expect("a well-formed page");
        assert_eq!(page.total(), u32::try_from(count).expect("small"));
        assert_eq!(page.offset(), offset);
        assert!(page.accounts().len() <= SESSION_ACCOUNTS_PER_PAGE);
        for account in page.accounts() {
            walked.push(account.login_name().to_string());
        }
        if page.is_last() {
            break;
        }
        offset += u32::try_from(page.accounts().len()).expect("small");
    }
    assert_eq!(walked.len(), count);
    assert_eq!(walked[0], "user0");
    assert_eq!(walked[count - 1], std::format!("user{}", count - 1));
    assert_eq!(harness.sink.count(events::SESSION_ACCOUNTS_SENT), 3);
}

#[test]
fn an_offset_past_the_end_yields_a_consistent_empty_page() {
    let mut directory = MockDirectory::of(2);
    let mut harness = Harness::new();
    let reply = harness.serve_as_greeter(
        &SessionRequest::Accounts { offset: 99 },
        &mut directory,
        &FixedAuth,
    );
    let page = decode_account_page(&reply).expect("a well-formed page");
    assert_eq!(page.total(), 2);
    assert!(page.accounts().is_empty());
    assert!(page.is_last());
}

// --- The production directory -------------------------------------------

fn record(
    username: &str,
    uid: u32,
    display_name: &str,
    state: AccountState,
    home: Option<&str>,
    shell: Option<&str>,
) -> UserRecord {
    let identity = Identity {
        username,
        uid: tairix_users::Uid(uid),
        primary_gid: tairix_users::Gid(uid),
        supplementary_gids: &[],
        display_name,
        home,
        shell,
        capabilities: CapabilitySet::empty(),
        state,
    };
    if state == AccountState::NoLogin {
        UserRecord::new(identity, StoredPassword::NeverAuthenticates).expect("a valid record")
    } else {
        UserRecord::with_password(identity, b"secret", [0x42; 16], MIN_ITERATIONS)
            .expect("a valid record")
    }
}

fn mixed_db() -> UsersDb {
    UsersDb::new(vec![
        record(
            "ada",
            1000,
            "Ada Lovelace",
            AccountState::Active,
            Some("/Users/ada"),
            Some("/System/Commands/elsh.app/Run"),
        ),
        record(
            "grace",
            1001,
            "",
            AccountState::Active,
            Some("/Users/grace"),
            Some("/System/Commands/elsh.app/Run"),
        ),
        record(
            "mallory",
            1002,
            "Locked Out",
            AccountState::Locked,
            Some("/Users/mallory"),
            Some("/System/Commands/elsh.app/Run"),
        ),
        record("devmgr", 10, "", AccountState::NoLogin, None, None),
    ])
    .expect("a valid database")
}

#[test]
fn the_directory_offers_only_accounts_a_login_could_succeed_for() {
    let db = mixed_db();
    let mut live = LiveSessions::new();
    let directory = DbAccounts::new(Some(&db), &mut live);
    // The locked account and the no-login service account are both absent:
    // a tile that could never accept a secret is never drawn.
    assert_eq!(directory.total(), 2);
    let page = directory.page(0);
    let names: Vec<&str> = page.iter().map(SessionAccount::login_name).collect();
    assert_eq!(names, ["ada", "grace"]);
}

#[test]
fn an_account_with_no_home_or_no_shell_is_not_offered() {
    // The no-login service account in the fixture is exactly that case: no
    // home, no shell, and so nothing a session could start from.
    let db = mixed_db();
    let mut live = LiveSessions::new();
    let directory = DbAccounts::new(Some(&db), &mut live);
    assert!(directory
        .page(0)
        .iter()
        .all(|account| account.login_name() != "devmgr"));
    // An *active* account cannot lack them at all: the record format
    // refuses to build one, so the chooser can never meet a half-account.
    for (home, shell) in [
        (None, Some("/System/Commands/elsh.app/Run")),
        (Some("/Users/x"), None),
        (None, None),
    ] {
        assert!(
            UserRecord::with_password(
                Identity {
                    username: "halfling",
                    uid: tairix_users::Uid(1003),
                    primary_gid: tairix_users::Gid(1003),
                    supplementary_gids: &[],
                    display_name: "",
                    home,
                    shell,
                    capabilities: CapabilitySet::empty(),
                    state: AccountState::Active,
                },
                b"secret",
                [0x42; 16],
                MIN_ITERATIONS,
            )
            .is_err(),
            "an active account with home={home:?} shell={shell:?} was accepted"
        );
    }
}

#[test]
fn an_account_with_no_display_name_is_shown_under_its_login_name() {
    let db = mixed_db();
    let mut live = LiveSessions::new();
    let directory = DbAccounts::new(Some(&db), &mut live);
    let page = directory.page(0);
    let grace = page
        .iter()
        .find(|account| account.login_name() == "grace")
        .expect("offered");
    assert_eq!(grace.display_name(), "grace");
    let ada = page
        .iter()
        .find(|account| account.login_name() == "ada")
        .expect("offered");
    assert_eq!(ada.display_name(), "Ada Lovelace");
}

#[test]
fn the_live_flag_comes_from_the_session_table() {
    let db = mixed_db();
    let mut live = LiveSessions::new();
    live.insert("ada", 1000, 42).expect("a fresh account");
    let directory = DbAccounts::new(Some(&db), &mut live);
    let page = directory.page(0);
    assert!(page
        .iter()
        .find(|account| account.login_name() == "ada")
        .expect("offered")
        .has_live_session());
    assert!(!page
        .iter()
        .find(|account| account.login_name() == "grace")
        .expect("offered")
        .has_live_session());
}

#[test]
fn no_database_offers_no_accounts() {
    let mut live = LiveSessions::new();
    let directory = DbAccounts::new(None, &mut live);
    assert_eq!(directory.total(), 0);
    assert!(directory.page(0).is_empty());
}
