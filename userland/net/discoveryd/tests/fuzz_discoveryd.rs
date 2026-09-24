//! Deterministic fuzz harness for `discoveryd`'s trust boundary.
//!
//! The decoder parses what any peer on the segment sent and may be
//! compromised by it; the front must stay whole whatever its decoder then
//! says. The invariants:
//!
//! 1. Both directions' codecs are canonical: every frame an encoder writes
//!    decodes to what was encoded, and a frame that decodes re-encodes to
//!    exactly its bytes.
//! 2. The real decoder, fed any frame, never panics, ends its session on a
//!    frame out of order, and emits only frames the front believes.
//! 3. Against a decoder saying anything at all, the front never panics, never
//!    asks to be woken at or before the instant it last acted on, configures
//!    every decoder first and once under fresh keys and tells it its links
//!    before relaying from one, spaces its ticks, contains a decoder that
//!    says what no decoder says, relays again once the replacement starts,
//!    delivers a tick owed while its queue was full once the queue drains,
//!    and sends only to the group or to a peer it relayed from.
//! 4. Any call bytes from any caller are answered with a reply the client's
//!    decoders read, and never panic the front.
//!
//! Every structural case runs on every iteration; the random draws choose
//! content within it.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;

use tairix_abi::discovery_ipc::{
    Answer, Change, CollectReply, DiscoveryRequest, Entry, Query, ServiceTypeField, Transport,
    DISCOVERY_MAX_REPLY, ID_REPLY_LEN,
};
use tairix_abi::net::{SocketAddr, SocketDatagram, SocketDelivery, SocketLinkEvent};
use tairix_abi::net_ipc::{address_parts, IF_NAME_LEN};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::{CapabilityId, CapabilitySummary, Errno, Origin, ProcId, TrustDomain};
use tairix_discoveryd::decoder::Decoder;
use tairix_discoveryd::front::{Front, Host, Sockets, MIN_TICK_INTERVAL_NS, OUTBOUND_QUEUE};
use tairix_discoveryd::grants::Grants;
use tairix_discoveryd::wire::{
    Form, FromDecoder, ToDecoder, CACHE_KEY_LEN, DATAGRAM_HEADER_LEN, MAX_FROM_DECODER,
    MAX_TO_DECODER, RNG_KEY_LEN,
};
use tairix_fuzzseed::Prng;
use tairix_log::{Event, Sink};
use tairix_net::dns::Name;
use tairix_net::mdns::{Destination, MessageWriter, RData, Record, Section, GROUP_V4, GROUP_V6};
use tairix_sandbox::proto::{ProtoError, FRAME_HEADER_LEN};
use tairix_sandbox::session::{
    FrameOut, SessionDescriptors, SessionService, SessionStep, SessionTransport,
};
use tairix_sandbox::supervise::SessionLauncher;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 200;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;

/// The kinds of thing a hostile decoder says, one per round in turn.
const SAYINGS: usize = 12;

/// The family sockets the front listens on.
const V4: u32 = 1;
const V6: u32 = 2;

const INTERFACES: [&[u8]; 3] = [b"eth0", b"wlan0", b"bond1"];

#[derive(Clone, Copy)]
struct Quiet;

impl Sink for Quiet {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// What the front sent and rang.
#[derive(Default)]
struct World {
    draws: u8,
    sent: Vec<SocketAddr>,
}

/// A stand-in host: each draw a fresh byte pattern, so generations differ.
#[derive(Clone, Default)]
struct Keys(Rc<RefCell<World>>);

impl Host for Keys {
    fn fill_random(&mut self, out: &mut [u8]) -> Result<(), Errno> {
        let mut world = self.0.borrow_mut();
        world.draws = world.draws.wrapping_add(1);
        out.fill(world.draws);
        Ok(())
    }

