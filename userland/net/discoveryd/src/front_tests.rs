//! Unit tests for the front, over in-process decoder workers: a recording
//! one, a doomed one, one that lies, and — end to end — the real decoder.

use super::{Entropy, Fatal, Front, MIN_TICK_INTERVAL_NS, OUTBOUND_QUEUE, RELAY_SOURCE_BURST};
use crate::decoder::Decoder;
use crate::events::DECODER_STARTED;
use crate::wire::{FromDecoder, ToDecoder, DATAGRAM_HEADER_LEN};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::net::{SocketAddr, SocketDatagram};
use tairix_abi::net_ipc::{NetAddrFamily, IF_NAME_LEN};
use tairix_abi::Errno;
use tairix_log::{Event, EventId, Sink};
use tairix_net::dns::Name;
use tairix_net::mdns::{MessageWriter, RData, Record, Section};
use tairix_net::Ipv4Addr;
use tairix_sandbox::host::EVENT_WORKER_CRASHED;
use tairix_sandbox::loopback::{LoopbackSession, LoopbackSessionLauncher};
use tairix_sandbox::proto::FRAME_HEADER_LEN;
use tairix_sandbox::session::{
    FrameOut, SessionDescriptors, SessionService, SessionStep, SessionTransport,
};
use tairix_sandbox::supervise::SessionLauncher;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;

/// Captures the id of every logged event.
#[derive(Clone, Default)]
struct RecordingSink {
    ids: Rc<RefCell<Vec<EventId>>>,
}

impl RecordingSink {
    fn count(&self, id: EventId) -> usize {
        self.ids.borrow().iter().filter(|seen| **seen == id).count()
    }
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.ids.borrow_mut().push(event.id);
    }
}

/// A deterministic stand-in for the kernel CSPRNG: each draw is a fresh
/// byte pattern, so two decoders' keys differ.
#[derive(Default)]
struct Counter(u8);

impl Entropy for Counter {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno> {
        self.0 = self.0.wrapping_add(1);
        out.fill(self.0);
        Ok(())
    }
}

/// A random source that has died.
struct Dead;

impl Entropy for Dead {
    fn fill(&mut self, _out: &mut [u8]) -> Result<(), Errno> {
        Err(Errno::EntropyNotReady)
    }
}

/// How a recording worker answers a relayed datagram.
#[derive(Clone, Copy)]
enum Answer {
    Nothing,
    Deadline(Option<u64>),
    Garbage,
}

/// A worker that records every frame and answers ticks with "no deadline".
struct Recorder {
    log: Rc<RefCell<Vec<Vec<u8>>>>,
    answer: Answer,
}

impl SessionService for Recorder {
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
        self.log.borrow_mut().push(request.to_vec());
        match ToDecoder::decode(request) {
            Ok(ToDecoder::Tick { .. }) => {
                let _ = out.frame(&FromDecoder::Deadline(None).encode());
            }
            Ok(ToDecoder::Datagram { .. }) => match self.answer {
                Answer::Nothing => {}
                Answer::Deadline(at) => {
                    let _ = out.frame(&FromDecoder::Deadline(at).encode());
                }
                Answer::Garbage => {
                    let _ = out.frame(b"\x09nonsense");
                }
            },
            _ => {}
        }
        SessionStep::Continue
    }
}

/// A launched worker: a recording one, or one whose transport has died.
enum Worker {
    Healthy(LoopbackSession<Recorder>),
    Doomed,
}

impl SessionTransport for Worker {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        match self {
            Self::Healthy(session) => session.read(buf),
            Self::Doomed => Err(Errno::BrokenPipe),
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        match self {
            Self::Healthy(session) => session.write(buf),
            Self::Doomed => Ok(buf.len()),
        }
    }

    fn descriptors(&self) -> Option<SessionDescriptors> {
        None
    }

    fn dispose(self) -> Option<i32> {
        None
    }
}

/// Launches recording workers, except where the script says a launch is
/// doomed.
struct Workers {
    doomed: VecDeque<bool>,
    log: Rc<RefCell<Vec<Vec<u8>>>>,
    launches: Rc<RefCell<usize>>,
    answer: Answer,
}

impl SessionLauncher for Workers {
    type Transport = Worker;

    fn launch(&mut self) -> Result<Worker, Errno> {
        *self.launches.borrow_mut() += 1;
        if self.doomed.pop_front().unwrap_or(false) {
            return Ok(Worker::Doomed);
        }
        Ok(Worker::Healthy(LoopbackSession::new(Recorder {
            log: self.log.clone(),
            answer: self.answer,
        })))
    }
}

