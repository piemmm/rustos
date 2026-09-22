//! Unit tests for the duplex session seam.
//!
//! The healthy duplex path runs over the in-process
//! [`crate::loopback::LoopbackSession`], exactly as a consumer's own host
//! tests will; the containment paths script a failing
//! [`SessionTransport`] directly, so every way a worker can die is covered
//! without processes.

use super::{
    head_declared, serve_session, FrameOut, SandboxSession, SessionBounds, SessionDescriptors,
    SessionError, SessionService, SessionStep, SessionTransport, EVENT_SESSION_FAILED,
    MIN_QUEUE_BYTES,
};
use crate::loopback::LoopbackSession;
use crate::proto::{Channel, ProtoError, FRAME_HEADER_LEN, MAX_FRAME};
use crate::worker::ServeEnd;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::Errno;
use tairix_log::{Event, EventId, Level, Sink};

/// A workable bound for the tests: room for a handful of small frames.
const BOUND: usize = 256;

fn bounds() -> SessionBounds {
    SessionBounds::new(BOUND, BOUND).expect("a 256-byte bound is workable")
}

/// Captures `(id, level)` of every logged event.
#[derive(Clone, Default)]
struct RecordingSink {
    events: Rc<RefCell<Vec<(EventId, Level)>>>,
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.events.borrow_mut().push((event.id, event.level));
    }
}

/// Discards every event.
struct SilentSink;

impl Sink for SilentSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// Echoes each request back with a `>` prefix: one frame in, one out.
struct Tagger;

impl SessionService for Tagger {
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
        let mut framed = Vec::with_capacity(request.len() + 1);
        framed.push(b'>');
        framed.extend_from_slice(request);
        let _ = out.frame(&framed);
        SessionStep::Continue
    }
}

/// Answers one inbound frame with `fan` outbound ones — the shape a
/// one-shot request/reply seam cannot express.
struct FanOut {
    fan: usize,
}

impl SessionService for FanOut {
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
        for index in 0..self.fan {
            let mut framed = request.to_vec();
            framed.push(u8::try_from(index).unwrap_or(0));
            let _ = out.frame(&framed);
        }
        SessionStep::Continue
    }
}

/// Answers nothing and closes the session on a `bye` frame.
struct Quitter;

impl SessionService for Quitter {
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
        if request == b"bye" {
            return SessionStep::Finished;
        }
        let _ = out.frame(b"ok");
        SessionStep::Continue
    }
}

/// Framed bytes for `payload`, as a worker's transport would deliver them.
fn framed(payload: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(payload.len())
        .expect("test payload fits")
        .to_le_bytes()
        .to_vec();
    out.extend_from_slice(payload);
    out
}

/// A transport reading from a scripted byte stream (in `chunk`-sized
/// pieces, so a frame split across reads is exercised) and recording
/// everything the session writes, with optional injected failures.
struct Scripted {
    /// The worker's scripted output; a read past its end is the worker
    /// closing its stream.
    inbound: Vec<u8>,
    at: usize,
    /// Bytes one read may take, so a frame split across reads is covered.
    chunk: usize,
    /// Everything the session wrote, readable after the session owns it.
    written: Rc<RefCell<Vec<u8>>>,
    read_fails: Option<Errno>,
    write_fails: Option<Errno>,
    /// Accept at most this many bytes per write (0 = accept nothing).
    write_cap: Option<usize>,
    disposed: Rc<RefCell<usize>>,
}

impl Scripted {
    fn over(inbound: Vec<u8>) -> Self {
        Self {
            inbound,
            at: 0,
            chunk: usize::MAX,
            written: Rc::new(RefCell::new(Vec::new())),
            read_fails: None,
            write_fails: None,
            write_cap: None,
            disposed: Rc::new(RefCell::new(0)),
        }
    }
}

