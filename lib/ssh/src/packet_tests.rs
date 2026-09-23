extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use super::{
    Opened, Opener, PacketError, Sealer, EPOCH_PACKETS, LENGTH_LEN, MAX_PACKET_LEN, MAX_PADDING,
    MIN_PADDING, REKEY_PACKETS,
};
use crate::algorithm::{Cipher, KeyError, Keys, Mac};

/// Every keyed framing: each authenticated cipher alone, each counter-mode
/// cipher with each MAC.
fn every_framing() -> Vec<(Cipher, Option<Mac>)> {
    let mut all = Vec::new();
    for cipher in Cipher::ALL {
        if cipher.is_aead() {
            all.push((cipher, None));
        } else {
            all.extend(Mac::ALL.into_iter().map(|mac| (cipher, Some(mac))));
        }
    }
    all
}

/// Deterministic key material for `cipher` and `mac`, distinct per `salt`.
fn keys(cipher: Cipher, mac: Option<Mac>, salt: u8) -> Keys {
    let fill = |len: usize, lane: u8| -> Vec<u8> {
        (0..len)
            .map(|at| u8::try_from(at % 251).expect("small") ^ lane ^ salt)
            .collect()
    };
    Keys::new(
        cipher,
        mac,
        &fill(cipher.key_len(), 0x11),
        &fill(cipher.iv_len(), 0x22),
        &fill(mac.map_or(0, Mac::key_len), 0x33),
    )
    .expect("lengths from the algorithm")
}

fn keyed_pair(cipher: Cipher, mac: Option<Mac>) -> (Sealer, Opener) {
    let mut sealer = Sealer::new();
    let mut opener = Opener::new();
    sealer.rekey(&keys(cipher, mac, 7), true, None);
    opener.rekey(&keys(cipher, mac, 7), true, None);
    (sealer, opener)
}

fn seal(sealer: &mut Sealer, payload: &[u8], padding: u8) -> Vec<u8> {
    let framing = sealer.framing(payload.len()).expect("fits a packet");
    let mut frame = vec![0u8; framing.total];
    frame[framing.payload_range()].copy_from_slice(payload);
    frame[framing.padding_range()].fill(padding);
    sealer.seal(&mut frame, &framing).expect("seals");
    frame
}

/// Open every whole packet in `wire`, in order.
fn open_all(opener: &mut Opener, wire: &mut Vec<u8>) -> Result<Vec<(u32, Vec<u8>)>, PacketError> {
    let mut packets = Vec::new();
    loop {
        match opener.open(wire)? {
            Opened::Need(_) => return Ok(packets),
            Opened::Packet {
                framed,
                payload,
                sequence,
            } => {
                packets.push((sequence, wire[payload].to_vec()));
                wire.drain(..framed);
            }
        }
    }
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|at| u8::try_from(at % 253).expect("small").wrapping_mul(31) ^ seed)
        .collect()
}

#[test]
fn keys_refuse_a_wrong_pairing_or_length() {
    let key = [1u8; 64];
    let iv = [2u8; 16];
    let mac_key = [3u8; 64];
    assert_eq!(
        Keys::new(
            Cipher::Aes128Gcm,
            Some(Mac::HmacSha256),
            &key[..16],
            &iv[..12],
            &mac_key[..32]
        )
        .err(),
        Some(KeyError::Pairing)
    );
    assert_eq!(
        Keys::new(Cipher::Aes128Ctr, None, &key[..16], &iv, &[]).err(),
        Some(KeyError::Pairing)
    );
    assert_eq!(
        Keys::new(
            Cipher::Aes128Ctr,
            Some(Mac::HmacSha256),
            &key[..17],
            &iv,
            &mac_key[..32]
        )
        .err(),
        Some(KeyError::Length)
    );
    assert_eq!(
        Keys::new(Cipher::ChaCha20Poly1305, None, &key, &iv[..1], &[]).err(),
        Some(KeyError::Length)
    );
    assert_eq!(
        Keys::new(
            Cipher::Aes256Ctr,
            Some(Mac::HmacSha512),
            &key[..32],
            &iv,
            &mac_key[..32]
        )
        .err(),
        Some(KeyError::Length)
    );
    let keys = Keys::new(
        Cipher::Aes192Ctr,
        Some(Mac::HmacSha512Etm),
        &key[..24],
        &iv,
        &mac_key,
    )
    .expect("valid");
    assert_eq!(
        (keys.cipher(), keys.mac()),
        (Cipher::Aes192Ctr, Some(Mac::HmacSha512Etm))
    );
}

