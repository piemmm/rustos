use ed25519_dalek::{Signer, SigningKey};

use super::{
    client_auth_payload, realm_auth_payload, refuse, respond, transcript_of, Established,
    HandshakeError, Initiator, Outcome, Pinning, HELLO_LEN, MAX_HANDSHAKE_LEN, REFUSED_LEN,
    SERVER_HELLO_LEN,
};
use crate::bounds::{HANDSHAKE_MAGIC, MAX_RECORD_LEN, PROTOCOL_VERSION};
use crate::error::{DisconnectReason, WireError};
use crate::session::{Session, SessionError};

const CLIENT_EPHEMERAL: [u8; 32] = [0x41; 32];
const REALM_EPHEMERAL: [u8; 32] = [0x42; 32];
const CLIENT_NONCE: [u8; 32] = [0x43; 32];
const REALM_NONCE: [u8; 32] = [0x44; 32];

fn realm_key() -> SigningKey {
    SigningKey::from_bytes(&[0x51; 32])
}

/// The realm's identity signer. `lib/crypto` exposes verification only, so
/// the secret lives with its holder and is handed in as a callback — here the
/// test stands in for that holder.
fn signer(key: &SigningKey) -> impl FnOnce(&[u8]) -> [u8; 64] + '_ {
    move |payload: &[u8]| key.sign(payload).to_bytes()
}

/// What a realm answered: the outcome, and a copy of the bytes it would send.
struct Answered {
    outcome: Outcome,
    bytes: [u8; SERVER_HELLO_LEN],
    len: usize,
}