impl SessionTransport for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        if let Some(errno) = self.read_fails {
            return Err(errno);
        }
        let take = buf.len().min(self.chunk).min(self.inbound.len() - self.at);
        buf[..take].copy_from_slice(&self.inbound[self.at..self.at + take]);
        self.at += take;
        Ok(take)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        if let Some(errno) = self.write_fails {
            return Err(errno);
        }
        let take = self.write_cap.map_or(buf.len(), |cap| cap.min(buf.len()));
        self.written.borrow_mut().extend_from_slice(&buf[..take]);
        Ok(take)
    }

    fn descriptors(&self) -> Option<SessionDescriptors> {
        Some(SessionDescriptors {
            read_fd: 7,
            write_fd: 8,
        })
    }

    fn dispose(self) -> Option<i32> {
        *self.disposed.borrow_mut() += 1;
        Some(139)
    }
}

/// Flush everything queued and read back everything the fake produced —
/// the in-process stand-in for one wait-set wake of each direction.
fn turn<S: SessionService, K: Sink>(
    session: &mut SandboxSession<LoopbackSession<S>, K>,
) -> Result<(), SessionError> {
    while session.wants_write() {
        session.on_writable()?;
    }
    if session.wants_read() {
        session.on_readable()?;
    }
    Ok(())
}

/// Drain every complete frame the session holds.
fn drain<T: SessionTransport, K: Sink>(
    session: &mut SandboxSession<T, K>,
) -> Result<Vec<Vec<u8>>, SessionError> {
    let mut frames = Vec::new();
    while let Some(frame) = session.recv()? {
        frames.push(frame);
    }
    Ok(frames)
}

#[test]
fn frames_cross_in_order_in_both_directions() {
    let mut session =
        SandboxSession::new(LoopbackSession::new(Tagger), bounds(), SilentSink).expect("committed");
    session.send(b"alpha").expect("queued");
    session.send(b"beta").expect("queued");
    // Two frames are in flight at once: the seam is not request/reply-locked.
    assert!(session.wants_write());
    turn(&mut session).expect("no failure");
    assert!(!session.wants_write());
    assert_eq!(
        drain(&mut session).expect("no failure"),
        vec![b">alpha".to_vec(), b">beta".to_vec()],
    );
    // Nothing further is pending, and a read with nothing there is a
    // no-op rather than a spurious end-of-stream.
    session.on_readable().expect("no failure");
    assert_eq!(
        drain(&mut session).expect("no failure"),
        Vec::<Vec<u8>>::new()
    );
    assert!(!session.peer_finished());
}

#[test]
fn one_inbound_frame_may_answer_with_many() {
    let mut session = SandboxSession::new(
        LoopbackSession::new(FanOut { fan: 5 }),
        bounds(),
        SilentSink,
    )
    .expect("committed");
    session.send(b"x").expect("queued");
    turn(&mut session).expect("no failure");
    let frames = drain(&mut session).expect("no failure");
    assert_eq!(frames.len(), 5);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame.as_slice(),
            &[b'x', u8::try_from(index).expect("small")]
        );
    }
}

#[test]
fn a_frame_split_across_reads_is_assembled_before_it_is_delivered() {
    // One frame, delivered a byte at a time: nothing is reported until the
    // whole of it has arrived.
    let mut transport = Scripted::over(framed(b"hello"));
    transport.chunk = 1;
    let mut session = SandboxSession::new(transport, bounds(), SilentSink).expect("committed");
    for _ in 0..FRAME_HEADER_LEN + b"hello".len() - 1 {
        session.on_readable().expect("no failure");
        assert_eq!(session.recv().expect("no failure"), None);
    }
    session.on_readable().expect("no failure");
    assert_eq!(session.recv().expect("no failure"), Some(b"hello".to_vec()));
    assert_eq!(session.recv().expect("no failure"), None);
}

#[test]
fn an_empty_payload_is_a_legal_frame_in_both_directions() {
    let mut session =
        SandboxSession::new(Scripted::over(framed(b"")), bounds(), SilentSink).expect("committed");
    session.send(b"").expect("queued");
    session.on_writable().expect("no failure");
    session.on_readable().expect("no failure");
    assert_eq!(session.recv().expect("no failure"), Some(Vec::new()));
}

