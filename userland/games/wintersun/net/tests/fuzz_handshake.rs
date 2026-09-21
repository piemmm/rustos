//! Deterministic fuzz harness and property model for the session handshake.
//!
//! The handshake runs before any key exists, so both its messages are read
//! straight off the wire from an unauthenticated peer — the earliest
//! attacker-reachable surface the realm has. The properties driven here are
//! the ones the design claims:
//!
//! * neither side ever panics, whatever bytes arrive;
//! * a realm **always** answers, and a refusal names the reason the
//!   connection ends with;
//! * a client accepts an answer only when the realm's signature covers the
//!   transcript of the exact bytes *it* sent — so a tampered `Hello`, a
//!   tampered answer, a signature lifted from another exchange, or a
//!   substituted realm key are all refused;
//! * a completed handshake yields two distinct directional keys, equal and
//!   opposite at the two ends.
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a
//! fresh, logged seed; `cargo xtask fuzz` extends it to a wall-clock budget.

use tairix_crypto::Ed25519SecretKey;

use tairix_wintersun_net::bounds::{HANDSHAKE_MAGIC, PROTOCOL_VERSION};
use tairix_wintersun_net::handshake::{
    refuse, respond, HandshakeError, Initiator, Outcome, Pinning, HELLO_LEN, MAX_HANDSHAKE_LEN,
    REFUSED_LEN, SERVER_HELLO_LEN,
};
use tairix_wintersun_net::DisconnectReason;

mod corpus;

/// Fixed-iteration sweep run once by a plain `cargo test`. Lower than the
/// codec harnesses': every iteration runs real curve and signature
/// arithmetic, which is the point and also the cost.
const SMOKE_ITERATIONS: u64 = 400;

/// The realm's identity signer. `lib/crypto` verifies but never signs, so
/// the secret stays with its holder and is handed in as a callback.
fn sign_with(key: &Ed25519SecretKey) -> impl FnOnce(&[u8]) -> [u8; 64] + '_ {
    move |payload: &[u8]| *key.sign(payload).as_bytes()
}

/// A realm answer to arbitrary bytes: it must not panic, and it must produce
/// something to send whichever way it went.
fn answer(hello: &[u8], key: &Ed25519SecretKey, ephemeral: [u8; 32], nonce: [u8; 32]) -> Vec<u8> {
    let identity = *key.public_key().as_bytes();
    let response = respond(hello, ephemeral, nonce, &identity, sign_with(key));
    let sent = response.to_send().to_vec();
    match response.outcome {
        Outcome::Accepted(established) => {
            assert_eq!(sent.len(), SERVER_HELLO_LEN);
            assert_ne!(
                established.keys.sending(),
                established.keys.receiving(),
                "the two directions must never share a key"
            );
        }
        Outcome::Refused(reason) => {
            assert_eq!(sent.len(), REFUSED_LEN);
            // A refusal always says why, and the reason is one a client can
            // decode rather than a bare closed socket.
            assert!(DisconnectReason::ALL.contains(&reason));
        }
    }
    assert!(!sent.is_empty(), "a realm always has something to send");
    sent
}

/// A client reading an arbitrary answer: it must not panic, and it must
/// either complete or refuse with a reason.
fn read_answer(initiator: Initiator, bytes: &[u8], pinned: Option<&[u8; 32]>) {
    match initiator.finish(bytes, pinned) {
        Ok(established) => {
            assert_ne!(established.keys.sending(), established.keys.receiving());
            assert!(matches!(
                established.pinning,
                Pinning::Matched | Pinning::FirstUse
            ));
            if let Some(known) = pinned {
                assert_eq!(&established.realm_identity, known);
            }
        }
        Err(err) => {
            assert!(DisconnectReason::ALL.contains(&err.disconnect_reason()));
        }
    }
}

/// The parties of one exchange, drawn fresh so no two iterations share a key.
struct Parties {
    realm: Ed25519SecretKey,
    identity: [u8; 32],
    client_ephemeral: [u8; 32],
    realm_ephemeral: [u8; 32],
    client_nonce: [u8; 32],
    realm_nonce: [u8; 32],
}

impl Parties {
    fn drawn(rng: &mut corpus::Lcg) -> Self {
        let realm = Ed25519SecretKey::from_seed(&rng.bytes32());
        let identity = *realm.public_key().as_bytes();
        Self {
            realm,
            identity,
            client_ephemeral: rng.bytes32(),
            realm_ephemeral: rng.bytes32(),
            client_nonce: rng.bytes32(),
            realm_nonce: rng.bytes32(),
        }
    }

    fn start(&self) -> (Initiator, [u8; HELLO_LEN]) {
        Initiator::start(self.client_ephemeral, self.client_nonce)
    }

