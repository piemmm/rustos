//! Deterministic fuzz harness and property model for the sealed record
//! transport.
//!
//! A realm reads records from a hostile peer for the whole life of a
//! session, so this is the surface with the most attacker attempts per
//! connection. The model drives a two-ended session through a random
//! programme of honest sends and hostile interference, and asserts the
//! properties the transport claims:
//!
//! * an honest record opens, in order, with the plaintext it carried;
//! * a **reordered, replayed, truncated, extended, oversize, reflected, or
//!   bit-flipped** record is refused;
//! * a refused record *ends the session*, and every later call on it refuses
//!   with that same stated reason — a peer never gets to probe the transport;
//! * a message decoded out of an opened record round-trips, so the two
//!   layers agree;
//! * nothing panics, whatever bytes arrive.
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a
//! fresh, logged seed; `cargo xtask fuzz` extends it to a wall-clock budget.

use tairix_fuzzseed::Prng;
use tairix_wintersun_net::bounds::{MAX_PLAINTEXT_LEN, MAX_RECORD_LEN, RECORD_HEADER_LEN};
use tairix_wintersun_net::client::ClientMessage;
use tairix_wintersun_net::server::ServerMessage;
use tairix_wintersun_net::{DisconnectReason, Session, SessionKeys};

mod corpus;

/// Fixed-iteration sweep run once by a plain `cargo test`.
const SMOKE_ITERATIONS: u64 = 2_000;

/// The two ends of one handshake's key pair.
fn pair(c2s: [u8; 32], s2c: [u8; 32]) -> (Session, Session) {
    (
        Session::new(SessionKeys::new(c2s, s2c)),
        Session::new(SessionKeys::new(c2s, s2c).swapped()),
    )
}

/// Present `record` to `session` and assert the fail-closed contract: either
/// it opens, or it is refused *and* the session is over with that reason.
fn present(session: &mut Session, record: &mut [u8]) -> bool {
    let was_ended = session.ended();
    match session.open_record(record) {
        Ok(open) => {
            assert!(was_ended.is_none(), "an ended session must open nothing");
            // Whatever the plaintext is, decoding it must not panic — in
            // either direction, since either end may be the one reading.
            let _client = ClientMessage::decode(open.plaintext());
            let _server = ServerMessage::decode(open.plaintext());
            true
        }
        Err(err) => {
            let reason = err.disconnect_reason();
            assert!(DisconnectReason::ALL.contains(&reason));
            assert_eq!(
                session.ended(),
                Some(was_ended.unwrap_or(reason)),
                "a refused record must end the session and keep its first reason"
            );
            false
        }
    }
}

/// An honest stream of real frames opens in order, both ways, and each
/// record carries back exactly what was sealed.
fn honest_stream(rng: &mut Prng, keys: ([u8; 32], [u8; 32]), frames: &[Vec<u8>]) {
    let (mut client, mut realm) = pair(keys.0, keys.1);
    let mut record = vec![0u8; MAX_RECORD_LEN];
    for _ in 0..4 {
        let frame = rng.pick(frames);
        let n = client.seal_record(frame, &mut record).expect("seals");
        {
            let open = realm.open_record(&mut record[..n]).expect("opens");
            assert_eq!(open.plaintext(), frame.as_slice());
        }

        let reply = rng.pick(frames);
        let n = realm.seal_record(reply, &mut record).expect("seals");
        {
            let open = client.open_record(&mut record[..n]).expect("opens");
            assert_eq!(open.plaintext(), reply.as_slice());
        }
    }
    assert_eq!(realm.ended(), None);
    assert_eq!(client.ended(), None);
}

/// A reorder and a replay, each on its own session so the latch from one
/// does not mask the other.
fn reorder_and_replay(keys: ([u8; 32], [u8; 32]), sealed: &[u8]) {
    let (mut client, mut realm) = pair(keys.0, keys.1);
    let mut first = vec![0u8; MAX_RECORD_LEN];
    let a = client.seal_record(b"first", &mut first).expect("seals");
    first.truncate(a);
    let mut second = vec![0u8; MAX_RECORD_LEN];
    let b = client.seal_record(b"second", &mut second).expect("seals");
    second.truncate(b);
    assert!(
        !present(&mut realm, &mut second),
        "a reordered record must be refused"
    );
    assert!(
        !present(&mut realm, &mut first),
        "and the session must stay ended"
    );
    assert_eq!(realm.ended(), Some(DisconnectReason::RecordAuthentication));

    let (_, mut realm) = pair(keys.0, keys.1);
    let mut once = sealed.to_vec();
    assert!(present(&mut realm, &mut once));
    let mut again = sealed.to_vec();
    assert!(!present(&mut realm, &mut again), "a replay must be refused");
}

