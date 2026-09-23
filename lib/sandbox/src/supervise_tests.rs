//! Unit tests for the supervised session.
//!
//! The healthy path runs over in-process [`LoopbackSession`] workers, as a
//! consumer's own tests will; every failure is scripted on a transport of
//! its own, so each way a worker can die is covered without processes.

use super::{
    SessionLauncher, SupervisedSession, RESTART_BASE_NS, RESTART_CAP_NS, RESTART_STABLE_NS,
};
use crate::host::{EVENT_WORKER_CRASHED, EVENT_WORKER_UNAVAILABLE};
use crate::loopback::LoopbackSessionLauncher;
use crate::session::{
    FrameOut, SessionBounds, SessionDescriptors, SessionError, SessionService, SessionStep,
    SessionTransport, EVENT_SESSION_FAILED,
};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::Errno;
use tairix_log::{Event, EventId, Sink};

fn bounds() -> SessionBounds {
    SessionBounds::new(256, 256).expect("workable")
}

/// Captures the id of every logged event.
#[derive(Clone, Default)]
struct RecordingSink {
    ids: Rc<RefCell<Vec<EventId>>>,
}

impl RecordingSink {
    fn ids(&self) -> Vec<EventId> {
        self.ids.borrow().clone()
    }
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.ids.borrow_mut().push(event.id);
    }
}

/// Echoes each frame with a `>` prefix, and closes the session on `bye`
/// after answering it.
struct Echo;

impl SessionService for Echo {
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep {
        let mut framed = Vec::with_capacity(request.len() + 1);
        framed.push(b'>');
        framed.extend_from_slice(request);
        let _ = out.frame(&framed);
        if request == b"bye" {
            SessionStep::Finished
        } else {
            SessionStep::Continue
        }
    }
}

fn echoing() -> SupervisedSession<LoopbackSessionLauncher<fn() -> Echo>, RecordingSink> {
    let factory: fn() -> Echo = || Echo;
    SupervisedSession::new(
        LoopbackSessionLauncher::new(factory),
        bounds(),
        RecordingSink::default(),
    )
}

/// Flush everything queued and read back what the in-process worker made.
fn turn<L: SessionLauncher>(
    session: &mut SupervisedSession<L, RecordingSink>,
    now: u64,
) -> Result<Vec<Vec<u8>>, SessionError> {
    while session.wants_write() {
        session.on_writable(now)?;
    }
    if session.wants_read() {
        session.on_readable(now)?;
    }
    let mut frames = Vec::new();
    while let Some(frame) = session.recv(now, <[u8]>::to_vec)? {
        frames.push(frame);
    }
    Ok(frames)
}

/// A worker that fails on its first transport operation, reporting
/// `exit_code` when reaped.
struct Doomed {
    read_fails: Errno,
    reaped: Rc<RefCell<usize>>,
}

impl SessionTransport for Doomed {
    fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(self.read_fails)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        Ok(buf.len())
    }

    fn descriptors(&self) -> Option<SessionDescriptors> {
        Some(SessionDescriptors {
            read_fd: 5,
            write_fd: 6,
        })
    }

    fn dispose(self) -> Option<i32> {
        *self.reaped.borrow_mut() += 1;
        Some(139)
    }
}

/// Hands out scripted launches in order; past the script every launch is
/// refused.
struct Scripted {
    launches: VecDeque<Result<Doomed, Errno>>,
    attempts: usize,
}

impl SessionLauncher for Scripted {
    type Transport = Doomed;

    fn launch(&mut self) -> Result<Doomed, Errno> {
        self.attempts += 1;
        self.launches.pop_front().unwrap_or(Err(Errno::NotFound))
    }
}

fn doomed(reaped: &Rc<RefCell<usize>>) -> Doomed {
    Doomed {
        read_fails: Errno::BrokenPipe,
        reaped: reaped.clone(),
    }
}

fn scripted(
    launches: Vec<Result<Doomed, Errno>>,
) -> (SupervisedSession<Scripted, RecordingSink>, RecordingSink) {
    let sink = RecordingSink::default();
    let session = SupervisedSession::new(
        Scripted {
            launches: launches.into(),
            attempts: 0,
        },
        bounds(),
        sink.clone(),
    );
    (session, sink)
}

