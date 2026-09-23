extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use super::{
    Ours, Received, SendError, Transport, TransportError, MAX_BACKLOG, PADDING_FLOOR,
    PADDING_LOW_WATER, PADDING_RESERVE,
};
use crate::algorithm::{Cipher, Keys, Mac};
use crate::ident::{Ident, IdentError, MAX_BANNER_LINES};
use crate::msg::{self, DisconnectReason};
use crate::packet::{PacketError, Sealer};
use crate::Role;

/// An upper-layer message number the transport treats as opaque.
const CHANNEL_DATA: u8 = 94;

/// What a transport handed up, owned so a test can act between items.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Banner(Vec<u8>),
    Message(u32, Vec<u8>),
    NewKeys,
    Debug(bool, Vec<u8>),
    Unimplemented(u32),
    Disconnected(DisconnectReason, Vec<u8>),
}

fn owned(received: Received<'_>) -> Event {
    match received {
        Received::Banner(text) => Event::Banner(text.to_vec()),
        Received::Message { sequence, payload } => Event::Message(sequence, payload.to_vec()),
        Received::NewKeys => Event::NewKeys,
        Received::Debug {
            always_display,
            message,
            ..
        } => Event::Debug(always_display, message.to_vec()),
        Received::Unimplemented { sequence } => Event::Unimplemented(sequence),
        Received::Disconnected {
            reason,
            description,
            ..
        } => Event::Disconnected(reason, description.to_vec()),
    }
}

fn next(transport: &mut Transport) -> Result<Option<Event>, TransportError> {
    Ok(transport.next_received()?.map(owned))
}

/// Take what `transport` has, stopping after each key-exchange message so the
/// caller can act on it — settle strictness, install keys — before the next
/// is opened, as the layer above does.
fn drain(transport: &mut Transport) -> Result<Vec<Event>, TransportError> {
    let mut events = Vec::new();
    while let Some(event) = next(transport)? {
        let stop = match &event {
            Event::Disconnected(..) => true,
            Event::Message(_, payload) => {
                payload[0] == msg::KEXINIT || (30..=49).contains(&payload[0])
            }
            _ => false,
        };
        events.push(event);
        if stop {
            break;
        }
    }
    Ok(events)
}

/// Move everything `from` has sealed to `to`, and return what `to` makes of it.
fn pump(from: &mut Transport, to: &mut Transport) -> Result<Vec<Event>, TransportError> {
    let bytes = from.pending_output().to_vec();
    from.consume_output(bytes.len()).expect("open");
    let mut events = Vec::new();
    let mut rest = &bytes[..];
    loop {
        let taken = to.receive(rest)?;
        rest = &rest[taken..];
        events.extend(drain(to)?);
        if rest.is_empty() || to.is_closed() {
            return Ok(events);
        }
    }
}

fn connect() -> (Transport, Transport) {
    let client =
        Transport::new(Role::Client, Ident::new("client_1", None).expect("valid")).expect("room");
    let server =
        Transport::new(Role::Server, Ident::new("server_1", None).expect("valid")).expect("room");
    (client, server)
}

fn keys(cipher: Cipher, mac: Option<Mac>, salt: u8) -> Keys {
    let fill = |len: usize, lane: u8| -> Vec<u8> {
        (0..len)
            .map(|at| u8::try_from(at % 199).expect("small") ^ lane ^ salt)
            .collect()
    };
    Keys::new(
        cipher,
        mac,
        &fill(cipher.key_len(), 0x40),
        &fill(cipher.iv_len(), 0x50),
        &fill(mac.map_or(0, Mac::key_len), 0x60),
    )
    .expect("lengths from the algorithm")
}

fn padding(transport: &mut Transport) {
    let want = transport.padding_wanted();
    let bytes: Vec<u8> = (0..want)
        .map(|at| u8::try_from(at % 256).expect("small"))
        .collect();
    transport.supply_padding(&bytes).expect("open");
}

