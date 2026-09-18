use super::{Session, SessionError, SessionKeys};
use crate::bounds::{MAX_PLAINTEXT_LEN, MAX_RECORD_LEN, RECORD_HEADER_LEN};
use crate::error::DisconnectReason;

const C2S: [u8; 32] = [0x11; 32];
const S2C: [u8; 32] = [0x22; 32];

/// The two ends of one handshake's key pair.
fn pair() -> (Session, Session) {
    (
        Session::new(SessionKeys::new(C2S, S2C)),
        Session::new(SessionKeys::new(C2S, S2C).swapped()),
    )
}

fn seal(session: &mut Session, plaintext: &[u8], out: &mut [u8]) -> usize {
    session.seal_record(plaintext, out).expect("seals")
}

#[test]
fn a_sealed_record_opens_at_the_other_end() {
    let (mut client, mut realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"an intent", &mut record);
    let open = realm.open_record(&mut record[..n]).expect("opens");
    assert_eq!(open.plaintext(), b"an intent");
}

#[test]
fn an_opened_record_is_wiped_when_it_drops() {
    let (mut client, mut realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let secret = b"correct horse battery staple";
    let n = seal(&mut client, secret, &mut record);
    {
        let open = realm.open_record(&mut record[..n]).expect("opens");
        assert_eq!(open.plaintext(), secret);
    }
    let body = &record[RECORD_HEADER_LEN..RECORD_HEADER_LEN + secret.len()];
    assert!(
        body.iter().all(|b| *b == 0),
        "a decrypted credential must not survive its record"
    );
}

#[test]
fn many_records_stream_in_order() {
    let (mut client, mut realm) = pair();
    for i in 0u8..64 {
        let mut record = [0u8; MAX_RECORD_LEN];
        let payload = [i; 7];
        let n = seal(&mut client, &payload, &mut record);
        let open = realm.open_record(&mut record[..n]).expect("opens");
        assert_eq!(open.plaintext(), &payload);
    }
}

#[test]
fn an_empty_record_is_a_legal_record() {
    let (mut client, mut realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"", &mut record);
    let open = realm.open_record(&mut record[..n]).expect("opens");
    assert!(open.plaintext().is_empty());
}

#[test]
fn a_reordered_record_is_refused_and_ends_the_session() {
    let (mut client, mut realm) = pair();
    let mut first = [0u8; MAX_RECORD_LEN];
    let mut second = [0u8; MAX_RECORD_LEN];
    let a = seal(&mut client, b"first", &mut first);
    let b = seal(&mut client, b"second", &mut second);

    // The second record arrives first: its nonce is not the one the receiver
    // expects, so it does not authenticate.
    assert_eq!(
        realm.open_record(&mut second[..b]).err(),
        Some(SessionError::Authentication)
    );
    assert_eq!(
        realm.ended(),
        Some(DisconnectReason::RecordAuthentication),
        "a record that did not authenticate ends the session"
    );
    // And the session stays ended: the genuine first record is refused too.
    assert_eq!(
        realm.open_record(&mut first[..a]).err(),
        Some(SessionError::Ended(DisconnectReason::RecordAuthentication))
    );
}

#[test]
fn a_replayed_record_is_refused() {
    let (mut client, mut realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"once", &mut record);
    let replay = record;
    drop(realm.open_record(&mut record[..n]).expect("opens"));

    let mut again = replay;
    assert_eq!(
        realm.open_record(&mut again[..n]).err(),
        Some(SessionError::Authentication)
    );
    assert_eq!(realm.ended(), Some(DisconnectReason::RecordAuthentication));
}

#[test]
fn a_truncated_record_is_refused() {
    let (mut client, mut realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"a payload", &mut record);
    for len in 0..n {
        let mut copy = record;
        let mut fresh = Session::new(SessionKeys::new(C2S, S2C).swapped());
        assert!(
            fresh.open_record(&mut copy[..len]).is_err(),
            "a record short by {} bytes must be refused",
            n - len
        );
        assert!(fresh.ended().is_some());
    }
    // The untouched record still opens, so the refusals above were about the
    // truncation and not about the fixture.
    assert!(realm.open_record(&mut record[..n]).is_ok());
}

#[test]
fn a_record_extended_past_its_header_is_refused() {
    let (mut client, mut realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"a payload", &mut record);
    assert_eq!(
        realm.open_record(&mut record[..=n]).err(),
        Some(SessionError::Incomplete)
    );
    assert_eq!(realm.ended(), Some(DisconnectReason::RecordAuthentication));
}

#[test]
fn an_oversize_record_is_refused_on_its_header_alone() {
    let (_client, mut realm) = pair();
    let mut header = [0u8; RECORD_HEADER_LEN];
    let Ok(declared) = u32::try_from(MAX_PLAINTEXT_LEN + 1) else {
        unreachable!("the bound is far below u32::MAX")
    };
    header.copy_from_slice(&declared.to_le_bytes());
    assert_eq!(
        Session::peek_record_len(&header),
        Err(SessionError::RecordTooLarge)
    );

    // Presented as a whole record it is refused before any body byte is
    // touched, and it ends the session.
    let mut record = [0u8; MAX_RECORD_LEN];
    record[..RECORD_HEADER_LEN].copy_from_slice(&declared.to_le_bytes());
    assert_eq!(
        realm.open_record(&mut record).err(),
        Some(SessionError::RecordTooLarge)
    );
    assert_eq!(realm.ended(), Some(DisconnectReason::RecordTooLarge));
}

#[test]
fn a_record_longer_than_the_bound_is_refused_before_its_header_is_read() {
    let (_client, mut realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN + 1];
    assert_eq!(
        realm.open_record(&mut record).err(),
        Some(SessionError::RecordTooLarge)
    );
}

#[test]
fn every_single_bit_flip_in_a_record_is_refused() {
    let (mut client, _realm) = pair();
    let mut original = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"tamper with me", &mut original);
    for byte in 0..n {
        for bit in 0..8u32 {
            let mut copy = original;
            copy[byte] ^= 1u8 << bit;
            let mut realm = Session::new(SessionKeys::new(C2S, S2C).swapped());
            assert!(
                realm.open_record(&mut copy[..n]).is_err(),
                "a flip at byte {byte} bit {bit} must be refused"
            );
            assert!(realm.ended().is_some());
        }
    }
}

