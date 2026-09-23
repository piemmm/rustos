//! Deterministic fuzz harness for the binary packet protocol and the
//! transport state machine above it.
//!
//! Every byte a peer sends after its identification goes through here. The
//! invariants:
//!
//! * opening arbitrary bytes under any framing never panics, and a packet it
//!   reports lies inside the bytes it was given;
//! * sealed packets open to exactly what was sealed, under every framing,
//!   however the stream is divided;
//! * damage to an authenticated stream never yields a payload that was not
//!   sent: whatever opens before the first refusal is the originals, in order;
//! * a transport fed a hostile peer's bytes never panics, and once it refuses
//!   something it stays closed;
//! * under strict key exchange nothing but the exchange's own messages ever
//!   surfaces before the peer's first `SSH_MSG_NEWKEYS`; and
//! * after the peer's first `SSH_MSG_KEXINIT`, the first message outside the
//!   exchange ends a strict connection, whatever it is; and
//! * two transports carry random traffic across random rekeys intact and in
//!   order, with sends, deliveries, padding, and exchanges interleaved
//!   however the draw says — whatever the key-exchange gate and a short
//!   padding reserve held back.

use tairix_fuzzseed::Prng;
use tairix_ssh::algorithm::{Cipher, Keys, Mac};
use tairix_ssh::ident::Ident;
use tairix_ssh::msg;
use tairix_ssh::packet::{Opened, Opener, Sealer};
use tairix_ssh::transport::{Received, SendError, Transport, TransportError, PADDING_RESERVE};
use tairix_ssh::Role;

/// Fixed-iteration sweep run once by a plain `cargo test`.
const SMOKE_ITERATIONS: u64 = 250;

/// Largest payload a drawn packet carries.
const MAX_PAYLOAD: usize = 4000;

fn draw_framing(rng: &mut Prng) -> Option<(Cipher, Option<Mac>)> {
    if rng.below(8) == 0 {
        return None;
    }
    let cipher = *rng.pick(&Cipher::ALL);
    let mac = (!cipher.is_aead()).then(|| *rng.pick(&Mac::ALL));
    Some((cipher, mac))
}

fn draw_keys(rng: &mut Prng, cipher: Cipher, mac: Option<Mac>) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut key = vec![0u8; cipher.key_len()];
    let mut iv = vec![0u8; cipher.iv_len()];
    let mut integrity = vec![0u8; mac.map_or(0, Mac::key_len)];
    rng.fill(&mut key);
    rng.fill(&mut iv);
    rng.fill(&mut integrity);
    (key, iv, integrity)
}

fn keys_of(cipher: Cipher, mac: Option<Mac>, material: &(Vec<u8>, Vec<u8>, Vec<u8>)) -> Keys {
    Keys::new(cipher, mac, &material.0, &material.1, &material.2).expect("drawn to length")
}

/// A sealer and opener sharing one drawn framing and key.
fn draw_pair(rng: &mut Prng) -> (Sealer, Opener) {
    let mut sealer = Sealer::new();
    let mut opener = Opener::new();
    if let Some((cipher, mac)) = draw_framing(rng) {
        let material = draw_keys(rng, cipher, mac);
        let strict = rng.below(2) == 0;
        sealer.rekey(&keys_of(cipher, mac, &material), strict, None);
        opener.rekey(&keys_of(cipher, mac, &material), strict, None);
    }
    (sealer, opener)
}

fn seal(rng: &mut Prng, sealer: &mut Sealer, payload: &[u8]) -> Vec<u8> {
    let framing = sealer.framing(payload.len()).expect("drawn to fit");
    let mut frame = vec![0u8; framing.total];
    frame[framing.payload_range()].copy_from_slice(payload);
    rng.fill(&mut frame[framing.padding_range()]);
    sealer.seal(&mut frame, &framing).expect("seals");
    frame
}