/// Run one key exchange between the pair, the way the layer above drives
/// it: `SSH_MSG_KEXINIT` both ways, a method message each way, the keys
/// handed in, then `SSH_MSG_NEWKEYS` both ways.
fn exchange(
    client: &mut Transport,
    server: &mut Transport,
    strict: Option<bool>,
    cipher: Cipher,
    mac: Option<Mac>,
    salt: u8,
) {
    client
        .send(msg::KEXINIT, &[b"client-offer"])
        .expect("KEXINIT");
    server
        .send(msg::KEXINIT, &[b"server-offer"])
        .expect("KEXINIT");
    let at_server = pump(client, server).expect("genuine");
    assert!(matches!(&at_server[..], [Event::Message(_, p)] if p[0] == msg::KEXINIT));
    let at_client = pump(server, client).expect("genuine");
    assert!(matches!(&at_client[..], [Event::Message(_, p)] if p[0] == msg::KEXINIT));
    if let Some(strict) = strict {
        client.set_strict(strict).expect("KEXINIT came first");
        server.set_strict(strict).expect("KEXINIT came first");
    }
    client
        .send(30, &[b"client-share"])
        .expect("in the exchange");
    assert!(
        matches!(&pump(client, server).expect("genuine")[..], [Event::Message(_, p)] if p[0] == 30)
    );
    server
        .send(31, &[b"server-share"])
        .expect("in the exchange");
    let c2s = || keys(cipher, mac, salt);
    let s2c = || keys(cipher, mac, salt.wrapping_add(1));
    server
        .install_keys(s2c(), c2s())
        .expect("both KEXINITs seen");
    server.send_newkeys().expect("keys installed");
    let at_client = pump(server, client).expect("genuine");
    assert!(matches!(&at_client[..], [Event::Message(_, p), ..] if p[0] == 31));
    client
        .install_keys(c2s(), s2c())
        .expect("both KEXINITs seen");
    // The client pulls NEWKEYS only after installing, as the layer above does.
    assert_eq!(drain(client).expect("genuine"), [Event::NewKeys]);
    client.send_newkeys().expect("keys installed");
    padding(client);
    padding(server);
    let at_server = pump(client, server).expect("genuine");
    assert_eq!(at_server.last(), Some(&Event::NewKeys));
}

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

/// A plaintext packet carrying `payload`, as a hostile peer would frame one
/// the transport would never send.
fn plain(payload: &[u8]) -> Vec<u8> {
    let mut sealer = Sealer::new();
    let framing = sealer.framing(payload.len()).expect("fits");
    let mut frame = vec![0u8; framing.total];
    frame[framing.payload_range()].copy_from_slice(payload);
    sealer.seal(&mut frame, &framing).expect("seals");
    frame
}

/// A client that has read a server's identification and then `packets`.
fn client_after(packets: &[&[u8]]) -> (Transport, Result<Vec<Event>, TransportError>) {
    let (mut client, _) = connect();
    let mut stream = b"SSH-2.0-hostile\r\n".to_vec();
    for payload in packets {
        stream.extend_from_slice(&plain(payload));
    }
    let taken = client.receive(&stream).expect("room");
    assert_eq!(taken, stream.len());
    let events = drain(&mut client);
    (client, events)
}

#[test]
fn each_side_opens_with_its_identification_line() {
    let (client, server) = connect();
    assert_eq!(client.pending_output(), b"SSH-2.0-client_1\r\n");
    assert_eq!(server.pending_output(), b"SSH-2.0-server_1\r\n");
}

#[test]
fn every_framing_carries_traffic_after_an_exchange() {
    for (cipher, mac) in every_framing() {
        let (mut client, mut server) = connect();
        exchange(&mut client, &mut server, Some(true), cipher, mac, 3);
        for round in 0..20u8 {
            let data = vec![round; usize::from(round) * 97];
            client.send(CHANNEL_DATA, &[&data]).expect("open");
            server.send(CHANNEL_DATA, &[&data, b"!"]).expect("open");
            let at_server = pump(&mut client, &mut server).expect("genuine");
            let at_client = pump(&mut server, &mut client).expect("genuine");
            let mut expected = vec![CHANNEL_DATA];
            expected.extend_from_slice(&data);
            assert_eq!(
                at_server,
                [Event::Message(u32::from(round), expected.clone())]
            );
            expected.push(b'!');
            assert_eq!(
                at_client,
                [Event::Message(u32::from(round), expected)],
                "{cipher:?} {mac:?}"
            );
        }
    }
}