    fn transmit(
        &mut self,
        _interface: [u8; IF_NAME_LEN],
        to: SocketAddr,
        _payload: &[u8],
    ) -> Result<(), Errno> {
        self.0.borrow_mut().sent.push(to);
        Ok(())
    }

    fn ring(&mut self, _port: u64, _doorbell: &[u8]) -> Result<(), Errno> {
        Ok(())
    }

    fn watch(&mut self, _peer: ProcId) -> Result<(), Errno> {
        Ok(())
    }
}

fn named(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn iface(rng: &mut Prng) -> [u8; IF_NAME_LEN] {
    named(rng.pick(&INTERFACES))
}

fn peer(last: u8) -> IpAddr {
    if last.is_multiple_of(2) {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
    } else {
        IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, u16::from(last)))
    }
}

/// A well-formed announcement of a drawn host, so the engines' cache path
/// is reached rather than only the parser's refusal.
fn announcement(rng: &mut Prng) -> Vec<u8> {
    let hosts = ["printer.local", "scanner.local", "host.local"];
    let mut out = vec![0u8; 512];
    let mut writer = MessageWriter::new(&mut out, 0, true).expect("room for a header");
    let mut record = Record::unique(
        Name::encode(rng.pick(&hosts)).expect("fixed names encode"),
        RData::A(Ipv4Addr::from(rng.next_u32())),
    );
    record.ttl = [0, 1, 120, u32::MAX][rng.below(4)];
    assert!(writer.push_record(Section::Answer, &record));
    let len = writer.finish();
    out.truncate(len);
    out
}

fn encoded(frame: &ToDecoder<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_TO_DECODER];
    let len = frame.encode(&mut out).expect("a well-formed frame fits");
    out.truncate(len);
    out
}

fn framed(payload: &[u8]) -> Vec<u8> {
    let declared = u32::try_from(payload.len()).expect("a test frame is short");
    let mut out = declared.to_le_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// Invariant 1.
fn exercise_codecs(rng: &mut Prng, noise: &[u8]) {
    if let Ok(frame) = ToDecoder::decode(noise) {
        assert_eq!(encoded(&frame), noise, "an accepted frame is canonical");
    }
    if let Ok(frame) = FromDecoder::decode(noise) {
        assert_eq!(
            from_decoder(&frame),
            noise,
            "an accepted frame is canonical"
        );
    }

    let mut cache_key = [0u8; CACHE_KEY_LEN];
    let mut rng_key = [0u8; RNG_KEY_LEN];
    rng.fill(&mut cache_key);
    rng.fill(&mut rng_key);
    let shapes = [
        ToDecoder::Configure {
            cache_key: &cache_key,
            rng_key: &rng_key,
        },
        ToDecoder::Datagram {
            now: rng.next_u64(),
            interface: iface(rng),
            source: peer(rng.next_u8()),
            port: rng.next_u16(),
            payload: noise,
        },
        ToDecoder::Tick {
            now: rng.next_u64(),
        },
        ToDecoder::Link {
            now: rng.next_u64(),
            interface: iface(rng),
            up: rng.next_u64() & 1 == 0,
        },
        ToDecoder::Ask {
            now: rng.next_u64(),
            question: rng.next_u32(),
            form: *rng.pick(&FORMS),
            name: b"\x07printer\x05local\x00",
        },
        ToDecoder::Stop {
            question: rng.next_u32(),
        },
        ToDecoder::Replay {
            question: rng.next_u32(),
            token: rng.next_u32(),
        },
    ];
    for shape in shapes {
        let mut bytes = encoded(&shape);
        assert_eq!(ToDecoder::decode(&bytes), Ok(shape));
        let at = rng.below(bytes.len());
        bytes[at] ^= 1 << rng.below(8);
        if let Ok(mutated) = ToDecoder::decode(&bytes) {
            assert_eq!(
                encoded(&mutated),
                bytes,
                "a mutated frame is refused or canonical"
            );
        }
    }
}

/// Invariant 1, for the decoder's frames.
fn exercise_decoder_codec(rng: &mut Prng, noise: &[u8]) {
    let label = if noise.is_empty() {
        &b"x"[..]
    } else {
        &noise[..noise.len().min(63)]
    };
    let answer = Entry::Answer {
        request: rng.next_u32(),
        interface: iface(rng),
        change: [Change::Added, Change::Refreshed, Change::Retired][rng.below(3)],
        ttl: rng.next_u32(),
        answer: Answer::Instance { label },
    };
    let payload = if noise.is_empty() {
        &b"q"[..]
    } else {
        &noise[..noise.len().min(1232)]
    };
    for shape in [
        FromDecoder::Deadline(None),
        FromDecoder::Deadline(Some(rng.next_u64())),
        FromDecoder::Answer(answer),
        FromDecoder::Held {
            token: rng.next_u32(),
            entry: answer,
        },
        FromDecoder::Replayed {
            token: rng.next_u32(),
        },
        FromDecoder::Transmit {
            interface: iface(rng),
            to: Destination::Group,
            payload,
        },
        FromDecoder::Transmit {
            interface: iface(rng),
            to: Destination::Peer {
                addr: peer(rng.next_u8()),
                port: rng.next_u16(),
            },
            payload,
        },
        FromDecoder::Linked {
            interface: iface(rng),
            up: rng.next_u64() & 1 == 0,
        },
    ] {
        let mut bytes = from_decoder(&shape);
        assert_eq!(FromDecoder::decode(&bytes), Ok(shape));
        let at = rng.below(bytes.len());
        bytes[at] ^= 1 << rng.below(8);
        if let Ok(mutated) = FromDecoder::decode(&bytes) {
            assert_eq!(
                from_decoder(&mutated),
                bytes,
                "a mutated frame is refused or canonical"
            );
        }
    }
}

const FORMS: [Form; 7] = [
    Form::Instance,
    Form::Service,
    Form::Text,
    Form::AddressV4,
    Form::AddressV6,
    Form::Pointer,
    Form::Type,
];

fn from_decoder(frame: &FromDecoder<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_FROM_DECODER];
    let len = frame.encode(&mut out).expect("a well-formed frame fits");
    out.truncate(len);
    out
}

