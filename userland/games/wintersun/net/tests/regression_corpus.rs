//! The committed regression corpus, replayed with pinned verdicts.
//!
//! `fuzz_wire.rs` proves the decoders never panic over a continuing
//! pseudo-random stream; this is the companion corpus the charter requires
//! alongside it, so a crashing input once found is replayed on the bytes it
//! was filed under rather than only by a fuzzer that may not draw them again.
//!
//! Two contracts:
//!
//! 1. [`corpus`] holds one frame per message kind, per variant, and per field
//!    boundary, built from the crate's own encoders — so it cannot drift from
//!    the source of truth. Every entry must decode, re-encode to the same
//!    bytes, and re-decode equal.
//! 2. The hand-written entries below pin a *fixed* accept-or-reject verdict
//!    on bytes that ring an accept/reject edge, so a change that silently
//!    loosens a bound (admits a control character, a count past its cap, a
//!    non-canonical padding byte) or tightens one (refuses a legal frame)
//!    fails here.
//!
//! No crash has been found in these decoders to date. A new one is appended
//! below as a named byte literal with its own verdict test.

use tairix_wintersun_net::bounds::{
    MAX_ENTITIES_IN_INTEREST, MAX_GAME_EVENTS, MAX_PLAINTEXT_LEN, MAX_RECORD_LEN, MAX_WORLD_EDITS,
};
use tairix_wintersun_net::client::ClientMessage;
use tairix_wintersun_net::server::ServerMessage;
use tairix_wintersun_net::{DisconnectReason, Session, SessionKeys, WireError};

mod corpus;

#[test]
fn every_corpus_frame_decodes_and_re_encodes_to_the_same_bytes() {
    let mut out = vec![0u8; MAX_PLAINTEXT_LEN];
    for frame in corpus::client_frames() {
        let message = ClientMessage::decode(&frame).expect("a corpus frame decodes");
        let n = message.encode(&mut out).expect("and re-encodes");
        assert_eq!(&out[..n], frame.as_slice());
        assert_eq!(
            ClientMessage::decode(&out[..n]).expect("and re-decodes"),
            message
        );
    }
    for frame in corpus::server_frames() {
        let message = ServerMessage::decode(&frame).expect("a corpus frame decodes");
        let n = message.encode(&mut out).expect("and re-encodes");
        assert_eq!(&out[..n], frame.as_slice());
        assert_eq!(
            ServerMessage::decode(&out[..n]).expect("and re-decodes"),
            message
        );
    }
}

#[test]
fn every_corpus_frame_fits_one_record_and_survives_the_transport() {
    let (c2s, s2c) = ([0x31u8; 32], [0x32u8; 32]);
    let mut client = Session::new(SessionKeys::new(c2s, s2c));
    let mut realm = Session::new(SessionKeys::new(c2s, s2c).swapped());
    let mut record = vec![0u8; MAX_RECORD_LEN];
    for frame in corpus::client_frames() {
        assert!(frame.len() <= MAX_PLAINTEXT_LEN);
        let n = client.seal_record(&frame, &mut record).expect("seals");
        let open = realm.open_record(&mut record[..n]).expect("opens");
        assert_eq!(open.plaintext(), frame.as_slice());
    }
    for frame in corpus::server_frames() {
        assert!(frame.len() <= MAX_PLAINTEXT_LEN);
        let n = client.seal_record(&frame, &mut record).expect("seals");
        let open = realm.open_record(&mut record[..n]).expect("opens");
        assert_eq!(open.plaintext(), frame.as_slice());
    }
}

/// A snapshot whose count is one past the interest cap, header only. The
/// decoder must refuse on the count alone, before reading an entity.
const SNAPSHOT_COUNT_PAST_CAP: &[u8] = &[
    0x03, 0x00, // kind: Snapshot
    0, 0, 0, 0, 0, 0, 0, 0, // tick
    0, 0, 0, 0, 0, 0, 0, 0, // acknowledged intent
    0x01, 0x01, // count = 257, one past the cap of 256
];

/// A delta whose entered and updated counts are each inside the cap but
/// whose sum is not. Refusing each alone would admit a frame twice the size
/// the interest model can produce.
const DELTA_SUM_PAST_CAP: &[u8] = &[
    0x04, 0x00, // kind: Delta
    0, 0, 0, 0, 0, 0, 0, 0, // tick
    0, 0, 0, 0, 0, 0, 0, 0, // acknowledged intent
    0x00, 0x01, // entered = 256
    0x01, 0x00, // updated = 1
    0x00, 0x00, // departed = 0
];

/// A chat line carrying an ANSI escape sequence. Chat is rendered as data,
/// so this must never reach a console.
const CHAT_WITH_ESCAPE: &[u8] = &[
    0x04, 0x00, // kind: Chat
    0x01, // channel: Say
    0x00, // no whisper target
    0x09, 0x00, // body length 9
    b'c', b'l', b'e', b'a', b'r', 0x1B, b'[', b'2', b'J',
];

/// A `Say` line carrying a whisper target. A whisper names exactly one
/// account and no other channel names any, so the mismatch is refused.
const SAY_WITH_A_TARGET: &[u8] = &[
    0x04, 0x00, // kind: Chat
    0x01, // channel: Say
    0x02, b'h', b'i', // a target, which Say may not carry
    0x02, 0x00, b'h', b'i',
];