#[test]
fn a_bound_below_one_header_and_a_byte_is_refused() {
    for too_small in [0, 1, FRAME_HEADER_LEN] {
        assert_eq!(
            SessionBounds::new(too_small, BOUND),
            Err(SessionError::BoundTooSmall)
        );
        assert_eq!(
            SessionBounds::new(BOUND, too_small),
            Err(SessionError::BoundTooSmall)
        );
    }
    let smallest = SessionBounds::new(MIN_QUEUE_BYTES, MIN_QUEUE_BYTES).expect("the floor works");
    // At the floor exactly one payload byte crosses per frame, which is
    // what makes `OutboundFull` transient rather than permanent.
    assert_eq!(smallest.max_send_payload(), 1);
    assert_eq!(smallest.max_recv_payload(), 1);
}

#[test]
fn the_send_ceiling_is_derived_from_the_bound_and_never_exceeds_the_frame_cap() {
    let small = SessionBounds::new(64, 64).expect("workable");
    assert_eq!(small.max_send_payload(), 64 - FRAME_HEADER_LEN);
    let huge = SessionBounds::new(MAX_FRAME * 4, MAX_FRAME * 4).expect("workable");
    assert_eq!(huge.max_send_payload(), MAX_FRAME);
    assert_eq!(huge.max_recv_payload(), MAX_FRAME);
}

#[test]
fn an_over_ceiling_payload_is_permanently_refused_and_queues_nothing() {
    let mut session =
        SandboxSession::new(Scripted::over(Vec::new()), bounds(), SilentSink).expect("committed");
    let oversize = vec![0u8; session.bounds().max_send_payload() + 1];
    assert_eq!(session.send(&oversize), Err(SessionError::FrameTooLarge));
    assert!(!session.wants_write(), "nothing was queued");
    // The session is unharmed: an ordinary frame still goes.
    session.send(b"fine").expect("queued");
    session.on_writable().expect("no failure");
}

#[test]
fn a_full_queue_refuses_transiently_and_leaves_the_queue_untouched() {
    let mut session = SandboxSession::new(
        Scripted::over(Vec::new()),
        SessionBounds::new(32, 32).expect("workable"),
        SilentSink,
    )
    .expect("committed");
    let payload = vec![b'z'; 8];
    let per_frame = FRAME_HEADER_LEN + payload.len();
    let fits = 32 / per_frame;
    let written = Rc::clone(&session.transport.as_ref().expect("live").written);
    for _ in 0..fits {
        session.send(&payload).expect("queued");
    }
    // The next one does not fit...
    assert_eq!(session.send(&payload), Err(SessionError::OutboundFull));
    // ...and refusing it wrote no partial frame: exactly the frames that
    // fitted come out, byte for byte.
    session.on_writable().expect("no failure");
    let mut expected = Vec::new();
    for _ in 0..fits {
        expected.extend_from_slice(&framed(&payload));
    }
    assert_eq!(*written.borrow(), expected);
    // Draining made room, so the refusal really was transient.
    session.send(&payload).expect("queued after the drain");
}

#[test]
fn wants_write_tracks_the_queue_and_wants_read_tracks_the_back_pressure() {
    let mut transport = Scripted::over(Vec::new());
    // Accept one byte per write, so the queue drains slowly.
    transport.write_cap = Some(1);
    let mut session = SandboxSession::new(transport, bounds(), SilentSink).expect("committed");
    assert!(!session.wants_write(), "nothing queued yet");
    assert!(session.wants_read(), "the accumulator starts empty");
    session.send(b"ab").expect("queued");
    assert!(session.wants_write());
    for _ in 0..FRAME_HEADER_LEN + 2 {
        assert!(session.wants_write());
        session.on_writable().expect("no failure");
    }
    assert!(
        !session.wants_write(),
        "the queue drained one byte at a time"
    );
}