/// Open everything `wire` holds, returning the payloads before the first
/// refusal, checking every report lies inside the bytes it was given.
fn open_stream(opener: &mut Opener, wire: &[u8], chunks: &[usize]) -> Vec<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut payloads = Vec::new();
    let mut offset = 0;
    let mut sizes = chunks.iter().copied().cycle();
    while offset < wire.len() {
        let take = sizes.next().unwrap_or(1).max(1).min(wire.len() - offset);
        buffer.extend_from_slice(&wire[offset..offset + take]);
        offset += take;
        loop {
            match opener.open(&mut buffer) {
                Ok(Opened::Need(need)) => {
                    assert!(need > buffer.len(), "asked for bytes it already had");
                    break;
                }
                Ok(Opened::Packet {
                    framed, payload, ..
                }) => {
                    assert!(framed <= buffer.len() && payload.end <= framed && !payload.is_empty());
                    payloads.push(buffer[payload].to_vec());
                    buffer.drain(..framed);
                }
                Err(_) => return payloads,
            }
        }
    }
    payloads
}

fn draw_payloads(rng: &mut Prng) -> Vec<Vec<u8>> {
    (0..=rng.at_most(7))
        .map(|_| {
            let mut payload = vec![0u8; 1 + rng.at_most(MAX_PAYLOAD)];
            rng.fill(&mut payload);
            payload
        })
        .collect()
}

fn draw_chunks(rng: &mut Prng) -> Vec<usize> {
    (0..=rng.at_most(4)).map(|_| 1 + rng.at_most(700)).collect()
}

/// The packet layer on its own: noise, then a sealed stream round trip.
fn packet_round(rng: &mut Prng) {
    let (_, mut opener) = draw_pair(rng);
    let mut noise = vec![0u8; rng.at_most(2048)];
    rng.fill(&mut noise);
    let _ = open_stream(&mut opener, &noise, &draw_chunks(rng));

    let (mut sealer, mut opener) = draw_pair(rng);
    let payloads = draw_payloads(rng);
    let mut wire = Vec::new();
    for payload in &payloads {
        wire.extend_from_slice(&seal(rng, &mut sealer, payload));
    }
    let chunks = draw_chunks(rng);
    assert_eq!(
        open_stream(&mut opener, &wire, &chunks),
        payloads,
        "a sealed stream did not round-trip"
    );
}

/// Damage an authenticated stream and check nothing but originals open.
fn damage_round(rng: &mut Prng) {
    let Some((cipher, mac)) = draw_framing(rng) else {
        return;
    };
    let material = draw_keys(rng, cipher, mac);
    let mut sealer = Sealer::new();
    sealer.rekey(&keys_of(cipher, mac, &material), true, None);
    let payloads = draw_payloads(rng);
    let mut wire = Vec::new();
    for payload in &payloads {
        wire.extend_from_slice(&seal(rng, &mut sealer, payload));
    }
    for _ in 0..=rng.at_most(3) {
        let at = rng.below(wire.len());
        wire[at] ^= 1 << rng.below(8);
    }
    let mut opener = Opener::new();
    opener.rekey(&keys_of(cipher, mac, &material), true, None);
    let survived = open_stream(&mut opener, &wire, &draw_chunks(rng));
    assert!(survived.len() <= payloads.len());
    assert_eq!(
        &payloads[..survived.len()],
        &survived[..],
        "{cipher:?} {mac:?}: damage produced a payload"
    );
}

/// What a transport surfaced, owned.
#[derive(Debug)]
enum Event {
    Message(Vec<u8>),
    NewKeys,
    Other,
}

fn next(transport: &mut Transport) -> Result<Option<Event>, TransportError> {
    Ok(transport.next_received()?.map(|received| match received {
        Received::Message { payload, .. } => Event::Message(payload.to_vec()),
        Received::NewKeys => Event::NewKeys,
        _ => Event::Other,
    }))
}

fn plain(payload: &[u8]) -> Vec<u8> {
    let mut sealer = Sealer::new();
    let framing = sealer.framing(payload.len()).expect("fits");
    let mut frame = vec![0u8; framing.total];
    frame[framing.payload_range()].copy_from_slice(payload);
    sealer.seal(&mut frame, &framing).expect("seals");
    frame
}