impl Answered {
    fn sent(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn established(self) -> Established {
        match self.outcome {
            Outcome::Accepted(established) => established,
            Outcome::Refused(reason) => unreachable!("expected acceptance, got {reason}"),
        }
    }

    fn reason(&self) -> DisconnectReason {
        match self.outcome {
            Outcome::Accepted(_) => unreachable!("expected a refusal"),
            Outcome::Refused(reason) => reason,
        }
    }
}

fn answer(
    hello: &[u8],
    ephemeral: [u8; 32],
    nonce: [u8; 32],
    key: &SigningKey,
    identity: &[u8; 32],
) -> Answered {
    let response = respond(hello, ephemeral, nonce, identity, signer(key));
    let mut bytes = [0u8; SERVER_HELLO_LEN];
    let len = response.to_send().len();
    bytes[..len].copy_from_slice(response.to_send());
    Answered {
        outcome: response.outcome,
        bytes,
        len,
    }
}

/// The ordinary case: one realm, one client, a completed handshake.
fn handshake_with(
    client_ephemeral: [u8; 32],
    realm_ephemeral: [u8; 32],
) -> (Established, Established) {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (initiator, hello) = Initiator::start(client_ephemeral, CLIENT_NONCE);
    let answered = answer(&hello, realm_ephemeral, REALM_NONCE, &key, &identity);
    let message = answered.bytes;
    let realm = answered.established();
    let client = initiator.finish(&message, None).expect("completes");
    (client, realm)
}

fn handshake() -> (Established, Established) {
    handshake_with(CLIENT_EPHEMERAL, REALM_EPHEMERAL)
}

#[test]
fn both_ends_derive_the_same_transcript_and_keys() {
    let (client, realm) = handshake();
    assert_eq!(client.transcript, realm.transcript);
    assert_eq!(client.realm_identity, realm.realm_identity);
    // The keys are the same pair, ordered from each end's point of view.
    assert_eq!(client.keys.sending(), realm.keys.receiving());
    assert_eq!(client.keys.receiving(), realm.keys.sending());
    assert_ne!(
        client.keys.sending(),
        client.keys.receiving(),
        "the two directions must not share a key, or a record could be reflected"
    );
}

#[test]
fn a_session_built_from_the_handshake_carries_traffic_both_ways() {
    let (client_half, realm_half) = handshake();
    let mut client = Session::new(client_half.keys);
    let mut realm = Session::new(realm_half.keys);

    let mut record = [0u8; MAX_RECORD_LEN];
    let n = client.seal_record(b"intent", &mut record).expect("seals");
    assert_eq!(
        realm
            .open_record(&mut record[..n])
            .expect("opens")
            .plaintext(),
        b"intent"
    );

    let n = realm.seal_record(b"snapshot", &mut record).expect("seals");
    assert_eq!(
        client
            .open_record(&mut record[..n])
            .expect("opens")
            .plaintext(),
        b"snapshot"
    );
}

#[test]
fn each_message_is_its_own_fixed_length() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (_, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    assert_eq!(hello.len(), HELLO_LEN);
    let answered = answer(&hello, REALM_EPHEMERAL, REALM_NONCE, &key, &identity);
    assert_eq!(answered.sent().len(), SERVER_HELLO_LEN);
    assert_eq!(
        refuse(DisconnectReason::Banned).to_send().len(),
        REFUSED_LEN
    );
}

#[test]
fn a_first_connect_pins_the_realm_and_a_later_one_matches() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (initiator, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let message = answer(&hello, REALM_EPHEMERAL, REALM_NONCE, &key, &identity).bytes;
    let first = initiator.finish(&message, None).expect("completes");
    assert_eq!(first.pinning, Pinning::FirstUse);
    assert_eq!(first.realm_identity, identity);

    let (initiator, hello) = Initiator::start([0x45; 32], [0x46; 32]);
    let message = answer(&hello, [0x47; 32], [0x48; 32], &key, &identity).bytes;
    let second = initiator
        .finish(&message, Some(&first.realm_identity))
        .expect("completes");
    assert_eq!(second.pinning, Pinning::Matched);
    assert_ne!(
        first.transcript, second.transcript,
        "fresh ephemerals must give a fresh transcript"
    );
}

#[test]
fn a_substituted_realm_is_surfaced_not_accepted() {
    let impostor = SigningKey::from_bytes(&[0x52; 32]);
    let identity = impostor.verifying_key().to_bytes();
    let pinned = realm_key().verifying_key().to_bytes();
    let (initiator, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let message = answer(&hello, REALM_EPHEMERAL, REALM_NONCE, &impostor, &identity).bytes;
    // The impostor's signature is perfectly valid — for its own key. What
    // refuses it is the pin.
    assert_eq!(
        initiator.finish(&message, Some(&pinned)).err(),
        Some(HandshakeError::IdentityChanged {
            pinned,
            presented: identity,
        })
    );
    assert_eq!(
        HandshakeError::IdentityChanged {
            pinned,
            presented: identity,
        }
        .disconnect_reason(),
        DisconnectReason::RealmIdentityChanged
    );
}

#[test]
fn a_signature_over_another_transcript_does_not_verify() {
    // The realm answers one hello but signs the transcript of another: the
    // binding is what catches it.
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (initiator, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let (_, other_hello) = Initiator::start([0x49; 32], [0x4A; 32]);
    let other = answer(&other_hello, REALM_EPHEMERAL, REALM_NONCE, &key, &identity).bytes;
    let mut ours = answer(&hello, REALM_EPHEMERAL, REALM_NONCE, &key, &identity).bytes;
    let at = SERVER_HELLO_LEN - 64;
    ours[at..].copy_from_slice(&other[at..]);
    assert_eq!(
        initiator.finish(&ours, None).err(),
        Some(HandshakeError::Authentication)
    );
}

#[test]
fn every_single_bit_flip_in_the_realm_answer_is_refused() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (_, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let message = answer(&hello, REALM_EPHEMERAL, REALM_NONCE, &key, &identity).bytes;
    for byte in 0..SERVER_HELLO_LEN {
        for bit in 0..8u32 {
            let mut copy = message;
            copy[byte] ^= 1u8 << bit;
            let (initiator, _) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
            // Either the answer no longer parses, or its signature no longer
            // covers what it now says — never silently accepted.
            assert!(
                initiator.finish(&copy, Some(&identity)).is_err(),
                "a flip at byte {byte} bit {bit} must be refused"
            );
        }
    }
}

#[test]
fn a_flipped_hello_gives_a_transcript_the_client_cannot_match() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (initiator, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    // A middle box alters the client's nonce on the way out. The realm signs
    // what it saw; the client verifies against what it sent, and they differ.
    let mut tampered = hello;
    tampered[HELLO_LEN - 1] ^= 0x01;
    let message = answer(&tampered, REALM_EPHEMERAL, REALM_NONCE, &key, &identity).bytes;
    assert_eq!(
        initiator.finish(&message, None).err(),
        Some(HandshakeError::Authentication)
    );
}

#[test]
fn a_malformed_hello_is_refused_with_a_message_that_says_why() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (_, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);

    let cases: [&[u8]; 4] = [
        &[],
        &hello[..HELLO_LEN - 1],
        &[0u8; HELLO_LEN + 1],
        &[0u8; HELLO_LEN],
    ];
    for bytes in cases {
        let answered = answer(bytes, REALM_EPHEMERAL, REALM_NONCE, &key, &identity);
        assert_eq!(answered.reason(), DisconnectReason::MalformedFrame);
        // The refusal is a decodable message, so the client learns the
        // reason rather than seeing a closed socket.
        let message = answered.bytes;
        let (initiator, _) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
        assert_eq!(
            initiator.finish(&message[..REFUSED_LEN], None).err(),
            Some(HandshakeError::Refused(DisconnectReason::MalformedFrame))
        );
    }
}

#[test]
fn a_hello_naming_another_protocol_version_is_refused_as_such() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (_, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let mut other = hello;
    other[4..6].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
    let answered = answer(&other, REALM_EPHEMERAL, REALM_NONCE, &key, &identity);
    assert_eq!(answered.reason(), DisconnectReason::ProtocolVersion);
}

#[test]
fn a_hello_with_the_wrong_magic_is_refused() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (_, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let mut other = hello;
    other[..4].copy_from_slice(&(HANDSHAKE_MAGIC ^ 1).to_le_bytes());
    let answered = answer(&other, REALM_EPHEMERAL, REALM_NONCE, &key, &identity);
    assert_eq!(answered.reason(), DisconnectReason::MalformedFrame);
}

#[test]
fn a_realm_answer_of_an_unexpected_shape_is_refused() {
    let (initiator, _) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    // A well-formed header naming a kind only a client sends.
    let mut forged = [0u8; 8];
    forged[..4].copy_from_slice(&HANDSHAKE_MAGIC.to_le_bytes());
    forged[4..6].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    forged[6..8].copy_from_slice(&1u16.to_le_bytes());
    assert_eq!(
        initiator.finish(&forged, None).err(),
        Some(HandshakeError::Malformed(WireError::UnknownDiscriminant))
    );
}

#[test]
fn an_oversize_realm_answer_is_refused_before_it_is_parsed() {
    let (initiator, _) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let forged = [0u8; MAX_HANDSHAKE_LEN + 1];
    assert_eq!(
        initiator.finish(&forged, None).err(),
        Some(HandshakeError::Malformed(WireError::BoundExceeded))
    );
}

#[test]
fn every_truncation_of_the_realm_answer_is_refused() {
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (_, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let message = answer(&hello, REALM_EPHEMERAL, REALM_NONCE, &key, &identity).bytes;
    for len in 0..SERVER_HELLO_LEN {
        let (initiator, _) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
        assert!(
            initiator.finish(&message[..len], None).is_err(),
            "a realm answer short by {} bytes must be refused",
            SERVER_HELLO_LEN - len
        );
    }
}

#[test]
fn a_non_contributory_realm_key_is_refused() {
    // A realm offering a small-order point would force both ends to a shared
    // secret it chose. The signature over it is valid, so only the agreement
    // check catches it.
    let key = realm_key();
    let identity = key.verifying_key().to_bytes();
    let (initiator, hello) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    let mut forged = answer(&hello, REALM_EPHEMERAL, REALM_NONCE, &key, &identity).bytes;
    forged[8..40].fill(0);
    // Re-sign, so the only thing wrong is the ephemeral key itself.
    let transcript = transcript_of(&hello, &forged[..SERVER_HELLO_LEN - 64]);
    let signature = key.sign(&realm_auth_payload(&transcript)).to_bytes();
    forged[SERVER_HELLO_LEN - 64..].copy_from_slice(&signature);
    assert_eq!(
        initiator.finish(&forged, None).err(),
        Some(HandshakeError::Authentication)
    );
}

#[test]
fn a_realm_refusal_carries_every_reason_it_can_state() {
    for reason in DisconnectReason::ALL {
        let response = refuse(*reason);
        let mut message = [0u8; REFUSED_LEN];
        message.copy_from_slice(response.to_send());
        let (initiator, _) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
        assert_eq!(
            initiator.finish(&message, None).err(),
            Some(HandshakeError::Refused(*reason))
        );
    }
}

#[test]
fn a_refusal_naming_an_unassigned_reason_is_refused() {
    let response = refuse(DisconnectReason::Banned);
    let mut message = [0u8; REFUSED_LEN];
    message.copy_from_slice(response.to_send());
    message[8..10].copy_from_slice(&999u16.to_le_bytes());
    let (initiator, _) = Initiator::start(CLIENT_EPHEMERAL, CLIENT_NONCE);
    assert_eq!(
        initiator.finish(&message, None).err(),
        Some(HandshakeError::Malformed(WireError::UnknownDiscriminant))
    );
}

#[test]
fn an_account_proof_binds_to_its_own_session_and_no_other() {
    let (first, _) = handshake();
    let (second, _) = handshake_with([0x61; 32], [0x62; 32]);
    assert_ne!(first.transcript, second.transcript);
    assert_ne!(
        client_auth_payload(&first.transcript),
        client_auth_payload(&second.transcript),
        "a captured account proof must not verify on another session"
    );
}

#[test]
fn an_account_key_signing_its_transcript_verifies_and_a_stale_one_does_not() {
    use tairix_crypto::{Ed25519PublicKey, Ed25519Signature};

    let (_, first) = handshake();
    let (_, second) = handshake_with([0x63; 32], [0x64; 32]);
    let account = SigningKey::from_bytes(&[0x71; 32]);
    let public =
        Ed25519PublicKey::from_bytes(&account.verifying_key().to_bytes()).expect("a valid key");

    let payload = client_auth_payload(&first.transcript);
    let signature = Ed25519Signature::from_bytes(account.sign(&payload).to_bytes());
    assert!(public.verify(&payload, &signature).is_ok());

    // The same proof presented on a different session is checked against
    // that session's transcript, and fails.
    let replayed = client_auth_payload(&second.transcript);
    assert!(public.verify(&replayed, &signature).is_err());
}

#[test]
fn every_handshake_refusal_names_the_reason_the_connection_ends_with() {
    assert_eq!(
        HandshakeError::Malformed(WireError::Truncated).disconnect_reason(),
        DisconnectReason::MalformedFrame
    );
    assert_eq!(
        HandshakeError::ProtocolVersion.disconnect_reason(),
        DisconnectReason::ProtocolVersion
    );
    assert_eq!(
        HandshakeError::Authentication.disconnect_reason(),
        DisconnectReason::HandshakeFailed
    );
    assert_eq!(
        HandshakeError::Refused(DisconnectReason::RateLimited).disconnect_reason(),
        DisconnectReason::RateLimited
    );
}

#[test]
fn a_session_from_one_handshake_cannot_open_anothers_records() {
    let (first, _) = handshake();
    let (_, second) = handshake_with([0x65; 32], [0x66; 32]);
    let mut client = Session::new(first.keys);
    let mut stranger = Session::new(second.keys);
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = client
        .seal_record(b"for my realm", &mut record)
        .expect("seals");
    assert_eq!(
        stranger.open_record(&mut record[..n]).err(),
        Some(SessionError::Authentication)
    );
}
