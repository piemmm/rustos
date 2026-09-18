//! Every fixed bound the protocol enforces, in one place.
//!
//! These are **security bounds on untrusted input**, not capacities: they
//! exist so a hostile peer cannot make a decoder read, iterate, or reserve
//! more than the protocol can legitimately produce. They do not scale with the
//! machine and they do not move to suit a caller — widening one to admit a
//! frame is a regression, not a fix. Where a *capacity* is genuinely wanted
//! (how many clients a realm serves, how much the gateway buffers) it belongs
//! with the resource-limit facility, not here.
//!
//! The record bound is **derived** from the largest message the encoders can
//! produce rather than picked. A hand-picked record cap that happened to sit
//! below a legal message would refuse honest traffic — a bound must be
//! consistent with the thing it bounds.

use tairix_abi::time::Time64;

/// Protocol version carried in the handshake header and echoed in
/// [`Welcome`](crate::server::Welcome). A mismatch is refused at connect and
/// never negotiated down.
pub const PROTOCOL_VERSION: u16 = 1;

/// Handshake-header magic, version-independent so a misdirected connection is
/// rejected before anything else is read.
pub const HANDSHAKE_MAGIC: u32 = u32::from_le_bytes(*b"WSNT");

/// Largest number of entities one client may be told about at a tick.
///
/// This is the hard cap interest management degrades into — above it a crowd
/// is chosen by priority rather than flooding the link — and it is also the
/// renderer's visible-entity budget, so it is one constant, not two.
pub const MAX_ENTITIES_IN_INTEREST: usize = 256;

/// Largest number of stored world edits one `WorldDelta` may carry.
pub const MAX_WORLD_EDITS: usize = 256;

/// Largest number of play events one `Event` message may carry.
pub const MAX_GAME_EVENTS: usize = 64;

/// Longest realm account name, in bytes.
pub const MAX_ACCOUNT_NAME_LEN: usize = 32;

/// Longest password a `Password` credential may carry, in bytes.
pub const MAX_PASSWORD_LEN: usize = 128;

/// Longest chat body, in bytes.
pub const MAX_CHAT_BYTES: usize = 512;

/// Longest console command, in bytes.
pub const MAX_CONSOLE_COMMAND_BYTES: usize = 512;

/// Longest console reply body, in bytes.
pub const MAX_CONSOLE_REPLY_BYTES: usize = 2048;

/// Highest authoritative tick rate a realm may declare. The default is 30 Hz;
/// the ceiling exists so a hostile `Welcome` cannot drive a client's fixed-step
/// loop into an unbounded catch-up.
pub const MAX_TICK_HZ: u16 = 240;

/// Longest day a realm may declare, in seconds (thirty days).
pub const MAX_DAY_LENGTH_SECONDS: u32 = 30 * 24 * 60 * 60;

/// World sub-units per world unit on the wire.
///
/// A power of two, so converting between the simulation's `f64` and this
/// fixed-point form is exact in binary floating point and cannot accumulate a
/// rounding difference between two Tier-1 targets.
pub const WORLD_SUB_UNITS_PER_UNIT: i32 = 1024;

/// Largest permitted squared magnitude of a movement direction, in the Q1.15
/// units a direction's components use.
///
/// `32767` is one, and the bound is `32768²` rather than `32767²` so a
/// correctly rounded diagonal (`23170, 23170`) is accepted while a
/// component-wise saturated `(32767, 32767)` — which is √2 long and would ask
/// the server for more speed than a unit vector — is not.
pub const MAX_DIRECTION_MAGNITUDE_SQ: i64 = 32_768 * 32_768;

/// Bytes of a message's kind tag, which every message body follows.
pub const MESSAGE_HEADER_LEN: usize = 2;

/// Bytes of a sealed record's cleartext header: the plaintext length.
pub const RECORD_HEADER_LEN: usize = 4;

/// Bytes of a `Time64` on this wire — the one definition, not a second one.
pub const TIME64_LEN: usize = Time64::WIRE_LEN;

