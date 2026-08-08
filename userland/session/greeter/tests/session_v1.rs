//! The two halves of `session-v1`, against each other.
//!
//! Every other test in this crate answers the greeter's requests from a mock,
//! which proves the client is self-consistent and nothing about whether the
//! *authority* agrees with it. Here the transport seam is wired straight to
//! `tairix_login::handle_session_request`, the real decision half, so a
//! divergence in either direction — a field encoded one way and read another,
//! a page the client walks differently from the way the authority pages it, a
//! verdict shape one side does not recognise — fails this test rather than
//! surfacing on a machine nobody can log into.
//!
//! This is the one place the login authority is a dependency of the greeter,
//! and it is a **test-only** edge (a dev-dependency): at run time the two are
//! separate processes that share nothing but the wire types in `lib/abi`.

use tairix_abi::session_ipc::{SessionAccount, SESSION_ACCOUNTS_PER_PAGE};
use tairix_abi::time::Duration64;
use tairix_abi::Errno;
use tairix_greeter::{Verdict, Verifier};
use tairix_greeter_service::accounts::{load_accounts, SessionTransport};
use tairix_greeter_service::SessionVerifier;
use tairix_log::{Event, Sink};
use tairix_login::session::{AuthenticatedUser, Authenticator, Credentials, Gid, Uid};
use tairix_login::{handle_session_request, AttemptBudget, SessionDirectory};

/// The console both sides are on.
const CONSOLE: u64 = 1;

/// The uid the kernel attests for the greeter service account.
const GREETER: u32 = tairix_users::GREETER_UID.0;

/// The one account the fixture authority knows, and its secret.
const ACCOUNT: &str = "ann";
const SECRET: &str = "correct-horse";

/// Swallows the authority's audit records: this test is about the wire, and
/// the audit trail has its own tests on the authority's side.
struct Quiet;

impl Sink for Quiet {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// The machine's accounts, as many as the test asked for.
struct Directory {
    accounts: Vec<SessionAccount>,
}

impl Directory {
    fn holding(count: usize) -> Self {
        let accounts = (0..count)
            .map(|index| {
                let login = if index == 0 {
                    ACCOUNT.to_string()
                } else {
                    format!("user{index}")
                };
                let display = format!("Account {index}");
                SessionAccount::new(&display, &login, index % 3 == 0).expect("short fixture names")
            })
            .collect();
        Self { accounts }
    }
}

impl SessionDirectory for Directory {
    fn total(&self) -> u32 {
        u32::try_from(self.accounts.len()).unwrap_or(u32::MAX)
    }

    fn page(&self, offset: u32) -> Vec<SessionAccount> {
        let from = usize::try_from(offset).unwrap_or(usize::MAX);
        self.accounts
            .iter()
            .skip(from)
            .take(SESSION_ACCOUNTS_PER_PAGE)
            .copied()
            .collect()
    }

    fn background(&mut self, _peer_uid: u32) -> bool {
        // The greeter never asks a session to step aside; that is the
        // desktop's fast-switch path, tested on the authority's own side.
        false
    }
}

/// Verifies exactly one account's one secret, with the identical refusal for
/// everything else — the same fail-closed shape the production authenticator
/// has.
struct OneAccount;

impl Authenticator for OneAccount {
    fn authenticate(&self, credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
        if credentials.username != ACCOUNT || credentials.password != SECRET {
            return Err(Errno::PermissionDenied);
        }
        Ok(AuthenticatedUser {
            username: ACCOUNT.to_string(),
            uid: Uid(1000),
            primary_gid: Gid(1000),
            supplementary_gids: Vec::new(),
            capabilities: tairix_caps::CapabilitySet::empty(),
            home: "Users:/ann".to_string(),
            shell: "/System/Commands/elsh.app/Run".to_string(),
        })
    }

    fn authenticate_uid(&self, _uid: u32, _password: &str) -> Result<AuthenticatedUser, Errno> {
        Err(Errno::PermissionDenied)
    }
}

/// The transport that *is* the authority: every request the greeter sends is
/// decided by `handle_session_request` in the same process, under the greeter's
/// attested identity on the authority's own console.
struct Authority {
    accounts: Directory,
    budget: AttemptBudget,
    now: Duration64,
    peer_uid: Option<u32>,
}

impl Authority {
    fn holding(count: usize) -> Self {
        Self {
            accounts: Directory::holding(count),
            budget: AttemptBudget::default(),
            now: Duration64::ZERO,
            peer_uid: Some(GREETER),
        }
    }