#[test]
fn strict_numbering_restarts_at_every_newkeys_and_lax_numbering_does_not() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        1,
    );
    client.send(CHANNEL_DATA, &[b"a"]).expect("open");
    assert_eq!(
        pump(&mut client, &mut server).expect("genuine"),
        [Event::Message(0, vec![CHANNEL_DATA, b'a'])]
    );
    // A rekey restarts it again: strictness lasts the whole connection.
    exchange(
        &mut client,
        &mut server,
        None,
        Cipher::Aes256Ctr,
        Some(Mac::HmacSha512Etm),
        9,
    );
    client.send(CHANNEL_DATA, &[b"b"]).expect("open");
    assert_eq!(
        pump(&mut client, &mut server).expect("genuine"),
        [Event::Message(0, vec![CHANNEL_DATA, b'b'])]
    );

    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(false),
        Cipher::Aes128Gcm,
        None,
        1,
    );
    client.send(CHANNEL_DATA, &[b"c"]).expect("open");
    // KEXINIT, the method message, and NEWKEYS went first.
    assert_eq!(
        pump(&mut client, &mut server).expect("genuine"),
        [Event::Message(3, vec![CHANNEL_DATA, b'c'])]
    );
}

#[test]
fn strict_kex_refuses_a_packet_ahead_of_the_first_kexinit() {
    // CVE-2023-48795: an attacker's IGNORE before the server's KEXINIT.
    let (mut client, events) = client_after(&[&[msg::IGNORE, 0, 0, 0, 0], &[msg::KEXINIT, 1]]);
    let events = events.expect("the transport cannot know yet");
    assert!(matches!(&events[..], [Event::Message(1, _)]));
    assert_eq!(client.set_strict(true), Err(TransportError::StrictKex));
    assert!(client.is_closed());
    // Lax, the same stream is acceptable.
    let (mut client, _) = client_after(&[&[msg::IGNORE, 0, 0, 0, 0], &[msg::KEXINIT, 1]]);
    client.set_strict(false).expect("lax");
}

