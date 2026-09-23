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
//!    every decoder first and once under fresh keys, spaces its ticks,
//!    contains a decoder that says what no decoder says, relays again once
//!    the replacement starts, and delivers a tick owed while its queue was
//!    full once the queue drains.
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

use tairix_abi::net::{SocketAddr, SocketDatagram};
use tairix_abi::net_ipc::{address_parts, IF_NAME_LEN};
use tairix_abi::Errno;
use tairix_discoveryd::decoder::Decoder;
use tairix_discoveryd::front::{Entropy, Front, MIN_TICK_INTERVAL_NS, OUTBOUND_QUEUE};
use tairix_discoveryd::wire::{
    FromDecoder, ToDecoder, CACHE_KEY_LEN, DATAGRAM_HEADER_LEN, DEADLINE_LEN, MAX_TO_DECODER,
    RNG_KEY_LEN,
};
use tairix_fuzzseed::Prng;
use tairix_log::{Event, Sink};
use tairix_net::dns::Name;
use tairix_net::mdns::{MessageWriter, RData, Record, Section};
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
const SAYINGS: usize = 9;

#[derive(Clone, Copy)]
struct Quiet;

impl Sink for Quiet {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// A stand-in CSPRNG: each draw a fresh byte pattern, so generations differ.
struct Keys(u8);

impl Entropy for Keys {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno> {
        self.0 = self.0.wrapping_add(1);
        out.fill(self.0);
        Ok(())
    }
}

fn iface(rng: &mut Prng) -> [u8; IF_NAME_LEN] {
    let names: [&[u8]; 3] = [b"eth0", b"wlan0", b"bond1"];
    let name = rng.pick(&names);
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
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
            frame.encode().as_slice(),
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
    for shape in [
        FromDecoder::Deadline(None),
        FromDecoder::Deadline(Some(rng.next_u64())),
    ] {
        let mut bytes = shape.encode();
        assert_eq!(FromDecoder::decode(&bytes), Ok(shape));
        bytes[rng.below(DEADLINE_LEN)] ^= 1 << rng.below(8);
        if let Ok(mutated) = FromDecoder::decode(&bytes) {
            assert_eq!(
                mutated.encode(),
                bytes,
                "a mutated frame is refused or canonical"
            );
        }
    }
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

/// Invariant 2.
fn exercise_decoder(rng: &mut Prng, noise: &[u8]) {
    let configure = encoded(&ToDecoder::Configure {
        cache_key: &[rng.next_u8(); CACHE_KEY_LEN],
        rng_key: &[rng.next_u8(); RNG_KEY_LEN],
    });
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

    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.handle(&configure, &mut Believed),
        SessionStep::Continue
    );
    let mut now = 0u64;
    for _ in 0..16 {
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
    }
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
        socket: 1,
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
    if front.wants_datagrams() {
        return;
    }
    let at = front
        .wake_at()
        .expect("a contained decoder's replacement is always due");
    *now = (*now).max(at);
    wake(front, *now);
    drain(front, *now);
    assert!(front.wants_datagrams(), "the replacement is live");
}

/// Load what the live decoder says in a round of `kind`; `true` when it is
/// something no decoder says, so the front must contain it.
fn say(wires: &Wires, kind: usize, rng: &mut Prng, noise: &[u8], now: u64) -> bool {
    let mut wires = wires.borrow_mut();
    let wire = wires.last_mut().expect("launched");
    let (said, condemned) = match kind {
        0 => {
            let at = rng.next_u64() % now.saturating_add(SEC);
            (framed(&FromDecoder::Deadline(Some(at)).encode()), false)
        }
        1 => (
            framed(&FromDecoder::Deadline(Some(rng.next_u64())).encode()),
            false,
        ),
        2 => (framed(&FromDecoder::Deadline(None).encode()), false),
        3 => (framed(&[0xFF]), true),
        4 => (u32::MAX.to_le_bytes().to_vec(), true),
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
    let mut front = Front::new(
        Launcher {
            wires: wires.clone(),
        },
        Quiet,
        Keys(0),
    )
    .expect("the relay buffer commits");
    let mut now = 0u64;
    let mut last = 0u8;
    wake(&mut front, now);
    drain(&mut front, now);

    for round in 0..2 * SAYINGS {
        revive(&mut front, &mut now);
        wake(&mut front, now);
        last = last.wrapping_add(1);
        front.on_datagram(now, &delivery(rng, last, noise));
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
            assert!(!front.wants_datagrams(), "the decoder was contained");
            assert!(front.wake_at().is_some(), "and its replacement is due");
        }
        now = now.saturating_add(u64::from(rng.next_u32() % 300) * MS);
    }

    // A decoder that asks for time already come and then stops reading
    // with its queue full to the byte: the tick it is owed waits on room.
    revive(&mut front, &mut now);
    {
        let mut wires = wires.borrow_mut();
        let wire = wires.last_mut().expect("launched");
        wire.says
            .extend_from_slice(&framed(&FromDecoder::Deadline(Some(now)).encode()));
    }
    hear(&mut front, &wires, now);
    set_stalled(&wires, true);
    let mut exact = vec![0u8; OUTBOUND_QUEUE / 32 - FRAME_HEADER_LEN - DATAGRAM_HEADER_LEN];
    rng.fill(&mut exact);
    let mut relayed = 0;
    while front.wants_datagrams() {
        last = last.wrapping_add(1);
        front.on_datagram(now, &delivery(rng, last, &exact));
        relayed += 1;
        assert!(relayed <= 33, "the queue holds exactly 32 and one held");
    }
    now += MIN_TICK_INTERVAL_NS;
    wake(&mut front, now);
    set_stalled(&wires, false);
    drain(&mut front, now);
    {
        let wires = wires.borrow();
        let written = frames(&wires.last().expect("launched").written);
        let tail = &written[written.len().saturating_sub(2)..];
        assert!(
            matches!(tail, [ToDecoder::Datagram { .. }, ToDecoder::Tick { now: at }] if *at == now),
            "the owed tick followed the held datagram once the queue drained"
        );
    }

    let mut last_tick: Option<u64> = None;
    let mut last_key: Option<[u8; CACHE_KEY_LEN]> = None;
    for wire in wires.borrow().iter() {
        let written = frames(&wire.written);
        let Some(ToDecoder::Configure { cache_key, .. }) = written.first() else {
            panic!("every decoder is configured first");
        };
        assert_ne!(last_key, Some(**cache_key), "every decoder is keyed afresh");
        last_key = Some(**cache_key);
        for frame in &written[1..] {
            match frame {
                ToDecoder::Configure { .. } => panic!("a decoder is configured once"),
                ToDecoder::Tick { now: at } => {
                    if let Some(previous) = last_tick {
                        assert!(*at >= previous + MIN_TICK_INTERVAL_NS, "ticks are spaced");
                    }
                    last_tick = Some(*at);
                }
                ToDecoder::Datagram { .. } => {}
            }
        }
    }
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
            exercise_decoder(&mut rng, &noise[..size]);
            exercise_front(&mut rng, &noise[..size]);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
