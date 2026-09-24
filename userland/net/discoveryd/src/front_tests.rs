//! Unit tests for the front, over in-process decoder workers: a scripted
//! one, a doomed one, one that lies, and — end to end — the real decoder.

use super::{
    Fatal, Front, Host, Sockets, ASKER_WINDOW_NS, MIN_TICK_INTERVAL_NS, OUTBOUND_QUEUE,
    RELAY_SOURCE_BURST,
};
use crate::decoder::Decoder;
use crate::events::{DECODER_STARTED, REQUEST_DENIED};
use crate::grants::Grants;
use crate::wire::{FromDecoder, ToDecoder, DATAGRAM_HEADER_LEN, MAX_FROM_DECODER};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::discovery_ipc::{
    decode_doorbell, decode_id_reply, Answer, Change, CollectReply, DiscoveryRequest, Entry,
    Families, Query, ServiceTypeField, Transport, DISCOVERY_MAX_REPLY,
};
use tairix_abi::net::{SocketAddr, SocketDatagram, SocketDelivery, SocketLinkEvent};
use tairix_abi::net_ipc::{NetAddrFamily, IF_NAME_LEN};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::{
    AppIdentity, CapabilityId, CapabilitySummary, Errno, Origin, ProcId, PublisherId, TrustDomain,
};
use tairix_log::{Event, EventId, Sink};
use tairix_net::dns::Name;
use tairix_net::mdns::{Destination, MessageWriter, RData, Record, Section};
use tairix_net::{IpAddr, Ipv4Addr};
use tairix_sandbox::host::EVENT_WORKER_CRASHED;
use tairix_sandbox::loopback::{LoopbackSession, LoopbackSessionLauncher};
use tairix_sandbox::proto::FRAME_HEADER_LEN;
use tairix_sandbox::session::{
    FrameOut, SessionDescriptors, SessionService, SessionStep, SessionTransport,
};
use tairix_sandbox::supervise::SessionLauncher;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const V4: u32 = 1;
const V6: u32 = 2;
const PORT: u64 = 0x77;

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

/// What the front did to the world, and how the world answers it.
#[derive(Default)]
struct World {
    draws: u8,
    entropy_dead: bool,
    sent: Vec<([u8; IF_NAME_LEN], SocketAddr, Vec<u8>)>,
    rings: Vec<(u64, u32)>,
    ring_full: bool,
    watched: Vec<ProcId>,
}

/// The front's view of the world, shared with the test.
#[derive(Clone, Default)]
struct FakeHost(Rc<RefCell<World>>);

impl Host for FakeHost {
    fn fill_random(&mut self, out: &mut [u8]) -> Result<(), Errno> {
        let mut world = self.0.borrow_mut();
        if world.entropy_dead {
            return Err(Errno::EntropyNotReady);
        }
        world.draws = world.draws.wrapping_add(1);
        out.fill(world.draws);
        Ok(())
    }

    fn transmit(
        &mut self,
        interface: [u8; IF_NAME_LEN],
        to: SocketAddr,
        payload: &[u8],
    ) -> Result<(), Errno> {
        self.0
            .borrow_mut()
            .sent
            .push((interface, to, payload.to_vec()));
        Ok(())
    }

    fn ring(&mut self, port: u64, doorbell: &[u8]) -> Result<(), Errno> {
        let mut world = self.0.borrow_mut();
        if world.ring_full {
            return Err(Errno::WouldBlock);
        }
        let session = decode_doorbell(doorbell).expect("a doorbell");
        world.rings.push((port, session));
        Ok(())
    }

    fn watch(&mut self, peer: ProcId) -> Result<(), Errno> {
        self.0.borrow_mut().watched.push(peer);
        Ok(())
    }
}

/// How a scripted worker answers a relayed datagram.
#[derive(Clone, Copy)]
enum Script {
    Nothing,
    Deadline(Option<u64>),
    Garbage,
    /// An answer to a question no front ever asked.
    Unasked,
    /// A datagram straight back to this peer.
    Reply(IpAddr, u16),
}