/// An authentication refusal whose fixed-width padding is not zero — the
/// covert channel a tolerant decoder would open.
const AUTH_REFUSAL_WITH_DIRTY_PADDING: &[u8] = &[
    0x02, 0x00, // kind: AuthResult
    0x02, // outcome: Refused
    0x01, 0, 0, 0, 0, 0, 0, 0, // padding, first byte set
];

/// A `Welcome` declaring a zero tick rate, which would divide a client's
/// fixed step by nothing.
const WELCOME_WITH_ZERO_TICK_RATE: &[u8] = &[
    0x01, 0x00, // kind: Welcome
    0x01, 0x00, // protocol version
    0, 0, 0, 0, 0, 0, 0, 0, // realm seed
    0x00, 0x00, // tick_hz = 0
    0x2C, 0x01, 0x00, 0x00, // day length = 300
];

/// An events batch claiming more events than the cap allows.
const EVENTS_COUNT_PAST_CAP: &[u8] = &[
    0x06, 0x00, // kind: Events
    0x41, 0x00, // count = 65, one past the cap of 64
];

/// A world delta claiming more edits than the cap allows.
const WORLD_DELTA_COUNT_PAST_CAP: &[u8] = &[
    0x05, 0x00, // kind: WorldDelta
    0, 0, 0, 0, 0, 0, 0, 0, // chunk
    0x01, 0x01, // count = 257, one past the cap of 256
];

/// A disconnect naming a code outside the closed set.
const DISCONNECT_WITH_UNASSIGNED_REASON: &[u8] = &[
    0x0A, 0x00, // kind: Disconnect
    0xFF, 0x00, // an unassigned reason code
];

#[test]
fn a_count_past_its_cap_is_refused_before_the_run_is_read() {
    assert_eq!(
        ServerMessage::decode(SNAPSHOT_COUNT_PAST_CAP),
        Err(WireError::BoundExceeded)
    );
    assert_eq!(
        ServerMessage::decode(EVENTS_COUNT_PAST_CAP),
        Err(WireError::BoundExceeded)
    );
    assert_eq!(
        ServerMessage::decode(WORLD_DELTA_COUNT_PAST_CAP),
        Err(WireError::BoundExceeded)
    );
    // The bytes stop at the count, so a decoder that read the run first
    // would have reported truncation instead.
    assert!(SNAPSHOT_COUNT_PAST_CAP.len() < MAX_ENTITIES_IN_INTEREST);
    assert!(EVENTS_COUNT_PAST_CAP.len() < MAX_GAME_EVENTS);
    assert!(WORLD_DELTA_COUNT_PAST_CAP.len() < MAX_WORLD_EDITS);
}

#[test]
fn a_delta_whose_runs_sum_past_the_interest_cap_is_refused() {
    assert_eq!(
        ServerMessage::decode(DELTA_SUM_PAST_CAP),
        Err(WireError::BoundExceeded)
    );
}

#[test]
fn chat_carrying_a_control_sequence_is_refused() {
    assert_eq!(
        ClientMessage::decode(CHAT_WITH_ESCAPE),
        Err(WireError::BadText)
    );
}

#[test]
fn a_channel_and_target_mismatch_is_refused() {
    assert_eq!(
        ClientMessage::decode(SAY_WITH_A_TARGET),
        Err(WireError::FieldOutOfRange)
    );
}

#[test]
fn non_canonical_padding_is_refused() {
    assert_eq!(
        ServerMessage::decode(AUTH_REFUSAL_WITH_DIRTY_PADDING),
        Err(WireError::NonCanonicalPadding)
    );
}

#[test]
fn a_realm_parameter_outside_its_range_is_refused() {
    assert_eq!(
        ServerMessage::decode(WELCOME_WITH_ZERO_TICK_RATE),
        Err(WireError::FieldOutOfRange)
    );
}

#[test]
fn an_unassigned_disconnect_reason_is_refused() {
    assert_eq!(
        ServerMessage::decode(DISCONNECT_WITH_UNASSIGNED_REASON),
        Err(WireError::UnknownDiscriminant)
    );
    // Every assigned one decodes, so the refusal above is about the code and
    // not about the frame's shape.
    for reason in DisconnectReason::ALL {
        let mut frame = DISCONNECT_WITH_UNASSIGNED_REASON.to_vec();
        frame[2..4].copy_from_slice(&reason.as_u16().to_le_bytes());
        assert_eq!(
            ServerMessage::decode(&frame),
            Ok(ServerMessage::Disconnect(*reason))
        );
    }
}

#[test]
fn every_pinned_entry_is_refused_rather_than_merely_not_crashing() {
    // A corpus entry that started passing would mean a bound moved, which is
    // the regression this file exists to catch.
    for frame in [CHAT_WITH_ESCAPE, SAY_WITH_A_TARGET] {
        assert!(ClientMessage::decode(frame).is_err());
    }
    for frame in [
        SNAPSHOT_COUNT_PAST_CAP,
        DELTA_SUM_PAST_CAP,
        AUTH_REFUSAL_WITH_DIRTY_PADDING,
        WELCOME_WITH_ZERO_TICK_RATE,
        EVENTS_COUNT_PAST_CAP,
        WORLD_DELTA_COUNT_PAST_CAP,
        DISCONNECT_WITH_UNASSIGNED_REASON,
    ] {
        assert!(ServerMessage::decode(frame).is_err());
    }
}
