//! Refusals, and the reason a connection ends.
//!
//! Nothing here is advisory. Every decode refusal, every handshake refusal,
//! and every record that fails to authenticate maps to exactly one
//! [`DisconnectReason`], which is what the peer is told before the connection
//! closes — a session never ends in silence, and never ends with a code the
//! other end has to guess at.

use core::fmt;

/// Why a frame was refused.
///
/// The variants distinguish *shapes* of malformation, not values, so a test
/// can assert why a hostile frame was refused without the peer learning
/// anything it did not already know from having built the frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WireError {
    /// Fewer bytes remained than the field needs.
    Truncated,
    /// The frame decoded, but bytes remained after it. No honest encoder
    /// produces that shape, so it is refused rather than ignored.
    TrailingBytes,
    /// A message kind, variant discriminant, or reason code outside the
    /// closed set.
    UnknownDiscriminant,
    /// A fixed-width variant's zero padding was not zero — a non-canonical
    /// encoding, and a covert channel if it were tolerated.
    NonCanonicalPadding,
    /// A count or length exceeded its fixed bound, or two counts that must
    /// sum within one bound did not.
    BoundExceeded,
    /// A field's value is outside the range the protocol defines for it.
    FieldOutOfRange,
    /// A text field was not UTF-8, was empty where it may not be, or carried
    /// a control character.
    BadText,
    /// The output buffer is too small for the frame being written.
    BufferTooSmall,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "frame truncated",
            Self::TrailingBytes => "trailing bytes after frame",
            Self::UnknownDiscriminant => "unknown discriminant",
            Self::NonCanonicalPadding => "non-canonical padding",
            Self::BoundExceeded => "field bound exceeded",
            Self::FieldOutOfRange => "field out of range",
            Self::BadText => "malformed text field",
            Self::BufferTooSmall => "output buffer too small",
        })
    }
}

impl WireError {
    /// The reason the connection ends when this refusal happens.
    ///
    /// Every malformation collapses to one code: telling a peer *which* field
    /// of the frame it forged was wrong helps nobody but the peer.
    #[must_use]
    pub const fn disconnect_reason(self) -> DisconnectReason {
        match self {
            Self::BufferTooSmall => DisconnectReason::Internal,
            _ => DisconnectReason::MalformedFrame,
        }
    }
}

/// Why a session ended, as it travels on the wire and as a human reads it.
///
/// A closed set with stable codes: an abnormal end always states its reason,
/// and an unrecognised code is refused at decode rather than shown as a
/// number.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisconnectReason {
    /// The peer speaks a protocol version this build does not.
    ProtocolVersion,
    /// A frame did not decode.
    MalformedFrame,
    /// A record's declared length exceeds the fixed record bound.
    RecordTooLarge,
    /// A record did not authenticate: it was tampered with, replayed,
    /// reordered, truncated, or reflected back at its sender.
    RecordAuthentication,
    /// The record sequence for this direction is exhausted. Unreachable in
    /// practice; refused rather than wrapped, because a repeated nonce would
    /// void the cipher's guarantee.
    SequenceExhausted,
    /// The handshake did not complete: a bad realm signature, or a peer key
    /// that contributes nothing to the agreement.
    HandshakeFailed,
    /// The realm presented a different identity key than the one pinned on
    /// first connect. Surfaced to the player, never silently accepted.
    RealmIdentityChanged,
    /// The account or credential was refused. Deliberately one code for every
    /// cause, so accounts cannot be probed.
    AuthenticationFailed,
    /// The client's content documents differ from the realm's.
    ContentDigestMismatch,
    /// The client's world generator differs from the realm's, so it would
    /// walk on ground the realm does not simulate.
    GeneratorDigestMismatch,
    /// The client's rules differ from the realm's.
    RulesDigestMismatch,
    /// A per-peer rate limit was reached.
    RateLimited,
    /// The peer cannot keep up and was shed rather than allowed to grow an
    /// unbounded queue.
    BackPressure,
    /// The peer stopped answering.
    Timeout,
    /// The realm is shutting down cleanly.
    ShuttingDown,
    /// A moderator ended the session.
    Kicked,
    /// The account is banned from the realm.
    Banned,
    /// The realm failed on its own side. No detail: a fault's shape is for
    /// the realm's log, not its clients.
    Internal,
}