/// A worker that records every frame, acknowledges every link as a decoder
/// does, and answers ticks with "no deadline".
struct Scripted {
    log: Rc<RefCell<Vec<Vec<u8>>>>,
    script: Script,
}

fn encoded(frame: &FromDecoder<'_>) -> Vec<u8> {
    let mut out = vec![0u8; MAX_FROM_DECODER];
    let len = frame.encode(&mut out).expect("fits");
    out.truncate(len);
    out
}

impl SessionService for Scripted {
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
        self.log.borrow_mut().push(request.to_vec());
        match ToDecoder::decode(request) {
            Ok(ToDecoder::Tick { .. }) => {
                let _ = out.frame(&encoded(&FromDecoder::Deadline(None)));
            }
            Ok(ToDecoder::Link { interface, up, .. }) => {
                let _ = out.frame(&encoded(&FromDecoder::Linked { interface, up }));
            }
            Ok(ToDecoder::Datagram { interface, .. }) => match self.script {
                Script::Nothing => {}
                Script::Deadline(at) => {
                    let _ = out.frame(&encoded(&FromDecoder::Deadline(at)));
                }
                Script::Garbage => {
                    let _ = out.frame(b"\x09nonsense");
                }
                Script::Unasked => {
                    let _ = out.frame(&encoded(&FromDecoder::Answer(Entry::Answer {
                        request: 999,
                        interface,
                        change: Change::Added,
                        ttl: 1,
                        answer: Answer::Instance { label: b"x" },
                    })));
                }
                Script::Reply(addr, port) => {
                    let _ = out.frame(&encoded(&FromDecoder::Transmit {
                        interface,
                        to: Destination::Peer { addr, port },
                        payload: b"reply",
                    }));
                }
            },
            _ => {}
        }
        SessionStep::Continue
    }
}

/// A launched worker: a scripted one, or one whose transport has died.
enum Worker {
    Healthy(LoopbackSession<Scripted>),
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

/// Launches scripted workers, except where the script says a launch is
/// doomed.
struct Workers {
    doomed: VecDeque<bool>,
    log: Rc<RefCell<Vec<Vec<u8>>>>,
    launches: Rc<RefCell<usize>>,
    script: Script,
}

impl SessionLauncher for Workers {
    type Transport = Worker;

    fn launch(&mut self) -> Result<Worker, Errno> {
        *self.launches.borrow_mut() += 1;
        if self.doomed.pop_front().unwrap_or(false) {
            return Ok(Worker::Doomed);
        }
        Ok(Worker::Healthy(LoopbackSession::new(Scripted {
            log: self.log.clone(),
            script: self.script,
        })))
    }
}

struct Harness {
    front: Front<Workers, RecordingSink, FakeHost>,
    sink: RecordingSink,
    world: FakeHost,
    log: Rc<RefCell<Vec<Vec<u8>>>>,
    launches: Rc<RefCell<usize>>,
}

fn sockets() -> Sockets {
    Sockets {
        v4: Some(V4),
        v6: Some(V6),
    }
}

fn harness_with(world: FakeHost, doomed: &[bool], script: Script) -> Harness {
    let sink = RecordingSink::default();
    let log = Rc::new(RefCell::new(Vec::new()));
    let launches = Rc::new(RefCell::new(0));
    let workers = Workers {
        doomed: doomed.iter().copied().collect(),
        log: log.clone(),
        launches: launches.clone(),
        script,
    };
    Harness {
        front: Front::new(
            workers,
            sink.clone(),
            world.clone(),
            sockets(),
            Grants::default(),
        )
        .expect("the buffers commit"),
        sink,
        world,
        log,
        launches,
    }
}

fn harness(script: Script) -> Harness {
    harness_with(FakeHost::default(), &[], script)
}

/// One turn of each direction, as a wait-set would give.
fn turn<L: SessionLauncher>(front: &mut Front<L, RecordingSink, FakeHost>, now: u64) {
    for _ in 0..8 {
        while front.wants_write() {
            front.on_decoder_writable(now);
        }
        if !front.wants_read() {
            break;
        }
        front.on_decoder_readable(now);
    }
}

/// Start the first decoder at `now`, flush its configuration, and bring
/// `eth0` up on the IPv4 socket.
fn started(h: &mut Harness, now: u64) {
    h.front.on_wake(now).expect("keys draw");
    turn(&mut h.front, now);
    link(&mut h.front, now, V4, b"eth0", true);
}

fn link<L: SessionLauncher>(
    front: &mut Front<L, RecordingSink, FakeHost>,
    now: u64,
    socket: u32,
    name: &[u8],
    up: bool,
) {
    front.on_delivery(
        now,
        &SocketDelivery::Link(SocketLinkEvent {
            socket,
            interface: iface(name),
            up,
        }),
    );
    turn(front, now);
}

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

fn from(last: u8, on_link: bool, payload: &[u8]) -> SocketDatagram<'_> {
    from_on(b"eth0", last, on_link, payload)
}

