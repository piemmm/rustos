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

use tairix_abi::desktop::{Appearance, DesktopInfo};
use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_abi::input::{KeyInput, KeyValue, Modifiers, PointerButtonCode};
use tairix_abi::origin::{ProcId, PROC_ID_LEN};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::window_ipc::{PointerAction, WindowEvent, WindowRequest};
use tairix_abi::Errno;
use tairix_display::{FrameRegion, ShmMapper};
use tairix_geometry::{Rect, Scale};

use crate::client::{EventSource, WindowClient, WindowEvents, WindowTransport};
use crate::desktop::Desktop;
use crate::server::{
    CallerIdentity, EventSink, PinDecision, WindowHost, WindowServer, WINDOWS_PER_CLIENT_MAX,
    WINDOW_REPLY_MAX,
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

/// The serving session identity the loopback server stamps into every
/// successful create reply.
const SERVER: ProcId = ProcId::from_raw([0x5D; PROC_ID_LEN]);

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

/// A host recording every bridge call, optionally refusing opens, picker
/// requests, pins, and drag offers.
struct RecordingHost {
    opened: Vec<(u64, DisplayMode, String, bool)>,
    presented: Vec<(u64, Vec<u8>, DamageRect)>,
    resized: Vec<(u64, DisplayMode)>,
    closed: Vec<u64>,
    picks: Vec<u64>,
    pins: Vec<(ProcId, u64, String)>,
    drag_offers: Vec<(ProcId, u64, String)>,
    drag_withdraws: Vec<(ProcId, u64)>,
    blur_sets: Vec<(u64, u16)>,
    refuse_open: bool,
    refuse_resize: Option<Errno>,
    refuse_pick: Option<Errno>,
    /// The outcome the next `PinBundle` receives.
    pin_decision: PinDecision,
    /// Whether the next `DragOffer` is accepted.
    drag_offer_accepts: bool,
    /// The desktop this host composites, or the refusal a host with no
    /// screen to describe answers with.
    desktop: Result<DesktopInfo, Errno>,
}

impl Default for RecordingHost {
    fn default() -> Self {
        Self {
            opened: Vec::new(),
            presented: Vec::new(),
            resized: Vec::new(),
            closed: Vec::new(),
            picks: Vec::new(),
            pins: Vec::new(),
            drag_offers: Vec::new(),
            drag_withdraws: Vec::new(),
            blur_sets: Vec::new(),
            refuse_open: false,
            refuse_resize: None,
            refuse_pick: None,
            pin_decision: PinDecision::Pinned,
            drag_offer_accepts: true,
            desktop: Ok(sample_desktop()),
        }
    }
}

/// The desktop the host tests report: a 1024x768 screen at the reference
/// density, in the default dark appearance.
fn sample_desktop() -> DesktopInfo {
    match DesktopInfo::new(1024, 768, 100, Appearance::Dark) {
        Ok(info) => info,
        Err(_) => unreachable!("a 1024x768 screen at 100% is in range"),
    }
}

impl WindowHost for RecordingHost {
    fn window_opened(
        &mut self,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
        resizable: bool,
    ) -> Result<(), Errno> {
        if self.refuse_open {
            return Err(Errno::WouldBlock);
        }
        self.opened
            .push((window_id, *surface, String::from(title), resizable));
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

    fn window_resized(&mut self, window_id: u64, surface: &DisplayMode) -> Result<(), Errno> {
        if let Some(err) = self.refuse_resize {
            return Err(err);
        }
        self.resized.push((window_id, *surface));
        Ok(())
    }

    fn window_closed(&mut self, window_id: u64) {
        self.closed.push(window_id);
    }

    fn pick_requested(&mut self, window_id: u64) -> Result<(), Errno> {
        if let Some(err) = self.refuse_pick {
            return Err(err);
        }
        self.picks.push(window_id);
        Ok(())
    }

    fn pin_requested(&mut self, owner: ProcId, window: u64, path: &str) -> PinDecision {
        self.pins.push((owner, window, String::from(path)));
        self.pin_decision
    }

    fn drag_offered(&mut self, owner: ProcId, window: u64, path: &str) -> bool {
        self.drag_offers.push((owner, window, String::from(path)));
        self.drag_offer_accepts
    }

    fn drag_withdrawn(&mut self, owner: ProcId, window: u64) {
        self.drag_withdraws.push((owner, window));
    }

    fn backdrop_blur_set(&mut self, window: u64, radius_px: u16) {
        self.blur_sets.push((window, radius_px));
    }

    fn desktop(&mut self) -> Result<DesktopInfo, Errno> {
        self.desktop
    }
}

/// A host implementing only the mandatory bridge methods, so the
/// trait's fail-closed defaults for pinning and dragging run untouched.
struct MinimalHost;

impl WindowHost for MinimalHost {
    fn window_opened(
        &mut self,
        _window_id: u64,
        _surface: &DisplayMode,
        _title: &str,
        _resizable: bool,
    ) -> Result<(), Errno> {
        Ok(())
    }

    fn window_presented(
        &mut self,
        _window_id: u64,
        _surface: &DisplayMode,
        _frame: &[u8],
        _damage: DamageRect,
    ) -> Result<(), Errno> {
        Ok(())
    }

    fn window_resized(&mut self, _window_id: u64, _surface: &DisplayMode) -> Result<(), Errno> {
        Ok(())
    }

    fn window_closed(&mut self, _window_id: u64) {}

    fn pick_requested(&mut self, _window_id: u64) -> Result<(), Errno> {
        Ok(())
    }

    fn desktop(&mut self) -> Result<DesktopInfo, Errno> {
        Ok(sample_desktop())
    }
}

/// A sink recording each delivered event by endpoint, doubling as the
/// backing queue the client-side event source pops.
#[derive(Default)]
struct QueueSink {
    delivered: VecDeque<(u64, [u8; WindowEvent::WIRE_LEN])>,
}

impl EventSink for QueueSink {
    fn deliver(&mut self, endpoint: u64, event: &WindowEvent) -> Result<(), Errno> {
        self.delivered.push_back((endpoint, event.to_le_bytes()));
        Ok(())
    }
}

/// A sink that refuses everything with the owner's mailbox-full signal — the
/// back-pressure a session takes responsibility for.
struct FullSink;

impl EventSink for FullSink {
    fn deliver(&mut self, _endpoint: u64, _event: &WindowEvent) -> Result<(), Errno> {
        Err(Errno::WouldBlock)
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
            server: WindowServer::new(MockMapper::with_regions(regions), SERVER),
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

/// Create through `client`, returning just the minted window id (the
/// server stamp is asserted once in the loopback round trip).
fn create_id(
    client: &mut WindowClient<Rc<RefCell<Loopback>>>,
    shm: u64,
    events: u64,
    frames: u32,
    title: &str,
) -> Result<u64, Errno> {
    client
        .create(shm, events, frames, &SURFACE, title, false)
        .map(|(id, _)| id)
}

#[test]
fn an_app_learns_its_desktop_before_it_owns_a_window() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    // Nothing has been created: the query names no window precisely so an
    // app can size its first one to a screen it knows.
    assert_eq!(loopback.borrow().server.window_count(), 0);
    let info = client.desktop().expect("the session describes its desktop");
    assert_eq!(info, sample_desktop());

    let desktop = Desktop::new(info).expect("the reported scale is one Scale admits");
    assert_eq!(desktop.screen(), Rect::new(0, 0, 1024, 768));
    assert_eq!(desktop.scale(), Scale::ONE);
    assert_eq!(desktop.appearance(), Appearance::Dark);
    // At the reference density a logical size is its own physical size,
    // so the app's own choice stands — unless it exceeds the screen, which
    // caps it.
    assert_eq!(desktop.window_size(640, 480), (640, 480));
    assert_eq!(desktop.window_size(2000, 900), (1024, 768));
}

#[test]
fn a_window_is_sized_at_the_desktops_density_and_capped_to_the_screen() {
    let dense = DesktopInfo::new(1024, 768, 200, Appearance::Dark)
        .expect("a 1024x768 screen at 200% is in range");
    let desktop = Desktop::new(dense).expect("200% is one Scale admits");

    // The app authors its preference in logical pixels, so at twice the
    // density it asks for twice the pixels...
    assert_eq!(desktop.window_size(320, 240), (640, 480));
    // ...but never more than the screen it must appear on, which is what
    // a fixed-size window authored for a roomier desktop would otherwise
    // ask for once doubled.
    assert_eq!(desktop.window_size(800, 600), (1024, 768));
}

#[test]
fn a_session_with_no_desktop_refuses_rather_than_inventing_one() {
    let loopback = Loopback::with_regions(&[]);
    loopback.borrow_mut().host.desktop = Err(Errno::NotFound);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    assert_eq!(client.desktop(), Err(Errno::NotFound));
}

#[test]
fn a_desktop_change_reaches_every_live_window() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let first = create_id(&mut client, 7, EVENTS_A, 1, "Files").expect("first window opens");
    let second = create_id(&mut client, 8, EVENTS_A, 1, "Viewer").expect("second window opens");

    let switched = DesktopInfo::new(1024, 768, 100, Appearance::Light)
        .expect("a 1024x768 screen at 100% is in range");
    let mut sink = QueueSink::default();
    {
        let inner = &mut *loopback.borrow_mut();
        // The desktop belongs to the seat, so the session reaches every
        // window rather than guessing which apps care.
        assert_eq!(inner.server.window_ids(), alloc::vec![first, second]);
        for window_id in inner.server.window_ids() {
            inner
                .server
                .deliver_event(
                    &mut sink,
                    &WindowEvent::DesktopChanged {
                        window_id,
                        desktop: switched,
                    },
                )
                .expect("a live window takes the event");
        }
    }
    assert_eq!(sink.delivered.len(), 2);

    // The app side adopts the change once and reports it as news only the
    // first time, so a repeated announcement costs no repaint.
    let mut desktop = Desktop::new(sample_desktop()).expect("the sample scale is in range");
    let mut delivered = sink
        .delivered
        .iter()
        .map(|(_, frame)| WindowEvent::from_bytes(frame).expect("a delivered event decodes"));
    let first_event = delivered.next().expect("the first window was told");
    assert_eq!(desktop.apply(&first_event), Ok(true));
    assert_eq!(desktop.appearance(), Appearance::Light);
    let second_event = delivered.next().expect("the second window was told");
    assert_eq!(desktop.apply(&second_event), Ok(false));

    // An unrelated event is not a desktop change and leaves it alone.
    assert_eq!(
        desktop.apply(&WindowEvent::CloseRequested { window_id: first }),
        Ok(false)
    );
    assert_eq!(desktop.appearance(), Appearance::Light);
}

#[test]
fn a_desktop_the_client_cannot_draw_at_is_refused_and_the_last_good_one_stands() {
    let mut desktop = Desktop::new(sample_desktop()).expect("the sample scale is in range");

    // The wire admits any non-zero percentage; what a *usable* scale is
    // belongs to the geometry type, so a percentage outside its range is
    // refused rather than clamped to something the session did not ask
    // for.
    let absurd = DesktopInfo::new(1024, 768, 5, Appearance::Light)
        .expect("the wire accepts any non-zero percentage");
    assert_eq!(Desktop::new(absurd), Err(Errno::OutOfRange));
    assert_eq!(
        desktop.apply(&WindowEvent::DesktopChanged {
            window_id: 1,
            desktop: absurd,
        }),
        Err(Errno::OutOfRange)
    );
    assert_eq!(desktop.scale(), Scale::ONE);
    assert_eq!(desktop.appearance(), Appearance::Dark);
}

#[test]
fn create_present_close_round_trips_through_the_loopback() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let (window, server) = client
        .create(7, EVENTS_A, 2, &SURFACE, "Files", false)
        .expect("a valid create succeeds");
    assert_eq!(window, 1);
    assert_eq!(
        server, SERVER,
        "the reply is stamped with the session identity apps authenticate events against"
    );
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 1);
        assert_eq!(
            inner.host.opened,
            alloc::vec![(1, SURFACE, String::from("Files"), false)]
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
fn create_forwards_the_resizable_flag_to_the_host() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    client
        .create(7, EVENTS_A, 1, &SURFACE, "fixed", false)
        .expect("fixed window");
    client
        .create(8, EVENTS_A, 1, &SURFACE, "resizable", true)
        .expect("resizable window");

    let inner = loopback.borrow();
    let flags: Vec<bool> = inner
        .host
        .opened
        .iter()
        .map(|(_, _, _, resizable)| *resizable)
        .collect();
    assert_eq!(flags, alloc::vec![false, true]);
}

#[test]
fn resize_remaps_the_window_and_presents_at_the_new_size() {
    // A larger surface the window is resized onto.
    const BIG: DisplayMode = DisplayMode {
        width_px: 8,
        height_px: 3,
        stride_bytes: 32,
        format: DisplayFormat::Bgra8888,
    };
    const BIG_FRAME_LEN: usize = 96;

    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN), (20, 2 * BIG_FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = create_id(&mut client, 7, EVENTS_A, 2, "Files").expect("create succeeds");

    // Re-map the window onto the larger region: the host is told the new
    // geometry, the id and count are unchanged.
    client.resize(window, 20, 2, &BIG).expect("resize succeeds");
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 1);
        assert_eq!(inner.host.resized, alloc::vec![(window, BIG)]);
    }

    // A present now shapes its frame against the new surface: full damage
    // of the *larger* surface is accepted (it would have been out of
    // bounds against the old one).
    let damage = DamageRect::full(&BIG);
    client
        .present(window, 1, damage)
        .expect("present at new size");
    {
        let inner = loopback.borrow();
        let (id, frame, seen) = inner.host.presented.last().expect("a present");
        assert_eq!(*id, window);
        assert_eq!(*seen, damage);
        assert_eq!(frame.len(), BIG_FRAME_LEN);
    }
}

