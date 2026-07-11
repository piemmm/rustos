//! Host tests: the server engine against mock seams, and the client
//! halves wired to a real [`WindowServer`] through a loopback transport,
//! so both halves are proven against the one shared definition of the
//! protocol semantics.

extern crate alloc;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use rustos_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use rustos_abi::input::{KeyInput, KeyValue, Modifiers, PointerButtonCode};
use rustos_abi::origin::{ProcId, PROC_ID_LEN};
use rustos_abi::reply::decode_status_reply;
use rustos_abi::window_ipc::{PointerAction, WindowEvent};
use rustos_abi::Errno;
use rustos_display::{FrameRegion, ShmMapper};

use crate::client::{EventSource, WindowClient, WindowEvents, WindowTransport};
use crate::server::{
    CallerIdentity, EventSink, WindowHost, WindowServer, WINDOWS_PER_CLIENT_MAX, WINDOW_REPLY_MAX,
};

/// 4×3 BGRA test surface, stride == one scanline.
const SURFACE: DisplayMode = DisplayMode {
    width_px: 4,
    height_px: 3,
    stride_bytes: 16,
    format: DisplayFormat::Bgra8888,
};

/// Bytes one SURFACE frame occupies.
const FRAME_LEN: usize = 48;

/// The ticket the loopback presents for client A.
const TICKET_A: u64 = 1;
/// The ticket the loopback presents for client B.
const TICKET_B: u64 = 2;
/// A ticket attested as the kernel sentinel.
const TICKET_KERNEL: u64 = 8;
/// A ticket whose attestation fails.
const TICKET_UNATTESTED: u64 = 9;

/// The event endpoint client A names in its creates.
const EVENTS_A: u64 = 0xA000;
/// The event endpoint client B names in its creates.
const EVENTS_B: u64 = 0xB000;

/// A mapped region backed by a `Vec` with deterministic per-handle
/// content, so a presented frame slice is checkable byte for byte.
struct MockRegion(Vec<u8>);

impl FrameRegion for MockRegion {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A mapper over a fixed table of grant handle → region size.
struct MockMapper {
    regions: BTreeMap<u64, usize>,
}

impl MockMapper {
    fn with_regions(regions: &[(u64, usize)]) -> Self {
        Self {
            regions: regions.iter().copied().collect(),
        }
    }
}

/// The deterministic content byte at offset `i` of `handle`'s region.
fn region_byte(handle: u64, i: usize) -> u8 {
    handle.to_le_bytes()[0].wrapping_add(i.to_le_bytes()[0])
}

impl ShmMapper for MockMapper {
    type Region = MockRegion;

    fn map(&mut self, handle: u64, min_len: usize) -> Result<MockRegion, Errno> {
        let &len = self.regions.get(&handle).ok_or(Errno::NotFound)?;
        if len < min_len {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(MockRegion(
            (0..len).map(|i| region_byte(handle, i)).collect(),
        ))
    }
}

/// The attestation table: each ticket maps to a fixed identity.
struct MockIdentity;

fn proc_id(fill: u8) -> ProcId {
    ProcId::from_raw([fill; PROC_ID_LEN])
}

impl CallerIdentity for MockIdentity {
    fn caller(&mut self, ticket: u64) -> Result<ProcId, Errno> {
        match ticket {
            TICKET_A => Ok(proc_id(0xA1)),
            TICKET_B => Ok(proc_id(0xB2)),
            TICKET_KERNEL => Ok(ProcId::KERNEL),
            _ => Err(Errno::NotFound),
        }
    }
}

/// A host recording every bridge call, optionally refusing opens.
#[derive(Default)]
struct RecordingHost {
    opened: Vec<(u64, DisplayMode, String)>,
    presented: Vec<(u64, Vec<u8>, DamageRect)>,
    closed: Vec<u64>,
    refuse_open: bool,
}

impl WindowHost for RecordingHost {
    fn window_opened(
        &mut self,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
    ) -> Result<(), Errno> {
        if self.refuse_open {
            return Err(Errno::WouldBlock);
        }
        self.opened.push((window_id, *surface, String::from(title)));
        Ok(())
    }