/// Holds every frame the decoder emits to the front's reader.
struct Believed;

impl FrameOut for Believed {
    fn frame(&mut self, payload: &[u8]) -> Result<(), ProtoError> {
        assert!(
            FromDecoder::decode(payload).is_ok(),
            "the decoder sends only frames the front believes"
        );
        Ok(())
    }
}

/// Invariant 2: only a configuration is taken first, and only a datagram for a
/// link it was told of after.
fn exercise_decoder_order(rng: &mut Prng, noise: &[u8], configure: &[u8]) {
    let early = [
        encoded(&ToDecoder::Tick {
            now: rng.next_u64(),
        }),
        encoded(&ToDecoder::Datagram {
            now: rng.next_u64(),
            interface: iface(rng),
            source: peer(rng.next_u8()),
            port: 5353,
            payload: noise,
        }),
    ];
    for frame in early {
        assert_eq!(
            Decoder::new().handle(&frame, &mut Believed),
            SessionStep::Finished,
            "nothing but a configuration is taken first"
        );
    }
    // The first datagrams below are for links it is told of; any other is
    // one the front would never relay.
    let mut unlinked = Decoder::new();
    assert_eq!(
        unlinked.handle(configure, &mut Believed),
        SessionStep::Continue
    );
    let stray = encoded(&ToDecoder::Datagram {
        now: 0,
        interface: named(b"eth0"),
        source: peer(rng.next_u8()),
        port: 5353,
        payload: noise,
    });
    assert_eq!(
        unlinked.handle(&stray, &mut Believed),
        SessionStep::Finished
    );
}