#[test]
fn strict_kex_refuses_anything_but_the_exchange_before_the_first_newkeys() {
    for intruder in [
        &[msg::IGNORE, 0, 0, 0, 0][..],
        &[msg::DEBUG, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        &[msg::UNIMPLEMENTED, 0, 0, 0, 7],
        &[msg::SERVICE_REQUEST, 0, 0, 0, 0],
        &[7, 0, 0, 0, 0],
        &[22],
        &[CHANNEL_DATA],
    ] {
        let (mut client, events) = client_after(&[&[msg::KEXINIT, 1]]);
        assert_eq!(events.expect("KEXINIT").len(), 1);
        client.set_strict(true).expect("KEXINIT came first");
        client.receive(&plain(intruder)).expect("room");
        assert_eq!(
            drain(&mut client),
            Err(TransportError::StrictKex),
            "{intruder:?}"
        );
        assert!(client.is_closed());
    }
}

#[test]
fn lax_kex_tolerates_the_generic_messages_but_not_the_layers_above() {
    let (mut client, events) = client_after(&[&[msg::KEXINIT, 1]]);
    events.expect("KEXINIT");
    client.set_strict(false).expect("lax");
    let debug = [msg::DEBUG, 1, 0, 0, 0, 2, b'h', b'i', 0, 0, 0, 0];
    for tolerated in [
        &[msg::IGNORE, 0, 0, 0, 0][..],
        &debug,
        &[msg::UNIMPLEMENTED, 0, 0, 0, 7],
    ] {
        client.receive(&plain(tolerated)).expect("room");
    }
    assert_eq!(
        drain(&mut client).expect("tolerated"),
        [Event::Debug(true, b"hi".to_vec()), Event::Unimplemented(7)]
    );
    client.receive(&plain(&[CHANNEL_DATA])).expect("room");
    assert_eq!(
        drain(&mut client),
        Err(TransportError::Unexpected(CHANNEL_DATA))
    );
}

#[test]
fn nothing_after_the_first_kexinit_is_opened_until_strictness_is_settled() {
    let (mut client, events) = client_after(&[&[msg::KEXINIT, 1], &[30, 1]]);
    assert_eq!(events.expect("KEXINIT").len(), 1);
    // Refused whether or not anything follows it, so the misuse cannot
    // depend on how packets happened to arrive.
    assert_eq!(next(&mut client), Err(TransportError::OutOfPhase));
    assert!(client.is_closed());
    let (mut client, _) = client_after(&[&[msg::KEXINIT, 1]]);
    assert_eq!(next(&mut client), Err(TransportError::OutOfPhase));
}

#[test]
fn phase_rules_on_what_the_peer_sends_are_enforced() {
    // A second KEXINIT inside one exchange.
    let (mut client, _) = client_after(&[&[msg::KEXINIT, 1]]);
    client.set_strict(false).expect("lax");
    client.receive(&plain(&[msg::KEXINIT, 2])).expect("room");
    assert_eq!(
        drain(&mut client),
        Err(TransportError::Unexpected(msg::KEXINIT))
    );
    // A method message before any exchange.
    let (_, events) = client_after(&[&[30, 1]]);
    assert_eq!(events, Err(TransportError::Unexpected(30)));
    // NEWKEYS before any keys were installed for it.
    let (mut client, _) = client_after(&[&[msg::KEXINIT, 1]]);
    client.set_strict(true).expect("strict");
    client.receive(&plain(&[msg::NEWKEYS])).expect("room");
    assert_eq!(
        drain(&mut client),
        Err(TransportError::Unexpected(msg::NEWKEYS))
    );
    // The layers above before the first exchange completes.
    let (_, events) = client_after(&[&[50, 0, 0, 0, 0]]);
    assert_eq!(events, Err(TransportError::Unexpected(50)));
}

#[test]
fn a_malformed_transport_message_is_refused() {
    for (payload, number) in [
        (&[msg::DISCONNECT, 0, 0, 0][..], msg::DISCONNECT),
        (&[msg::DISCONNECT, 0, 0, 0, 2, 0, 0, 0, 9], msg::DISCONNECT),
        (&[msg::DEBUG, 1, 0, 0, 0, 1, b'x'], msg::DEBUG),
        (&[msg::UNIMPLEMENTED, 0, 0, 0, 7, 0], msg::UNIMPLEMENTED),
    ] {
        let (_, events) = client_after(&[payload]);
        assert_eq!(
            events,
            Err(TransportError::Malformed(number)),
            "{payload:?}"
        );
    }
    // A NEWKEYS carrying anything is not a NEWKEYS.
    let (mut client, mut server) = connect();
    client.send(msg::KEXINIT, &[b"c"]).expect("first");
    let mut stream = server.pending_output().to_vec();
    stream.extend_from_slice(&plain(&[msg::KEXINIT, 1]));
    client.receive(&stream).expect("room");
    drain(&mut client).expect("KEXINIT");
    client.set_strict(false).expect("lax");
    client
        .install_keys(
            keys(Cipher::Aes128Gcm, None, 1),
            keys(Cipher::Aes128Gcm, None, 2),
        )
        .expect("both exchanged");
    client.receive(&plain(&[msg::NEWKEYS, 0])).expect("room");
    assert_eq!(
        drain(&mut client),
        Err(TransportError::Malformed(msg::NEWKEYS))
    );
    server.disconnect(DisconnectReason::BY_APPLICATION, "done");
}

#[test]
fn what_may_be_sent_follows_the_phase() {
    let (mut client, _) = connect();
    assert_eq!(
        client.send(CHANNEL_DATA, &[]),
        Err(SendError::OutOfPhase),
        "first must be KEXINIT"
    );
    assert_eq!(
        client.send(30, &[]),
        Err(SendError::OutOfPhase),
        "no exchange yet"
    );
    assert_eq!(client.send(0, &[]), Err(SendError::OutOfPhase));
    assert_eq!(client.send(msg::DISCONNECT, &[]), Err(SendError::Reserved));
    assert_eq!(client.send(msg::NEWKEYS, &[]), Err(SendError::Reserved));
    client.send(msg::KEXINIT, &[b"x"]).expect("first");
    assert_eq!(
        client.send(msg::KEXINIT, &[b"y"]),
        Err(SendError::OutOfPhase),
        "one per exchange"
    );
    assert_eq!(
        client.send_newkeys(),
        Err(SendError::OutOfPhase),
        "no keys yet"
    );
    client.send(30, &[b"share"]).expect("inside the exchange");
    assert!(!client.is_closed(), "refusals leave the connection alone");
}

#[test]
fn keys_or_strictness_out_of_order_are_fatal() {
    let (mut client, _) = connect();
    assert_eq!(
        client.install_keys(
            keys(Cipher::Aes128Gcm, None, 1),
            keys(Cipher::Aes128Gcm, None, 2)
        ),
        Err(TransportError::OutOfPhase)
    );
    assert!(client.is_closed());
    let (mut client, _) = connect();
    assert_eq!(
        client.set_strict(true),
        Err(TransportError::OutOfPhase),
        "no KEXINIT yet"
    );
    // Settled once only.
    let (mut client, _) = client_after(&[&[msg::KEXINIT, 1]]);
    client.set_strict(true).expect("first");
    assert_eq!(client.set_strict(true), Err(TransportError::OutOfPhase));
    // Keys before strictness is settled.
    let (mut client, _) = connect();
    client.send(msg::KEXINIT, &[b"c"]).expect("first");
    client.receive(b"SSH-2.0-s\r\n").expect("room");
    client.receive(&plain(&[msg::KEXINIT, 1])).expect("room");
    drain(&mut client).expect("KEXINIT");
    assert_eq!(
        client.install_keys(
            keys(Cipher::Aes128Gcm, None, 1),
            keys(Cipher::Aes128Gcm, None, 2)
        ),
        Err(TransportError::OutOfPhase)
    );
}

#[test]
fn the_layer_aboves_messages_wait_out_an_exchange_and_leave_in_order() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::ChaCha20Poly1305,
        None,
        5,
    );
    client.send(msg::KEXINIT, &[b"rekey"]).expect("rekey");
    for byte in 0..5u8 {
        client.send(CHANNEL_DATA, &[&[byte]]).expect("held");
    }
    assert!(client.is_holding());
    client
        .send(30, &[b"method"])
        .expect("the exchange is never held");
    let mut at_server = pump(&mut client, &mut server).expect("genuine");
    at_server.extend(drain(&mut server).expect("genuine"));
    assert!(
        matches!(&at_server[..], [Event::Message(_, k), Event::Message(_, m)] if k[0] == msg::KEXINIT && m[0] == 30),
        "only the exchange's own messages went out: {at_server:?}"
    );
    server.send(msg::KEXINIT, &[b"rekey"]).expect("answer");
    server.send(31, &[b"reply"]).expect("method");
    server
        .install_keys(
            keys(Cipher::Aes256Gcm, None, 7),
            keys(Cipher::Aes256Gcm, None, 6),
        )
        .expect("keys");
    server.send_newkeys().expect("keys installed");
    let mut at_client = pump(&mut server, &mut client).expect("genuine");
    at_client.extend(drain(&mut client).expect("genuine"));
    assert!(
        matches!(&at_client[..], [Event::Message(_, k), Event::Message(_, r)] if k[0] == msg::KEXINIT && r[0] == 31)
    );
    client
        .install_keys(
            keys(Cipher::Aes256Gcm, None, 6),
            keys(Cipher::Aes256Gcm, None, 7),
        )
        .expect("keys");
    assert_eq!(drain(&mut client).expect("genuine"), [Event::NewKeys]);
    client.send_newkeys().expect("keys installed");
    assert!(!client.is_holding(), "NEWKEYS releases them");
    let at_server = pump(&mut client, &mut server).expect("genuine");
    let mut expected = vec![Event::NewKeys];
    expected.extend((0..5u8).map(|byte| Event::Message(u32::from(byte), vec![CHANNEL_DATA, byte])));
    assert_eq!(at_server, expected, "released under the new keys, in order");
}