impl DisconnectReason {
    /// The stable wire code.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        match self {
            Self::ProtocolVersion => 1,
            Self::MalformedFrame => 2,
            Self::RecordTooLarge => 3,
            Self::RecordAuthentication => 4,
            Self::SequenceExhausted => 5,
            Self::HandshakeFailed => 6,
            Self::RealmIdentityChanged => 7,
            Self::AuthenticationFailed => 8,
            Self::ContentDigestMismatch => 9,
            Self::GeneratorDigestMismatch => 10,
            Self::RulesDigestMismatch => 11,
            Self::RateLimited => 12,
            Self::BackPressure => 13,
            Self::Timeout => 14,
            Self::ShuttingDown => 15,
            Self::Kicked => 16,
            Self::Banned => 17,
            Self::Internal => 18,
        }
    }

    /// Decode a wire code, refusing one outside the closed set.
    ///
    /// # Errors
    ///
    /// [`WireError::UnknownDiscriminant`] for any unassigned code.
    pub const fn from_u16(code: u16) -> Result<Self, WireError> {
        Ok(match code {
            1 => Self::ProtocolVersion,
            2 => Self::MalformedFrame,
            3 => Self::RecordTooLarge,
            4 => Self::RecordAuthentication,
            5 => Self::SequenceExhausted,
            6 => Self::HandshakeFailed,
            7 => Self::RealmIdentityChanged,
            8 => Self::AuthenticationFailed,
            9 => Self::ContentDigestMismatch,
            10 => Self::GeneratorDigestMismatch,
            11 => Self::RulesDigestMismatch,
            12 => Self::RateLimited,
            13 => Self::BackPressure,
            14 => Self::Timeout,
            15 => Self::ShuttingDown,
            16 => Self::Kicked,
            17 => Self::Banned,
            18 => Self::Internal,
            _ => return Err(WireError::UnknownDiscriminant),
        })
    }

    /// Every reason, in wire-code order. The one enumeration, so a test or a
    /// diagnostic cannot fall behind the set.
    pub const ALL: &'static [Self] = &[
        Self::ProtocolVersion,
        Self::MalformedFrame,
        Self::RecordTooLarge,
        Self::RecordAuthentication,
        Self::SequenceExhausted,
        Self::HandshakeFailed,
        Self::RealmIdentityChanged,
        Self::AuthenticationFailed,
        Self::ContentDigestMismatch,
        Self::GeneratorDigestMismatch,
        Self::RulesDigestMismatch,
        Self::RateLimited,
        Self::BackPressure,
        Self::Timeout,
        Self::ShuttingDown,
        Self::Kicked,
        Self::Banned,
        Self::Internal,
    ];
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ProtocolVersion => "protocol version not supported",
            Self::MalformedFrame => "malformed frame",
            Self::RecordTooLarge => "record too large",
            Self::RecordAuthentication => "record failed authentication",
            Self::SequenceExhausted => "record sequence exhausted",
            Self::HandshakeFailed => "handshake failed",
            Self::RealmIdentityChanged => "realm identity key changed",
            Self::AuthenticationFailed => "authentication failed",
            Self::ContentDigestMismatch => "content documents differ from the realm's",
            Self::GeneratorDigestMismatch => "world generator differs from the realm's",
            Self::RulesDigestMismatch => "rules differ from the realm's",
            Self::RateLimited => "rate limited",
            Self::BackPressure => "too far behind the realm",
            Self::Timeout => "timed out",
            Self::ShuttingDown => "realm shutting down",
            Self::Kicked => "removed by a moderator",
            Self::Banned => "banned from this realm",
            Self::Internal => "realm internal error",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{DisconnectReason, WireError};

    #[test]
    fn every_reason_round_trips_its_code() {
        for reason in DisconnectReason::ALL {
            assert_eq!(DisconnectReason::from_u16(reason.as_u16()), Ok(*reason));
        }
    }

    #[test]
    fn reason_codes_are_distinct_and_dense_from_one() {
        for (index, reason) in DisconnectReason::ALL.iter().enumerate() {
            let Ok(position) = u16::try_from(index) else {
                unreachable!("a short list")
            };
            assert_eq!(reason.as_u16(), position + 1);
        }
    }

    #[test]
    fn unassigned_codes_are_refused() {
        let past_end = u16::try_from(DisconnectReason::ALL.len()).expect("small") + 1;
        for code in [0, past_end, past_end + 1, u16::MAX] {
            assert_eq!(
                DisconnectReason::from_u16(code),
                Err(WireError::UnknownDiscriminant)
            );
        }
    }

    #[test]
    fn every_malformation_ends_the_connection_with_a_stated_reason() {
        for err in [
            WireError::Truncated,
            WireError::TrailingBytes,
            WireError::UnknownDiscriminant,
            WireError::NonCanonicalPadding,
            WireError::BoundExceeded,
            WireError::FieldOutOfRange,
            WireError::BadText,
        ] {
            assert_eq!(err.disconnect_reason(), DisconnectReason::MalformedFrame);
        }
        assert_eq!(
            WireError::BufferTooSmall.disconnect_reason(),
            DisconnectReason::Internal
        );
    }
}