/// A hostile server's opening: an identification, then packets or noise.
fn hostile_round(rng: &mut Prng) {
    let ident = Ident::new("fuzz", None).expect("valid");
    let mut client = Transport::new(Role::Client, ident).expect("room");
    let strict = rng.below(2) == 0;
    let mut stream = b"SSH-2.0-hostile\r\n".to_vec();
    let starts_with_kexinit = rng.below(4) != 0;
    if starts_with_kexinit {
        stream.extend_from_slice(&plain(&[msg::KEXINIT, 1, 2, 3]));
    }
    let numbers: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 20, 21, 30, 50, 94];
    for _ in 0..rng.at_most(6) {
        if rng.below(5) == 0 {
            let mut noise = vec![0u8; rng.at_most(64)];
            rng.fill(&mut noise);
            stream.extend_from_slice(&noise);
        } else {
            let mut payload = vec![*rng.pick(&numbers)];
            let mut body = vec![0u8; rng.at_most(24)];
            rng.fill(&mut body);
            payload.extend_from_slice(&body);
            stream.extend_from_slice(&plain(&payload));
        }
    }
    let mut settled = false;
    let mut offset = 0;
    'feed: while offset < stream.len() {
        let take = (1 + rng.at_most(200)).min(stream.len() - offset);
        match client.receive(&stream[offset..offset + take]) {
            Ok(taken) => offset += taken,
            Err(_) => break,
        }
        loop {
            match next(&mut client) {
                Ok(None) => break,
                Ok(Some(Event::Message(payload))) => {
                    if payload[0] == msg::KEXINIT && !settled {
                        settled = true;
                        if client.set_strict(strict).is_err() {
                            assert!(strict, "lax settles whatever came first");
                            break 'feed;
                        }
                    } else if strict && settled {
                        assert!(
                            (30..=49).contains(&payload[0]),
                            "strict KEX let message {} surface",
                            payload[0]
                        );
                    }
                }
                Ok(Some(Event::NewKeys | Event::Other)) => {
                    assert!(
                        !(strict && settled),
                        "strict KEX let a non-exchange message surface"
                    );
                }
                Err(_) => break 'feed,
            }
        }
    }
    if client.is_closed() {
        assert_eq!(client.next_received().err(), Some(TransportError::Closed));
        assert_eq!(client.receive(b"x").err(), Some(TransportError::Closed));
        assert_eq!(client.send(94, &[]), Err(SendError::Closed));
    }
}

/// Strict key exchange, exactly: after the peer's first `SSH_MSG_KEXINIT`,
/// method messages surface and the first message outside the exchange ends
/// the connection with `StrictKex`, whatever it is and whatever follows.
fn strict_round(rng: &mut Prng) {
    let mut client =
        Transport::new(Role::Client, Ident::new("fuzz", None).expect("valid")).expect("room");
    let mut stream = b"SSH-2.0-hostile\r\n".to_vec();
    stream.extend_from_slice(&plain(&[msg::KEXINIT, 9]));
    let methods = rng.at_most(4);
    for _ in 0..methods {
        let mut payload = vec![u8::try_from(30 + rng.below(20)).expect("small")];
        let mut body = vec![0u8; rng.at_most(16)];
        rng.fill(&mut body);
        payload.extend_from_slice(&body);
        stream.extend_from_slice(&plain(&payload));
    }
    let intruder = loop {
        let number = rng.next_u8();
        if number != msg::DISCONNECT && number != msg::NEWKEYS && !(30..=49).contains(&number) {
            break number;
        }
    };
    let mut payload = vec![intruder];
    let mut body = vec![0u8; rng.at_most(16)];
    rng.fill(&mut body);
    payload.extend_from_slice(&body);
    stream.extend_from_slice(&plain(&payload));
    stream.extend_from_slice(&plain(&[94, 1, 2]));
    assert_eq!(client.receive(&stream).expect("room"), stream.len());
    assert!(matches!(next(&mut client), Ok(Some(Event::Message(p))) if p[0] == msg::KEXINIT));
    client.set_strict(true).expect("KEXINIT came first");
    for _ in 0..methods {
        assert!(
            matches!(next(&mut client), Ok(Some(Event::Message(p))) if (30..=49).contains(&p[0]))
        );
    }
    assert_eq!(
        next(&mut client).err(),
        Some(TransportError::StrictKex),
        "intruder {intruder}"
    );
    assert!(client.is_closed());
}