#[test]
fn a_peer_disconnect_surfaces_and_closes() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Ctr,
        Some(Mac::HmacSha256),
        2,
    );
    server.disconnect(DisconnectReason::BY_APPLICATION, "maintenance window");
    assert!(server.is_closed());
    assert_eq!(server.send(CHANNEL_DATA, &[]), Err(SendError::Closed));
    let events = pump(&mut server, &mut client).expect("genuine");
    assert_eq!(
        events,
        [Event::Disconnected(
            DisconnectReason::BY_APPLICATION,
            b"maintenance window".to_vec()
        )]
    );
    assert!(client.is_closed());
    assert_eq!(client.next_received().err(), Some(TransportError::Closed));
    assert_eq!(client.receive(b"more").err(), Some(TransportError::Closed));
}

#[test]
fn a_fatal_error_tells_the_peer_why() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes256Gcm,
        None,
        4,
    );
    server.send(CHANNEL_DATA, &[b"payload"]).expect("open");
    let mut bytes = server.pending_output().to_vec();
    server.consume_output(bytes.len()).expect("open");
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    client.receive(&bytes).expect("room");
    assert_eq!(
        drain(&mut client),
        Err(TransportError::Packet(PacketError::Integrity))
    );
    assert!(client.is_closed());
    let events = pump(&mut client, &mut server).expect("genuine");
    assert_eq!(
        events,
        [Event::Disconnected(
            DisconnectReason::MAC_ERROR,
            b"corrupted MAC on input".to_vec()
        )]
    );
}