/// Invariant 2, for links and questions that end.
fn exercise_decoder_edges(
    rng: &mut Prng,
    noise: &[u8],
    decoder: &mut Decoder,
    configure: &[u8],
    now: u64,
) {
    // A link going down and a question stopped: every frame about them after
    // is still taken.
    let down = encoded(&ToDecoder::Link {
        now,
        interface: named(b"wlan0"),
        up: false,
    });
    assert_eq!(decoder.handle(&down, &mut Believed), SessionStep::Continue);
    let stop = encoded(&ToDecoder::Stop { question: 1 });
    assert_eq!(decoder.handle(&stop, &mut Believed), SessionStep::Continue);
    assert_eq!(
        decoder.handle(&stop, &mut Believed),
        SessionStep::Finished,
        "a question stopped twice was never asked the second time"
    );
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.handle(configure, &mut Believed),
        SessionStep::Continue
    );
    for frame in [
        encoded(&ToDecoder::Link {
            now: 0,
            interface: named(b"eth0"),
            up: true,
        }),
        encoded(&ToDecoder::Datagram {
            now: 1,
            interface: named(b"eth0"),
            source: peer(rng.next_u8()),
            port: 5353,
            payload: noise,
        }),
    ] {
        assert_eq!(decoder.handle(&frame, &mut Believed), SessionStep::Continue);
    }
}

/// Invariant 2.
fn exercise_decoder(rng: &mut Prng, noise: &[u8]) {
    let configure = encoded(&ToDecoder::Configure {
        cache_key: &[rng.next_u8(); CACHE_KEY_LEN],
        rng_key: &[rng.next_u8(); RNG_KEY_LEN],
    });
    exercise_decoder_order(rng, noise, &configure);
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.handle(&configure, &mut Believed),
        SessionStep::Continue
    );
    for name in INTERFACES {
        let up = encoded(&ToDecoder::Link {
            now: 0,
            interface: named(name),
            up: true,
        });
        assert_eq!(decoder.handle(&up, &mut Believed), SessionStep::Continue);
    }
    let asks = [
        ("_ipp._tcp.local", Form::Instance),
        ("printer.local", Form::AddressV4),
        ("host.local", Form::AddressV6),
        ("_services._dns-sd._udp.local", Form::Type),
    ];
    for (question, (name, form)) in (0u32..).zip(asks) {
        let name = Name::encode(name).expect("fixed names encode");
        let ask = encoded(&ToDecoder::Ask {
            now: 0,
            question,
            form,
            name: name.as_wire(),
        });
        assert_eq!(decoder.handle(&ask, &mut Believed), SessionStep::Continue);
    }
    let mut now = 0u64;
    for round in 0..16u32 {
        // Now and then an instant earlier than one already seen.
        now = if rng.below(4) == 0 {
            rng.next_u64() % now.saturating_add(1)
        } else {
            now.saturating_add(u64::from(rng.next_u32()) * 1_000)
        };
        let announced = announcement(rng);
        for payload in [announced.as_slice(), noise] {
            let frame = encoded(&ToDecoder::Datagram {
                now,
                interface: iface(rng),
                source: peer(rng.next_u8()),
                port: [5353, rng.next_u16()][rng.below(2)],
                payload,
            });
            assert_eq!(
                decoder.handle(&frame, &mut Believed),
                SessionStep::Continue,
                "a datagram never ends a configured decoder"
            );
        }
        let tick = encoded(&ToDecoder::Tick { now });
        assert_eq!(decoder.handle(&tick, &mut Believed), SessionStep::Continue);
        let replay = encoded(&ToDecoder::Replay {
            question: round % 4,
            token: round,
        });
        assert_eq!(
            decoder.handle(&replay, &mut Believed),
            SessionStep::Continue
        );
    }
    exercise_decoder_edges(rng, noise, &mut decoder, &configure, now);
    // Arbitrary bytes as a frame: taken or refused, never a panic.
    let _ = decoder.handle(noise, &mut Believed);
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.handle(&configure, &mut Believed),
        SessionStep::Continue
    );
    assert_eq!(
        decoder.handle(&configure, &mut Believed),
        SessionStep::Finished,
        "a second configuration ends the session"
    );
}