/// The keys of the exchange under way, which both sides install.
struct Epoch {
    cipher: Cipher,
    mac: Option<Mac>,
    c2s: (Vec<u8>, Vec<u8>, Vec<u8>),
    s2c: (Vec<u8>, Vec<u8>, Vec<u8>),
}

/// How far one direction of an exchange has got, as the mock sees it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Progress {
    Idle,
    /// Its `SSH_MSG_KEXINIT` is exchanged.
    Offered,
    /// Its `SSH_MSG_NEWKEYS` is exchanged.
    Switched,
}

/// One side of a connection and a mock of the layer above it: just enough
/// key-exchange driving to rekey on demand, with the method reduced to one
/// message each way.
struct Side {
    transport: Transport,
    role: Role,
    settled: bool,
    /// What this side has sent of the exchange under way.
    ours: Progress,
    /// What it has received.
    theirs: Progress,
    /// The upper-layer payloads it has received, in arrival order.
    received: Vec<Vec<u8>>,
    /// The upper-layer payloads it has sent, in send order.
    sent: Vec<Vec<u8>>,
}

impl Side {
    fn new(role: Role) -> Self {
        let name = if role == Role::Client { "c" } else { "s" };
        Self {
            transport: Transport::new(role, Ident::new(name, None).expect("valid")).expect("room"),
            role,
            settled: false,
            ours: Progress::Idle,
            theirs: Progress::Idle,
            received: Vec::new(),
            sent: Vec::new(),
        }
    }

    fn in_exchange(&self) -> bool {
        self.ours != Progress::Idle || self.theirs != Progress::Idle
    }

    fn send_kexinit(&mut self) {
        self.transport
            .send(msg::KEXINIT, &[b"offer"])
            .expect("KEXINIT");
        self.ours = Progress::Offered;
        self.after_offers();
    }

    /// The client's method message goes once both offers are exchanged.
    fn after_offers(&mut self) {
        if self.role == Role::Client
            && self.ours == Progress::Offered
            && self.theirs == Progress::Offered
        {
            self.transport
                .send(30, &[b"share"])
                .expect("inside the exchange");
        }
    }

    fn install(&mut self, epoch: &Epoch) {
        let (out, inbound) = if self.role == Role::Client {
            (&epoch.c2s, &epoch.s2c)
        } else {
            (&epoch.s2c, &epoch.c2s)
        };
        self.transport
            .install_keys(
                keys_of(epoch.cipher, epoch.mac, out),
                keys_of(epoch.cipher, epoch.mac, inbound),
            )
            .expect("both offers exchanged");
    }

    fn send_newkeys(&mut self) {
        self.transport.send_newkeys().expect("installed");
        self.ours = Progress::Switched;
        self.finish_if_done();
    }

    fn finish_if_done(&mut self) {
        if self.ours == Progress::Switched && self.theirs == Progress::Switched {
            self.ours = Progress::Idle;
            self.theirs = Progress::Idle;
        }
    }

    /// Act on one item, as the layer above would.
    fn handle(&mut self, event: Event, epoch: Option<&Epoch>, strict: bool) {
        match event {
            Event::Message(payload) if payload[0] == 94 => self.received.push(payload),
            Event::Message(payload) if payload[0] == msg::KEXINIT => {
                self.theirs = Progress::Offered;
                if !self.settled {
                    self.transport
                        .set_strict(strict)
                        .expect("KEXINIT came first");
                    self.settled = true;
                }
                if self.ours == Progress::Idle {
                    self.send_kexinit();
                } else {
                    self.after_offers();
                }
            }
            Event::Message(payload) if payload[0] == 30 && self.role == Role::Server => {
                self.transport
                    .send(31, &[b"reply"])
                    .expect("inside the exchange");
                self.install(epoch.expect("an exchange is under way"));
                self.send_newkeys();
            }
            Event::Message(payload) if payload[0] == 31 && self.role == Role::Client => {
                self.install(epoch.expect("an exchange is under way"));
            }
            Event::NewKeys => {
                self.theirs = Progress::Switched;
                if self.ours == Progress::Switched {
                    self.finish_if_done();
                } else {
                    self.send_newkeys();
                }
            }
            other => panic!("the mock protocol sends nothing else: {other:?}"),
        }
    }
}

