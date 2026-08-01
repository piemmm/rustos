//! Host tests for the array-composition protocol.

use super::{
    MemberOffer, MembershipEnd, RAID_MAX_REQUEST, RAID_OFFER_MAGIC, RAID_REGISTRY_ENDPOINT,
    RAID_VERSION_V1,
};
use crate::ipc::is_reserved_endpoint;
use crate::reply::{encode_status_reply, STATUS_REPLY_LEN};
use crate::Errno;

const OFFER: MemberOffer = MemberOffer {
    endpoint: 0x0102_0304_0506_0708,
    window: 0x1112_1314_1516_1718,
    node: 0x2122_2324,
};

fn encoded() -> [u8; MemberOffer::WIRE_LEN] {
    let mut buf = [0u8; MemberOffer::WIRE_LEN];
    assert_eq!(OFFER.encode(&mut buf), Ok(MemberOffer::WIRE_LEN));
    buf
}

#[test]
fn the_registry_rendezvous_is_reserved_so_it_cannot_be_squatted() {
    assert!(
        is_reserved_endpoint(RAID_REGISTRY_ENDPOINT),
        "an unprivileged binder of this id would be delegated every array member on the machine"
    );
}

#[test]
fn an_offer_round_trips() {
    assert_eq!(MemberOffer::decode(&encoded()), Ok(OFFER));
}

#[test]
fn the_receive_buffer_is_exactly_one_offer() {
    assert_eq!(RAID_MAX_REQUEST, MemberOffer::WIRE_LEN);
}

#[test]
fn a_short_frame_is_refused() {
    let buf = encoded();
    for len in 0..MemberOffer::WIRE_LEN {
        assert_eq!(
            MemberOffer::decode(&buf[..len]),
            Err(Errno::LengthOutOfRange),
            "a truncated offer is never guessed at"
        );
    }
}

#[test]
fn a_foreign_magic_or_version_is_refused() {
    let mut buf = encoded();
    buf[0] ^= 0xff;
    assert_eq!(MemberOffer::decode(&buf), Err(Errno::BadMagic));

    let mut buf = encoded();
    buf[4..6].copy_from_slice(&(RAID_VERSION_V1 + 1).to_le_bytes());
    assert_eq!(MemberOffer::decode(&buf), Err(Errno::BadMagic));
}

#[test]
fn an_offer_naming_no_resource_is_refused() {
    for zeroed in [
        MemberOffer {
            endpoint: 0,
            ..OFFER
        },
        MemberOffer { window: 0, ..OFFER },
    ] {
        let mut buf = [0u8; MemberOffer::WIRE_LEN];
        assert_eq!(zeroed.encode(&mut buf), Ok(MemberOffer::WIRE_LEN));
        assert_eq!(
            MemberOffer::decode(&buf),
            Err(Errno::NotFound),
            "an id of zero names nothing, so the offer can only be malformed or a probe"
        );
    }
}

#[test]
fn a_node_id_of_zero_is_carried_rather_than_refused() {
    // Unlike the two resource ids, the node id conveys nothing the composer
    // acts on directly, so an unknown one costs a poorer audit record, not a
    // wrong access.
    let offer = MemberOffer { node: 0, ..OFFER };
    let mut buf = [0u8; MemberOffer::WIRE_LEN];
    assert_eq!(offer.encode(&mut buf), Ok(MemberOffer::WIRE_LEN));
    assert_eq!(MemberOffer::decode(&buf), Ok(offer));
}

#[test]
fn encoding_into_a_short_buffer_is_refused() {
    let mut buf = [0u8; MemberOffer::WIRE_LEN - 1];
    assert_eq!(OFFER.encode(&mut buf), Err(Errno::BufferTooSmall));
}

#[test]
fn the_frame_carries_its_magic_and_version_first() {
    let buf = encoded();
    assert_eq!(
        u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        RAID_OFFER_MAGIC
    );
    assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), RAID_VERSION_V1);
}

#[test]
fn a_clean_release_tells_the_agent_to_offer_again() {
    let reply = encode_status_reply(Ok(()));
    let end = MembershipEnd::from_reply(Some(&reply));
    assert_eq!(end, MembershipEnd::Released);
    assert!(end.should_reoffer());
}

#[test]
fn a_refusal_stops_the_agent_re_offering_an_unchanged_device() {
    let reply = encode_status_reply(Err(Errno::NotFound));
    let end = MembershipEnd::from_reply(Some(&reply));
    assert_eq!(end, MembershipEnd::Refused(Errno::NotFound));
    assert!(!end.should_reoffer());
}

#[test]
fn a_cancelled_call_reads_as_the_composer_going_away() {
    let end = MembershipEnd::from_reply(None);
    assert_eq!(end, MembershipEnd::ComposerGone);
    assert!(
        end.should_reoffer(),
        "a composer that restarts must be able to reassemble the array"
    );
}

#[test]
fn an_undecodable_reply_is_read_as_a_refusal_not_a_release() {
    // A short frame and a status word that is no defined errno both fail
    // closed: a composer whose frames cannot be decoded is not one to keep
    // handing a disk to.
    for reply in [
        &[][..],
        &[0u8; STATUS_REPLY_LEN - 1][..],
        &i32::MIN.to_le_bytes()[..],
    ] {
        let end = MembershipEnd::from_reply(Some(reply));
        assert!(
            matches!(end, MembershipEnd::Refused(_)),
            "a malformed reply must never read as a clean release"
        );
        assert!(!end.should_reoffer());
    }
}