/// One launched decoder as the harness drives it.
#[derive(Default)]
struct Wire {
    /// Every byte the front wrote to it.
    written: Vec<u8>,
    /// What it says next.
    says: Vec<u8>,
    /// Whether its stream ends once `says` is spent.
    ended: bool,
    /// Whether it has stopped reading.
    stalled: bool,
}

type Wires = Rc<RefCell<Vec<Wire>>>;

/// A decoder transport answering whatever its wire says — a compromised
/// decoder, as far as the front can tell.
struct Hostile {
    wires: Wires,
    index: usize,
}

impl SessionTransport for Hostile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut wires = self.wires.borrow_mut();
        let wire = &mut wires[self.index];
        if wire.says.is_empty() {
            return if wire.ended {
                Ok(0)
            } else {
                Err(Errno::WouldBlock)
            };
        }
        let take = buf.len().min(wire.says.len());
        buf[..take].copy_from_slice(&wire.says[..take]);
        wire.says.drain(..take);
        Ok(take)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        let mut wires = self.wires.borrow_mut();
        let wire = &mut wires[self.index];
        if wire.stalled {
            return Err(Errno::WouldBlock);
        }
        wire.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn descriptors(&self) -> Option<SessionDescriptors> {
        None
    }

    fn dispose(self) -> Option<i32> {
        None
    }
}

struct Launcher {
    wires: Wires,
}

impl SessionLauncher for Launcher {
    type Transport = Hostile;

    fn launch(&mut self) -> Result<Hostile, Errno> {
        let mut wires = self.wires.borrow_mut();
        wires.push(Wire::default());
        Ok(Hostile {
            wires: self.wires.clone(),
            index: wires.len() - 1,
        })
    }
}

type Under = Front<Launcher, Quiet, Keys>;

/// Tell the front every interface is up on the IPv4 socket.
fn links_up(front: &mut Under) {
    for name in INTERFACES {
        front.on_delivery(
            0,
            &SocketDelivery::Link(SocketLinkEvent {
                socket: V4,
                interface: named(name),
                up: true,
            }),
        );
    }
}

/// The frames the front wrote to one decoder, each required to decode.
fn frames(written: &[u8]) -> Vec<ToDecoder<'_>> {
    let mut out = Vec::new();
    let mut rest = written;
    while !rest.is_empty() {
        let (header, body) = rest.split_at(FRAME_HEADER_LEN);
        let declared = u32::from_le_bytes(header.try_into().expect("a whole header"));
        let (frame, tail) = body.split_at(usize::try_from(declared).expect("fits"));
        out.push(
            ToDecoder::decode(frame).expect("the front writes only frames a decoder believes"),
        );
        rest = tail;
    }
    out
}

fn delivery<'a>(rng: &mut Prng, last: u8, payload: &'a [u8]) -> SocketDatagram<'a> {
    let (family, addr) = address_parts(peer(last));
    SocketDatagram {
        socket: V4,
        interface: iface(rng),
        source: SocketAddr {
            family,
            addr,
            port: 5353,
        },
        source_on_link: true,
        payload,
    }
}

/// One turn of the owner's loop at `now`, holding the invariant that keeps
/// it from spinning.
fn wake(front: &mut Under, now: u64) {
    front.on_wake(now).expect("keys draw");
    if let Some(at) = front.wake_at() {
        assert!(at > now, "a wake at {at} after acting at {now} would spin");
    }
}

/// Every write turn the front asks for, into a decoder taking everything.
fn drain(front: &mut Under, now: u64) {
    for _ in 0..10_000 {
        if !front.wants_write() {
            return;
        }
        front.on_decoder_writable(now);
    }
    panic!("the queue never drained into a decoder reading everything");
}