#[test]
fn resize_is_refused_fail_closed() {
    const BIG: DisplayMode = DisplayMode {
        width_px: 8,
        height_px: 3,
        stride_bytes: 32,
        format: DisplayFormat::Bgra8888,
    };

    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN), (20, 2 * 96)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = create_id(&mut client, 7, EVENTS_A, 2, "Files").expect("create succeeds");

    // A window the caller does not own answers like one that never
    // existed, and the host is never told.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(client.resize(window, 20, 2, &BIG), Err(Errno::NotFound));
    loopback.borrow_mut().ticket = TICKET_A;

    // A host that refuses the resize leaves the old geometry intact: the
    // window still presents at its original size, not the (refused) new one.
    loopback.borrow_mut().host.refuse_resize = Some(Errno::WouldBlock);
    assert_eq!(client.resize(window, 20, 2, &BIG), Err(Errno::WouldBlock));
    loopback.borrow_mut().host.refuse_resize = None;
    client
        .present(window, 0, full_damage())
        .expect("still presents at the original size");
    // Full damage of the larger surface is still out of bounds — the
    // window was never actually resized.
    assert_eq!(
        client.present(window, 0, DamageRect::full(&BIG)),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn window_ids_are_minted_monotonically_and_never_reused() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let first = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("first");
    let second = create_id(&mut client, 7, EVENTS_A, 1, "b").expect("second");
    client.close(first).expect("close");
    let third = create_id(&mut client, 7, EVENTS_A, 1, "c").expect("third");
    assert_eq!((first, second, third), (1, 2, 3));
}