#[test]
fn the_first_worker_starts_at_once_as_generation_one() {
    let mut session = echoing();
    assert!(!session.is_live());
    assert_eq!(session.restart_deadline(), Some(0));
    assert_eq!(session.start(0), Some(1));
    assert!(session.is_live());
    assert_eq!(session.restart_deadline(), None);
    // A live worker is not started again.
    assert_eq!(session.start(0), None);
}

#[test]
fn frames_cross_a_supervised_worker_in_order_in_both_directions() {
    let mut session = echoing();
    session.start(0).expect("starts");
    session.send(b"one").expect("queued");
    session.send(b"two").expect("queued");
    assert_eq!(
        turn(&mut session, 1).expect("healthy"),
        [b">one".to_vec(), b">two".to_vec()]
    );
}

#[test]
fn a_crashed_worker_is_reaped_logged_and_replaced_after_the_paced_delay() {
    let reaped = Rc::new(RefCell::new(0));
    let (mut session, sink) = scripted(alloc::vec![Ok(doomed(&reaped)), Ok(doomed(&reaped))]);
    assert_eq!(session.start(10), Some(1));

    assert_eq!(session.on_readable(20), Err(SessionError::WorkerFailed));
    assert_eq!(*reaped.borrow(), 1);
    assert!(!session.is_live());
    assert_eq!(sink.ids(), [EVENT_WORKER_CRASHED]);
    assert_eq!(session.restart_deadline(), Some(20 + RESTART_BASE_NS));

    // Not before the deadline: a crafted crash buys no early respawn.
    assert_eq!(session.start(20 + RESTART_BASE_NS - 1), None);
    assert_eq!(session.start(20 + RESTART_BASE_NS), Some(2));
    assert!(session.is_live());
}

#[test]
fn consecutive_failures_double_the_delay_to_the_cap_and_a_stable_run_resets_it() {
    let reaped = Rc::new(RefCell::new(0));
    let launches = (0..32).map(|_| Ok(doomed(&reaped))).collect();
    let (mut session, _sink) = scripted(launches);
    let mut now = 0;
    assert_eq!(session.start(now), Some(1));
    let mut delays = Vec::new();
    for _ in 0..12 {
        let _ = session.on_readable(now);
        let due = session.restart_deadline().expect("scheduled");
        delays.push(due - now);
        now = due;
        assert!(session.start(now).is_some());
    }
    assert_eq!(delays[0], RESTART_BASE_NS);
    assert_eq!(delays[1], 2 * RESTART_BASE_NS);
    assert_eq!(delays[2], 4 * RESTART_BASE_NS);
    assert_eq!(*delays.last().expect("some"), RESTART_CAP_NS);
    assert!(delays.iter().all(|delay| *delay <= RESTART_CAP_NS));

    // Up for the stable window: the next failure is an isolated one.
    now += RESTART_STABLE_NS;
    let _ = session.on_readable(now);
    assert_eq!(session.restart_deadline(), Some(now + RESTART_BASE_NS));
}

#[test]
fn a_launch_that_fails_is_logged_unavailable_and_paced_like_a_crash() {
    let reaped = Rc::new(RefCell::new(0));
    let (mut session, sink) = scripted(alloc::vec![
        Err(Errno::PermissionDenied),
        Ok(doomed(&reaped))
    ]);
    assert_eq!(session.start(0), None);
    assert_eq!(sink.ids(), [EVENT_WORKER_UNAVAILABLE]);
    assert_eq!(session.restart_deadline(), Some(RESTART_BASE_NS));
    assert_eq!(session.start(1), None, "the failed launch is paced");
    assert_eq!(session.start(RESTART_BASE_NS), Some(1));
}

#[test]
fn a_worker_that_closes_its_stream_has_failed_once_its_frames_are_taken() {
    let mut session = echoing();
    session.start(0).expect("starts");
    session.send(b"bye").expect("queued");
    while session.wants_write() {
        session.on_writable(1).expect("written");
    }
    session.on_readable(1).expect("the reply is read");
    // The frame sent before the close still arrives...
    assert_eq!(session.recv(1, <[u8]>::to_vec), Ok(Some(b">bye".to_vec())));
    // ...the next read finds the stream closed on a frame boundary, which
    // is no framing fault...
    session.on_readable(1).expect("a clean close");
    // ...but a supervised worker that ends has failed.
    assert_eq!(
        session.recv(1, <[u8]>::to_vec),
        Err(SessionError::WorkerFailed)
    );
    assert!(!session.is_live());
    assert_eq!(session.restart_deadline(), Some(1 + RESTART_BASE_NS));
    assert_eq!(session.start(1 + RESTART_BASE_NS), Some(2));
}