fn from_on<'a>(name: &[u8], last: u8, on_link: bool, payload: &'a [u8]) -> SocketDatagram<'a> {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&[192, 168, 1, last]);
    SocketDatagram {
        socket: V4,
        interface: iface(name),
        source: SocketAddr {
            family: NetAddrFamily::V4,
            addr,
            port: 5353,
        },
        source_on_link: on_link,
        payload,
    }
}

fn deliver<L: SessionLauncher>(
    front: &mut Front<L, RecordingSink, FakeHost>,
    now: u64,
    datagram: &SocketDatagram<'_>,
) {
    front.on_delivery(now, &SocketDelivery::Datagram(*datagram));
}

/// The frames the workers saw.
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

fn told(log: &Rc<RefCell<Vec<Vec<u8>>>>) -> Vec<&'static str> {
    seen(log)
        .iter()
        .map(|frame| match ToDecoder::decode(frame) {
            Ok(ToDecoder::Configure { .. }) => "configure",
            Ok(ToDecoder::Datagram { .. }) => "datagram",
            Ok(ToDecoder::Tick { .. }) => "tick",
            Ok(ToDecoder::Link { up: true, .. }) => "up",
            Ok(ToDecoder::Link { up: false, .. }) => "down",
            Ok(ToDecoder::Ask { .. }) => "ask",
            Ok(ToDecoder::Stop { .. }) => "stop",
            Ok(ToDecoder::Replay { .. }) => "replay",
            Err(_) => "?",
        })
        .collect()
}

/// A caller of the discovery endpoint.
fn origin(proc_byte: u8, caps: &[CapabilityId], app: Option<AppIdentity>) -> Origin {
    let mut summary = CapabilitySummary::EMPTY;
    for &cap in caps {
        summary.insert(cap);
    }
    let origin = Origin::new(
        TrustDomain::User,
        1000 + u32::from(proc_byte),
        100,
        u64::from(proc_byte),
        ProcId::from_raw([proc_byte; 16]),
        summary,
        tairix_abi::ORIGIN_CONSOLE_NONE,
    );
    match app {
        Some(app) => origin.with_app(app),
        None => origin,
    }
}

fn printer_app() -> AppIdentity {
    AppIdentity::new("os.example.printing", PublisherId::from_raw([7; 32])).unwrap()
}

fn call<L: SessionLauncher>(
    front: &mut Front<L, RecordingSink, FakeHost>,
    now: u64,
    who: &Origin,
    request: &DiscoveryRequest<'_>,
) -> Vec<u8> {
    let mut bytes = vec![0u8; 512];
    let len = request.encode(&mut bytes).expect("encodes");
    let mut reply = vec![0u8; DISCOVERY_MAX_REPLY];
    let len = front.serve(now, who, &bytes[..len], &mut reply);
    reply.truncate(len);
    turn(front, now);
    reply
}

fn ipp() -> ServiceTypeField<'static> {
    ServiceTypeField {
        name: b"ipp",
        transport: Transport::Tcp,
    }
}