#[test]
fn a_caller_cannot_touch_another_clients_window() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = create_id(&mut client, 7, EVENTS_A, 1, "A's").expect("A creates");

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
        create_id(&mut client, 99, EVENTS_A, 1, "x"),
        Err(Errno::NotFound)
    );
    // A region too small for the frames it claims to hold.
    assert_eq!(
        create_id(&mut client, 8, EVENTS_A, 1, "x"),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        create_id(&mut client, 7, EVENTS_A, 3, "x"),
        Err(Errno::LengthOutOfRange)
    );
    // A kernel-domain caller is not a window client.
    loopback.borrow_mut().ticket = TICKET_KERNEL;
    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x", false),
        Err(Errno::PermissionDenied)
    );
    // A caller the kernel cannot attest.
    loopback.borrow_mut().ticket = TICKET_UNATTESTED;
    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x", false),
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
        create_id(&mut client, 7, EVENTS_A, 1, "w").expect("in cap");
    }
    assert_eq!(
        create_id(&mut client, 7, EVENTS_A, 1, "w"),
        Err(Errno::NoSpace)
    );
    // Another client still has its own budget.
    loopback.borrow_mut().ticket = TICKET_B;
    create_id(&mut client, 7, EVENTS_B, 1, "w").expect("B's own cap");
}