    fn answer(&self, hello: &[u8]) -> Vec<u8> {
        answer(hello, &self.realm, self.realm_ephemeral, self.realm_nonce)
    }
}

/// The honest exchange completes, and both ends agree on who the realm is.
fn honest_exchange(parties: &Parties) {
    let (initiator, hello) = parties.start();
    let sent = parties.answer(&hello);
    let established = initiator
        .finish(&sent, None)
        .expect("an honest exchange completes");
    assert_eq!(established.pinning, Pinning::FirstUse);
    assert_eq!(established.realm_identity, parties.identity);
}

/// Every way the transcript binding can be broken, and the pin that catches
/// the one a valid signature does not.
fn tampered_exchanges(rng: &mut corpus::Lcg, parties: &Parties) {
    // The client's hello is altered in flight. The realm signs what it saw;
    // the client checks what it sent.
    let (initiator, hello) = parties.start();
    let mut tampered = hello;
    let pos = rng.bounded(HELLO_LEN - 1);
    tampered[pos] ^= 1u8 << rng.bit();
    let sent = parties.answer(&tampered);
    assert!(
        initiator.finish(&sent, Some(&parties.identity)).is_err(),
        "an answer bound to a transcript the client did not send must be refused"
    );

    // The realm's answer is altered on the way back.
    let (initiator, hello) = parties.start();
    let mut sent = parties.answer(&hello);
    if !sent.is_empty() {
        let pos = rng.bounded(sent.len() - 1);
        sent[pos] ^= 1u8 << rng.bit();
    }
    assert!(
        initiator.finish(&sent, Some(&parties.identity)).is_err(),
        "a tampered answer must be refused"
    );

    // A substituted realm: a valid signature under a key the client did not
    // pin. Only the pin refuses this one.
    let impostor = Ed25519SecretKey::from_seed(&rng.bytes32());
    if *impostor.public_key().as_bytes() != parties.identity {
        let (initiator, hello) = parties.start();
        let sent = answer(
            &hello,
            &impostor,
            parties.realm_ephemeral,
            parties.realm_nonce,
        );
        assert!(
            matches!(
                initiator.finish(&sent, Some(&parties.identity)),
                Err(HandshakeError::IdentityChanged { .. })
            ),
            "a changed realm identity must be surfaced, never accepted"
        );
    }

    // An answer lifted from a different exchange: the transcript binds it to
    // the hello it answered, and to no other.
    let (first, _) = parties.start();
    let (_, other_hello) = Initiator::start(rng.bytes32(), rng.bytes32());
    let other = parties.answer(&other_hello);
    assert!(
        first.finish(&other, Some(&parties.identity)).is_err(),
        "an answer from another exchange must not complete this one"
    );
}

/// Arbitrary and plausible-but-forged bytes at each end.
fn hostile_bytes(rng: &mut corpus::Lcg, parties: &Parties) {
    let len = rng.bounded(MAX_HANDSHAKE_LEN + 8);
    let noise = rng.blob(len);
    parties.answer(&noise);
    let (initiator, _) = parties.start();
    read_answer(initiator, &noise, Some(&parties.identity));

    // A well-formed header over a noise body, which is what exercises the
    // magic, version and kind arms rather than stopping at the first field.
    let mut forged = Vec::with_capacity(MAX_HANDSHAKE_LEN);
    forged.extend_from_slice(&HANDSHAKE_MAGIC.to_le_bytes());
    let version = if rng.byte().is_multiple_of(4) {
        PROTOCOL_VERSION
    } else {
        u16::from(rng.byte())
    };
    forged.extend_from_slice(&version.to_le_bytes());
    forged.extend_from_slice(&u16::from(rng.byte() % 5).to_le_bytes());
    let body = rng.bounded(MAX_HANDSHAKE_LEN - 8);
    forged.extend(rng.blob(body));
    let (initiator, _) = parties.start();
    read_answer(initiator, &forged, None);
    parties.answer(&forged);
}

#[test]
fn the_handshake_never_panics_and_only_completes_on_a_bound_transcript() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng =
        corpus::Lcg::seeded("the_handshake_never_panics_and_only_completes_on_a_bound_transcript");

    let mut iteration: u64 = 0;
    loop {
        let parties = Parties::drawn(&mut rng);
        honest_exchange(&parties);
        tampered_exchanges(&mut rng, &parties);
        hostile_bytes(&mut rng, &parties);

        // Every refusal the realm can state decodes back to that reason.
        let reason = DisconnectReason::ALL[rng.bounded(DisconnectReason::ALL.len() - 1)];
        let refusal = refuse(reason);
        assert_eq!(refusal.to_send().len(), REFUSED_LEN);
        let (initiator, _) = parties.start();
        assert_eq!(
            initiator.finish(refusal.to_send(), None).err(),
            Some(HandshakeError::Refused(reason))
        );

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