/// The larger of two lengths, in a `const` context.
#[must_use]
pub const fn max_len(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

/// Encoded length of one entity's state.
pub const ENTITY_STATE_LEN: usize = 8 + 2 + 4 + 4 + 2 + 2 + 2;

/// Encoded length of one departed entity id.
pub const ENTITY_ID_LEN: usize = 8;

/// Encoded length of an account id.
pub const ACCOUNT_ID_LEN: usize = 8;

/// Encoded length of a world position: two signed sub-unit coordinates.
pub const WORLD_POINT_LEN: usize = 4 + 4;

/// Payload bytes a world edit's widest variant uses, after its kind tag. A
/// narrower variant zero-pads to this, and the padding is checked on decode.
pub const WORLD_CHANGE_PAYLOAD_LEN: usize = 5;

/// Encoded length of one world edit: cell, kind tag, and the fixed payload.
pub const WORLD_EDIT_LEN: usize = 2 + 2 + 1 + WORLD_CHANGE_PAYLOAD_LEN;

/// Payload bytes a play event's widest variant uses, after its point.
pub const PLAY_EVENT_PAYLOAD_LEN: usize = 21;

/// Encoded length of one play event: tick, kind tag, point, fixed payload.
pub const GAME_EVENT_LEN: usize = 8 + 1 + WORLD_POINT_LEN + PLAY_EVENT_PAYLOAD_LEN;

/// Encoded length of a tick instant: a tick and a phase within it.
pub const TICK_INSTANT_LEN: usize = 8 + 2;

/// Encoded length of an aim: an optional target, a point, and the view time
/// the server clamps a lag-compensation rewind against.
pub const AIM_LEN: usize = 1 + ENTITY_ID_LEN + WORLD_POINT_LEN + TICK_INSTANT_LEN;

/// Largest client message body the encoders can produce.
pub const MAX_CLIENT_BODY_LEN: usize = {
    let authenticate = 1 + 1 + MAX_ACCOUNT_NAME_LEN + max_len(1 + MAX_PASSWORD_LEN, 32 + 64);
    let select_character = 8;
    let intent =
        8 + TICK_INSTANT_LEN + 1 + max_len(max_len(4, 2 + AIM_LEN), max_len(8, 1 + 2 + 1 + 2));
    let chat = 1 + 1 + MAX_ACCOUNT_NAME_LEN + 2 + MAX_CHAT_BYTES;
    let console = 2 + MAX_CONSOLE_COMMAND_BYTES;
    let ping = 8;
    max_len(
        max_len(
            max_len(authenticate, select_character),
            max_len(intent, chat),
        ),
        max_len(console, ping),
    )
};

/// Largest server message body the encoders can produce.
pub const MAX_SERVER_BODY_LEN: usize = {
    let welcome = 2 + 8 + 2 + 4 + 32 + 32 + 32;
    let auth_result = 1 + ACCOUNT_ID_LEN;
    let snapshot = 8 + 8 + 2 + MAX_ENTITIES_IN_INTEREST * ENTITY_STATE_LEN;
    // A delta's entered and updated sets are both inside the interest set, so
    // together they cannot exceed its cap; the decoder enforces that sum, and
    // the bound is sized to it rather than to twice the cap.
    let delta = 8
        + 8
        + 2
        + 2
        + 2
        + MAX_ENTITIES_IN_INTEREST * ENTITY_STATE_LEN
        + MAX_ENTITIES_IN_INTEREST * ENTITY_ID_LEN;
    let world_delta = WORLD_POINT_LEN + 2 + MAX_WORLD_EDITS * WORLD_EDIT_LEN;
    let events = 2 + MAX_GAME_EVENTS * GAME_EVENT_LEN;
    let chat = 1 + 1 + MAX_ACCOUNT_NAME_LEN + 2 + MAX_CHAT_BYTES;
    let console_reply = 1 + 2 + MAX_CONSOLE_REPLY_BYTES;
    let pong = 8 + TIME64_LEN + 8;
    let disconnect = 2;
    max_len(
        max_len(
            max_len(max_len(welcome, auth_result), max_len(snapshot, delta)),
            max_len(max_len(world_delta, events), max_len(chat, console_reply)),
        ),
        max_len(pong, disconnect),
    )
};

/// Largest plaintext one sealed record may carry, derived from the widest
/// message either direction can produce.
pub const MAX_PLAINTEXT_LEN: usize =
    MESSAGE_HEADER_LEN + max_len(MAX_CLIENT_BODY_LEN, MAX_SERVER_BODY_LEN);

/// Largest sealed record on the wire: header, ciphertext, and tag.
pub const MAX_RECORD_LEN: usize =
    RECORD_HEADER_LEN + MAX_PLAINTEXT_LEN + tairix_crypto::AEAD_TAG_LEN;

// A record bound that could refuse a message the encoders can legitimately
// produce would be a defect rather than a defence, so the relationship is a
// build-time guarantee, not something a test might be removed along with.
const _: () = assert!(MAX_PLAINTEXT_LEN > MAX_CLIENT_BODY_LEN);
const _: () = assert!(MAX_PLAINTEXT_LEN > MAX_SERVER_BODY_LEN);
const _: () = assert!(MAX_RECORD_LEN > MAX_PLAINTEXT_LEN);

#[cfg(test)]
mod tests {
    use super::{max_len, MAX_DIRECTION_MAGNITUDE_SQ, WORLD_SUB_UNITS_PER_UNIT};

    #[test]
    fn world_sub_units_are_a_power_of_two() {
        let units = u32::try_from(WORLD_SUB_UNITS_PER_UNIT).expect("positive");
        assert!(
            units.is_power_of_two(),
            "conversion to and from the simulation's f64 must be exact"
        );
    }

    #[test]
    fn direction_bound_admits_a_rounded_diagonal_and_refuses_a_saturated_one() {
        let diagonal = 23_170i64;
        assert!(diagonal * diagonal * 2 <= MAX_DIRECTION_MAGNITUDE_SQ);
        let saturated = 32_767i64;
        assert!(saturated * saturated * 2 > MAX_DIRECTION_MAGNITUDE_SQ);
    }

    #[test]
    fn max_len_picks_the_larger() {
        assert_eq!(max_len(3, 9), 9);
        assert_eq!(max_len(9, 3), 9);
        assert_eq!(max_len(4, 4), 4);
    }
}