/// Move `from`'s output to `to` in drawn chunks, `to` acting on each item as
/// it surfaces.
fn deliver(rng: &mut Prng, from: &mut Side, to: &mut Side, epoch: Option<&Epoch>, strict: bool) {
    let bytes = from.transport.pending_output().to_vec();
    from.transport.consume_output(bytes.len()).expect("open");
    let mut offset = 0;
    while offset < bytes.len() {
        let take = (1 + rng.at_most(3000)).min(bytes.len() - offset);
        offset += to
            .transport
            .receive(&bytes[offset..offset + take])
            .expect("open");
        while let Some(event) = next(&mut to.transport).expect("genuine traffic") {
            to.handle(event, epoch, strict);
        }
    }
}

fn draw_epoch(rng: &mut Prng) -> Epoch {
    let (cipher, mac) = draw_framing(rng).unwrap_or((Cipher::Aes128Gcm, None));
    Epoch {
        cipher,
        mac,
        c2s: draw_keys(rng, cipher, mac),
        s2c: draw_keys(rng, cipher, mac),
    }
}

/// Top up `side`'s padding: mostly a trickle, so the reserve hovers near
/// empty and messages wait on it, and sometimes all it asks for.
fn feed(rng: &mut Prng, side: &mut Side) {
    let amount = if rng.below(4) == 0 {
        side.transport.padding_wanted()
    } else {
        rng.at_most(48)
    };
    let mut bytes = vec![0u8; amount.min(PADDING_RESERVE)];
    rng.fill(&mut bytes);
    side.transport.supply_padding(&bytes).expect("open");
}

/// Two transports carrying random traffic across random rekeys, with sends,
/// deliveries, padding, and exchanges interleaved however the draw says.
fn traffic_round(rng: &mut Prng) {
    let mut sides = [Side::new(Role::Client), Side::new(Role::Server)];
    let strict = rng.below(2) == 0;
    let mut epoch = Some(draw_epoch(rng));
    sides[0].send_kexinit();
    let steps = rng.at_most(40);
    for step in 0..steps + 64 {
        let settling = step >= steps;
        if epoch.is_some() && !sides[0].in_exchange() && !sides[1].in_exchange() {
            epoch = None;
        }
        let pick = if settling {
            2 + rng.below(2)
        } else {
            rng.below(6)
        };
        let side = rng.below(2);
        match pick {
            0 => {
                let mut payload = vec![94u8; 1 + rng.at_most(MAX_PAYLOAD)];
                rng.fill(&mut payload[1..]);
                if sides[side].transport.send(94, &[&payload[1..]]).is_ok() {
                    sides[side].sent.push(payload);
                }
            }
            1 if epoch.is_none() => {
                epoch = Some(draw_epoch(rng));
                sides[side].send_kexinit();
            }
            2 | 3 => {
                let (client, server) = sides.split_at_mut(1);
                let (from, to) = if pick == 2 {
                    (&mut client[0], &mut server[0])
                } else {
                    (&mut server[0], &mut client[0])
                };
                deliver(rng, from, to, epoch.as_ref(), strict);
            }
            _ => {
                if settling || rng.below(3) != 0 {
                    feed(rng, &mut sides[side]);
                }
            }
        }
        if settling {
            for side in &mut sides {
                let want = side.transport.padding_wanted();
                side.transport
                    .supply_padding(&vec![0x5A; want])
                    .expect("open");
            }
        }
    }
    for side in &sides {
        assert!(!side.transport.is_holding(), "held messages never left");
        assert!(
            side.transport.pending_output().is_empty(),
            "output never delivered"
        );
    }
    assert_eq!(
        sides[1].received, sides[0].sent,
        "client-to-server traffic arrived out of order or damaged"
    );
    assert_eq!(
        sides[0].received, sides[1].sent,
        "server-to-client traffic arrived out of order or damaged"
    );
}

#[test]
fn the_packet_protocol_and_transport_hold_their_invariants() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "the_packet_protocol_and_transport_hold_their_invariants",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut iteration: u64 = 0;
    loop {
        packet_round(&mut rng);
        damage_round(&mut rng);
        hostile_round(&mut rng);
        strict_round(&mut rng);
        traffic_round(&mut rng);
        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