#[test]
fn the_names_are_the_negotiated_ones_and_round_trip() {
    for cipher in Cipher::ALL {
        assert_eq!(Cipher::from_name(cipher.name().as_bytes()), Some(cipher));
    }
    for mac in Mac::ALL {
        assert_eq!(Mac::from_name(mac.name().as_bytes()), Some(mac));
    }
    for refused in [
        "aes128-cbc",
        "3des-cbc",
        "arcfour",
        "none",
        "hmac-sha1",
        "hmac-md5",
        "",
    ] {
        assert_eq!(Cipher::from_name(refused.as_bytes()), None);
        assert_eq!(Mac::from_name(refused.as_bytes()), None);
    }
}

#[test]
fn padding_is_the_least_the_framing_allows_and_aligns_what_it_must() {
    let mut framings: Vec<Sealer> = vec![Sealer::new()];
    for (cipher, mac) in every_framing() {
        framings.push(keyed_pair(cipher, mac).0);
    }
    for sealer in &framings {
        for len in 1..=600 {
            let framing = sealer.framing(len).expect("fits");
            let keyed = sealer.keys.as_ref();
            let block = keyed.map_or(8, |k| k.cipher.block_len());
            let outside = keyed.is_some_and(super::Keyed::length_outside_blocks);
            let tag = keyed.map_or(0, super::Keyed::tag_len);
            let packet_len = 1 + len + framing.padding;
            let aligned = if outside {
                packet_len
            } else {
                LENGTH_LEN + packet_len
            };
            assert_eq!(aligned % block, 0, "len {len}");
            assert!((MIN_PADDING..MIN_PADDING + block).contains(&framing.padding));
            assert!(framing.padding <= MAX_PADDING);
            assert_eq!(framing.total, LENGTH_LEN + packet_len + tag);
        }
    }
}