#[test]
fn a_full_accumulator_withdraws_read_readiness_until_the_owner_drains() {
    // Fill the inbound accumulator exactly: the owner must drain before it
    // can ask for more, which is the back-pressure that blocks the worker.
    let payload = vec![b'q'; 12];
    let per_frame = FRAME_HEADER_LEN + payload.len();
    let limit = per_frame * 3;
    let mut stream = Vec::new();
    for _ in 0..3 {
        stream.extend_from_slice(&framed(&payload));
    }
    let mut session = SandboxSession::new(
        Scripted::over(stream),
        SessionBounds::new(BOUND, limit).expect("workable"),
        SilentSink,
    )
    .expect("committed");
    session.on_readable().expect("no failure");
    assert!(!session.wants_read(), "the accumulator is full");
    assert_eq!(session.recv().expect("no failure"), Some(payload.clone()));
    assert!(session.wants_read(), "draining one frame made room");
    assert_eq!(drain(&mut session).expect("no failure").len(), 2);
    assert!(session.wants_read());
}

#[test]
fn a_clean_end_of_stream_finishes_and_a_truncated_one_is_contained() {
    // Clean: the worker's stream ends exactly on a frame boundary.
    let sink = RecordingSink::default();
    let mut session = SandboxSession::new(Scripted::over(framed(b"last")), bounds(), sink.clone())
        .expect("committed");
    session.on_readable().expect("no failure");
    session.on_readable().expect("the boundary end is clean");
    assert!(session.peer_finished());
    assert!(!session.wants_read(), "nothing more can arrive");
    // Frames already accumulated are still drainable after the end.
    assert_eq!(session.recv().expect("no failure"), Some(b"last".to_vec()));
    assert!(sink.events.borrow().is_empty(), "a clean end logs nothing");

    // Truncated: the stream ends inside a declared frame.
    let mut ragged = framed(b"whole");
    ragged.extend_from_slice(&framed(b"cut")[..=FRAME_HEADER_LEN]);
    let sink = RecordingSink::default();
    let mut session =
        SandboxSession::new(Scripted::over(ragged), bounds(), sink.clone()).expect("committed");
    session.on_readable().expect("no failure");
    assert_eq!(session.on_readable(), Err(SessionError::WorkerFailed));
    assert_eq!(
        sink.events.borrow().as_slice(),
        &[(EVENT_SESSION_FAILED, Level::Warn)]
    );
}

#[test]
fn a_worker_frame_above_the_inbound_ceiling_is_refused_before_it_is_copied() {
    let sink = RecordingSink::default();
    let limit = 64;
    // A header declaring one byte more than the accumulator could ever
    // hold: refused on sight rather than wedging the queue forever.
    let mut declaration = u32::try_from(limit - FRAME_HEADER_LEN + 1)
        .expect("small")
        .to_le_bytes()
        .to_vec();
    declaration.push(0);
    let mut session = SandboxSession::new(
        Scripted::over(declaration),
        SessionBounds::new(BOUND, limit).expect("workable"),
        sink.clone(),
    )
    .expect("committed");
    assert_eq!(session.on_readable(), Err(SessionError::WorkerFailed));
    assert_eq!(
        sink.events.borrow().as_slice(),
        &[(EVENT_SESSION_FAILED, Level::Warn)]
    );
    // A frame at the ceiling exactly is admitted, so the bound is the
    // ceiling and not one below it.
    let at_ceiling = framed(&vec![7u8; limit - FRAME_HEADER_LEN]);
    let mut session = SandboxSession::new(
        Scripted::over(at_ceiling),
        SessionBounds::new(BOUND, limit).expect("workable"),
        SilentSink,
    )
    .expect("committed");
    session.on_readable().expect("no failure");
    assert_eq!(
        session.recv().expect("no failure"),
        Some(vec![7u8; limit - FRAME_HEADER_LEN])
    );
}