/// Open a session for `who` and browse `_ipp._tcp` in it.
fn browse<L: SessionLauncher>(
    front: &mut Front<L, RecordingSink, FakeHost>,
    now: u64,
    who: &Origin,
) -> Result<(u32, u32), Errno> {
    let session = decode_id_reply(&call(
        front,
        now,
        who,
        &DiscoveryRequest::Open { deliver_port: PORT },
    ))?;
    let request = decode_id_reply(&call(
        front,
        now,
        who,
        &DiscoveryRequest::Start {
            session,
            query: Query::Browse { service: ipp() },
        },
    ))?;
    Ok((session, request))
}

/// Collect a session, returning each entry as `(request, kind, text)`.
fn collected<L: SessionLauncher>(
    front: &mut Front<L, RecordingSink, FakeHost>,
    who: &Origin,
    session: u32,
) -> Vec<(u32, &'static str, Vec<u8>)> {
    let reply = call(
        front,
        0,
        who,
        &DiscoveryRequest::Collect {
            session,
            capacity: u32::try_from(DISCOVERY_MAX_REPLY).unwrap(),
        },
    );
    let parsed = CollectReply::parse(&reply).expect("a collect reply");
    parsed
        .entries()
        .map(|entry| match entry {
            Entry::Answer {
                request,
                change,
                answer: Answer::Instance { label },
                ..
            } => (
                request,
                match change {
                    Change::Added => "added",
                    Change::Refreshed => "refreshed",
                    Change::Retired => "retired",
                },
                label.to_vec(),
            ),
            Entry::Answer { request, .. } => (request, "answer", Vec::new()),
            Entry::Flush { request, .. } => (request, "flush", Vec::new()),
            Entry::Lost { request } => (request, "lost", Vec::new()),
        })
        .collect()
}

