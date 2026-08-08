//! Relaying one offered secret to the session authority.

use tairix_abi::session_ipc::{
    SessionRequest, SessionVerdict, SESSION_MAX_REPLY, SESSION_MAX_REQUEST,
};
use tairix_abi::time::Duration64;
use tairix_greeter::{Verdict, Verifier};
use tairix_util::secret::Wiped;

use crate::accounts::SessionTransport;

/// What the authority answered about one offered secret.
///
/// Kept for the embedder because the surface swallows the verdict: the screen
/// only needs to know it is still asking, while the service still has to
/// audit the answer and present the lockout that came with it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Answer {
    /// The answer itself.
    pub verdict: Verdict,
    /// How long the account must wait before offering again. Zero unless the
    /// authority refused with a lockout.
    pub retry_after: Duration64,
}

impl Answer {
    /// No answer was obtained, so there is no lockout to present either.
    const fn unreachable() -> Self {
        Self {
            verdict: Verdict::Unreachable,
            retry_after: Duration64::ZERO,
        }
    }
}

/// The `session-v1` client behind the authentication surface.
///
/// It decides nothing. It encodes the account and the secret, asks the
/// authority, and reports the answer — and a reply that is not a verdict
/// frame is [`Verdict::Unreachable`], never a pass and never a refusal,
/// because the authority answers a request it will not honour with a
/// well-formed frame of the *other* shape rather than an error.
///
/// The request buffer is a field rather than a local, sized once so encoding
/// can never reallocate and strand a copy of the secret in a freed block. It
/// is erased at the end of every exchange, whatever the answer, and again
/// when the verifier is dropped.
pub struct SessionVerifier<T: SessionTransport> {
    transport: T,
    answer: Option<Answer>,
    request: Wiped<SESSION_MAX_REQUEST>,
    reply: [u8; SESSION_MAX_REPLY],
}

impl<T: SessionTransport> SessionVerifier<T> {
    /// A verifier speaking over `transport`.
    pub const fn new(transport: T) -> Self {
        Self {
            transport,
            answer: None,
            request: Wiped::new(),
            reply: [0u8; SESSION_MAX_REPLY],
        }
    }

    /// The authority's last answer, taken exactly once.
    ///
    /// Taken rather than read so an embedder cannot apply one refusal's
    /// lockout twice, or audit one answer on two rounds of the event loop.
    pub fn take_answer(&mut self) -> Option<Answer> {
        self.answer.take()
    }

    /// Ask the authority about `secret`, erasing the request before returning.
    fn exchange(&mut self, account: &str, secret: &str) -> Answer {
        let answer = self.ask(account, secret);
        self.request.wipe();
        answer
    }

    /// The exchange itself, over the buffer [`exchange`] erases.
    ///
    /// [`exchange`]: Self::exchange
    fn ask(&mut self, account: &str, secret: &str) -> Answer {
        let request = SessionRequest::Authenticate {
            username: account,
            password: secret,
        };
        let Ok(len) = request.encode(&mut self.request[..]) else {
            return Answer::unreachable();
        };
        let Ok(read) = self.transport.call(&self.request[..len], &mut self.reply) else {
            return Answer::unreachable();
        };
        let read = read.min(self.reply.len());
        match SessionVerdict::decode(&self.reply[..read]) {
            Ok(SessionVerdict::Accepted) => Answer {
                verdict: Verdict::Verified,
                retry_after: Duration64::ZERO,
            },
            Ok(SessionVerdict::Refused { retry_after }) => Answer {
                verdict: Verdict::Refused,
                retry_after,
            },
            Err(_) => Answer::unreachable(),
        }
    }

    /// The request buffer, for the tests that assert it was erased.
    #[cfg(test)]
    fn request_bytes(&self) -> &[u8] {
        &self.request[..]
    }

    /// The authority behind this verifier, for the tests that inspect it.
    #[cfg(test)]
    const fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: SessionTransport> Verifier for SessionVerifier<T> {
    fn verify(&mut self, account: &str, secret: &str) -> Verdict {
        let answer = self.exchange(account, secret);
        self.answer = Some(answer);
        answer.verdict
    }
}

#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