#[test]
fn openssh_pads_its_newkeys_to_ten_bytes_and_so_does_this() {
    // `0000000c 0a 15 00*10`, the exact NEWKEYS packet OpenSSH 10.2p1 sends.
    let mut sealer = Sealer::new();
    let frame = seal(&mut sealer, &[21], 0);
    assert_eq!(frame, [0, 0, 0, 12, 10, 21, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn a_payload_past_the_packet_bound_is_refused() {
    let sealer = Sealer::new();
    assert!(sealer.framing(MAX_PACKET_LEN - 1 - MIN_PADDING - 8).is_ok());
    assert_eq!(sealer.framing(MAX_PACKET_LEN), Err(PacketError::TooLarge));
    assert_eq!(sealer.framing(usize::MAX), Err(PacketError::TooLarge));
}

#[test]
fn every_framing_round_trips_and_numbers_its_packets() {
    let mut framings: Vec<(Sealer, Opener)> = vec![(Sealer::new(), Opener::new())];
    for (cipher, mac) in every_framing() {
        framings.push(keyed_pair(cipher, mac));
    }
    for (sealer, opener) in &mut framings {
        let mut wire = Vec::new();
        let mut sent = Vec::new();
        for (index, len) in [1usize, 2, 15, 16, 17, 100, 1000, 32_768]
            .into_iter()
            .enumerate()
        {
            let body = payload(len, u8::try_from(index).expect("small"));
            wire.extend_from_slice(&seal(sealer, &body, 0xa5));
            sent.push((u32::try_from(index).expect("small"), body));
        }
        assert_eq!(open_all(opener, &mut wire).expect("genuine"), sent);
        assert!(wire.is_empty());
        assert_eq!(sealer.sequence(), opener.sequence());
    }
}

#[test]
fn a_packet_is_opened_the_same_however_its_bytes_arrive() {
    for (cipher, mac) in every_framing() {
        let (mut sealer, mut opener) = keyed_pair(cipher, mac);
        let body = payload(300, 9);
        let wire = seal(&mut sealer, &body, 0x3c);
        let mut buffer = Vec::new();
        let mut decoded = None;
        for &byte in &wire {
            buffer.push(byte);
            match opener.open(&mut buffer).expect("genuine") {
                Opened::Need(need) => assert!(need > buffer.len()),
                Opened::Packet {
                    framed, payload, ..
                } => {
                    assert_eq!(framed, wire.len());
                    decoded = Some(buffer[payload].to_vec());
                }
            }
        }
        assert_eq!(decoded, Some(body), "{cipher:?} {mac:?}");
    }
}

#[test]
fn a_change_to_any_byte_of_an_authenticated_packet_is_refused() {
    for (cipher, mac) in every_framing() {
        let (mut sealer, _) = keyed_pair(cipher, mac);
        let wire = seal(&mut sealer, &payload(40, 1), 0x77);
        for at in 0..wire.len() {
            for flip in [0x01u8, 0x80] {
                let mut opener = keyed_pair(cipher, mac).1;
                let mut damaged = wire.clone();
                damaged[at] ^= flip;
                let outcome = opener.open(&mut damaged);
                assert!(
                    matches!(outcome, Err(_) | Ok(Opened::Need(_))),
                    "{cipher:?} {mac:?}: a flip at {at} was accepted"
                );
            }
        }
    }
}

#[test]
fn a_wrong_key_is_refused() {
    for (cipher, mac) in every_framing() {
        let (mut sealer, _) = keyed_pair(cipher, mac);
        let mut wire = seal(&mut sealer, &payload(40, 2), 0);
        let mut stranger = Opener::new();
        stranger.rekey(&keys(cipher, mac, 8), true, None);
        assert!(
            !matches!(stranger.open(&mut wire), Ok(Opened::Packet { .. })),
            "{cipher:?} {mac:?}"
        );
    }
}

#[test]
fn a_deleted_packet_leaves_the_next_one_unopenable() {
    // The prefix-truncation shape: the peer drops the first packet and hands
    // on the second as though it were first.
    for (cipher, mac) in every_framing() {
        let (mut sealer, mut opener) = keyed_pair(cipher, mac);
        let _first = seal(&mut sealer, &payload(40, 3), 0);
        let mut second = seal(&mut sealer, &payload(40, 4), 0);
        assert!(
            !matches!(opener.open(&mut second), Ok(Opened::Packet { .. })),
            "{cipher:?} {mac:?}"
        );
    }
}

/// A plaintext packet with the given fields, for the checks that need a
/// packet no sealer would frame.
fn plain(packet_len: u32, padding: u8, fill: usize) -> Vec<u8> {
    let mut wire = packet_len.to_be_bytes().to_vec();
    wire.push(padding);
    wire.resize(LENGTH_LEN + fill, 0x42);
    wire
}

#[test]
fn a_length_out_of_bounds_or_off_the_block_is_refused_before_buffering() {
    for (packet_len, err) in [
        (0, true),
        (4, true),
        (11, true),
        (12, false),
        (13, true),
        (u32::try_from(MAX_PACKET_LEN).expect("fits") + 4, true),
        (u32::MAX, true),
    ] {
        let mut wire = plain(packet_len, 4, 1);
        let outcome = Opener::new().open(&mut wire);
        if err {
            assert_eq!(outcome, Err(PacketError::Length), "{packet_len}");
        } else {
            assert!(matches!(outcome, Ok(Opened::Need(16))), "{packet_len}");
        }
    }
    // A clear length is judged the moment its four bytes are here.
    let mut wire = u32::MAX.to_be_bytes().to_vec();
    assert_eq!(Opener::new().open(&mut wire), Err(PacketError::Length));
}

#[test]
fn padding_too_short_or_leaving_no_payload_is_refused() {
    for (padding, err) in [
        (3, true),
        (4, false),
        (10, false),
        (11, true),
        (12, true),
        (255, true),
    ] {
        let mut wire = plain(12, padding, 16);
        let outcome = Opener::new().open(&mut wire);
        if err {
            assert_eq!(outcome, Err(PacketError::Padding), "{padding}");
        } else {
            assert!(matches!(outcome, Ok(Opened::Packet { .. })), "{padding}");
        }
    }
}

#[test]
fn a_frame_not_of_its_framings_length_is_refused_without_a_sequence_number() {
    let mut sealer = Sealer::new();
    let framing = sealer.framing(10).expect("fits");
    let mut short = vec![0u8; framing.total - 1];
    assert_eq!(sealer.seal(&mut short, &framing), Err(PacketError::Frame));
    assert_eq!(sealer.sequence(), 0);
    // A framing computed before a rekey no longer describes the frame.
    sealer.rekey(&keys(Cipher::Aes128Gcm, None, 1), true, None);
    let mut frame = vec![0u8; framing.total];
    assert_eq!(sealer.seal(&mut frame, &framing), Err(PacketError::Frame));
}

#[test]
fn a_key_is_never_asked_to_reuse_a_sequence_number() {
    for (cipher, mac) in every_framing() {
        let (mut sealer, mut opener) = keyed_pair(cipher, mac);
        sealer.epoch.packets = EPOCH_PACKETS - 1;
        opener.epoch.packets = EPOCH_PACKETS - 1;
        let mut wire = seal(&mut sealer, &[94], 0);
        assert!(matches!(opener.open(&mut wire), Ok(Opened::Packet { .. })));
        let framing = sealer.framing(1).expect("fits");
        let mut frame = vec![0u8; framing.total];
        assert_eq!(
            sealer.seal(&mut frame, &framing),
            Err(PacketError::SequenceExhausted)
        );
        assert_eq!(
            opener.open(&mut [0u8; 64]),
            Err(PacketError::SequenceExhausted)
        );
        // New keys start a new sequence space.
        sealer.rekey(&keys(cipher, mac, 9), false, None);
        assert!(sealer.seal(&mut frame, &framing).is_ok());
    }
}

#[test]
fn the_sequence_number_wraps_under_changing_keys_and_resets_when_strict() {
    let mut sealer = Sealer::new();
    sealer.epoch.sequence = u32::MAX;
    let _ = seal(&mut sealer, &[2], 0);
    assert_eq!(sealer.sequence(), 0, "RFC 4253 6.4: the number wraps");
    let _ = seal(&mut sealer, &[2], 0);
    sealer.rekey(&keys(Cipher::Aes256Gcm, None, 3), false, None);
    assert_eq!(sealer.sequence(), 1, "a lax rekey keeps counting");
    sealer.rekey(&keys(Cipher::Aes256Gcm, None, 4), true, None);
    assert_eq!(sealer.sequence(), 0, "a strict rekey starts again at zero");
}

#[test]
fn a_rekey_falls_due_at_the_rfc_4344_thresholds() {
    for (cipher, mac) in every_framing() {
        let (mut sealer, mut opener) = keyed_pair(cipher, mac);
        assert!(!sealer.rekey_due() && !opener.rekey_due());
        sealer.epoch.packets = REKEY_PACKETS - 1;
        let mut wire = seal(&mut sealer, &[94], 0);
        assert!(sealer.rekey_due(), "{cipher:?}: packet count");
        opener.epoch.bytes = cipher.rekey_bytes() - 1;
        let _ = opener.open(&mut wire).expect("genuine");
        assert!(opener.rekey_due(), "{cipher:?}: byte count");
    }
    // The unencrypted framing has nothing to rekey.
    let mut clear = Sealer::new();
    clear.epoch.packets = REKEY_PACKETS;
    assert!(!clear.rekey_due());
}

#[test]
fn a_configured_limit_tightens_but_never_loosens_the_cipher_bound() {
    let mut sealer = Sealer::new();
    sealer.rekey(
        &keys(Cipher::Aes128Ctr, Some(Mac::HmacSha256), 1),
        true,
        Some(1000),
    );
    let mut sent = 0;
    while !sealer.rekey_due() {
        sent += seal(&mut sealer, &payload(100, 1), 0).len();
    }
    assert!(
        (1000..1300).contains(&sent),
        "{sent} bytes before the limit"
    );
    let mut loose = Sealer::new();
    loose.rekey(
        &keys(Cipher::ChaCha20Poly1305, None, 1),
        true,
        Some(u64::MAX),
    );
    assert_eq!(loose.keys.as_ref().map(|k| k.byte_limit), Some(1 << 30));
}

#[test]
fn the_rekey_byte_bounds_are_the_ones_openssh_applies() {
    assert_eq!(Cipher::ChaCha20Poly1305.rekey_bytes(), 1 << 30);
    for cipher in [
        Cipher::Aes128Gcm,
        Cipher::Aes256Gcm,
        Cipher::Aes128Ctr,
        Cipher::Aes256Ctr,
    ] {
        assert_eq!(cipher.rekey_bytes(), (1 << 32) * 16, "2^32 blocks of 16");
    }
}

#[test]
fn chacha20_poly1305_takes_the_payload_key_first() {
    // K_2 encrypts the payload and keys Poly1305; K_1 encrypts only the
    // length. Swapping them must change what the length field becomes.
    let mut material = [0u8; 64];
    for (at, byte) in material.iter_mut().enumerate() {
        *byte = u8::try_from(at).expect("small");
    }
    let forward = Keys::new(Cipher::ChaCha20Poly1305, None, &material, &[], &[]).expect("valid");
    let mut swapped_material = [0u8; 64];
    swapped_material[..32].copy_from_slice(&material[32..]);
    swapped_material[32..].copy_from_slice(&material[..32]);
    let swapped =
        Keys::new(Cipher::ChaCha20Poly1305, None, &swapped_material, &[], &[]).expect("valid");
    let mut one = Sealer::new();
    one.rekey(&forward, true, None);
    let mut two = Sealer::new();
    two.rekey(&swapped, true, None);
    let a = seal(&mut one, &[94, 1, 2, 3], 0);
    let b = seal(&mut two, &[94, 1, 2, 3], 0);
    assert_ne!(a[..4], b[..4]);
    let mut expected_length = [0u8, 0, 0, 16];
    tairix_crypto::chacha20_apply(
        &super::window::<32, 32, 64>(&material),
        &0u64.to_be_bytes(),
        0,
        &mut expected_length,
    )
    .expect("one block");
    assert_eq!(a[..4], expected_length, "K_1 is the second half");
}