    fn window_presented(
        &mut self,
        window_id: u64,
        _surface: &DisplayMode,
        frame: &[u8],
        damage: DamageRect,
    ) -> Result<(), Errno> {
        self.presented.push((window_id, frame.to_vec(), damage));
        Ok(())
    }

    fn window_closed(&mut self, window_id: u64) {
        self.closed.push(window_id);
    }
}

/// A sink recording each delivered event by endpoint, doubling as the
/// backing queue the client-side event source pops.
#[derive(Default)]
struct QueueSink {
    delivered: VecDeque<(u64, [u8; WindowEvent::WIRE_LEN])>,
}

impl EventSink for QueueSink {
    fn deliver(&mut self, endpoint: u64, event: &[u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno> {
        self.delivered.push_back((endpoint, *event));
        Ok(())
    }
}

/// The in-process loopback: a real server behind the client seams.
struct Loopback {
    server: WindowServer<MockMapper>,
    host: RecordingHost,
    identity: MockIdentity,
    /// The ticket the "kernel" attaches to the next in-flight call.
    ticket: u64,
}

impl Loopback {
    fn with_regions(regions: &[(u64, usize)]) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            server: WindowServer::new(MockMapper::with_regions(regions)),
            host: RecordingHost::default(),
            identity: MockIdentity,
            ticket: TICKET_A,
        }))
    }
}

impl WindowTransport for Rc<RefCell<Loopback>> {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        let mut frame = [0u8; WINDOW_REPLY_MAX];
        let inner = &mut *self.borrow_mut();
        let len = inner.server.serve(
            &mut inner.host,
            &mut inner.identity,
            inner.ticket,
            request,
            &mut frame,
        );
        if reply.len() < len {
            return Err(Errno::BufferTooSmall);
        }
        reply[..len].copy_from_slice(&frame[..len]);
        Ok(len)
    }
}

/// The client-side event source popping the recorded deliveries for one
/// endpoint (the app's own event endpoint in production).
struct QueueSource {
    queue: VecDeque<[u8; WindowEvent::WIRE_LEN]>,
}

impl EventSource for QueueSource {
    fn next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno> {
        let frame = self.queue.pop_front().ok_or(Errno::WouldBlock)?;
        *event = frame;
        Ok(())
    }
}

/// Full damage over SURFACE.
fn full_damage() -> DamageRect {
    DamageRect::full(&SURFACE)
}

#[test]
fn create_present_close_round_trips_through_the_loopback() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = client
        .create(7, EVENTS_A, 2, &SURFACE, "Files")
        .expect("a valid create succeeds");
    assert_eq!(window, 1);
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 1);
        assert_eq!(
            inner.host.opened,
            alloc::vec![(1, SURFACE, String::from("Files"))]
        );
    }

    // Present the second frame: the host sees exactly that frame's bytes.
    let damage = DamageRect {
        x: 1,
        y: 1,
        width_px: 2,
        height_px: 2,
    };
    client.present(window, 1, damage).expect("present succeeds");
    {
        let inner = loopback.borrow();
        let (id, frame, seen) = &inner.host.presented[0];
        assert_eq!(*id, window);
        assert_eq!(*seen, damage);
        let expected: Vec<u8> = (FRAME_LEN..2 * FRAME_LEN)
            .map(|i| region_byte(7, i))
            .collect();
        assert_eq!(*frame, expected);
    }

    client.close(window).expect("close succeeds");
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 0);
        assert_eq!(inner.host.closed, alloc::vec![window]);
    }
    // The window is gone: a second close finds nothing.
    assert_eq!(client.close(window), Err(Errno::NotFound));
}

#[test]
fn window_ids_are_minted_monotonically_and_never_reused() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let first = client.create(7, EVENTS_A, 1, &SURFACE, "a").expect("first");
    let second = client
        .create(7, EVENTS_A, 1, &SURFACE, "b")
        .expect("second");
    client.close(first).expect("close");
    let third = client.create(7, EVENTS_A, 1, &SURFACE, "c").expect("third");
    assert_eq!((first, second, third), (1, 2, 3));
}