#[test]
fn a_refused_host_open_commits_nothing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    loopback.borrow_mut().host.refuse_open = true;
    let mut client = WindowClient::new(Rc::clone(&loopback));

    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x", false),
        Err(Errno::WouldBlock)
    );
    loopback.borrow_mut().host.refuse_open = false;
    // The refused create consumed no id.
    let window = create_id(&mut client, 7, EVENTS_A, 1, "x").expect("retry");
    assert_eq!(window, 1);
}

#[test]
fn present_bounds_are_enforced() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 2, "x").expect("create");

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

    let a1 = create_id(&mut client, 7, EVENTS_A, 1, "a1").expect("a1");
    let a2 = create_id(&mut client, 7, EVENTS_A, 1, "a2").expect("a2");
    loopback.borrow_mut().ticket = TICKET_B;
    let b1 = create_id(&mut client, 7, EVENTS_B, 1, "b1").expect("b1");

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
        &[0xFFu8; WindowRequest::WIRE_LEN],
        &mut reply,
    );
    assert_eq!(decode_status_reply(&reply[..len]), Err(Errno::BadMagic));
}

#[test]
fn events_reach_the_owning_endpoint_and_decode_through_the_client() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let a = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    loopback.borrow_mut().ticket = TICKET_B;
    let b = create_id(&mut client, 7, EVENTS_B, 1, "b").expect("b");

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
        let mut inner = loopback.borrow_mut();
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
        waiter.wait(&mut client),
        Ok(WindowEvent::Focus {
            window_id: a,
            focused: true
        })
    );
    assert_eq!(
        waiter.wait(&mut client),
        Ok(WindowEvent::Key { window_id: a, key })
    );
    assert_eq!(
        waiter.wait(&mut client),
        Ok(WindowEvent::CloseRequested { window_id: a })
    );
    // An empty queue surfaces the source's wait condition, never a
    // fabricated event.
    assert_eq!(waiter.wait(&mut client), Err(Errno::WouldBlock));
}

