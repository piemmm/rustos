use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::session_ipc::{
    encode_account_page, SessionAccount, SessionRequest, SESSION_ACCOUNTS_PER_PAGE,
};
use tairix_abi::Errno;

use super::{load_accounts, DirectoryError, SessionTransport, MAX_ACCOUNTS};

fn account(login: &str, live: bool) -> SessionAccount {
    SessionAccount::new(login, login, live).expect("a short test name")
}

/// An honest directory: it reports the truth and pages in order.
struct Honest {
    accounts: Vec<SessionAccount>,
    calls: usize,
}

impl Honest {
    fn holding(count: usize) -> Self {
        let accounts = (0..count)
            .map(|index| account(&alloc::format!("user{index}"), index % 2 == 0))
            .collect();
        Self { accounts, calls: 0 }
    }
}

impl SessionTransport for Honest {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        self.calls += 1;
        let SessionRequest::Accounts { offset } = SessionRequest::decode(request)? else {
            return Err(Errno::OutOfRange);
        };
        let total = u32::try_from(self.accounts.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let from = usize::try_from(offset).map_err(|_| Errno::OutOfRange)?;
        let page: Vec<SessionAccount> = self
            .accounts
            .iter()
            .skip(from)
            .take(SESSION_ACCOUNTS_PER_PAGE)
            .copied()
            .collect();
        encode_account_page(reply, total, offset, &page)
    }
}

/// A directory that claims an enormous list and never runs out of pages.
struct Endless {
    calls: usize,
}

impl SessionTransport for Endless {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        self.calls += 1;
        let SessionRequest::Accounts { offset } = SessionRequest::decode(request)? else {
            return Err(Errno::OutOfRange);
        };
        let page = vec![account("busy", false); SESSION_ACCOUNTS_PER_PAGE];
        encode_account_page(reply, u32::MAX, offset, &page)
    }
}

/// A directory that echoes a page starting somewhere else.
struct Mismatched;

impl SessionTransport for Mismatched {
    fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        encode_account_page(reply, 64, 32, &[account("elsewhere", false)])
    }
}

/// Nothing is listening.
struct Silent;

impl SessionTransport for Silent {
    fn call(&mut self, _request: &[u8], _reply: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno::TimedOut)
    }
}

/// Something answered, but not this protocol.
struct Garbage;

impl SessionTransport for Garbage {
    fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        let noise = b"not a page at all";
        reply[..noise.len()].copy_from_slice(noise);
        Ok(noise.len())
    }
}

#[test]
fn one_page_becomes_one_tile_each() {
    let mut authority = Honest::holding(3);
    let tiles = load_accounts(&mut authority).expect("an honest directory");
    assert_eq!(tiles.len(), 3);
    assert_eq!(tiles[0].login_name(), "user0");
    assert_eq!(tiles[2].login_name(), "user2");
    assert_eq!(authority.calls, 1);
}

#[test]
fn a_multi_page_list_is_walked_to_the_end() {
    let count = SESSION_ACCOUNTS_PER_PAGE * 2 + 5;
    let mut authority = Honest::holding(count);
    let tiles = load_accounts(&mut authority).expect("an honest directory");
    assert_eq!(tiles.len(), count);
    assert_eq!(authority.calls, 3);
    assert_eq!(
        tiles[count - 1].login_name(),
        alloc::format!("user{}", count - 1)
    );
}

#[test]
fn the_live_session_flag_reaches_the_tile() {
    let mut authority = Honest::holding(2);
    let tiles = load_accounts(&mut authority).expect("an honest directory");
    assert!(tiles[0].has_live_session());
    assert!(!tiles[1].has_live_session());
}

#[test]
fn an_empty_directory_is_an_empty_list_not_an_error() {
    let mut authority = Honest::holding(0);
    let tiles = load_accounts(&mut authority).expect("an empty directory is an answer");
    assert!(tiles.is_empty());
    assert_eq!(authority.calls, 1);
}

#[test]
fn a_lying_total_cannot_spin_the_walk() {
    let mut authority = Endless { calls: 0 };
    let tiles = load_accounts(&mut authority).expect("a bounded walk");
    assert_eq!(tiles.len(), MAX_ACCOUNTS);
    assert_eq!(authority.calls, MAX_ACCOUNTS / SESSION_ACCOUNTS_PER_PAGE);
}

#[test]
fn a_page_from_the_wrong_offset_is_refused() {
    assert_eq!(
        load_accounts(&mut Mismatched).err(),
        Some(DirectoryError::Malformed)
    );
}

#[test]
fn no_answer_is_unreachable() {
    assert_eq!(
        load_accounts(&mut Silent).err(),
        Some(DirectoryError::Unreachable)
    );
    assert_eq!(DirectoryError::Unreachable.reason(), "unreachable");
}

#[test]
fn an_answer_that_is_not_a_page_is_malformed() {
    assert_eq!(
        load_accounts(&mut Garbage).err(),
        Some(DirectoryError::Malformed)
    );
    assert_eq!(DirectoryError::Malformed.reason(), "malformed reply");
}