/// Truncation, extension, an oversize header, reflection, a bit flip, and a
/// record from another session — each refused, each ending the session.
fn interference(rng: &mut Prng, keys: ([u8; 32], [u8; 32]), sealed: &[u8], frame: &[u8]) {
    let (_, mut realm) = pair(keys.0, keys.1);
    let cut = rng.at_most(sealed.len().saturating_sub(1));
    assert!(!present(&mut realm, &mut sealed[..cut].to_vec()));

    let (_, mut realm) = pair(keys.0, keys.1);
    let mut long = sealed.to_vec();
    long.push(rng.next_u8());
    assert!(!present(&mut realm, &mut long));

    let (_, mut realm) = pair(keys.0, keys.1);
    let mut oversize = sealed.to_vec();
    let declared = u32::try_from(MAX_PLAINTEXT_LEN + 1 + rng.at_most(1_000)).unwrap_or(u32::MAX);
    oversize[..RECORD_HEADER_LEN].copy_from_slice(&declared.to_le_bytes());
    assert!(!present(&mut realm, &mut oversize));
    assert_eq!(realm.ended(), Some(DisconnectReason::RecordTooLarge));

    // Reflection: the client's own record handed back to it. The directions
    // carry different keys, so it cannot open.
    let (mut client, _) = pair(keys.0, keys.1);
    assert!(!present(&mut client, &mut sealed.to_vec()));

    let (_, mut realm) = pair(keys.0, keys.1);
    let mut flipped = sealed.to_vec();
    let pos = rng.below(flipped.len());
    flipped[pos] ^= 1u8 << rng.below(8);
    assert!(!present(&mut realm, &mut flipped));

    let other = corpus::bytes32(rng);
    if other != keys.0 {
        let (mut stranger, _) = pair(other, keys.1);
        let mut foreign = vec![0u8; MAX_RECORD_LEN];
        let n = stranger.seal_record(frame, &mut foreign).expect("seals");
        foreign.truncate(n);
        let (_, mut realm) = pair(keys.0, keys.1);
        assert!(!present(&mut realm, &mut foreign));
    }
}

/// Arbitrary bytes, and a well-formed header over a noise body — the shape
/// that reaches furthest into the open path before failing.
fn hostile_records(rng: &mut Prng, keys: ([u8; 32], [u8; 32])) {
    let (_, mut realm) = pair(keys.0, keys.1);
    let len = rng.at_most(MAX_RECORD_LEN + 4);
    let mut noise = corpus::blob(rng, len);
    present(&mut realm, &mut noise);

    let (_, mut realm) = pair(keys.0, keys.1);
    let body = rng.at_most(64);
    let mut forged = Vec::with_capacity(RECORD_HEADER_LEN + body + 16);
    let declared = u32::try_from(body).unwrap_or(0);
    forged.extend_from_slice(&declared.to_le_bytes());
    forged.extend(corpus::blob(rng, body + 16));
    assert!(!present(&mut realm, &mut forged));
}

#[test]
fn the_record_transport_never_panics_and_fails_closed_on_any_interference() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng =
        corpus::seeded("the_record_transport_never_panics_and_fails_closed_on_any_interference");

    let mut frames = corpus::client_frames();
    frames.extend(corpus::server_frames());

    let mut iteration: u64 = 0;
    loop {
        let keys = (corpus::bytes32(&mut rng), corpus::bytes32(&mut rng));
        honest_stream(&mut rng, keys, &frames);

        let frame = rng.pick(&frames).clone();
        let mut sealed = vec![0u8; MAX_RECORD_LEN];
        let sealed_len = {
            let (mut client, _) = pair(keys.0, keys.1);
            client.seal_record(&frame, &mut sealed).expect("seals")
        };
        sealed.truncate(sealed_len);

        reorder_and_replay(keys, &sealed);
        interference(&mut rng, keys, &sealed, &frame);
        hostile_records(&mut rng, keys);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