/// Read turns until the front has taken everything its decoder said.
fn hear(front: &mut Under, wires: &Wires, now: u64) {
    for _ in 0..10_000 {
        let pending = wires
            .borrow()
            .last()
            .is_some_and(|wire| !wire.says.is_empty() || wire.ended);
        if !pending || !front.wants_read() {
            return;
        }
        front.on_decoder_readable(now);
    }
    panic!("the front stopped taking what its decoder said");
}

/// Start a replacement if the decoder is contained. Only called with the
/// queue drained, so a front not taking datagrams has no live decoder.
fn revive(front: &mut Under, now: &mut u64) {
    if front.wants_deliveries() {
        return;
    }
    let at = front
        .wake_at()
        .expect("a contained decoder's replacement is always due");
    *now = (*now).max(at);
    wake(front, *now);
    drain(front, *now);
    assert!(front.wants_deliveries(), "the replacement is live");
}

/// Load what the live decoder says in a round of `kind`; `true` when it is
/// something no decoder says, so the front must contain it.
fn say(wires: &Wires, kind: usize, rng: &mut Prng, noise: &[u8], now: u64) -> bool {
    let mut wires = wires.borrow_mut();
    let wire = wires.last_mut().expect("launched");
    let (said, condemned) = match kind {
        0 => {
            let at = rng.next_u64() % now.saturating_add(SEC);
            (
                framed(&from_decoder(&FromDecoder::Deadline(Some(at)))),
                false,
            )
        }
        1 => (
            framed(&from_decoder(&FromDecoder::Deadline(Some(rng.next_u64())))),
            false,
        ),
        2 => (framed(&from_decoder(&FromDecoder::Deadline(None))), false),
        3 => (framed(&[0xFF]), true),
        4 => (u32::MAX.to_le_bytes().to_vec(), true),
        // An answer to a question the front never asked.
        9 => (
            framed(&from_decoder(&FromDecoder::Answer(Entry::Answer {
                request: u32::MAX,
                interface: named(b"eth0"),
                change: Change::Added,
                ttl: 1,
                answer: Answer::Instance { label: b"x" },
            }))),
            true,
        ),
        // A datagram for the group: sent within budget once the link has
        // settled, never a reason to contain the decoder.
        10 => (
            framed(&from_decoder(&FromDecoder::Transmit {
                interface: named(b"eth0"),
                to: Destination::Group,
                payload: b"query",
            })),
            false,
        ),
        // An acknowledgement of a link it was never told of.
        11 => (
            framed(&from_decoder(&FromDecoder::Linked {
                interface: named(b"zz9"),
                up: true,
            })),
            true,
        ),
        // A whole frame of noise: believed or contained, and aligned either way.
        5 => (framed(&noise[..noise.len().min(64)]), false),
        6 => {
            wire.ended = true;
            (noise.to_vec(), true)
        }
        7 => {
            wire.ended = true;
            (Vec::new(), true)
        }
        _ => (Vec::new(), false),
    };
    wire.says.extend_from_slice(&said);
    condemned
}

fn set_stalled(wires: &Wires, stalled: bool) {
    wires.borrow_mut().last_mut().expect("launched").stalled = stalled;
}