    /// The same authority, but reached by somebody who is not the greeter.
    fn from_an_impostor(count: usize) -> Self {
        Self {
            peer_uid: Some(GREETER + 1),
            ..Self::holding(count)
        }
    }
}

impl SessionTransport for Authority {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        let served = handle_session_request(
            request,
            self.peer_uid,
            CONSOLE,
            CONSOLE,
            &mut self.accounts,
            &OneAccount,
            &mut self.budget,
            self.now,
            &Quiet,
            reply,
        );
        assert!(
            !served.stepped_aside,
            "the greeter never asks a session to step aside"
        );
        Ok(served.len)
    }
}

#[test]
fn the_accounts_the_authority_publishes_become_the_tiles_the_greeter_draws() {
    let mut authority = Authority::holding(3);
    let tiles = load_accounts(&mut authority).expect("the authority answered");
    assert_eq!(tiles.len(), 3);
    assert_eq!(tiles[0].login_name(), ACCOUNT);
    assert_eq!(tiles[0].display_name(), "Account 0");
    assert!(tiles[0].has_live_session());
    assert!(!tiles[1].has_live_session());
}

#[test]
fn a_multi_page_list_is_paged_the_way_the_authority_pages_it() {
    let count = SESSION_ACCOUNTS_PER_PAGE * 2 + 7;
    let mut authority = Authority::holding(count);
    let tiles = load_accounts(&mut authority).expect("the authority answered");
    assert_eq!(tiles.len(), count);
    assert_eq!(tiles[count - 1].login_name(), format!("user{}", count - 1));
}

#[test]
fn a_wrong_secret_comes_back_refused_with_a_lockout_the_greeter_can_present() {
    let mut verifier = SessionVerifier::new(Authority::holding(1));
    assert_eq!(verifier.verify(ACCOUNT, "not-the-secret"), Verdict::Refused);
    let answer = verifier.take_answer().expect("an answer came back");
    assert_eq!(answer.verdict, Verdict::Refused);
}

#[test]
fn the_right_secret_comes_back_accepted() {
    let mut verifier = SessionVerifier::new(Authority::holding(1));
    assert_eq!(verifier.verify(ACCOUNT, SECRET), Verdict::Verified);
    assert_eq!(
        verifier.take_answer().map(|answer| answer.verdict),
        Some(Verdict::Verified)
    );
}

#[test]
fn an_unknown_account_is_refused_exactly_like_a_wrong_secret() {
    let mut unknown = SessionVerifier::new(Authority::holding(1));
    assert_eq!(unknown.verify("nobody", SECRET), Verdict::Refused);
    let unknown = unknown.take_answer().expect("an answer came back");

    let mut wrong = SessionVerifier::new(Authority::holding(1));
    assert_eq!(wrong.verify(ACCOUNT, "not-the-secret"), Verdict::Refused);
    let wrong = wrong.take_answer().expect("an answer came back");

    assert_eq!(
        unknown, wrong,
        "a reply must not distinguish an unknown account from a wrong secret"
    );
}

#[test]
fn the_whole_flow_pages_then_refuses_then_accepts() {
    let mut authority = Authority::holding(3);
    let tiles = load_accounts(&mut authority).expect("the authority answered");
    assert_eq!(tiles.len(), 3);
    assert_eq!(tiles[0].login_name(), ACCOUNT);

    let mut verifier = SessionVerifier::new(authority);
    assert_eq!(verifier.verify(ACCOUNT, "first-guess"), Verdict::Refused);
    verifier.take_answer().expect("an answer came back");

    assert_eq!(verifier.verify(ACCOUNT, SECRET), Verdict::Verified);
    assert_eq!(
        verifier.take_answer().map(|answer| answer.verdict),
        Some(Verdict::Verified)
    );
}

#[test]
fn a_repeated_wrong_secret_earns_a_lockout_the_surface_can_present() {
    let mut verifier = SessionVerifier::new(Authority::holding(1));
    let mut waited = Duration64::ZERO;
    for _ in 0..8 {
        assert_eq!(verifier.verify(ACCOUNT, "guess"), Verdict::Refused);
        let answer = verifier.take_answer().expect("an answer came back");
        if answer.retry_after > waited {
            waited = answer.retry_after;
        }
    }
    assert!(
        waited > Duration64::ZERO,
        "the authority's attempt budget eventually reports a wait, and the \
         greeter carries it to the surface"
    );
}

#[test]
fn a_caller_that_is_not_the_greeter_learns_nothing() {
    let mut impostor = Authority::from_an_impostor(3);
    let tiles = load_accounts(&mut impostor).expect("a well-formed empty page, not an error");
    assert!(
        tiles.is_empty(),
        "an unauthorised caller is answered with no accounts at all"
    );

    let mut verifier = SessionVerifier::new(Authority::from_an_impostor(1));
    assert_eq!(
        verifier.verify(ACCOUNT, SECRET),
        Verdict::Refused,
        "the right secret from the wrong caller is still a refusal"
    );
}