#[test]
fn a_transport_failure_is_contained_reaped_logged_and_latched() {
    let sink = RecordingSink::default();
    let mut transport = Scripted::over(Vec::new());
    transport.read_fails = Some(Errno::BadAddress);
    let disposed = Rc::clone(&transport.disposed);
    let mut session = SandboxSession::new(transport, bounds(), sink.clone()).expect("committed");
    assert!(session.descriptors().is_some());

    assert_eq!(session.on_readable(), Err(SessionError::WorkerFailed));
    // Reaped once, logged once, and never replaced.
    assert_eq!(*disposed.borrow(), 1);
    assert_eq!(
        sink.events.borrow().as_slice(),
        &[(EVENT_SESSION_FAILED, Level::Warn)]
    );
    assert!(session.descriptors().is_none(), "the transport is gone");

    // Latched: every later call refuses without touching the transport,
    // and nothing further is logged.
    assert_eq!(session.send(b"x"), Err(SessionError::WorkerFailed));
    assert_eq!(session.on_readable(), Err(SessionError::WorkerFailed));
    assert_eq!(session.on_writable(), Err(SessionError::WorkerFailed));
    assert_eq!(session.recv(), Err(SessionError::WorkerFailed));
    assert!(!session.wants_read());
    assert!(!session.wants_write());
    assert_eq!(*disposed.borrow(), 1);
    assert_eq!(sink.events.borrow().len(), 1);
    assert_eq!(session.end(), None, "an ended session was already reaped");
}

#[test]
fn a_write_that_accepts_nothing_is_the_peer_being_gone() {
    let sink = RecordingSink::default();
    let mut transport = Scripted::over(Vec::new());
    transport.write_cap = Some(0);
    let disposed = Rc::clone(&transport.disposed);
    let mut session = SandboxSession::new(transport, bounds(), sink.clone()).expect("committed");
    session.send(b"x").expect("queued");
    assert_eq!(session.on_writable(), Err(SessionError::WorkerFailed));
    assert_eq!(*disposed.borrow(), 1);
    assert_eq!(
        sink.events.borrow().as_slice(),
        &[(EVENT_SESSION_FAILED, Level::Warn)]
    );
}

#[test]
fn a_would_block_report_is_a_no_op_not_a_failure() {
    let sink = RecordingSink::default();
    let mut transport = Scripted::over(Vec::new());
    transport.read_fails = Some(Errno::WouldBlock);
    transport.write_fails = Some(Errno::WouldBlock);
    let mut session = SandboxSession::new(transport, bounds(), sink.clone()).expect("committed");
    session.send(b"x").expect("queued");
    session
        .on_readable()
        .expect("nothing right now is not a failure");
    session
        .on_writable()
        .expect("nothing right now is not a failure");
    assert!(session.wants_write(), "the queue is untouched");
    assert!(!session.peer_finished());
    assert!(sink.events.borrow().is_empty());
}

#[test]
fn dropping_a_live_session_reaps_its_worker() {
    let transport = Scripted::over(Vec::new());
    let disposed = Rc::clone(&transport.disposed);
    let session = SandboxSession::new(transport, bounds(), SilentSink).expect("committed");
    drop(session);
    assert_eq!(*disposed.borrow(), 1);
    // Ending one explicitly reports the worker's exit code.
    let transport = Scripted::over(Vec::new());
    let session = SandboxSession::new(transport, bounds(), SilentSink).expect("committed");
    assert_eq!(session.end(), Some(139));
}

#[test]
fn the_event_id_is_frozen() {
    // The identifier is a contract with log consumers; renumbering it is
    // an ABI break this test refuses.
    assert_eq!(EVENT_SESSION_FAILED, EventId(6002));
}

#[test]
fn the_queues_do_not_grow_across_a_long_session() {
    // Drive one session through many round trips and confirm neither
    // queue's arena grows: both are committed once and compacted in
    // place, never reallocated or left accumulating a consumed prefix
    // (which would exhaust memory over a long-lived connection).
    let mut session =
        SandboxSession::new(LoopbackSession::new(Tagger), bounds(), SilentSink).expect("committed");
    let baseline = (session.outbound.arena.len(), session.inbound.arena.len());
    assert_eq!(baseline, (BOUND, BOUND));
    for _ in 0..10_000 {
        session.send(b"payload").expect("queued");
        turn(&mut session).expect("no failure");
        assert_eq!(
            session.recv().expect("no failure"),
            Some(b">payload".to_vec())
        );
        assert_eq!(session.recv().expect("no failure"), None);
        assert_eq!(
            (session.outbound.arena.len(), session.inbound.arena.len()),
            baseline,
            "a session queue grew across reuse"
        );
    }
}