#[test]
fn a_description_is_clipped_on_a_character_boundary() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        4,
    );
    let long = "\u{e9}".repeat(200);
    server.disconnect(DisconnectReason::PROTOCOL_ERROR, &long);
    let events = pump(&mut server, &mut client).expect("genuine");
    let [Event::Disconnected(_, text)] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(text.len(), 128);
    assert!(core::str::from_utf8(text).is_ok());
}

#[test]
fn plaintext_packets_are_zero_padded_and_draw_no_padding() {
    let (mut client, _) = connect();
    client.supply_padding(&[0xAB; 64]).expect("open");
    client.send(msg::KEXINIT, &[b"offer"]).expect("first");
    assert_eq!(client.reserve.len(), 64);
    let sent = client.pending_output();
    let packet = &sent[b"SSH-2.0-client_1\r\n".len()..];
    let padding = usize::from(packet[4]);
    assert!(packet[packet.len() - padding..].iter().all(|&b| b == 0));
}

#[test]
fn padding_is_asked_for_below_the_low_water_mark() {
    let (mut client, _) = connect();
    assert_eq!(client.padding_wanted(), PADDING_RESERVE);
    client
        .supply_padding(&[1; PADDING_LOW_WATER - 1])
        .expect("open");
    assert_eq!(
        client.padding_wanted(),
        PADDING_RESERVE - (PADDING_LOW_WATER - 1)
    );
    client.supply_padding(&[1]).expect("open");
    assert_eq!(client.padding_wanted(), 0);
    let took = client.supply_padding(&[1; PADDING_RESERVE]).expect("open");
    assert_eq!(
        took,
        PADDING_RESERVE - PADDING_LOW_WATER,
        "never more than the reserve holds"
    );
}

#[test]
fn a_short_reserve_holds_ordinary_messages_until_padding_arrives() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Ctr,
        Some(Mac::HmacSha256Etm),
        8,
    );
    let spare = client.reserve.len() - PADDING_FLOOR;
    client.reserve.discard_front(spare);
    client.send(CHANNEL_DATA, &[b"waits"]).expect("held");
    assert!(client.is_holding());
    client
        .send(CHANNEL_DATA, &[b"behind"])
        .expect("held, in order");
    assert!(pump(&mut client, &mut server).expect("genuine").is_empty());
    padding(&mut client);
    assert!(!client.is_holding());
    assert_eq!(
        pump(&mut client, &mut server).expect("genuine"),
        [
            Event::Message(0, b"\x5ewaits".to_vec()),
            Event::Message(1, b"\x5ebehind".to_vec())
        ]
    );
}