#[test]
fn a_caller_cannot_touch_another_clients_window() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = client
        .create(7, EVENTS_A, 1, &SURFACE, "A's")
        .expect("A creates");

    // B presents and closes A's window: refused exactly like a window
    // that does not exist, and A's window survives untouched.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.present(window, 0, full_damage()),
        Err(Errno::NotFound)
    );
    assert_eq!(client.close(window), Err(Errno::NotFound));
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 1);
        assert!(inner.host.closed.is_empty());
    }

    // A still owns it.
    loopback.borrow_mut().ticket = TICKET_A;
    client
        .present(window, 0, full_damage())
        .expect("A presents");
}

#[test]
fn create_is_refused_fail_closed() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN), (8, FRAME_LEN - 1)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    // An unknown grant handle.
    assert_eq!(
        client.create(99, EVENTS_A, 1, &SURFACE, "x"),
        Err(Errno::NotFound)
    );
    // A region too small for the frames it claims to hold.
    assert_eq!(
        client.create(8, EVENTS_A, 1, &SURFACE, "x"),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        client.create(7, EVENTS_A, 3, &SURFACE, "x"),
        Err(Errno::LengthOutOfRange)
    );
    // A kernel-domain caller is not a window client.
    loopback.borrow_mut().ticket = TICKET_KERNEL;
    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x"),
        Err(Errno::PermissionDenied)
    );
    // A caller the kernel cannot attest.
    loopback.borrow_mut().ticket = TICKET_UNATTESTED;
    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x"),
        Err(Errno::NotFound)
    );
    // Nothing leaked out of any refusal.
    let inner = loopback.borrow();
    assert_eq!(inner.server.window_count(), 0);
    assert!(inner.host.opened.is_empty());
}

#[test]
fn a_client_is_bounded_to_its_window_cap() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    for _ in 0..WINDOWS_PER_CLIENT_MAX {
        client
            .create(7, EVENTS_A, 1, &SURFACE, "w")
            .expect("in cap");
    }
    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "w"),
        Err(Errno::NoSpace)
    );
    // Another client still has its own budget.
    loopback.borrow_mut().ticket = TICKET_B;
    client
        .create(7, EVENTS_B, 1, &SURFACE, "w")
        .expect("B's own cap");
}

#[test]
fn a_refused_host_open_commits_nothing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    loopback.borrow_mut().host.refuse_open = true;
    let mut client = WindowClient::new(Rc::clone(&loopback));

    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x"),
        Err(Errno::WouldBlock)
    );
    loopback.borrow_mut().host.refuse_open = false;
    // The refused create consumed no id.
    let window = client.create(7, EVENTS_A, 1, &SURFACE, "x").expect("retry");
    assert_eq!(window, 1);
}

#[test]
fn present_bounds_are_enforced() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = client
        .create(7, EVENTS_A, 2, &SURFACE, "x")
        .expect("create");

    // A frame index past the created count.
    assert_eq!(
        client.present(window, 2, full_damage()),
        Err(Errno::OutOfRange)
    );
    // Damage falling outside the surface.
    let outside = DamageRect {
        x: 3,
        y: 0,
        width_px: 2,
        height_px: 1,
    };
    assert_eq!(
        client.present(window, 0, outside),
        Err(Errno::LengthOutOfRange)
    );
    assert!(loopback.borrow().host.presented.is_empty());
}

#[test]
fn a_dead_clients_windows_are_torn_down_and_others_survive() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let a1 = client.create(7, EVENTS_A, 1, &SURFACE, "a1").expect("a1");
    let a2 = client.create(7, EVENTS_A, 1, &SURFACE, "a2").expect("a2");
    loopback.borrow_mut().ticket = TICKET_B;
    let b1 = client.create(7, EVENTS_B, 1, &SURFACE, "b1").expect("b1");

    {
        let inner = &mut *loopback.borrow_mut();
        inner.server.client_exited(&mut inner.host, proc_id(0xA1));
        assert_eq!(inner.server.window_count(), 1);
        assert_eq!(inner.host.closed, alloc::vec![a1, a2]);
    }
    // B's window still presents.
    client.present(b1, 0, full_damage()).expect("b1 lives");
}