#[test]
fn the_head_frame_helpers_read_a_declared_length_without_it_being_complete() {
    assert_eq!(head_declared(&[]), None);
    assert_eq!(head_declared(&[1, 0, 0]), None);
    assert_eq!(head_declared(&framed(b"abc")), Some(3));
    // A declaration with none of its payload yet present is still read.
    assert_eq!(head_declared(&framed(b"abc")[..FRAME_HEADER_LEN]), Some(3));
}

// --- the worker side ----------------------------------------------------

/// Scripted worker channel: reads the parent's frames from `input`,
/// collects the worker's own frames in `output`.
struct WorkerChannel {
    input: Vec<u8>,
    at: usize,
    output: Vec<u8>,
    write_fails: Option<Errno>,
}

impl WorkerChannel {
    fn over(frames: &[&[u8]]) -> Self {
        let mut input = Vec::new();
        for frame in frames {
            input.extend_from_slice(&framed(frame));
        }
        Self {
            input,
            at: 0,
            output: Vec::new(),
            write_fails: None,
        }
    }
}

impl Channel for WorkerChannel {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let take = buf.len().min(self.input.len() - self.at);
        buf[..take].copy_from_slice(&self.input[self.at..self.at + take]);
        self.at += take;
        Ok(take)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        if let Some(errno) = self.write_fails {
            return Err(errno);
        }
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }
}

/// Split `bytes` back into the payloads of the frames it holds.
fn unframe(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let len = head_declared(&bytes[at..]).expect("a whole header");
        frames.push(bytes[at + FRAME_HEADER_LEN..at + FRAME_HEADER_LEN + len].to_vec());
        at += FRAME_HEADER_LEN + len;
    }
    frames
}

#[test]
fn the_serve_loop_fans_one_request_out_to_many_frames() {
    let mut chan = WorkerChannel::over(&[b"a", b"b"]);
    assert_eq!(
        serve_session(&mut chan, &mut FanOut { fan: 3 }),
        ServeEnd::Finished
    );
    assert_eq!(
        unframe(&chan.output),
        vec![
            b"a\x00".to_vec(),
            b"a\x01".to_vec(),
            b"a\x02".to_vec(),
            b"b\x00".to_vec(),
            b"b\x01".to_vec(),
            b"b\x02".to_vec(),
        ]
    );
}

#[test]
fn a_service_can_close_the_session_itself() {
    let mut chan = WorkerChannel::over(&[b"hi", b"bye", b"never"]);
    assert_eq!(serve_session(&mut chan, &mut Quitter), ServeEnd::Ended);
    // Only the frame before the close was answered.
    assert_eq!(unframe(&chan.output), vec![b"ok".to_vec()]);
}

#[test]
fn a_dead_transport_ends_the_serve_loop_typed_on_the_first_failure() {
    let mut chan = WorkerChannel::over(&[b"a", b"b"]);
    chan.write_fails = Some(Errno::BrokenPipe);
    assert_eq!(
        serve_session(&mut chan, &mut FanOut { fan: 4 }),
        ServeEnd::Failed(ProtoError::PeerClosed)
    );
    // The latch means the fan-out's later frames never reached the
    // channel after the first refusal.
    assert!(chan.output.is_empty());
}

#[test]
fn a_parent_that_dies_mid_frame_fails_the_worker_loop_typed() {
    let mut chan = WorkerChannel::over(&[]);
    chan.input = 3u32.to_le_bytes().to_vec();
    assert_eq!(
        serve_session(&mut chan, &mut Tagger),
        ServeEnd::Failed(ProtoError::PeerClosed)
    );
}

#[test]
fn an_oversize_request_declaration_fails_the_worker_loop_before_allocation() {
    let mut chan = WorkerChannel::over(&[]);
    chan.input = u32::try_from(MAX_FRAME + 1)
        .expect("fits")
        .to_le_bytes()
        .to_vec();
    assert_eq!(
        serve_session(&mut chan, &mut Tagger),
        ServeEnd::Failed(ProtoError::Oversize)
    );
}