#[test]
fn a_message_never_overtakes_one_already_waiting() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        8,
    );
    // Under AES-GCM a 12-byte payload pads by 19 and an 11-byte one by 4: a
    // reserve between the two pads the second and not the first.
    let spare = client.reserve.len() - (PADDING_FLOOR + 4);
    client.reserve.discard_front(spare);
    client
        .send(CHANNEL_DATA, &[&[1; 11]])
        .expect("waits for padding");
    client
        .send(CHANNEL_DATA, &[&[2; 10]])
        .expect("could be padded, but waits its turn");
    assert!(pump(&mut client, &mut server).expect("genuine").is_empty());
    padding(&mut client);
    let events = pump(&mut client, &mut server).expect("genuine");
    let order: Vec<u8> = events
        .iter()
        .map(|event| match event {
            Event::Message(_, payload) => payload[1],
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(order, [1, 2]);
}

#[test]
fn the_exchange_draws_on_the_floor_ordinary_traffic_may_not_touch() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        8,
    );
    let spare = client.reserve.len() - PADDING_FLOOR;
    client.reserve.discard_front(spare);
    client
        .send(msg::KEXINIT, &[b"rekey"])
        .expect("the floor pads it");
    client.send(30, &[b"share"]).expect("and this");
    assert!(client.reserve.len() < PADDING_FLOOR);
    assert!(!client.is_holding());
    // Emptied, even the exchange waits for padding rather than failing.
    let left = client.reserve.len();
    client.reserve.discard_front(left);
    client.send(30, &[b"again"]).expect("waits");
    assert!(client.is_holding() && !client.is_closed());
    let at_server = pump(&mut client, &mut server).expect("genuine");
    let mut at_server_rest = drain(&mut server).expect("genuine");
    assert_eq!(at_server.len() + at_server_rest.len(), 2, "the third waits");
    padding(&mut client);
    assert!(!client.is_holding());
    at_server_rest = pump(&mut client, &mut server).expect("genuine");
    assert!(matches!(&at_server_rest[..], [Event::Message(_, m)] if m == b"\x1eagain"));
}

#[test]
fn a_newkeys_short_of_padding_switches_keys_only_when_it_is_sealed() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes256Gcm,
        None,
        2,
    );
    server.send(msg::KEXINIT, &[b"rekey"]).expect("rekey");
    let _ = pump(&mut server, &mut client).expect("genuine");
    client.send(msg::KEXINIT, &[b"rekey"]).expect("answer");
    client.send(30, &[b"share"]).expect("method");
    let _ = pump(&mut client, &mut server).expect("genuine");
    let _ = drain(&mut server).expect("genuine");
    server.send(31, &[b"reply"]).expect("method");
    server
        .install_keys(
            keys(Cipher::Aes128Ctr, Some(Mac::HmacSha256), 4),
            keys(Cipher::Aes128Ctr, Some(Mac::HmacSha256), 3),
        )
        .expect("keys");
    let left = server.reserve.len();
    server.reserve.discard_front(left);
    server.send_newkeys().expect("waits for padding");
    assert_eq!(server.kex.ours, Ours::Finishing);
    assert_eq!(
        server.send_newkeys(),
        Err(SendError::OutOfPhase),
        "once only"
    );
    assert_eq!(
        server.send(msg::KEXINIT, &[b"too soon"]),
        Err(SendError::OutOfPhase)
    );
    // Sent after NEWKEYS, so it must follow it under the new keys.
    server.send(CHANNEL_DATA, &[b"after"]).expect("held");
    padding(&mut server);
    assert!(!server.is_holding());
    assert_eq!(server.kex.ours, Ours::Idle, "sealed, so switched");
    let _ = pump(&mut server, &mut client).expect("genuine");
    client
        .install_keys(
            keys(Cipher::Aes128Ctr, Some(Mac::HmacSha256), 3),
            keys(Cipher::Aes128Ctr, Some(Mac::HmacSha256), 4),
        )
        .expect("keys");
    let mut rest = drain(&mut client).expect("genuine");
    rest.extend(drain(&mut client).expect("genuine"));
    assert_eq!(
        rest,
        [Event::NewKeys, Event::Message(0, b"\x5eafter".to_vec())]
    );
}

#[test]
fn a_rekey_falls_due_by_volume_or_by_the_hosts_interval() {
    let (mut client, mut server) = connect();
    client.set_rekey_limit(4096);
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        1,
    );
    assert!(!client.rekey_due());
    while !client.rekey_due() {
        client.send(CHANNEL_DATA, &[&[0; 500]]).expect("open");
        padding(&mut client);
        let _ = pump(&mut client, &mut server).expect("genuine");
    }
    client.send(msg::KEXINIT, &[b"rekey"]).expect("rekey");
    assert!(!client.rekey_due(), "an exchange under way answers it");

    let (mut client, mut server) = connect();
    client.rekey_interval_elapsed();
    assert!(
        !client.rekey_due(),
        "nothing to rekey before the first exchange"
    );
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        1,
    );
    client.rekey_interval_elapsed();
    assert!(client.rekey_due());
}