#[test]
fn a_record_reflected_back_at_its_sender_is_refused() {
    let (mut client, _realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"mine", &mut record);
    // The two directions carry different keys, so a client cannot be made to
    // accept its own traffic as the realm's.
    let mut echo = Session::new(SessionKeys::new(C2S, S2C));
    assert_eq!(
        echo.open_record(&mut record[..n]).err(),
        Some(SessionError::Authentication)
    );
}

#[test]
fn a_record_from_another_session_is_refused() {
    let (mut client, _realm) = pair();
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, b"session one", &mut record);
    // A different handshake yields different keys, so a captured record is
    // worthless against the next session.
    let mut other = Session::new(SessionKeys::new([0x33; 32], [0x44; 32]).swapped());
    assert_eq!(
        other.open_record(&mut record[..n]).err(),
        Some(SessionError::Authentication)
    );
}

#[test]
fn sealing_an_oversize_plaintext_or_into_a_short_buffer_is_refused() {
    let (mut client, _realm) = pair();
    let mut out = [0u8; MAX_RECORD_LEN];
    let too_long = [0u8; MAX_PLAINTEXT_LEN + 1];
    assert_eq!(
        client.seal_record(&too_long, &mut out),
        Err(SessionError::RecordTooLarge)
    );
    assert_eq!(
        client.seal_record(b"fits", &mut out[..4]),
        Err(SessionError::BufferTooSmall)
    );
    // Neither refusal consumed a sequence number or ended the session.
    assert_eq!(client.ended(), None);
    let n = seal(&mut client, b"fits", &mut out);
    let mut realm = Session::new(SessionKeys::new(C2S, S2C).swapped());
    assert!(realm.open_record(&mut out[..n]).is_ok());
}

#[test]
fn a_session_ended_deliberately_refuses_both_directions_with_that_reason() {
    let (mut client, _realm) = pair();
    client.end(DisconnectReason::Kicked);
    let mut out = [0u8; MAX_RECORD_LEN];
    assert_eq!(
        client.seal_record(b"too late", &mut out),
        Err(SessionError::Ended(DisconnectReason::Kicked))
    );
    assert_eq!(
        client.open_record(&mut out).err(),
        Some(SessionError::Ended(DisconnectReason::Kicked))
    );
    // The first reason stands: a later end does not overwrite it.
    client.end(DisconnectReason::Timeout);
    assert_eq!(client.ended(), Some(DisconnectReason::Kicked));
}

#[test]
fn a_record_length_is_computed_and_read_back_consistently() {
    for len in [0usize, 1, 100, MAX_PLAINTEXT_LEN] {
        let total = Session::record_len(len).expect("within the bound");
        let mut header = [0u8; RECORD_HEADER_LEN];
        let Ok(declared) = u32::try_from(len) else {
            unreachable!("the bound is far below u32::MAX")
        };
        header.copy_from_slice(&declared.to_le_bytes());
        assert_eq!(Session::peek_record_len(&header), Ok(total));
    }
    assert_eq!(
        Session::record_len(MAX_PLAINTEXT_LEN + 1),
        Err(SessionError::RecordTooLarge)
    );
    assert_eq!(
        Session::peek_record_len(&[0, 0]),
        Err(SessionError::Incomplete)
    );
}

#[test]
fn every_session_refusal_names_the_reason_the_connection_ends_with() {
    assert_eq!(
        SessionError::RecordTooLarge.disconnect_reason(),
        DisconnectReason::RecordTooLarge
    );
    assert_eq!(
        SessionError::Incomplete.disconnect_reason(),
        DisconnectReason::RecordAuthentication
    );
    assert_eq!(
        SessionError::Authentication.disconnect_reason(),
        DisconnectReason::RecordAuthentication
    );
    assert_eq!(
        SessionError::SequenceExhausted.disconnect_reason(),
        DisconnectReason::SequenceExhausted
    );
    assert_eq!(
        SessionError::BufferTooSmall.disconnect_reason(),
        DisconnectReason::Internal
    );
    assert_eq!(
        SessionError::Ended(DisconnectReason::Banned).disconnect_reason(),
        DisconnectReason::Banned
    );
}

#[test]
fn a_record_at_the_plaintext_bound_seals_and_opens() {
    let (mut client, mut realm) = pair();
    let plaintext = [0x5Au8; MAX_PLAINTEXT_LEN];
    let mut record = [0u8; MAX_RECORD_LEN];
    let n = seal(&mut client, &plaintext, &mut record);
    assert_eq!(n, MAX_RECORD_LEN);
    let open = realm.open_record(&mut record[..n]).expect("opens");
    assert_eq!(open.plaintext().len(), MAX_PLAINTEXT_LEN);
}