struct Harness<E: Entropy> {
    front: Front<Workers, RecordingSink, E>,
    sink: RecordingSink,
    log: Rc<RefCell<Vec<Vec<u8>>>>,
    launches: Rc<RefCell<usize>>,
}

fn harness_with<E: Entropy>(entropy: E, doomed: &[bool], answer: Answer) -> Harness<E> {
    let sink = RecordingSink::default();
    let log = Rc::new(RefCell::new(Vec::new()));
    let launches = Rc::new(RefCell::new(0));
    let workers = Workers {
        doomed: doomed.iter().copied().collect(),
        log: log.clone(),
        launches: launches.clone(),
        answer,
    };
    Harness {
        front: Front::new(workers, sink.clone(), entropy).expect("the relay buffer commits"),
        sink,
        log,
        launches,
    }
}

fn harness(answer: Answer) -> Harness<Counter> {
    harness_with(Counter::default(), &[], answer)
}

/// Start the first decoder at `now` and flush its configuration.
fn started<E: Entropy>(h: &mut Harness<E>, now: u64) {
    h.front.on_wake(now).expect("keys draw");
    flush(h, now);
}

/// One turn of each direction, as a wait-set would give.
fn flush<E: Entropy>(h: &mut Harness<E>, now: u64) {
    while h.front.wants_write() {
        h.front.on_decoder_writable(now);
    }
    if h.front.wants_read() {
        h.front.on_decoder_readable(now);
    }
}

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn from(last: u8, on_link: bool, payload: &[u8]) -> SocketDatagram<'_> {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&[192, 168, 1, last]);
    SocketDatagram {
        socket: 1,
        interface: iface(b"eth0"),
        source: SocketAddr {
            family: NetAddrFamily::V4,
            addr,
            port: 5353,
        },
        source_on_link: on_link,
        payload,
    }
}

/// The frames the workers saw, decoded.
fn seen(log: &Rc<RefCell<Vec<Vec<u8>>>>) -> Vec<Vec<u8>> {
    log.borrow().clone()
}