#[test]
fn the_first_decoder_is_due_at_once_and_is_keyed_before_anything_else() {
    let mut h = harness(Script::Nothing);
    assert_eq!(h.front.wake_at(), Some(0));
    h.front.on_wake(0).expect("keys draw");
    turn(&mut h.front, 0);
    match ToDecoder::decode(&seen(&h.log)[0]) {
        Ok(ToDecoder::Configure { cache_key, rng_key }) => {
            // The first draw keyed the answer fingerprints.
            assert!(cache_key.iter().all(|byte| *byte == 2));
            assert!(rng_key.iter().all(|byte| *byte == 3));
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
fn a_link_is_told_the_decoder_once_and_only_then_is_relayed_from() {
    let mut h = harness(Script::Nothing);
    h.front.on_wake(0).expect("keys draw");
    turn(&mut h.front, 0);
    deliver(&mut h.front, MS, &from(9, true, b"before"));
    link(&mut h.front, MS, V4, b"eth0", true);
    // The IPv6 socket rides the same link: nothing more to tell.
    link(&mut h.front, MS, V6, b"eth0", true);
    deliver(&mut h.front, MS, &from(9, true, b"after"));
    turn(&mut h.front, MS);
    assert_eq!(told(&h.log), ["configure", "up", "datagram"]);
    assert_eq!(relayed_payloads(&h.log), [b"after".to_vec()]);
}

#[test]
fn an_on_link_datagram_is_relayed_unread_and_an_off_link_one_never() {
    let mut h = harness(Script::Nothing);
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"\x00\x01anything at all"));
    deliver(&mut h.front, MS, &from(10, false, b"off link"));
    turn(&mut h.front, MS);
    assert_eq!(
        relayed_payloads(&h.log),
        [b"\x00\x01anything at all".to_vec()]
    );
}

#[test]
fn a_flooding_sender_is_held_to_its_budget_and_a_neighbour_still_relayed() {
    let mut h = harness(Script::Nothing);
    started(&mut h, 0);
    for _ in 0..500 {
        deliver(&mut h.front, MS, &from(9, true, b"flood"));
        turn(&mut h.front, MS);
    }
    deliver(&mut h.front, MS, &from(10, true, b"neighbour"));
    turn(&mut h.front, MS);
    let payloads = relayed_payloads(&h.log);
    let flood = payloads.iter().filter(|p| p.as_slice() == b"flood").count();
    assert_eq!(flood, RELAY_SOURCE_BURST as usize);
    assert_eq!(payloads.last().map(Vec::as_slice), Some(&b"neighbour"[..]));
}

#[test]
fn a_full_queue_holds_one_datagram_and_stops_the_drain_until_it_has_room() {
    let mut h = harness(Script::Nothing);
    started(&mut h, 0);
    let large = vec![0xC3u8; 8_000];
    let mut accepted = 0usize;
    while h.front.wants_deliveries() {
        deliver(&mut h.front, MS, &from(9, true, &large));
        accepted += 1;
        assert!(accepted < 64, "the queue is bounded");
    }
    deliver(&mut h.front, MS, &from(9, true, b"ignored"));
    turn(&mut h.front, MS);
    assert!(h.front.wants_deliveries(), "room made, the drain resumes");
    let payloads = relayed_payloads(&h.log);
    assert_eq!(payloads.len(), accepted, "the held datagram arrived too");
    assert!(payloads.iter().all(|p| p.as_slice() != b"ignored"));
}

#[test]
fn a_deadline_is_answered_by_one_tick_no_sooner_than_the_floor() {
    let mut h = harness(Script::Deadline(Some(5 * MS)));
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"x"));
    turn(&mut h.front, MS);
    assert_eq!(h.front.wake_at(), Some(MIN_TICK_INTERVAL_NS));
    h.front.on_wake(5 * MS).expect("no start due");
    turn(&mut h.front, 5 * MS);
    assert!(!told(&h.log).contains(&"tick"));
    h.front.on_wake(MIN_TICK_INTERVAL_NS).expect("no start due");
    assert_eq!(h.front.wake_at(), None, "one tick outstanding");
    turn(&mut h.front, MIN_TICK_INTERVAL_NS);
    assert_eq!(told(&h.log).iter().filter(|&&t| t == "tick").count(), 1);
    assert_eq!(h.front.wake_at(), None);
}

#[test]
fn a_tick_with_no_room_waits_for_the_queue_to_drain_rather_than_a_timer() {
    let mut h = harness(Script::Deadline(Some(5 * MS)));
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"x"));
    turn(&mut h.front, MS);
    let exact = vec![0x3Cu8; OUTBOUND_QUEUE / 32 - FRAME_HEADER_LEN - DATAGRAM_HEADER_LEN];
    while h.front.wants_deliveries() {
        deliver(&mut h.front, MS, &from(10, true, &exact));
    }
    let now = 20 * MS;
    h.front.on_wake(now).expect("no start due");
    assert_eq!(h.front.wake_at(), None, "an owed tick is not a timer");
    turn(&mut h.front, now);
    let frames = told(&h.log);
    let tick = frames
        .iter()
        .position(|&t| t == "tick")
        .expect("sent once drained");
    let last = frames
        .iter()
        .rposition(|&t| t == "datagram")
        .expect("relayed");
    assert!(tick > last, "the held datagram went first");
}