/// One queued redraw request for `window`, ready for the typed wait.
fn redraw_queue(window: u64) -> VecDeque<[u8; WindowEvent::WIRE_LEN]> {
    let mut queue = VecDeque::new();
    queue.push_back(WindowEvent::RedrawRequested { window_id: window }.to_le_bytes());
    queue
}

#[test]
fn a_redraw_request_re_presents_the_last_frame_without_the_app_acting() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 2, "a").expect("a");

    // A client that has never presented has no frame to re-send: the
    // request is a no-op, not an error.
    let mut waiter = WindowEvents::new(QueueSource {
        queue: redraw_queue(window),
    });
    assert_eq!(
        waiter.wait(&mut client),
        Ok(WindowEvent::RedrawRequested { window_id: window })
    );
    assert!(loopback.borrow().host.presented.is_empty());

    // After a present the library re-sends that frame with full-window
    // damage, so the app sees the event but need do nothing. The partial
    // damage of the original present is deliberately widened: a
    // re-established surface starts transparent, so only a full-window
    // present makes it correct again.
    let partial = DamageRect {
        x: 1,
        y: 1,
        width_px: 2,
        height_px: 1,
    };
    client.present(window, 1, partial).expect("present");
    assert_eq!(loopback.borrow().host.presented.len(), 1);
    let mut waiter = WindowEvents::new(QueueSource {
        queue: redraw_queue(window),
    });
    assert_eq!(
        waiter.wait(&mut client),
        Ok(WindowEvent::RedrawRequested { window_id: window })
    );
    {
        let inner = loopback.borrow();
        assert_eq!(inner.host.presented.len(), 2);
        let (id, _, seen) = inner.host.presented.last().expect("the re-present");
        assert_eq!(*id, window);
        assert_eq!(*seen, full_damage());
    }

    // A request naming a window this client does not own re-presents
    // nothing (fail closed).
    assert_eq!(client.answer_redraw(window + 500), Ok(false));
    assert_eq!(loopback.borrow().host.presented.len(), 2);
}

#[test]
fn event_routing_fails_closed() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    let mut sink = QueueSink::default();
    let mut inner = loopback.borrow_mut();
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
    // A pick conclusion no request preceded is a session bug, refused
    // rather than delivered — for both conclusions.
    assert_eq!(
        inner.server.deliver_event(
            &mut sink,
            &WindowEvent::FilePicked {
                window_id: window,
                handle: 7,
            }
        ),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        inner
            .server
            .deliver_event(&mut sink, &WindowEvent::PickCancelled { window_id: window }),
        Err(Errno::OutOfRange)
    );
    assert!(sink.delivered.is_empty());
}

