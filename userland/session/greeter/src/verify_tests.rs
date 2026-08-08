use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::session_ipc::{
    encode_account_page, SessionRequest, SessionVerdict, SESSION_LOGIN_NAME_MAX,
};
use tairix_abi::time::Duration64;
use tairix_abi::Errno;
use tairix_greeter::{Verdict, Verifier};

use super::SessionVerifier;
use crate::accounts::SessionTransport;

const SECRET: &str = "hunter2-and-then-some";

/// An authority answering with a fixed verdict, keeping the request bytes it
/// was handed so a test can check what actually went out.
struct Fixed {
    answer: Option<SessionVerdict>,
    seen: Vec<u8>,
}

impl Fixed {
    const fn answering(answer: SessionVerdict) -> Self {
        Self {
            answer: Some(answer),
            seen: Vec::new(),
        }
    }

    const fn silent() -> Self {
        Self {
            answer: None,
            seen: Vec::new(),
        }
    }

    /// The secret the last request carried, if it decoded as one.
    fn offered_secret(&self) -> Option<String> {
        match SessionRequest::decode(&self.seen) {
            Ok(SessionRequest::Authenticate { password, .. }) => Some(String::from(password)),
            _ => None,
        }
    }
}

impl SessionTransport for Fixed {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        self.seen.clear();
        self.seen.extend_from_slice(request);
        match self.answer {
            Some(verdict) => verdict.encode(reply),
            None => Err(Errno::TimedOut),
        }
    }
}

/// An authority answering with a well-formed frame of the wrong shape — what
/// the real one sends for a request it will not honour.
struct WrongShape;

impl SessionTransport for WrongShape {
    fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        encode_account_page(reply, 0, 0, &[])
    }
}

/// Whether `haystack` contains `needle` anywhere.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|slot| slot == needle)
}

#[test]
fn an_accepted_secret_verifies() {
    let mut verifier = SessionVerifier::new(Fixed::answering(SessionVerdict::Accepted));
    assert_eq!(verifier.verify("ann", SECRET), Verdict::Verified);
    let answer = verifier.take_answer().expect("an answer was recorded");
    assert_eq!(answer.verdict, Verdict::Verified);
    assert_eq!(answer.retry_after, Duration64::ZERO);
}

#[test]
fn a_refusal_carries_its_lockout() {
    let retry_after = Duration64::from_secs(30);
    let mut verifier =
        SessionVerifier::new(Fixed::answering(SessionVerdict::Refused { retry_after }));
    assert_eq!(verifier.verify("ann", SECRET), Verdict::Refused);
    let answer = verifier.take_answer().expect("an answer was recorded");
    assert_eq!(answer.verdict, Verdict::Refused);
    assert_eq!(answer.retry_after, retry_after);
}

#[test]
fn no_answer_is_unreachable_never_a_refusal() {
    let mut verifier = SessionVerifier::new(Fixed::silent());
    assert_eq!(verifier.verify("ann", SECRET), Verdict::Unreachable);
    let answer = verifier.take_answer().expect("an answer was recorded");
    assert_eq!(answer.verdict, Verdict::Unreachable);
    assert_eq!(answer.retry_after, Duration64::ZERO);
}

#[test]
fn a_reply_that_is_not_a_verdict_is_unreachable() {
    let mut verifier = SessionVerifier::new(WrongShape);
    assert_eq!(verifier.verify("ann", SECRET), Verdict::Unreachable);
}

#[test]
fn an_answer_is_taken_exactly_once() {
    let mut verifier = SessionVerifier::new(Fixed::answering(SessionVerdict::Accepted));
    assert!(verifier.take_answer().is_none());
    verifier.verify("ann", SECRET);
    assert!(verifier.take_answer().is_some());
    assert!(verifier.take_answer().is_none());
}

#[test]
fn a_name_too_long_for_the_wire_never_reaches_the_authority() {
    let mut verifier = SessionVerifier::new(Fixed::answering(SessionVerdict::Accepted));
    let overlong = "n".repeat(SESSION_LOGIN_NAME_MAX + 1);
    assert_eq!(verifier.verify(&overlong, SECRET), Verdict::Unreachable);
    assert!(verifier.transport().seen.is_empty());
}

#[test]
fn the_account_and_the_secret_are_what_went_out() {
    let mut verifier = SessionVerifier::new(Fixed::answering(SessionVerdict::Accepted));
    verifier.verify("ann", SECRET);
    let sent = SessionRequest::decode(&verifier.transport().seen);
    assert!(matches!(
        sent,
        Ok(SessionRequest::Authenticate {
            username: "ann",
            password: SECRET,
        })
    ));
    assert_eq!(
        verifier.transport().offered_secret().as_deref(),
        Some(SECRET)
    );
}

#[test]
fn the_request_buffer_holds_no_secret_after_any_verdict() {
    let answers = [
        Some(SessionVerdict::Accepted),
        Some(SessionVerdict::Refused {
            retry_after: Duration64::from_secs(5),
        }),
        None,
    ];
    for answer in answers {
        let authority = match answer {
            Some(verdict) => Fixed::answering(verdict),
            None => Fixed::silent(),
        };
        let mut verifier = SessionVerifier::new(authority);
        verifier.verify("ann", SECRET);
        assert!(
            !holds(verifier.request_bytes(), SECRET.as_bytes()),
            "the secret survived in the request buffer"
        );
        assert!(
            verifier.request_bytes().iter().all(|byte| *byte == 0),
            "the whole request buffer is erased, not just the secret"
        );
    }
}

#[test]
fn the_request_buffer_holds_no_secret_after_a_refused_encode() {
    let mut verifier = SessionVerifier::new(Fixed::answering(SessionVerdict::Accepted));
    let overlong = "n".repeat(SESSION_LOGIN_NAME_MAX + 1);
    verifier.verify(&overlong, SECRET);
    assert!(verifier.request_bytes().iter().all(|byte| *byte == 0));
}