fn relayed_payloads(log: &Rc<RefCell<Vec<Vec<u8>>>>) -> Vec<Vec<u8>> {
    seen(log)
        .iter()
        .filter_map(|frame| match ToDecoder::decode(frame) {
            Ok(ToDecoder::Datagram { payload, .. }) => Some(payload.to_vec()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_first_decoder_is_due_at_once_and_is_keyed_before_anything_else() {
    let mut h = harness(Answer::Nothing);
    assert_eq!(h.front.wake_at(), Some(0));
    started(&mut h, 0);
    let frames = seen(&h.log);
    match ToDecoder::decode(&frames[0]) {
        Ok(ToDecoder::Configure { cache_key, rng_key }) => {
            assert!(cache_key.iter().all(|byte| *byte == 1));
            assert!(rng_key.iter().all(|byte| *byte == 2));
        }
        other => panic!("the first frame configures, not {other:?}"),
    }
    assert_eq!(h.sink.count(DECODER_STARTED), 1);
    assert_eq!(
        h.front.wake_at(),
        None,
        "a live, quiet decoder needs no wake"
    );
}

#[test]
fn an_on_link_datagram_is_relayed_unread_and_an_off_link_one_never() {
    let mut h = harness(Answer::Nothing);
    started(&mut h, 0);
    h.front
        .on_datagram(MS, &from(9, true, b"\x00\x01anything at all"));
    h.front.on_datagram(MS, &from(10, false, b"off link"));
    flush(&mut h, MS);
    let frames = seen(&h.log);
    assert_eq!(frames.len(), 2);
    match ToDecoder::decode(&frames[1]) {
        Ok(ToDecoder::Datagram {
            now,
            interface,
            port,
            payload,
            ..
        }) => {
            assert_eq!(now, MS);
            assert_eq!(interface, iface(b"eth0"));
            assert_eq!(port, 5353);
            assert_eq!(payload, b"\x00\x01anything at all");
        }
        other => panic!("a relayed datagram, not {other:?}"),
    }
}

#[test]
fn a_flooding_sender_is_held_to_its_budget_and_a_neighbour_still_relayed() {
    let mut h = harness(Answer::Nothing);
    started(&mut h, 0);
    for _ in 0..500 {
        h.front.on_datagram(MS, &from(9, true, b"flood"));
        flush(&mut h, MS);
    }
    h.front.on_datagram(MS, &from(10, true, b"neighbour"));
    flush(&mut h, MS);
    let payloads = relayed_payloads(&h.log);
    let flood = payloads.iter().filter(|p| p.as_slice() == b"flood").count();
    assert_eq!(flood, RELAY_SOURCE_BURST as usize);
    assert_eq!(payloads.last().map(Vec::as_slice), Some(&b"neighbour"[..]));
}

#[test]
fn a_full_queue_holds_one_datagram_and_stops_the_drain_until_it_has_room() {
    let mut h = harness(Answer::Nothing);
    started(&mut h, 0);
    let large = vec![0xC3u8; 8_000];
    let mut accepted = 0usize;
    // Relay without flushing until the queue refuses one.
    while h.front.wants_datagrams() {
        h.front.on_datagram(MS, &from(9, true, &large));
        accepted += 1;
        assert!(accepted < 64, "the queue is bounded");
    }
    // While one is held nothing further is taken.
    h.front.on_datagram(MS, &from(9, true, b"ignored"));
    flush(&mut h, MS);
    assert!(h.front.wants_datagrams(), "room made, the drain resumes");
    let payloads = relayed_payloads(&h.log);
    assert_eq!(payloads.len(), accepted, "the held datagram arrived too");
    assert!(payloads.iter().all(|p| p.as_slice() != b"ignored"));
}

#[test]
fn a_deadline_is_answered_by_one_tick_no_sooner_than_the_floor() {
    let mut h = harness(Answer::Deadline(Some(5 * MS)));
    started(&mut h, 0);
    h.front.on_datagram(MS, &from(9, true, b"x"));
    flush(&mut h, MS);
    // The decoder wants time at 5 ms; the floor from the (never sent) last
    // tick moves that to 10 ms.
    assert_eq!(h.front.wake_at(), Some(MIN_TICK_INTERVAL_NS));
    h.front.on_wake(5 * MS).expect("no start due");
    flush(&mut h, 5 * MS);
    assert!(!seen(&h.log)
        .iter()
        .any(|frame| matches!(ToDecoder::decode(frame), Ok(ToDecoder::Tick { .. }))));
    h.front.on_wake(MIN_TICK_INTERVAL_NS).expect("no start due");
    // One tick outstanding: no further wake is asked for until it is answered.
    assert_eq!(h.front.wake_at(), None);
    flush(&mut h, MIN_TICK_INTERVAL_NS);
    let ticks: Vec<u64> = seen(&h.log)
        .iter()
        .filter_map(|frame| match ToDecoder::decode(frame) {
            Ok(ToDecoder::Tick { now }) => Some(now),
            _ => None,
        })
        .collect();
    assert_eq!(ticks, [MIN_TICK_INTERVAL_NS]);
    // Answered with "no deadline": nothing further is due.
    assert_eq!(h.front.wake_at(), None);
}

#[test]
fn a_tick_with_no_room_waits_for_the_queue_to_drain_rather_than_a_timer() {
    let mut h = harness(Answer::Deadline(Some(5 * MS)));
    started(&mut h, 0);
    h.front.on_datagram(MS, &from(9, true, b"x"));
    flush(&mut h, MS);
    // Frames that fill the queue exactly, so not even a tick fits beside them.
    let exact = vec![0x3Cu8; OUTBOUND_QUEUE / 32 - FRAME_HEADER_LEN - DATAGRAM_HEADER_LEN];
    while h.front.wants_datagrams() {
        h.front.on_datagram(MS, &from(10, true, &exact));
    }
    let now = 20 * MS;
    h.front.on_wake(now).expect("no start due");
    assert_eq!(h.front.wake_at(), None, "an owed tick is not a timer");
    flush(&mut h, now);
    let frames = seen(&h.log);
    let tick = frames
        .iter()
        .position(|frame| ToDecoder::decode(frame) == Ok(ToDecoder::Tick { now }))
        .expect("the owed tick was sent once the queue drained");
    let last_datagram = frames
        .iter()
        .rposition(|frame| matches!(ToDecoder::decode(frame), Ok(ToDecoder::Datagram { .. })))
        .expect("datagrams were relayed");
    assert!(tick > last_datagram, "the held datagram went first");
}

#[test]
fn a_crashed_decoder_is_forgotten_and_replaced_after_the_pace_with_fresh_keys() {
    let mut h = harness_with(Counter::default(), &[true], Answer::Nothing);
    h.front.on_wake(0).expect("keys draw");
    h.front.on_decoder_writable(0);
    // The doomed worker's first read is its death.
    h.front.on_decoder_readable(10 * MS);
    assert_eq!(h.sink.count(EVENT_WORKER_CRASHED), 1);
    assert!(!h.front.wants_datagrams(), "nothing is relayed to no one");
    let due = h.front.wake_at().expect("a replacement is scheduled");
    assert!(due > 10 * MS, "the replacement is paced, not immediate");
    h.front.on_wake(due - 1).expect("keys draw");
    assert_eq!(*h.launches.borrow(), 1, "not before its time");
    h.front.on_wake(due).expect("keys draw");
    flush(&mut h, due);
    assert_eq!(*h.launches.borrow(), 2);
    assert_eq!(h.sink.count(DECODER_STARTED), 2);
    // The replacement is keyed afresh, never with its predecessor's keys.
    match ToDecoder::decode(&seen(&h.log)[0]) {
        Ok(ToDecoder::Configure { cache_key, .. }) => {
            assert!(cache_key.iter().all(|byte| *byte == 3));
        }
        other => panic!("the replacement is configured first, not {other:?}"),
    }
    assert!(h.front.wants_datagrams());
}

#[test]
fn a_decoder_that_sends_a_frame_no_decoder_sends_is_condemned() {
    let mut h = harness(Answer::Garbage);
    started(&mut h, 0);
    h.front.on_datagram(MS, &from(9, true, b"x"));
    flush(&mut h, MS);
    assert_eq!(h.sink.count(EVENT_WORKER_CRASHED), 1);
    assert!(!h.front.wants_datagrams());
    assert!(h.front.wake_at().is_some_and(|at| at > MS));
}

#[test]
fn without_entropy_no_decoder_is_started_and_the_front_stops() {
    let mut h = harness_with(Dead, &[], Answer::Nothing);
    assert_eq!(
        h.front.on_wake(0),
        Err(Fatal::Entropy(Errno::EntropyNotReady))
    );
    assert_eq!(*h.launches.borrow(), 0);
    assert!(!h.front.wants_datagrams());
}

#[test]
fn before_any_decoder_is_live_nothing_is_relayed() {
    let mut h = harness(Answer::Nothing);
    h.front.on_datagram(MS, &from(9, true, b"early"));
    assert!(!h.front.wants_datagrams());
    started(&mut h, 2 * MS);
    assert!(relayed_payloads(&h.log).is_empty());
}

/// A multicast DNS response announcing one host address with `ttl`.
fn announcement(ttl: u32) -> Vec<u8> {
    let mut out = vec![0u8; 512];
    let mut writer = MessageWriter::new(&mut out, 0, true).expect("room for a header");
    let mut record = Record::unique(
        Name::encode("printer.local").expect("a host name"),
        RData::A(Ipv4Addr::new(10, 0, 0, 5)),
    );
    record.ttl = ttl;
    assert!(writer.push_record(Section::Answer, &record));
    let len = writer.finish();
    out.truncate(len);
    out
}

#[test]
fn end_to_end_the_real_decoder_caches_what_the_front_relays_and_expires_it_on_a_tick() {
    let sink = RecordingSink::default();
    let mut front = Front::new(
        LoopbackSessionLauncher::new(Decoder::new),
        sink.clone(),
        Counter::default(),
    )
    .expect("the relay buffer commits");
    let turn = |front: &mut Front<_, _, _>, now: u64| {
        while front.wants_write() {
            front.on_decoder_writable(now);
        }
        if front.wants_read() {
            front.on_decoder_readable(now);
        }
    };
    front.on_wake(0).expect("keys draw");
    turn(&mut front, 0);

    let announced = announcement(2);
    front.on_datagram(SEC, &from(9, true, &announced));
    turn(&mut front, SEC);
    // The decoder cached the record and asked for time at its expiry.
    assert_eq!(front.wake_at(), Some(3 * SEC));

    front.on_wake(3 * SEC).expect("no start due");
    turn(&mut front, 3 * SEC);
    // The tick expired it, and nothing further is due.
    assert_eq!(front.wake_at(), None);
    assert_eq!(sink.count(EVENT_WORKER_CRASHED), 0);
}