#[test]
fn pick_file_is_owner_bound_single_pending_and_concluded_by_delivery() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    // A window the caller does not own answers exactly like one that
    // never existed.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(client.pick_file(window), Err(Errno::NotFound));
    loopback.borrow_mut().ticket = TICKET_A;

    // The owner's request reaches the host and pends; a second request
    // while pending is refused without touching the host again.
    client.pick_file(window).expect("pick accepted");
    assert_eq!(loopback.borrow().host.picks, alloc::vec![window]);
    assert_eq!(client.pick_file(window), Err(Errno::AlreadyExists));
    assert_eq!(loopback.borrow().host.picks, alloc::vec![window]);

    // The conclusion delivers to the owner's endpoint and clears the
    // pending pick, so the app may ask again.
    let mut sink = QueueSink::default();
    loopback
        .borrow_mut()
        .server
        .deliver_event(
            &mut sink,
            &WindowEvent::FilePicked {
                window_id: window,
                handle: 9,
            },
        )
        .expect("conclusion delivered");
    assert_eq!(sink.delivered.len(), 1);
    assert_eq!(sink.delivered[0].0, EVENTS_A);
    // Exactly one conclusion follows each acceptance: a second one is
    // refused until a new pick is accepted.
    assert_eq!(
        loopback
            .borrow_mut()
            .server
            .deliver_event(&mut sink, &WindowEvent::PickCancelled { window_id: window }),
        Err(Errno::OutOfRange)
    );
    client.pick_file(window).expect("a fresh pick is accepted");
    loopback
        .borrow_mut()
        .server
        .deliver_event(&mut sink, &WindowEvent::PickCancelled { window_id: window })
        .expect("the cancel conclusion delivers");
}

/// A conclusion the sink refuses leaves the pick pending, so the window can
/// still be told later.
///
/// This is what makes a session's hold-back the fix rather than a
/// convenience: a sink that drops a refused conclusion would leave this
/// window's pick pending for the rest of its life — every later `pick_file`
/// refused `AlreadyExists`, with no conclusion that can ever clear it — while
/// a sink that *accepts* it (because it will deliver it from a hold-back)
/// concludes the pick exactly once, as the protocol says.
#[test]
fn a_conclusion_the_sink_refuses_stays_pending_until_one_is_accepted() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    client.pick_file(window).expect("pick accepted");

    assert_eq!(
        loopback.borrow_mut().server.deliver_event(
            &mut FullSink,
            &WindowEvent::PickCancelled { window_id: window }
        ),
        Err(Errno::WouldBlock),
        "a full mailbox is relayed, not swallowed"
    );
    assert_eq!(
        client.pick_file(window),
        Err(Errno::AlreadyExists),
        "the pick is still pending, so no second one starts"
    );

    // The session takes responsibility for the conclusion and the pick ends.
    let mut sink = QueueSink::default();
    loopback
        .borrow_mut()
        .server
        .deliver_event(&mut sink, &WindowEvent::PickCancelled { window_id: window })
        .expect("an accepted conclusion concludes it");
    assert_eq!(sink.delivered.len(), 1);
    client.pick_file(window).expect("the window may pick again");
}

#[test]
fn a_refused_picker_leaves_no_pending_pick() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    // The host cannot run a picker (its one slot is taken, or it holds
    // no filesystem authority): the refusal is relayed verbatim and no
    // pick pends — a conclusion is still refused, and a later request
    // (once the host recovers) is accepted.
    loopback.borrow_mut().host.refuse_pick = Some(Errno::AlreadyExists);
    assert_eq!(client.pick_file(window), Err(Errno::AlreadyExists));
    let mut sink = QueueSink::default();
    assert_eq!(
        loopback
            .borrow_mut()
            .server
            .deliver_event(&mut sink, &WindowEvent::PickCancelled { window_id: window }),
        Err(Errno::OutOfRange)
    );
    loopback.borrow_mut().host.refuse_pick = None;
    client
        .pick_file(window)
        .expect("accepted once the host can");
}

#[test]
fn pin_bundle_is_owner_bound_and_reaches_the_host_with_the_right_arguments() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    // A window the caller does not own answers exactly like one that
    // never existed, and the host is never told.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.pin_bundle(window, "/Apps/Editor.app"),
        Err(Errno::NotFound)
    );
    assert!(loopback.borrow().host.pins.is_empty());
    loopback.borrow_mut().ticket = TICKET_A;

    // The owner's request reaches the host with its attested identity,
    // the window, and the exact path.
    client
        .pin_bundle(window, "/Apps/Editor.app")
        .expect("pinned");
    assert_eq!(
        loopback.borrow().host.pins,
        alloc::vec![(proc_id(0xA1), window, String::from("/Apps/Editor.app"))]
    );
}