#[test]
fn a_session_needs_cap_net_and_its_owner_is_watched_once() {
    let mut h = harness(Script::Nothing);
    started(&mut h, 0);
    let stranger = origin(3, &[], None);
    assert_eq!(
        decode_id_reply(&call(
            &mut h.front,
            0,
            &stranger,
            &DiscoveryRequest::Open { deliver_port: PORT }
        )),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(h.sink.count(REQUEST_DENIED), 1);
    let who = origin(4, &[CapabilityId::NET], None);
    for _ in 0..2 {
        assert!(decode_id_reply(&call(
            &mut h.front,
            0,
            &who,
            &DiscoveryRequest::Open { deliver_port: PORT }
        ))
        .is_ok());
    }
    assert_eq!(h.world.0.borrow().watched, [ProcId::from_raw([4; 16])]);
}

#[test]
fn a_browse_is_admitted_by_a_grant_or_by_discover_all_and_refused_otherwise() {
    let mut h = harness(Script::Nothing);
    started(&mut h, 0);
    let plain = origin(5, &[CapabilityId::NET], Some(printer_app()));
    assert_eq!(
        browse(&mut h.front, 0, &plain),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(h.sink.count(REQUEST_DENIED), 1);
    let all = origin(
        6,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    assert!(browse(&mut h.front, 0, &all).is_ok());

    let publisher = "0707070707070707070707070707070707070707070707070707070707070707";
    let store = alloc::format!("browse os.example.printing {publisher} _ipp._tcp\n");
    let world = FakeHost::default();
    let mut granted = harness_with(world, &[], Script::Nothing);
    granted.front.grants = Grants::load(store.as_bytes()).expect("a well-formed store");
    started(&mut granted, 0);
    assert!(browse(&mut granted.front, 0, &plain).is_ok());
    // The grant is the bundle's: the same bundle under another publisher,
    // or another bundle, holds nothing.
    let impostor = AppIdentity::new("os.example.printing", PublisherId::from_raw([8; 32])).unwrap();
    let other = origin(7, &[CapabilityId::NET], Some(impostor));
    assert_eq!(
        browse(&mut granted.front, 0, &other),
        Err(Errno::PermissionDenied)
    );
    let ungranted = origin(8, &[CapabilityId::NET], None);
    assert_eq!(
        browse(&mut granted.front, 0, &ungranted),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn the_type_enumeration_needs_discover_all_and_a_host_lookup_only_cap_net() {
    let mut h = harness(Script::Nothing);
    started(&mut h, 0);
    let plain = origin(9, &[CapabilityId::NET], None);
    let session = decode_id_reply(&call(
        &mut h.front,
        0,
        &plain,
        &DiscoveryRequest::Open { deliver_port: PORT },
    ))
    .unwrap();
    assert_eq!(
        decode_id_reply(&call(
            &mut h.front,
            0,
            &plain,
            &DiscoveryRequest::Start {
                session,
                query: Query::Types
            }
        )),
        Err(Errno::PermissionDenied)
    );
    let host = Name::encode("printer.local").unwrap();
    assert!(decode_id_reply(&call(
        &mut h.front,
        0,
        &plain,
        &DiscoveryRequest::Start {
            session,
            query: Query::Host {
                name: host.as_wire(),
                families: Families::BOTH
            }
        }
    ))
    .is_ok());
    assert!(told(&h.log).contains(&"ask"));
}

#[test]
fn a_session_is_its_owners_alone() {
    let mut h = harness(Script::Nothing);
    started(&mut h, 0);
    let owner = origin(
        10,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    let (session, request) = browse(&mut h.front, 0, &owner).unwrap();
    let thief = origin(
        11,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    for stolen in [
        DiscoveryRequest::Collect {
            session,
            capacity: 8192,
        },
        DiscoveryRequest::Stop { session, request },
        DiscoveryRequest::Close { session },
    ] {
        assert_eq!(
            decode_status_reply(&call(&mut h.front, 0, &thief, &stolen)),
            Err(Errno::NotFound)
        );
    }
}

#[test]
fn a_decoder_that_answers_a_question_never_asked_is_condemned() {
    let mut h = harness(Script::Unasked);
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"x"));
    turn(&mut h.front, MS);
    assert_eq!(h.sink.count(EVENT_WORKER_CRASHED), 1);
    assert!(!h.front.wants_deliveries());
}

#[test]
fn a_decoder_that_sends_a_frame_no_decoder_sends_is_condemned() {
    let mut h = harness(Script::Garbage);
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"x"));
    turn(&mut h.front, MS);
    assert_eq!(h.sink.count(EVENT_WORKER_CRASHED), 1);
    assert!(!h.front.wants_deliveries());
    assert!(h.front.wake_at().is_some_and(|at| at > MS));
}

#[test]
fn a_direct_reply_goes_only_to_a_peer_just_relayed_from() {
    let asker = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9));
    let mut h = harness(Script::Reply(asker, 5353));
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"q"));
    turn(&mut h.front, MS);
    assert_eq!(h.world.0.borrow().sent.len(), 1, "answered the asker");
    let stranger = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 77));
    let mut h = harness(Script::Reply(stranger, 5353));
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"q"));
    turn(&mut h.front, MS);
    assert!(
        h.world.0.borrow().sent.is_empty(),
        "never a peer it did not hear"
    );
}