#[test]
fn the_backlog_is_bounded_and_draining_it_lifts_the_refusal() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        1,
    );
    let chunk = vec![0u8; 32_768];
    let mut sent = 0;
    loop {
        padding(&mut client);
        match client.send(CHANNEL_DATA, &[&chunk]) {
            Ok(()) => sent += 1,
            Err(SendError::Backlogged) => break,
            Err(other) => panic!("{other:?}"),
        }
        assert!(sent < 64, "the bound was never reached");
    }
    assert!(client.outbound_backlog() <= MAX_BACKLOG);
    let events = pump(&mut client, &mut server).expect("genuine");
    assert_eq!(events.len(), sent);
    client.send(CHANNEL_DATA, &[&chunk]).expect("room again");
}

#[test]
fn a_server_may_send_banner_lines_ahead_of_its_identification() {
    let (mut client, _) = connect();
    client
        .receive(b"Authorised use only\r\n\r\nSSH-2.0-OpenSSH_10.2\r\n")
        .expect("room");
    assert_eq!(
        drain(&mut client).expect("banners"),
        [
            Event::Banner(b"Authorised use only".to_vec()),
            Event::Banner(Vec::new())
        ]
    );
    assert_eq!(
        client.peer_ident().map(Ident::as_bytes),
        Some(&b"SSH-2.0-OpenSSH_10.2"[..])
    );
    // But no more of them than a client reads.
    let (mut client, _) = connect();
    let lines = b"x\r\n".repeat(MAX_BANNER_LINES + 1);
    client.receive(&lines).expect("room");
    let mut outcome = Ok(());
    for _ in 0..=MAX_BANNER_LINES {
        if let Err(err) = next(&mut client) {
            outcome = Err(err);
            break;
        }
    }
    assert_eq!(
        outcome,
        Err(TransportError::Ident(IdentError::TooManyLines))
    );
}

#[test]
fn a_server_refuses_a_client_line_that_is_not_its_identification() {
    let (_, mut server) = connect();
    server.receive(b"GET / HTTP/1.1\r\n").expect("room");
    assert_eq!(
        drain(&mut server),
        Err(TransportError::Ident(IdentError::UnexpectedLine))
    );
    assert!(server.is_closed());
    // Nobody who has not identified is sent a binary DISCONNECT.
    assert_eq!(server.pending_output(), b"SSH-2.0-server_1\r\n");
}

#[test]
fn receive_takes_what_fits_and_the_rest_follows_once_drained() {
    let (mut client, mut server) = connect();
    exchange(
        &mut client,
        &mut server,
        Some(true),
        Cipher::Aes128Gcm,
        None,
        1,
    );
    // Two of these fit the sender's backlog but not the receiver's buffer.
    let big = vec![7u8; 200_000];
    for _ in 0..2 {
        padding(&mut server);
        server
            .send(CHANNEL_DATA, &[&big])
            .expect("fits the backlog");
    }
    let bytes = server.pending_output().to_vec();
    let mut rest = &bytes[..];
    let mut received = 0;
    while !rest.is_empty() {
        let taken = client.receive(rest).expect("open");
        assert!(taken > 0, "the head packet is always taken");
        rest = &rest[taken..];
        received += drain(&mut client).expect("genuine").len();
    }
    assert_eq!(received, 2);
}

#[test]
fn byte_at_a_time_delivery_changes_nothing() {
    for (cipher, mac) in every_framing() {
        let (mut client, mut server) = connect();
        exchange(&mut client, &mut server, Some(true), cipher, mac, 6);
        for len in [1, 40, 333] {
            server
                .send(CHANNEL_DATA, &[&vec![0x33; len]])
                .expect("open");
        }
        let bytes = server.pending_output().to_vec();
        let mut events = Vec::new();
        for byte in bytes {
            assert_eq!(client.receive(&[byte]).expect("open"), 1);
            events.extend(drain(&mut client).expect("genuine"));
        }
        assert_eq!(events.len(), 3, "{cipher:?} {mac:?}");
    }
}