#[test]
fn pin_decision_maps_to_the_documented_status() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    for (decision, expected) in [
        (PinDecision::Pinned, Ok(())),
        (PinDecision::AlreadyPinned, Err(Errno::AlreadyExists)),
        (PinDecision::Full, Err(Errno::NoSpace)),
        (PinDecision::Refused, Err(Errno::PermissionDenied)),
    ] {
        loopback.borrow_mut().host.pin_decision = decision;
        assert_eq!(client.pin_bundle(window, "/Apps/Editor.app"), expected);
    }
}

#[test]
fn drag_offer_and_withdraw_are_owner_bound_and_reach_the_host() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    // A window the caller does not own is refused for both requests,
    // and the host is never told.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.drag_offer(window, "/Apps/Editor.app"),
        Err(Errno::NotFound)
    );
    assert_eq!(client.drag_withdraw(window), Err(Errno::NotFound));
    assert!(loopback.borrow().host.drag_offers.is_empty());
    assert!(loopback.borrow().host.drag_withdraws.is_empty());
    loopback.borrow_mut().ticket = TICKET_A;

    // The owner's offer reaches the host with its attested identity, the
    // window, and the exact path; the later withdraw reaches it too.
    client
        .drag_offer(window, "/Apps/Editor.app")
        .expect("offered");
    assert_eq!(
        loopback.borrow().host.drag_offers,
        alloc::vec![(proc_id(0xA1), window, String::from("/Apps/Editor.app"))]
    );
    client.drag_withdraw(window).expect("withdrawn");
    assert_eq!(
        loopback.borrow().host.drag_withdraws,
        alloc::vec![(proc_id(0xA1), window)]
    );
}

#[test]
fn set_backdrop_blur_is_owner_bound_and_reaches_the_host() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    // A window the caller does not own is refused, and the host is
    // never told.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(client.set_backdrop_blur(window, 8), Err(Errno::NotFound));
    assert!(loopback.borrow().host.blur_sets.is_empty());
    loopback.borrow_mut().ticket = TICKET_A;

    // The owner's radius reaches the host exactly, and a later call
    // replaces it rather than accumulating.
    client.set_backdrop_blur(window, 12).expect("set");
    assert_eq!(loopback.borrow().host.blur_sets, alloc::vec![(window, 12)]);
    client.set_backdrop_blur(window, 0).expect("disabled");
    assert_eq!(
        loopback.borrow().host.blur_sets,
        alloc::vec![(window, 12), (window, 0)]
    );
}

#[test]
fn a_refused_drag_offer_maps_to_permission_denied() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    loopback.borrow_mut().host.drag_offer_accepts = false;
    assert_eq!(
        client.drag_offer(window, "/Apps/Editor.app"),
        Err(Errno::PermissionDenied)
    );
    // The gesture is never fatal to the window: a withdraw (or a fresh
    // offer once the host recovers) still succeeds.
    assert_eq!(client.drag_withdraw(window), Ok(()));
}

#[test]
fn pin_and_drag_default_to_fail_closed_refusal() {
    // A host implementing only the mandatory bridge methods exercises
    // the trait's own defaults: a host that has not wired up pinning or
    // dragging must refuse rather than silently accept.
    let mapper = MockMapper::with_regions(&[(7, FRAME_LEN)]);
    let mut server = WindowServer::new(mapper, SERVER);
    let mut host = MinimalHost;
    let mut identity = MockIdentity;
    let mut reply = [0u8; WINDOW_REPLY_MAX];

    let create = WindowRequest::Create {
        shm_handle: 7,
        event_endpoint: EVENTS_A,
        frame_count: 1,
        width_px: SURFACE.width_px,
        height_px: SURFACE.height_px,
        stride_bytes: SURFACE.stride_bytes,
        format: SURFACE.format,
        title: tairix_abi::window_ipc::WindowTitle::new("a").expect("valid title"),
        resizable: false,
    }
    .to_le_bytes();
    let len = server.serve(&mut host, &mut identity, TICKET_A, &create, &mut reply);
    let (window, _) = tairix_abi::window_ipc::decode_create_reply(&reply[..len]).expect("created");

    let path = tairix_abi::window_ipc::BundleRef::new("/Apps/Editor.app").expect("valid path");
    let pin = WindowRequest::PinBundle { window, path }.to_le_bytes();
    let len = server.serve(&mut host, &mut identity, TICKET_A, &pin, &mut reply);
    assert_eq!(
        decode_status_reply(&reply[..len]),
        Err(Errno::PermissionDenied)
    );

    let offer = WindowRequest::DragOffer { window, path }.to_le_bytes();
    let len = server.serve(&mut host, &mut identity, TICKET_A, &offer, &mut reply);
    assert_eq!(
        decode_status_reply(&reply[..len]),
        Err(Errno::PermissionDenied)
    );

    // Withdrawing is infallible for an owned window even with no offer
    // to disarm: the default handler simply has nothing to do.
    let withdraw = WindowRequest::DragWithdraw { window }.to_le_bytes();
    let len = server.serve(&mut host, &mut identity, TICKET_A, &withdraw, &mut reply);
    assert_eq!(decode_status_reply(&reply[..len]), Ok(()));

    // Setting the backdrop blur is likewise infallible for an owned
    // window: the default handler has no compositor to tell.
    let blur = WindowRequest::SetBackdropBlur {
        window_id: window,
        radius_px: 8,
    }
    .to_le_bytes();
    let len = server.serve(&mut host, &mut identity, TICKET_A, &blur, &mut reply);
    assert_eq!(decode_status_reply(&reply[..len]), Ok(()));
}