/// Invariant 3.
fn exercise_front(rng: &mut Prng, noise: &[u8]) {
    let wires: Wires = Rc::default();
    let world = Keys::default();
    let mut front = Front::new(
        Launcher {
            wires: wires.clone(),
        },
        Quiet,
        world.clone(),
        Sockets {
            v4: Some(V4),
            v6: Some(V6),
        },
        Grants::default(),
    )
    .expect("the buffers commit");
    let mut now = 0u64;
    let mut last = 0u8;
    wake(&mut front, now);
    drain(&mut front, now);
    links_up(&mut front);
    drain(&mut front, now);

    for round in 0..2 * SAYINGS {
        revive(&mut front, &mut now);
        wake(&mut front, now);
        last = last.wrapping_add(1);
        front.on_delivery(now, &SocketDelivery::Datagram(delivery(rng, last, noise)));
        drain(&mut front, now);
        {
            let wires = wires.borrow();
            let written = frames(&wires.last().expect("launched").written);
            assert!(
                matches!(written.last(),
                    Some(ToDecoder::Datagram { now: at, payload, .. })
                        if *at == now && *payload == noise),
                "a live decoder is relayed to"
            );
        }
        let condemned = say(&wires, round % SAYINGS, rng, noise, now);
        hear(&mut front, &wires, now);
        if condemned {
            assert!(!front.wants_deliveries(), "the decoder was contained");
            assert!(front.wake_at().is_some(), "and its replacement is due");
        }
        now = now.saturating_add(u64::from(rng.next_u32() % 300) * MS);
    }

    owed_tick(rng, &mut front, &wires, &mut now, &mut last);
    check_written(&wires, &world);
}

/// Every decoder was configured first and once under fresh keys, ticked no
/// closer than the floor, and relayed to only from links it was told of; and
/// the front sent only to the groups.
fn check_written(wires: &Wires, world: &Keys) {
    let mut last_tick: Option<u64> = None;
    let mut last_key: Option<[u8; CACHE_KEY_LEN]> = None;
    for wire in wires.borrow().iter() {
        let written = frames(&wire.written);
        let Some(ToDecoder::Configure { cache_key, .. }) = written.first() else {
            panic!("every decoder is configured first");
        };
        assert_ne!(last_key, Some(**cache_key), "every decoder is keyed afresh");
        last_key = Some(**cache_key);
        let mut linked: Vec<[u8; IF_NAME_LEN]> = Vec::new();
        for frame in &written[1..] {
            match frame {
                ToDecoder::Configure { .. } => panic!("a decoder is configured once"),
                ToDecoder::Tick { now: at } => {
                    if let Some(previous) = last_tick {
                        assert!(*at >= previous + MIN_TICK_INTERVAL_NS, "ticks are spaced");
                    }
                    last_tick = Some(*at);
                }
                ToDecoder::Link {
                    interface,
                    up: true,
                    ..
                } => linked.push(*interface),
                ToDecoder::Link {
                    interface,
                    up: false,
                    ..
                } => linked.retain(|name| name != interface),
                ToDecoder::Datagram { interface, .. } => assert!(
                    linked.contains(interface),
                    "a datagram is relayed only from a link the decoder runs"
                ),
                ToDecoder::Ask { .. } | ToDecoder::Stop { .. } | ToDecoder::Replay { .. } => {}
            }
        }
    }
    // Whatever the decoders asked, the front sent only to the groups or to a
    // peer it had relayed from.
    for to in &world.0.borrow().sent {
        let group = [IpAddr::V4(GROUP_V4), IpAddr::V6(GROUP_V6)]
            .iter()
            .any(|group| address_parts(*group).1 == to.addr && to.port == 5353);
        assert!(group, "sent to {to:?}, which is not a group");
    }
}