#[test]
fn a_direct_reply_is_owed_only_for_a_while_after_the_asker_was_heard() {
    let asker = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9));
    let mut h = harness(Script::Reply(asker, 5353));
    started(&mut h, 0);
    deliver(&mut h.front, MS, &from(9, true, b"q"));
    turn(&mut h.front, MS);
    assert_eq!(h.world.0.borrow().sent.len(), 1);
    // The scripted reply to a second datagram from someone else, long after,
    // names the first asker, whose window has closed.
    let late = MS + ASKER_WINDOW_NS + MS;
    deliver(&mut h.front, late, &from(10, true, b"q"));
    turn(&mut h.front, late);
    assert_eq!(h.world.0.borrow().sent.len(), 1, "the window closed");
}

#[test]
fn a_crashed_decoder_is_replaced_after_the_pace_and_told_everything_again() {
    let mut h = harness_with(FakeHost::default(), &[true], Script::Nothing);
    h.front.on_wake(0).expect("keys draw");
    link(&mut h.front, 0, V4, b"eth0", true);
    h.front.on_decoder_readable(10 * MS);
    assert_eq!(h.sink.count(EVENT_WORKER_CRASHED), 1);
    assert!(!h.front.wants_deliveries(), "nothing is relayed to no one");
    let due = h.front.wake_at().expect("a replacement is scheduled");
    assert!(due > 10 * MS);
    h.front.on_wake(due - 1).expect("keys draw");
    assert_eq!(*h.launches.borrow(), 1, "not before its time");
    h.front.on_wake(due).expect("keys draw");
    turn(&mut h.front, due);
    assert_eq!(*h.launches.borrow(), 2);
    assert_eq!(h.sink.count(DECODER_STARTED), 2);
    // The replacement is keyed afresh and told the link its predecessor had.
    let frames = seen(&h.log);
    match ToDecoder::decode(&frames[0]) {
        Ok(ToDecoder::Configure { cache_key, .. }) => {
            assert!(cache_key.iter().all(|byte| *byte == 4));
        }
        other => panic!("the replacement is configured first, not {other:?}"),
    }
    assert_eq!(told(&h.log), ["configure", "up"]);
    assert!(h.front.wants_deliveries());
}

#[test]
fn without_entropy_no_decoder_is_started_and_the_front_stops() {
    let world = FakeHost::default();
    let mut h = harness_with(world.clone(), &[], Script::Nothing);
    world.0.borrow_mut().entropy_dead = true;
    assert_eq!(
        h.front.on_wake(0),
        Err(Fatal::Entropy(Errno::EntropyNotReady))
    );
    assert_eq!(*h.launches.borrow(), 0);
    assert!(!h.front.wants_deliveries());
}

/// A multicast DNS response carrying `records`.
fn response(records: &[Record]) -> Vec<u8> {
    let mut out = vec![0u8; 1200];
    let mut writer = MessageWriter::new(&mut out, 0, true).expect("room for a header");
    for record in records {
        assert!(writer.push_record(Section::Answer, record));
    }
    let len = writer.finish();
    out.truncate(len);
    out
}

fn printer(label: &[u8], ttl: u32) -> Record {
    let instance =
        Name::from_labels(&[label, b"_ipp", b"_tcp", b"local"]).expect("an instance name");
    let mut record = Record::shared(
        Name::encode("_ipp._tcp.local").unwrap(),
        RData::Ptr(instance),
    );
    record.ttl = ttl;
    record
}

