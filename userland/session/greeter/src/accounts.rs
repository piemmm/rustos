//! Reading the machine's offerable accounts off `session-v1`.

use alloc::vec::Vec;

use tairix_abi::session_ipc::{
    decode_account_page, SessionRequest, SESSION_ACCOUNTS_PER_PAGE, SESSION_MAX_REPLY,
    SESSION_MAX_REQUEST,
};
use tairix_abi::Errno;
use tairix_greeter::AccountTile;

/// The most accounts a chooser is built from.
///
/// A validation bound on the authority's answer, not a capacity to grow: a
/// page count is attacker-visible input, and a login screen that tried to
/// draw an unbounded list would allocate on an unauthenticated screen. Two
/// screens' worth of tiles is past anything a real chooser shows.
pub const MAX_ACCOUNTS: usize = 8 * SESSION_ACCOUNTS_PER_PAGE;

/// How a request reaches the session authority and its reply comes back.
///
/// An injected seam, so every branch of the paging walk and the verdict
/// relay is exercised on the host without a kernel. The production
/// implementation is one `ipc_call` against `SESSION_ENDPOINT`.
pub trait SessionTransport {
    /// Send `request` and fill `reply`, answering how many bytes came back.
    ///
    /// # Errors
    ///
    /// Whatever the transport reports. Every error is "no answer": the
    /// caller never reads it as a decision.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// Why no account list could be built.
///
/// Both leave the chooser standing with its typed-name tile alone; they are
/// distinguished so the audit record says which happened.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DirectoryError {
    /// The authority could not be reached at all.
    Unreachable,
    /// The authority answered, but not with a page of accounts.
    Malformed,
}

impl DirectoryError {
    /// A word for the audit record.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::Malformed => "malformed reply",
        }
    }
}

/// Page the whole account list into chooser tiles.
///
/// The walk is bounded three ways, because the authority's answer is input
/// like any other: by the `total` the first page reported, by
/// [`MAX_ACCOUNTS`], and by the offset having to advance on every round. A
/// page that repeats an offset, overruns the total, or never says it is last
/// therefore ends the walk instead of spinning — and the bound is checked as
/// soon as a page is consumed, so a filled list costs no further round trip.
///
/// # Errors
///
/// * [`DirectoryError::Unreachable`] — the transport gave no answer.
/// * [`DirectoryError::Malformed`] — an answer that is not an account page.
pub fn load_accounts<T: SessionTransport>(
    transport: &mut T,
) -> Result<Vec<AccountTile>, DirectoryError> {
    let mut request = [0u8; SESSION_MAX_REQUEST];
    let mut reply = [0u8; SESSION_MAX_REPLY];
    let mut tiles = Vec::new();
    let mut offset = 0u32;
    let mut total = None;

    loop {
        let page = {
            let len = SessionRequest::Accounts { offset }
                .encode(&mut request)
                .map_err(|_| DirectoryError::Malformed)?;
            let read = transport
                .call(&request[..len], &mut reply)
                .map_err(|_| DirectoryError::Unreachable)?;
            let read = read.min(reply.len());
            decode_account_page(&reply[..read]).map_err(|_| DirectoryError::Malformed)?
        };
        if page.offset() != offset {
            return Err(DirectoryError::Malformed);
        }
        let announced = usize::try_from(*total.get_or_insert(page.total()))
            .unwrap_or(usize::MAX)
            .min(MAX_ACCOUNTS);
        let room = announced.saturating_sub(tiles.len());
        for account in page.accounts().iter().take(room) {
            tiles.push(
                AccountTile::new(account.display_name(), account.login_name())
                    .with_live_session(account.has_live_session()),
            );
        }
        if tiles.len() >= announced || page.is_last() || page.accounts().is_empty() {
            return Ok(tiles);
        }
        let Ok(walked) = u32::try_from(page.accounts().len()) else {
            return Ok(tiles);
        };
        let Some(next) = offset.checked_add(walked) else {
            return Ok(tiles);
        };
        offset = next;
    }
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod tests;