/// A decoder that asks for time already come and then stops reading with its
/// queue full to the byte: the tick it is owed waits on room, and follows the
/// held datagram once there is some.
fn owed_tick(rng: &mut Prng, front: &mut Under, wires: &Wires, now: &mut u64, last: &mut u8) {
    revive(front, now);
    {
        let mut wires = wires.borrow_mut();
        let wire = wires.last_mut().expect("launched");
        wire.says
            .extend_from_slice(&framed(&from_decoder(&FromDecoder::Deadline(Some(*now)))));
    }
    hear(front, wires, *now);
    set_stalled(wires, true);
    let mut exact = vec![0u8; OUTBOUND_QUEUE / 32 - FRAME_HEADER_LEN - DATAGRAM_HEADER_LEN];
    rng.fill(&mut exact);
    let mut relayed = 0;
    while front.wants_deliveries() {
        *last = last.wrapping_add(1);
        front.on_delivery(
            *now,
            &SocketDelivery::Datagram(delivery(rng, *last, &exact)),
        );
        relayed += 1;
        assert!(relayed <= 33, "the queue holds exactly 32 and one held");
    }
    *now += MIN_TICK_INTERVAL_NS;
    wake(front, *now);
    set_stalled(wires, false);
    drain(front, *now);
    {
        let wires = wires.borrow();
        let written = frames(&wires.last().expect("launched").written);
        let tail = &written[written.len().saturating_sub(2)..];
        assert!(
            matches!(tail, [ToDecoder::Datagram { .. }, ToDecoder::Tick { now: at }] if *at == *now),
            "the owed tick followed the held datagram once the queue drained"
        );
    }
}

/// Invariant 4.
fn exercise_calls(rng: &mut Prng, noise: &[u8]) {
    let world = Keys::default();
    let mut front = Front::new(
        tairix_sandbox::loopback::LoopbackSessionLauncher::new(Decoder::new),
        Quiet,
        world,
        Sockets {
            v4: Some(V4),
            v6: None,
        },
        Grants::default(),
    )
    .expect("the buffers commit");
    front.on_wake(0).expect("keys draw");
    let mut summary = CapabilitySummary::EMPTY;
    for cap in [CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL] {
        if rng.next_u64() & 1 == 0 {
            summary.insert(cap);
        }
    }
    let who = Origin::new(
        TrustDomain::User,
        1000,
        100,
        7,
        ProcId::from_raw([rng.next_u8(); 16]),
        summary,
        tairix_abi::ORIGIN_CONSOLE_NONE,
    );
    let service = ServiceTypeField {
        name: b"ipp",
        transport: Transport::Tcp,
    };
    let mut reply = vec![0u8; DISCOVERY_MAX_REPLY];
    let mut session = 0;
    for step in 0..6 {
        let call = match step {
            0 => DiscoveryRequest::Open { deliver_port: 9 },
            1 => DiscoveryRequest::Start {
                session,
                query: Query::Browse { service },
            },
            2 => DiscoveryRequest::Start {
                session,
                query: Query::Types,
            },
            3 => DiscoveryRequest::Collect {
                session,
                capacity: rng.next_u32() % 16_384,
            },
            4 => DiscoveryRequest::Stop {
                session,
                request: rng.next_u32() % 4,
            },
            _ => DiscoveryRequest::Close { session },
        };
        let mut bytes = vec![0u8; 512];
        let len = call.encode(&mut bytes).expect("encodes");
        let len = front.serve(0, &who, &bytes[..len], &mut reply);
        let reply = &reply[..len];
        assert!(
            decode_status_reply(reply).is_err()
                || reply.len() == ID_REPLY_LEN
                || reply.len() == tairix_abi::reply::STATUS_REPLY_LEN
                || CollectReply::parse(reply).is_ok(),
            "a reply the client reads"
        );
        if step == 0 {
            if let Ok(id) = tairix_abi::discovery_ipc::decode_id_reply(reply) {
                session = id;
            }
        }
    }
    // And bytes that are no call at all.
    let len = front.serve(0, &who, noise, &mut reply);
    assert!(len > 0, "even noise is answered, with its refusal");
}

#[test]
fn hostile_frames_and_decoders_never_break_the_front() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "hostile_frames_and_decoders_never_break_the_front",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut noise = [0u8; 700];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = rng.below(noise.len() + 1);
            rng.fill(&mut noise[..size]);
            exercise_codecs(&mut rng, &noise[..size]);
            exercise_decoder_codec(&mut rng, &noise[..size]);
            exercise_decoder(&mut rng, &noise[..size]);
            exercise_front(&mut rng, &noise[..size]);
            exercise_calls(&mut rng, &noise[..size]);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