/// A worker whose first frame declares more than the owner's inbound bound
/// admits.
struct Oversize {
    sent: bool,
}

impl SessionTransport for Oversize {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.sent {
            return Err(Errno::WouldBlock);
        }
        self.sent = true;
        buf[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        Ok(4)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        Ok(buf.len())
    }

    fn descriptors(&self) -> Option<SessionDescriptors> {
        None
    }

    fn dispose(self) -> Option<i32> {
        None
    }
}

struct OversizeLauncher;

impl SessionLauncher for OversizeLauncher {
    type Transport = Oversize;

    fn launch(&mut self) -> Result<Oversize, Errno> {
        Ok(Oversize { sent: false })
    }
}

#[test]
fn a_frame_past_the_inbound_bound_contains_the_worker() {
    let sink = RecordingSink::default();
    let mut session = SupervisedSession::new(OversizeLauncher, bounds(), sink.clone());
    session.start(0).expect("starts");
    assert_eq!(session.on_readable(5), Err(SessionError::WorkerFailed));
    assert_eq!(sink.ids(), [EVENT_WORKER_CRASHED]);
    assert_eq!(session.restart_deadline(), Some(5 + RESTART_BASE_NS));
}

#[test]
fn a_condemned_worker_is_reaped_logged_and_replaced() {
    let reaped = Rc::new(RefCell::new(0));
    let (mut session, sink) = scripted(alloc::vec![Ok(doomed(&reaped)), Ok(doomed(&reaped))]);
    session.start(0).expect("starts");
    session.condemn(7, "sent an unbelievable frame");
    assert_eq!(*reaped.borrow(), 1);
    assert_eq!(sink.ids(), [EVENT_WORKER_CRASHED]);
    assert_eq!(session.restart_deadline(), Some(7 + RESTART_BASE_NS));
    // Condemning when nothing is live changes nothing.
    session.condemn(8, "again");
    assert_eq!(sink.ids().len(), 1);
    assert_eq!(session.restart_deadline(), Some(7 + RESTART_BASE_NS));
}

#[test]
fn with_no_live_worker_every_operation_is_refused_and_touches_nothing() {
    let mut session = echoing();
    assert_eq!(session.send(b"x"), Err(SessionError::WorkerFailed));
    assert_eq!(session.on_readable(0), Err(SessionError::WorkerFailed));
    assert_eq!(session.on_writable(0), Err(SessionError::WorkerFailed));
    assert_eq!(
        session.recv(0, <[u8]>::to_vec),
        Err(SessionError::WorkerFailed)
    );
    assert!(!session.wants_read());
    assert!(!session.wants_write());
    assert_eq!(session.descriptors(), None);
    // A refusal with nothing live is not a failure to pace.
    assert_eq!(session.restart_deadline(), Some(0));
}

#[test]
fn a_supervised_failure_is_never_recorded_as_a_session_ended() {
    let reaped = Rc::new(RefCell::new(0));
    let (mut session, sink) = scripted(alloc::vec![Ok(doomed(&reaped))]);
    session.start(0).expect("starts");
    let _ = session.on_readable(1);
    assert!(!sink.ids().contains(&EVENT_SESSION_FAILED));
}

#[test]
fn dropping_the_supervisor_reaps_the_live_worker() {
    let reaped = Rc::new(RefCell::new(0));
    let (mut session, _sink) = scripted(alloc::vec![Ok(doomed(&reaped))]);
    session.start(0).expect("starts");
    drop(session);
    assert_eq!(*reaped.borrow(), 1);
}

#[test]
fn each_worker_reports_its_own_descriptors() {
    let reaped = Rc::new(RefCell::new(0));
    let (mut session, _sink) = scripted(alloc::vec![Ok(doomed(&reaped))]);
    assert_eq!(session.descriptors(), None);
    session.start(0).expect("starts");
    assert_eq!(
        session.descriptors(),
        Some(SessionDescriptors {
            read_fd: 5,
            write_fd: 6,
        })
    );
    // The loopback fake occupies none.
    let mut loopback: SupervisedSession<_, RecordingSink> = SupervisedSession::new(
        LoopbackSessionLauncher::new(|| Echo),
        bounds(),
        RecordingSink::default(),
    );
    loopback.start(0).expect("starts");
    assert_eq!(loopback.descriptors(), None);
}