#[test]
fn a_malformed_request_answers_a_typed_status_refusal() {
    let loopback = Loopback::with_regions(&[]);
    let mut reply = [0u8; WINDOW_REPLY_MAX];
    let inner = &mut *loopback.borrow_mut();
    let len = inner.server.serve(
        &mut inner.host,
        &mut inner.identity,
        TICKET_A,
        &[0u8; 4],
        &mut reply,
    );
    assert_eq!(
        decode_status_reply(&reply[..len]),
        Err(Errno::BufferTooSmall)
    );
    let len = inner.server.serve(
        &mut inner.host,
        &mut inner.identity,
        TICKET_A,
        &[0xFFu8; 112],
        &mut reply,
    );
    assert_eq!(decode_status_reply(&reply[..len]), Err(Errno::BadMagic));
}

#[test]
fn events_reach_the_owning_endpoint_and_decode_through_the_client() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let a = client.create(7, EVENTS_A, 1, &SURFACE, "a").expect("a");
    loopback.borrow_mut().ticket = TICKET_B;
    let b = client.create(7, EVENTS_B, 1, &SURFACE, "b").expect("b");

    let key = KeyInput::Pressed {
        key: KeyValue::Char('x'),
        modifiers: Modifiers::default(),
    };
    let events = [
        WindowEvent::Focus {
            window_id: a,
            focused: true,
        },
        WindowEvent::Key { window_id: a, key },
        WindowEvent::Pointer {
            window_id: b,
            x: 3,
            y: 2,
            action: PointerAction::Pressed(PointerButtonCode::Primary),
        },
        WindowEvent::CloseRequested { window_id: a },
    ];
    let mut sink = QueueSink::default();
    {
        let inner = loopback.borrow();
        for event in &events {
            inner
                .server
                .deliver_event(&mut sink, event)
                .expect("routed");
        }
    }
    // Each event reached its owner's endpoint.
    let endpoints: Vec<u64> = sink.delivered.iter().map(|(e, _)| *e).collect();
    assert_eq!(
        endpoints,
        alloc::vec![EVENTS_A, EVENTS_A, EVENTS_B, EVENTS_A]
    );

    // The app-side wait decodes exactly what was routed to it.
    let queue: VecDeque<[u8; WindowEvent::WIRE_LEN]> = sink
        .delivered
        .iter()
        .filter(|(endpoint, _)| *endpoint == EVENTS_A)
        .map(|(_, frame)| *frame)
        .collect();
    let mut waiter = WindowEvents::new(QueueSource { queue });
    assert_eq!(
        waiter.wait(),
        Ok(WindowEvent::Focus {
            window_id: a,
            focused: true
        })
    );
    assert_eq!(waiter.wait(), Ok(WindowEvent::Key { window_id: a, key }));
    assert_eq!(
        waiter.wait(),
        Ok(WindowEvent::CloseRequested { window_id: a })
    );
    // An empty queue surfaces the source's wait condition, never a
    // fabricated event.
    assert_eq!(waiter.wait(), Err(Errno::WouldBlock));
}

#[test]
fn event_routing_fails_closed() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = client.create(7, EVENTS_A, 1, &SURFACE, "a").expect("a");

    let mut sink = QueueSink::default();
    let inner = loopback.borrow();
    // An unknown window routes nowhere.
    assert_eq!(
        inner
            .server
            .deliver_event(&mut sink, &WindowEvent::CloseRequested { window_id: 99 }),
        Err(Errno::NotFound)
    );
    // A pointer position outside the window's surface is a routing bug,
    // refused rather than delivered.
    assert_eq!(
        inner.server.deliver_event(
            &mut sink,
            &WindowEvent::Pointer {
                window_id: window,
                x: SURFACE.width_px,
                y: 0,
                action: PointerAction::Moved,
            }
        ),
        Err(Errno::OutOfRange)
    );
    assert!(sink.delivered.is_empty());
}
