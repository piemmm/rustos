//! Asking the desktop session to adopt a settings document.
//!
//! The one client of the pinboard rendezvous, shared by every surface that
//! edits the desktop's own settings — the wallpaper chooser's backdrop keys
//! and the Settings application's appearance keys. A second copy of this
//! round trip would be two places for "what did the session say" to drift
//! apart.
//!
//! Nothing here holds authority. An application publishes only its own
//! app-data scope, so no surface can write the desktop's document at all: it
//! renders the keys it edits and *asks*, and the session decides, applies,
//! persists, and answers with what the store did.

use alloc::string::String;

#[cfg(feature = "rt")]
use tairix_abi::pinboard_ipc::{PinboardDocument, PinboardRequest, PINBOARD_ENDPOINT};
#[cfg(feature = "rt")]
use tairix_abi::reply::{decode_status_reply, STATUS_REPLY_LEN};

/// The outcome of asking the desktop session to adopt a rendered settings
/// document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    /// The request is with the session and no answer has come back yet.
    ///
    /// A surface shows this rather than the previous attempt's answer, so a
    /// footer can never report a result the store has not given: the apply
    /// is carried out on a worker and the window keeps drawing meanwhile.
    Applying,
    /// The session adopted and persisted the change.
    Applied,
    /// The session refused the request, with the reason it gave.
    Refused(String),
    /// No desktop session answered the pinboard rendezvous at all (the
    /// call itself failed) — distinct from an authenticated refusal.
    NoDesktop,
}

/// Ask the desktop session to adopt `document` and report what it said.
///
/// Nothing is reported as applied that the session did not accept: a
/// rendezvous that does not answer and a typed refusal are distinct
/// outcomes, and the reply waits for the *store*, so an `Applied` means the
/// document was actually written.
#[cfg(feature = "rt")]
#[must_use]
pub fn apply(document: PinboardDocument) -> ApplyOutcome {
    let request = PinboardRequest::Apply { document }.to_le_bytes();
    let mut reply = [0u8; STATUS_REPLY_LEN];
    let Ok(len) = tairix_rt::ipc_call(PINBOARD_ENDPOINT, &request, &mut reply) else {
        return ApplyOutcome::NoDesktop;
    };
    match decode_status_reply(&reply[..len.min(reply.len())]) {
        Ok(()) => ApplyOutcome::Applied,
        Err(err) => ApplyOutcome::Refused(alloc::format!("{err:?}")),
    }
}