#[test]
fn a_wire_pointer_event_translates_to_the_position_then_the_button() {
    use crate::pointer_input_events;
    use tairix_geometry::Point;
    use tairix_input::{InputEvent, PointerButton};

    let at = Point::new(37, 91);

    // A bare move is one event: the position.
    let moved: Vec<InputEvent> = pointer_input_events(PointerAction::Moved, at).collect();
    assert_eq!(moved, [InputEvent::PointerMoved { to: at }]);

    // Every button transition is preceded by the position it happened at,
    // because the controls' press and release carry no coordinate.
    for (code, button) in [
        (PointerButtonCode::Primary, PointerButton::Primary),
        (PointerButtonCode::Secondary, PointerButton::Secondary),
        (PointerButtonCode::Middle, PointerButton::Middle),
    ] {
        let pressed: Vec<InputEvent> =
            pointer_input_events(PointerAction::Pressed(code), at).collect();
        assert_eq!(
            pressed,
            [
                InputEvent::PointerMoved { to: at },
                InputEvent::PointerPressed { button }
            ]
        );
        let released: Vec<InputEvent> =
            pointer_input_events(PointerAction::Released(code), at).collect();
        assert_eq!(
            released,
            [
                InputEvent::PointerMoved { to: at },
                InputEvent::PointerReleased { button }
            ]
        );
    }
}

#[test]
fn a_wire_key_event_translates_to_the_shared_key_vocabulary() {
    use crate::key_input_event;
    use tairix_abi::input::NamedKeyCode;
    use tairix_input::{InputEvent, Key, NamedKey};

    let held = Modifiers {
        shift: true,
        ctrl: false,
        alt: true,
        meta: false,
    };
    assert_eq!(
        key_input_event(KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Tab),
            modifiers: held,
        }),
        InputEvent::KeyPressed {
            key: Key::Named(NamedKey::Tab),
            modifiers: tairix_input::Modifiers {
                shift: true,
                ctrl: false,
                alt: true,
                meta: false,
            },
        }
    );
    // A release stays a release, and a character key carries its scalar.
    assert_eq!(
        key_input_event(KeyInput::Released {
            key: KeyValue::Char('q'),
            modifiers: Modifiers::default(),
        }),
        InputEvent::KeyReleased {
            key: Key::Char('q'),
            modifiers: tairix_input::Modifiers::default(),
        }
    );
    // The function keys keep their number rather than collapsing together.
    assert_eq!(
        key_input_event(KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::F7),
            modifiers: Modifiers::default(),
        }),
        InputEvent::KeyPressed {
            key: Key::Named(NamedKey::Function { number: 7 }),
            modifiers: tairix_input::Modifiers::default(),
        }
    );
}

#[test]
fn event_endpoint_ids_are_distinct_per_task_and_never_reserved() {
    use crate::event_endpoint_for;

    // Distinct kernel task ids yield distinct mailbox ids, and the id
    // embeds the pid recoverably (tag in the high bits, pid in the low).
    assert_ne!(event_endpoint_for(1), event_endpoint_for(2));
    assert_eq!(event_endpoint_for(0x1234) & 0xFFFF, 0x1234);
    // No app mailbox may land on a reserved well-known rendezvous.
    for pid in [0u64, 1, 7, 0x0000_FFFF_FFFF] {
        assert!(!tairix_abi::ipc::is_reserved_endpoint(event_endpoint_for(
            pid
        )));
    }
}