type RealFront = Front<LoopbackSessionLauncher<fn() -> Decoder>, RecordingSink, FakeHost>;

fn real() -> (RealFront, RecordingSink, FakeHost) {
    let sink = RecordingSink::default();
    let world = FakeHost::default();
    let mut front = Front::new(
        LoopbackSessionLauncher::new(Decoder::new as fn() -> Decoder),
        sink.clone(),
        world.clone(),
        sockets(),
        Grants::default(),
    )
    .expect("the buffers commit");
    front.on_wake(0).expect("keys draw");
    turn(&mut front, 0);
    link(&mut front, 0, V4, b"eth0", true);
    (front, sink, world)
}

#[test]
fn end_to_end_a_browse_asks_the_segment_and_is_rung_for_what_it_hears() {
    let (mut front, sink, world) = real();
    let who = origin(
        20,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    let (session, request) = browse(&mut front, 0, &who).unwrap();
    // The query goes out on the next tick, to the group, on IPv4 alone.
    let due = front.wake_at().expect("the question is due");
    front.on_wake(due).expect("no start due");
    turn(&mut front, due);
    let sent = world.0.borrow().sent.clone();
    assert!(!sent.is_empty());
    assert!(sent.iter().all(|(interface, to, _)| {
        *interface == iface(b"eth0") && to.port == 5353 && to.family == NetAddrFamily::V4
    }));

    deliver(
        &mut front,
        due + MS,
        &from(9, true, &response(&[printer(b"Hall Printer", 4500)])),
    );
    turn(&mut front, due + MS);
    assert_eq!(world.0.borrow().rings, [(PORT, session)], "rung once");
    assert_eq!(
        collected(&mut front, &who, session),
        [(request, "added", b"Hall Printer".to_vec())]
    );
    assert_eq!(sink.count(EVENT_WORKER_CRASHED), 0);
}

#[test]
fn end_to_end_a_link_that_goes_down_flushes_what_was_learned_on_it() {
    let (mut front, _, world) = real();
    let who = origin(
        21,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    let (session, request) = browse(&mut front, 0, &who).unwrap();
    deliver(
        &mut front,
        MS,
        &from(9, true, &response(&[printer(b"Lobby", 4500)])),
    );
    turn(&mut front, MS);
    assert_eq!(collected(&mut front, &who, session).len(), 1);
    link(&mut front, 2 * MS, V4, b"eth0", false);
    assert_eq!(
        collected(&mut front, &who, session),
        [(request, "flush", Vec::new())]
    );
    assert_eq!(world.0.borrow().rings.len(), 2);
}

#[test]
fn end_to_end_a_second_browse_of_the_same_type_is_replayed_what_is_held() {
    let (mut front, _, _) = real();
    let first = origin(
        22,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    let (one, _) = browse(&mut front, 0, &first).unwrap();
    deliver(
        &mut front,
        MS,
        &from(9, true, &response(&[printer(b"Lobby", 4500)])),
    );
    turn(&mut front, MS);
    let second = origin(
        23,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    let (two, request) = browse(&mut front, 2 * MS, &second).unwrap();
    assert_eq!(
        collected(&mut front, &second, two),
        [(request, "added", b"Lobby".to_vec())]
    );
    assert_eq!(collected(&mut front, &first, one).len(), 1, "told once");
}

#[test]
fn end_to_end_an_exited_client_stops_being_asked_for() {
    let (mut front, _, _) = real();
    let who = origin(
        24,
        &[CapabilityId::NET, CapabilityId::NET_DISCOVER_ALL],
        None,
    );
    browse(&mut front, 0, &who).unwrap();
    let before = front.wake_at();
    assert!(before.is_some(), "a question is due");
    front.on_peer_exit(MS, who.proc_id());
    turn(&mut front, MS);
    // The only question is stopped, so nothing further is asked of the segment.
    front.on_wake(10 * SEC).expect("no start due");
    turn(&mut front, 10 * SEC);
    assert_eq!(front.wake_at(), None);
}
