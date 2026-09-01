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
use tairix_abi::window_ipc::{
    AppBar, AppBarClick, AppMenu, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuRow,
    AppMenuRowView, MenuAnchor, MenuOutcome, MenuRefusal, PointerAction, WindowEvent,
    WindowRequest, WINDOW_TITLE_MAX,
};
use tairix_abi::Errno;
use tairix_display::{FrameRegion, ShmMapper};
use tairix_geometry::{Point, Rect, Region, Scale};

use crate::client::{
    damage_in, pointer_point, present_damage, EventSource, Repaint, WindowClient, WindowEvents,
    WindowTransport,
};
use crate::desktop::Desktop;
use crate::server::{
    client_frame_budget_bytes, CallerIdentity, EventSink, PopupSpec, WindowHost, WindowServer,
    WindowSizing, WINDOW_REPLY_MAX,
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

/// Window frames one test client may hold: eight [`SURFACE`] frames, so a
/// test can reach the bound in a handful of creates and prove the ninth is
/// refused. A real session derives this from the machine's RAM and its own
/// output ([`client_frame_budget_bytes`]).
const CLIENT_FRAME_MAX: u64 = 8 * FRAME_LEN as u64;

/// Frames a client may hold under [`CLIENT_FRAME_MAX`].
const FRAMES_PER_CLIENT: usize = 8;

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
/// requests.
struct RecordingHost {
    opened: Vec<(ProcId, u64, DisplayMode, String, WindowSizing)>,
    popups: Vec<(u64, u64, i32, i32, DisplayMode)>,
    presented: Vec<(u64, Vec<u8>, DamageRect)>,
    resized: Vec<(u64, DisplayMode)>,
    closed: Vec<u64>,
    picks: Vec<u64>,
    menu_opens: Vec<(u64, u64, MenuAnchor, AppMenu)>,
    blur_sets: Vec<(u64, u16)>,
    retitled: Vec<(u64, String)>,
    app_bars: Vec<(ProcId, AppBar)>,
    app_bars_withdrawn: Vec<ProcId>,
    refuse_app_bar: Option<Errno>,
    refuse_open: bool,
    refuse_popup: bool,
    refuse_resize: Option<Errno>,
    refuse_retitle: Option<Errno>,
    refuse_pick: Option<Errno>,
    refuse_menu_open: Option<Errno>,
    /// The desktop this host composites, or the refusal a host with no
    /// screen to describe answers with.
    desktop: Result<DesktopInfo, Errno>,
}

impl Default for RecordingHost {
    fn default() -> Self {
        Self {
            opened: Vec::new(),
            popups: Vec::new(),
            presented: Vec::new(),
            resized: Vec::new(),
            closed: Vec::new(),
            picks: Vec::new(),
            menu_opens: Vec::new(),
            blur_sets: Vec::new(),
            retitled: Vec::new(),
            app_bars: Vec::new(),
            app_bars_withdrawn: Vec::new(),
            refuse_app_bar: None,
            refuse_open: false,
            refuse_popup: false,
            refuse_resize: None,
            refuse_retitle: None,
            refuse_pick: None,
            refuse_menu_open: None,
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
        owner: ProcId,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
        sizing: WindowSizing,
    ) -> Result<(), Errno> {
        if self.refuse_open {
            return Err(Errno::WouldBlock);
        }
        self.opened
            .push((owner, window_id, *surface, String::from(title), sizing));
        Ok(())
    }

    fn popup_opened(
        &mut self,
        window_id: u64,
        parent_window_id: u64,
        offset_x: i32,
        offset_y: i32,
        surface: &DisplayMode,
    ) -> Result<(), Errno> {
        if self.refuse_popup {
            return Err(Errno::WouldBlock);
        }
        self.popups
            .push((window_id, parent_window_id, offset_x, offset_y, *surface));
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

    fn window_retitled(&mut self, window_id: u64, title: &str) -> Result<(), Errno> {
        if let Some(err) = self.refuse_retitle {
            return Err(err);
        }
        self.retitled.push((window_id, String::from(title)));
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

    fn menu_open_requested(
        &mut self,
        window_id: u64,
        open_id: u64,
        anchor: MenuAnchor,
        menu: &AppMenu,
    ) -> Result<(), Errno> {
        if let Some(err) = self.refuse_menu_open {
            return Err(err);
        }
        self.menu_opens.push((window_id, open_id, anchor, *menu));
        Ok(())
    }

    fn app_bar_declared(&mut self, owner: ProcId, bar: &AppBar) -> Result<(), Errno> {
        if let Some(err) = self.refuse_app_bar {
            return Err(err);
        }
        self.app_bars.push((owner, *bar));
        Ok(())
    }

    fn app_bar_withdrawn(&mut self, owner: ProcId) {
        self.app_bars_withdrawn.push(owner);
    }

    fn backdrop_blur_set(&mut self, window: u64, radius_px: u16) {
        self.blur_sets.push((window, radius_px));
    }

    fn desktop(&mut self) -> Result<DesktopInfo, Errno> {
        self.desktop
    }
}

/// A host implementing only the mandatory bridge methods, so the trait's
/// own defaults run untouched.
struct MinimalHost;

impl WindowHost for MinimalHost {
    fn window_opened(
        &mut self,
        _owner: ProcId,
        _window_id: u64,
        _surface: &DisplayMode,
        _title: &str,
        _sizing: WindowSizing,
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

    fn window_retitled(&mut self, _window_id: u64, _title: &str) -> Result<(), Errno> {
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
    /// Every frame the client put on the wire, in order.
    sent: alloc::vec::Vec<alloc::vec::Vec<u8>>,
}

impl Loopback {
    fn with_regions(regions: &[(u64, usize)]) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            server: WindowServer::new(MockMapper::with_regions(regions), SERVER, CLIENT_FRAME_MAX),
            host: RecordingHost::default(),
            identity: MockIdentity,
            ticket: TICKET_A,
            sent: alloc::vec::Vec::new(),
        }))
    }
}

/// Deliver one app-ward event through the real server, bridged to the
/// loopback's own host — the pair a session holds together, so a teardown
/// the delivery triggers reaches the compositor as it would in production.
fn deliver(
    loopback: &Rc<RefCell<Loopback>>,
    sink: &mut dyn EventSink,
    event: &WindowEvent,
) -> Result<(), Errno> {
    let inner = &mut *loopback.borrow_mut();
    inner.server.deliver_event(sink, event)
}

/// One request encoded exactly as a client sends it: a frame of its own
/// operation's length, which is what the server is given on the wire.
fn request_frame(request: &WindowRequest) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec![0u8; WindowRequest::MAX_WIRE_LEN];
    let len = request.encode(&mut out).expect("the max frame fits");
    out.truncate(len);
    out
}

impl WindowTransport for Rc<RefCell<Loopback>> {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        let mut frame = [0u8; WINDOW_REPLY_MAX];
        let inner = &mut *self.borrow_mut();
        inner.sent.push(request.to_vec());
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
        .create(
            shm,
            events,
            frames,
            &SURFACE,
            title,
            WindowSizing::default(),
        )
        .map(|(id, _)| id)
}

/// A one-frame SURFACE-shaped popup of `parent`, granted as `shm`, its
/// events routed to `events`, offset `(offset_x, offset_y)` from the
/// parent's client origin.
fn popup_spec(
    parent_window_id: u64,
    shm: u64,
    events: u64,
    offset_x: i32,
    offset_y: i32,
) -> PopupSpec {
    PopupSpec {
        parent_window_id,
        shm_handle: shm,
        event_endpoint: events,
        frame_count: 1,
        surface: SURFACE,
        offset_x,
        offset_y,
    }
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
        .create(7, EVENTS_A, 2, &SURFACE, "Files", WindowSizing::default())
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
            alloc::vec![(
                proc_id(0xA1),
                1,
                SURFACE,
                String::from("Files"),
                WindowSizing::default()
            )],
            "the host is told the kernel-attested owner, not anything the client said"
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
fn create_forwards_the_sizing_contract_to_the_host() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let floor = WindowSizing::Resizable {
        min_width_px: 240,
        min_height_px: 160,
    };

    client
        .create(7, EVENTS_A, 1, &SURFACE, "fixed", WindowSizing::Fixed)
        .expect("fixed window");
    client
        .create(8, EVENTS_A, 1, &SURFACE, "resizable", floor)
        .expect("resizable window");

    let inner = loopback.borrow();
    let sizings: Vec<WindowSizing> = inner
        .host
        .opened
        .iter()
        .map(|(_, _, _, _, sizing)| *sizing)
        .collect();
    // The host is the enforcer, so it receives the app's floor verbatim:
    // nothing between the two rounds, clamps, or drops it.
    assert_eq!(sizings, alloc::vec![WindowSizing::Fixed, floor]);
}

#[test]
fn a_below_minimum_resize_is_not_answered_with_a_resize_of_the_client_s_own() {
    // The size is negotiated once, at create: the app declares its floor
    // and the window manager enforces it. A reported size below that floor
    // therefore leaves the wire silent — an app that answered it by
    // resizing itself back up would fight the drag, frame by frame, which
    // is the flicker this negotiation removes.
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let floor = WindowSizing::Resizable {
        min_width_px: 240,
        min_height_px: 160,
    };
    let (window, _) = client
        .create(7, EVENTS_A, 1, &SURFACE, "Files", floor)
        .expect("create");

    let under = WindowEvent::Resized {
        window_id: window,
        width_px: 1,
        height_px: 1,
    };
    let mut queue = VecDeque::new();
    queue.push_back(under.to_le_bytes());
    let mut waiter = WindowEvents::new(QueueSource { queue });
    assert_eq!(waiter.wait(&mut client), Ok(under));

    let inner = loopback.borrow();
    assert!(
        inner.host.resized.is_empty(),
        "the reported size is laid out at, never bounced back as a resize"
    );
    assert_eq!(inner.server.window_count(), 1);
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
    assert_eq!(client.set_title(window, "B's"), Err(Errno::NotFound));
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 1);
        assert!(inner.host.closed.is_empty());
        assert!(inner.host.retitled.is_empty());
    }

    // A still owns it.
    loopback.borrow_mut().ticket = TICKET_A;
    client
        .present(window, 0, full_damage())
        .expect("A presents");
}

#[test]
fn an_owner_retitles_its_window_and_a_refusal_changes_nothing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = create_id(&mut client, 7, EVENTS_A, 1, "Files").expect("A creates");
    client
        .set_title(window, "Files - Documents")
        .expect("the owner retitles");
    assert_eq!(
        loopback.borrow().host.retitled,
        [(window, String::from("Files - Documents"))]
    );

    // A title the protocol refuses never reaches the session.
    let over_long = "t".repeat(WINDOW_TITLE_MAX + 1);
    assert_eq!(
        client.set_title(window, &over_long),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        client.set_title(window, "two\nlines"),
        Err(Errno::OutOfRange)
    );
    // An unknown window is refused, and a host refusal leaves the
    // previous title standing.
    assert_eq!(client.set_title(window + 1, "ghost"), Err(Errno::NotFound));
    loopback.borrow_mut().host.refuse_retitle = Some(Errno::WouldBlock);
    assert_eq!(client.set_title(window, "refused"), Err(Errno::WouldBlock));
    assert_eq!(loopback.borrow().host.retitled.len(), 1);
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
        client.create(7, EVENTS_A, 1, &SURFACE, "x", WindowSizing::default()),
        Err(Errno::PermissionDenied)
    );
    // A caller the kernel cannot attest.
    loopback.borrow_mut().ticket = TICKET_UNATTESTED;
    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x", WindowSizing::default()),
        Err(Errno::NotFound)
    );
    // Nothing leaked out of any refusal.
    let inner = loopback.borrow();
    assert_eq!(inner.server.window_count(), 0);
    assert!(inner.host.opened.is_empty());
}

/// The budget is derived from the machine, so a small board and a large
/// server get proportionate bounds from the same policy — and an
/// unanswered RAM query falls back to the display rather than to nothing.
#[test]
fn the_client_budget_scales_with_the_machine() {
    let screen = 1024 * 768 * 4;
    let small = client_frame_budget_bytes(1 << 30, screen); // 1 GiB
    let large = client_frame_budget_bytes(64 << 30, screen); // 64 GiB
    assert!(small > 0);
    assert_eq!(large, small * 64, "the bound follows the machine's RAM");
    // A screenful is the unit a window is measured in, so even the small
    // machine's bound holds a useful number of them.
    assert!(small > (screen as u64) * 8);
    // No RAM reading: bounded by the display instead of refusing every
    // window, and still bounded.
    let blind = client_frame_budget_bytes(0, screen);
    assert!(blind > 0);
    assert!(blind < small);
}

#[test]
fn a_client_is_bounded_by_the_bytes_it_holds_mapped() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    for _ in 0..FRAMES_PER_CLIENT {
        create_id(&mut client, 7, EVENTS_A, 1, "w").expect("within the budget");
    }
    assert_eq!(
        create_id(&mut client, 7, EVENTS_A, 1, "w"),
        Err(Errno::NoSpace)
    );
    // Another client still has its own budget.
    loopback.borrow_mut().ticket = TICKET_B;
    create_id(&mut client, 7, EVENTS_B, 1, "w").expect("B's own budget");
}

/// The bound is on bytes, so a client asking for *bigger* windows gets
/// correspondingly fewer of them — the thing a per-window count could not
/// express, and the reason it was the wrong bound.
#[test]
fn a_bigger_window_spends_more_of_the_same_budget() {
    const WIDE: DisplayMode = DisplayMode {
        width_px: 4,
        height_px: 3,
        stride_bytes: 16,
        format: DisplayFormat::Bgra8888,
    };
    // Four frames of this surface, so two windows fill the whole budget.
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN * 4)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    for _ in 0..2 {
        client
            .create(7, EVENTS_A, 4, &WIDE, "w", WindowSizing::default())
            .expect("within the budget");
    }
    assert_eq!(
        client.create(7, EVENTS_A, 4, &WIDE, "w", WindowSizing::default()),
        Err(Errno::NoSpace),
        "two four-frame windows spend what eight one-frame windows would"
    );
}

/// Releasing a window's frames is what makes a hidden window cost nothing:
/// the session unmaps its side, the client unmaps its own, and the pages go
/// because both did. Everything else about the window survives, and the next
/// paint re-attaches.
#[test]
fn released_frames_refuse_a_present_and_come_back_on_a_resize() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "w").expect("window");
    client
        .present(window, 0, DamageRect::full(&SURFACE))
        .expect("presents");

    // The session's half of the release.
    let released = loopback.borrow_mut().server.release_frames(window);
    assert_eq!(released, FRAME_LEN as u64, "the mapped bytes are reported");
    assert_eq!(
        loopback.borrow().server.window_count(),
        1,
        "the window itself survives its pixels"
    );
    assert_eq!(
        client.present(window, 0, DamageRect::full(&SURFACE)),
        Err(Errno::NotAttached),
        "a present with nothing attached is refused, not guessed at"
    );
    // The client's own next paint re-attaches with an ordinary resize.
    client
        .resize(window, 8, 1, &SURFACE)
        .expect("a released window re-attaches");
    client
        .present(window, 0, DamageRect::full(&SURFACE))
        .expect("and presents again");
}

/// A released window holds no frames, so its bytes are free for another
/// window: what the client may hold is what it is actually holding.
#[test]
fn released_frames_stop_counting_against_the_client_budget() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut opened = Vec::new();
    for _ in 0..FRAMES_PER_CLIENT {
        opened.push(create_id(&mut client, 7, EVENTS_A, 1, "w").expect("within the budget"));
    }
    assert_eq!(
        create_id(&mut client, 7, EVENTS_A, 1, "w"),
        Err(Errno::NoSpace),
        "the budget is full"
    );
    let released = loopback.borrow_mut().server.release_frames(opened[0]);
    assert_eq!(released, FRAME_LEN as u64);
    create_id(&mut client, 7, EVENTS_A, 1, "w").expect("the released window's bytes are free");
}

/// Releasing twice, or releasing a window that never existed, changes
/// nothing: the session may report a release the client already handled.
#[test]
fn releasing_frames_is_idempotent_and_total() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "w").expect("window");
    assert_eq!(
        loopback.borrow_mut().server.release_frames(window),
        FRAME_LEN as u64
    );
    assert_eq!(
        loopback.borrow_mut().server.release_frames(window),
        0,
        "a second release has nothing left to give back"
    );
    assert_eq!(
        loopback.borrow_mut().server.release_frames(9999),
        0,
        "an unknown window is not an error"
    );
}

/// A resize is the other way a client grows what it holds mapped, and the
/// one a per-window count is blind to.
#[test]
fn a_resize_past_the_budget_is_refused_and_keeps_the_old_geometry() {
    // Eight times as tall as [`SURFACE`], so one frame of it is the client's
    // whole budget and two frames are twice it.
    const TALL: DisplayMode = DisplayMode {
        width_px: 4,
        height_px: 24,
        stride_bytes: 16,
        format: DisplayFormat::Bgra8888,
    };
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN * 8 * 2)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "w").expect("first window");

    assert_eq!(
        client.resize(window, 8, 2, &TALL),
        Err(Errno::NoSpace),
        "a resize is charged against the same budget as a create"
    );
    // Resizing within the budget is still allowed, and the window keeps
    // working: the refusal above changed nothing.
    client
        .resize(window, 8, 1, &TALL)
        .expect("a resize inside the budget");
}

#[test]
fn a_refused_host_open_commits_nothing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    loopback.borrow_mut().host.refuse_open = true;
    let mut client = WindowClient::new(Rc::clone(&loopback));

    assert_eq!(
        client.create(7, EVENTS_A, 1, &SURFACE, "x", WindowSizing::default()),
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
fn create_popup_round_trips_and_present_and_close_act_on_its_own_id() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let parent = create_id(&mut client, 7, EVENTS_A, 1, "Files").expect("parent opens");
    let (popup, server) = client
        .create_popup(&popup_spec(parent, 8, EVENTS_A, 5, -7))
        .expect("popup opens");
    assert_eq!(popup, parent + 1);
    assert_eq!(
        server, SERVER,
        "a popup is stamped with the same session identity as a top-level create"
    );
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 2);
        // The popup went through the undecorated popup path, carrying its
        // parent and placement offsets — never the top-level `window_opened`
        // path, so it is never dressed with chrome or listed on the taskbar.
        assert_eq!(
            inner.host.popups,
            alloc::vec![(popup, parent, 5, -7, SURFACE)]
        );
        assert_eq!(
            inner.host.opened,
            alloc::vec![(
                proc_id(0xA1),
                parent,
                SURFACE,
                String::from("Files"),
                WindowSizing::default()
            )]
        );
    }

    // Present acts on the popup's own id exactly like a top-level window.
    client
        .present(popup, 0, full_damage())
        .expect("present into the popup");
    assert_eq!(loopback.borrow().host.presented[0].0, popup);

    // Closing the popup's own id tears down only the popup; the parent
    // survives.
    client.close(popup).expect("close the popup alone");
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 1);
        assert_eq!(inner.host.closed, alloc::vec![popup]);
    }
    client
        .present(parent, 0, full_damage())
        .expect("parent lives");
}

#[test]
fn a_popup_over_a_foreign_or_unknown_parent_is_refused() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let a_window = create_id(&mut client, 7, EVENTS_A, 1, "A").expect("A's window");

    // A parent the caller does not own answers exactly like one that never
    // existed, leaking nothing about another client's windows.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.create_popup(&popup_spec(a_window, 8, EVENTS_B, 0, 0)),
        Err(Errno::NotFound)
    );
    // An unknown parent id is refused the same way.
    loopback.borrow_mut().ticket = TICKET_A;
    assert_eq!(
        client.create_popup(&popup_spec(9999, 8, EVENTS_A, 0, 0)),
        Err(Errno::NotFound)
    );
    let inner = loopback.borrow();
    assert_eq!(inner.server.window_count(), 1);
    assert!(inner.host.popups.is_empty());
}

#[test]
fn a_popup_counts_against_the_same_per_client_budget() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let parent = create_id(&mut client, 7, EVENTS_A, 1, "w").expect("first window");
    for _ in 1..FRAMES_PER_CLIENT {
        create_id(&mut client, 7, EVENTS_A, 1, "w").expect("within the budget");
    }
    // The client now holds its whole budget; a popup cannot be used to
    // exceed it.
    assert_eq!(
        client.create_popup(&popup_spec(parent, 8, EVENTS_A, 0, 0)),
        Err(Errno::NoSpace)
    );
    assert!(loopback.borrow().host.popups.is_empty());
}

#[test]
fn a_kernel_caller_cannot_open_a_popup() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let parent = create_id(&mut client, 7, EVENTS_A, 1, "w").expect("parent");

    loopback.borrow_mut().ticket = TICKET_KERNEL;
    assert_eq!(
        client.create_popup(&popup_spec(parent, 8, EVENTS_A, 0, 0)),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn a_refused_host_popup_commits_nothing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let parent = {
        let mut client = WindowClient::new(Rc::clone(&loopback));
        let parent = create_id(&mut client, 7, EVENTS_A, 1, "w").expect("parent");
        loopback.borrow_mut().host.refuse_popup = true;
        assert_eq!(
            client.create_popup(&popup_spec(parent, 8, EVENTS_A, 0, 0)),
            Err(Errno::WouldBlock)
        );
        parent
    };
    // The refused popup consumed no id: the next window is the id the
    // popup would have taken.
    loopback.borrow_mut().host.refuse_popup = false;
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let next = create_id(&mut client, 7, EVENTS_A, 1, "w").expect("retry");
    assert_eq!(next, parent + 1);
    assert_eq!(loopback.borrow().server.window_count(), 2);
}

#[test]
fn closing_a_parent_tears_down_its_popups() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN), (9, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let parent = create_id(&mut client, 7, EVENTS_A, 1, "parent").expect("parent");
    let (menu, _) = client
        .create_popup(&popup_spec(parent, 8, EVENTS_A, 0, 0))
        .expect("menu popup");
    let (sheet, _) = client
        .create_popup(&popup_spec(parent, 9, EVENTS_A, 0, 0))
        .expect("sheet popup");
    assert_eq!(loopback.borrow().server.window_count(), 3);

    // Closing the parent closes both popups keyed to it.
    client.close(parent).expect("close parent");
    let inner = loopback.borrow();
    assert_eq!(inner.server.window_count(), 0);
    assert!(inner.host.closed.contains(&parent));
    assert!(inner.host.closed.contains(&menu));
    assert!(inner.host.closed.contains(&sheet));
}

#[test]
fn a_dead_clients_parent_and_popups_are_all_torn_down() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let parent = create_id(&mut client, 7, EVENTS_A, 1, "parent").expect("parent");
    let (popup, _) = client
        .create_popup(&popup_spec(parent, 8, EVENTS_A, 0, 0))
        .expect("popup");

    let inner = &mut *loopback.borrow_mut();
    inner.server.client_exited(&mut inner.host, proc_id(0xA1));
    assert_eq!(inner.server.window_count(), 0);
    assert!(inner.host.closed.contains(&parent));
    assert!(inner.host.closed.contains(&popup));
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
        &[0xFFu8; WindowRequest::MAX_WIRE_LEN],
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
            modifiers: Modifiers::default(),
        },
        WindowEvent::CloseRequested { window_id: a },
    ];
    let mut sink = QueueSink::default();
    {
        let inner = &mut *loopback.borrow_mut();
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
    let inner = &mut *loopback.borrow_mut();
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
                modifiers: Modifiers::default(),
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
    deliver(
        &loopback,
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
        deliver(
            &loopback,
            &mut sink,
            &WindowEvent::PickCancelled { window_id: window }
        ),
        Err(Errno::OutOfRange)
    );
    client.pick_file(window).expect("a fresh pick is accepted");
    deliver(
        &loopback,
        &mut sink,
        &WindowEvent::PickCancelled { window_id: window },
    )
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
        deliver(
            &loopback,
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
    deliver(
        &loopback,
        &mut sink,
        &WindowEvent::PickCancelled { window_id: window },
    )
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
        deliver(
            &loopback,
            &mut sink,
            &WindowEvent::PickCancelled { window_id: window }
        ),
        Err(Errno::OutOfRange)
    );
    loopback.borrow_mut().host.refuse_pick = None;
    client
        .pick_file(window)
        .expect("accepted once the host can");
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
fn backdrop_blur_defaults_to_an_accepted_no_op() {
    // A host implementing only the mandatory bridge methods exercises the
    // trait's own default: a host with no compositor to tell accepts the
    // radius and draws nothing, rather than failing a request it validated.
    let mapper = MockMapper::with_regions(&[(7, FRAME_LEN)]);
    let mut server = WindowServer::new(mapper, SERVER, CLIENT_FRAME_MAX);
    let mut host = MinimalHost;
    let mut identity = MockIdentity;
    let mut reply = [0u8; WINDOW_REPLY_MAX];

    let create = request_frame(&WindowRequest::Create {
        shm_handle: 7,
        event_endpoint: EVENTS_A,
        frame_count: 1,
        width_px: SURFACE.width_px,
        height_px: SURFACE.height_px,
        stride_bytes: SURFACE.stride_bytes,
        format: SURFACE.format,
        title: tairix_abi::window_ipc::WindowTitle::new("a").expect("valid title"),
        sizing: WindowSizing::Fixed,
    });
    let len = server.serve(&mut host, &mut identity, TICKET_A, &create, &mut reply);
    let (window, _) = tairix_abi::window_ipc::decode_create_reply(&reply[..len]).expect("created");

    // Setting the backdrop blur is infallible for an owned window: the
    // default handler has no compositor to tell.
    let blur = request_frame(&WindowRequest::SetBackdropBlur {
        window_id: window,
        radius_px: 8,
    });
    let len = server.serve(&mut host, &mut identity, TICKET_A, &blur, &mut reply);
    assert_eq!(decode_status_reply(&reply[..len]), Ok(()));
}

/// The menu a test application opens for one of its windows: a titled root
/// plate with one chooseable row and one submenu row.
fn sample_open_menu() -> AppMenu {
    let mut menu = AppMenu::titled(AppMenuLabel::new("Edit").expect("a valid title"));
    menu.push(AppMenuRow::Item(AppMenuItem::new(
        AppMenuItemId::new(1).expect("a valid id"),
        AppMenuLabel::new("Copy").expect("a valid label"),
    )))
    .expect("room");
    menu.push(AppMenuRow::Submenu {
        label: AppMenuLabel::new("Paste Special").expect("a valid label"),
        enabled: true,
    })
    .expect("room for a submenu row");
    menu
}

/// The anchor a right-click hands back: the window-local point the app was
/// given, with no extent.
fn sample_menu_anchor() -> MenuAnchor {
    MenuAnchor::new(12, 30, 0, 0).expect("a representable anchor")
}

/// One accepted open is answered exactly once, and its answer names the open
/// it belongs to.
///
/// The open id is what makes that a property rather than a hope: an
/// application that asked again while a previous answer was still in its
/// mailbox would otherwise read one gesture's dismissal as the next one's.
#[test]
fn a_menu_open_is_owner_bound_single_pending_and_answered_exactly_once() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    let menu = sample_open_menu();
    let anchor = sample_menu_anchor();

    // A window the caller does not own answers exactly like one that never
    // existed, and the host is never told.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.open_menu(window, anchor, &menu),
        Err(Errno::NotFound)
    );
    assert!(loopback.borrow().host.menu_opens.is_empty());
    loopback.borrow_mut().ticket = TICKET_A;

    // The owner's open reaches the host with the anchor and the whole menu
    // — title included — and mints an id.
    let open = client
        .open_menu(window, anchor, &menu)
        .expect("the open is accepted");
    assert_ne!(open, 0, "an open id is never zero");
    {
        let host = &loopback.borrow().host;
        assert_eq!(host.menu_opens.len(), 1);
        let (told_window, told_open, told_anchor, told_menu) = &host.menu_opens[0];
        assert_eq!((*told_window, *told_open), (window, open));
        assert_eq!(*told_anchor, anchor);
        assert_eq!(told_menu.title(), "Edit");
        assert_eq!(told_menu.len(), 2);
        assert!(told_menu.rows().any(|(row, _)| matches!(
            row,
            AppMenuRowView::Submenu { label, enabled }
                if label == "Paste Special" && enabled
        )));
    }

    // While it is unanswered a second open is refused without troubling the
    // host: the chain holds the seat's grab, so a well-behaved application
    // never reaches here.
    assert_eq!(
        client.open_menu(window, anchor, &menu),
        Err(Errno::AlreadyExists)
    );
    assert_eq!(loopback.borrow().host.menu_opens.len(), 1);

    // An outcome for some other open answers nothing and is refused rather
    // than delivered.
    let mut sink = QueueSink::default();
    assert_eq!(
        deliver(
            &loopback,
            &mut sink,
            &WindowEvent::MenuClosed {
                window_id: window,
                open_id: open + 1,
                outcome: MenuOutcome::Dismissed,
            }
        ),
        Err(Errno::OutOfRange)
    );
    assert!(sink.delivered.is_empty());

    // The outcome that names it delivers to the owner's endpoint, once.
    let chosen = WindowEvent::MenuClosed {
        window_id: window,
        open_id: open,
        outcome: MenuOutcome::Chosen(AppMenuItemId::new(1).expect("a valid id")),
    };
    deliver(&loopback, &mut sink, &chosen).expect("the outcome delivers");
    assert_eq!(sink.delivered.len(), 1);
    assert_eq!(sink.delivered[0].0, EVENTS_A);
    assert_eq!(sink.delivered[0].1, chosen.to_le_bytes());

    // A second outcome for the same open — the same one replayed, or a
    // contradicting dismissal — cannot be delivered.
    for outcome in [
        MenuOutcome::Chosen(AppMenuItemId::new(1).expect("a valid id")),
        MenuOutcome::Dismissed,
    ] {
        assert_eq!(
            deliver(
                &loopback,
                &mut sink,
                &WindowEvent::MenuClosed {
                    window_id: window,
                    open_id: open,
                    outcome,
                }
            ),
            Err(Errno::OutOfRange),
            "exactly one outcome follows an acceptance"
        );
    }
    assert_eq!(sink.delivered.len(), 1);

    // Answered, the window may open again — under a fresh id, so the two
    // gestures' answers can never be confused.
    let again = client
        .open_menu(window, anchor, &menu)
        .expect("a fresh open is accepted");
    assert_ne!(again, open, "an open id is never reused");
}

/// Two windows each hold their own open, and each answer reaches only its
/// own — which is what lets the seat's singleton close one chain and answer
/// *it* while another window's open stands.
#[test]
fn each_windows_open_is_answered_on_its_own() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let first = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    let second = create_id(&mut client, 8, EVENTS_A, 1, "b").expect("b");
    let menu = sample_open_menu();
    let anchor = sample_menu_anchor();

    let first_open = client
        .open_menu(first, anchor, &menu)
        .expect("the first open");
    let second_open = client
        .open_menu(second, anchor, &menu)
        .expect("the second open");
    assert_ne!(first_open, second_open);

    // The first window's open is answered by its own id and no other.
    let mut sink = QueueSink::default();
    assert_eq!(
        deliver(
            &loopback,
            &mut sink,
            &WindowEvent::MenuClosed {
                window_id: first,
                open_id: second_open,
                outcome: MenuOutcome::Dismissed,
            }
        ),
        Err(Errno::OutOfRange),
        "an open id belongs to the window that asked"
    );
    deliver(
        &loopback,
        &mut sink,
        &WindowEvent::MenuClosed {
            window_id: first,
            open_id: first_open,
            outcome: MenuOutcome::Dismissed,
        },
    )
    .expect("the displaced chain's own requester is answered");

    // The second window's open still stands and is answered in its turn.
    deliver(
        &loopback,
        &mut sink,
        &WindowEvent::MenuClosed {
            window_id: second,
            open_id: second_open,
            outcome: MenuOutcome::Chosen(AppMenuItemId::new(1).expect("a valid id")),
        },
    )
    .expect("and the standing chain answers separately");
    assert_eq!(sink.delivered.len(), 2);
}

/// A refused open records nothing, spends no id, and leaves no outcome owed.
#[test]
fn a_refused_menu_open_records_nothing_and_spends_no_id() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    let menu = sample_open_menu();
    let anchor = sample_menu_anchor();

    // The host cannot bring a chain up (its seat is held by a lock screen,
    // it is tearing down): the refusal is relayed verbatim.
    loopback.borrow_mut().host.refuse_menu_open = Some(Errno::NotSupported);
    assert_eq!(
        client.open_menu(window, anchor, &menu),
        Err(Errno::NotSupported)
    );

    // No open pends, so no outcome can be delivered for one.
    let mut sink = QueueSink::default();
    assert_eq!(
        deliver(
            &loopback,
            &mut sink,
            &WindowEvent::MenuClosed {
                window_id: window,
                open_id: 1,
                outcome: MenuOutcome::Dismissed,
            }
        ),
        Err(Errno::OutOfRange)
    );

    // And the refusal spent no id: the first accepted open is still the
    // first one minted.
    loopback.borrow_mut().host.refuse_menu_open = None;
    assert_eq!(
        client.open_menu(window, anchor, &menu),
        Ok(1),
        "a refused open consumes nothing"
    );
}

/// An outcome the sink refuses leaves the open owed, so the window can still
/// be told later — and cannot start a second chain in the meantime.
#[test]
fn an_outcome_the_sink_refuses_stays_owed_until_one_is_accepted() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    let menu = sample_open_menu();
    let anchor = sample_menu_anchor();
    let open = client
        .open_menu(window, anchor, &menu)
        .expect("the open is accepted");

    let refused = WindowEvent::MenuClosed {
        window_id: window,
        open_id: open,
        outcome: MenuOutcome::Refused(MenuRefusal::NoResources),
    };
    assert_eq!(
        deliver(&loopback, &mut FullSink, &refused),
        Err(Errno::WouldBlock),
        "a full mailbox is relayed, not swallowed"
    );
    assert_eq!(
        client.open_menu(window, anchor, &menu),
        Err(Errno::AlreadyExists),
        "the open is still owed, so no second chain starts"
    );

    let mut sink = QueueSink::default();
    deliver(&loopback, &mut sink, &refused).expect("an accepted outcome answers it");
    assert_eq!(sink.delivered.len(), 1);
    client
        .open_menu(window, anchor, &menu)
        .expect("the window may open again");
}

/// A desktop that composes no menu service refuses an open rather than
/// accepting one nothing will ever answer.
///
/// This is the trait's own default, exercised through a host implementing
/// only the mandatory bridge methods: a refused menu is an answer the
/// application reports and carries on from, never a chain left owed.
#[test]
fn a_host_with_no_menu_service_refuses_an_open() {
    let mapper = MockMapper::with_regions(&[(7, FRAME_LEN)]);
    let mut server = WindowServer::new(mapper, SERVER, CLIENT_FRAME_MAX);
    let mut host = MinimalHost;
    let mut identity = MockIdentity;
    let mut reply = [0u8; WINDOW_REPLY_MAX];

    let create = request_frame(&WindowRequest::Create {
        shm_handle: 7,
        event_endpoint: EVENTS_A,
        frame_count: 1,
        width_px: SURFACE.width_px,
        height_px: SURFACE.height_px,
        stride_bytes: SURFACE.stride_bytes,
        format: SURFACE.format,
        title: tairix_abi::window_ipc::WindowTitle::new("a").expect("valid title"),
        sizing: WindowSizing::Fixed,
    });
    let len = server.serve(&mut host, &mut identity, TICKET_A, &create, &mut reply);
    let (window, _) = tairix_abi::window_ipc::decode_create_reply(&reply[..len]).expect("created");

    let open = request_frame(&WindowRequest::OpenMenu {
        window_id: window,
        anchor: sample_menu_anchor(),
        menu: sample_open_menu(),
    });
    let len = server.serve(&mut host, &mut identity, TICKET_A, &open, &mut reply);
    assert_eq!(
        tairix_abi::window_ipc::decode_minted_id_reply(&reply[..len]),
        Err(Errno::NotSupported)
    );
}

/// The declaration a test application makes: it handles the click and
/// offers one chooseable row plus the session-rendered information row.
fn sample_app_bar(endpoint: u64) -> AppBar {
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Item(AppMenuItem::new(
        AppMenuItemId::new(1).expect("a valid id"),
        AppMenuLabel::new("New window").expect("a valid label"),
    )))
    .expect("room");
    menu.push(AppMenuRow::Info).expect("room");
    AppBar {
        event_endpoint: endpoint,
        click: AppBarClick::Open,
        menu,
    }
}

/// A client encodes every request into one buffer it holds for its own
/// lifetime, so a short request issued after a wide one is byte-for-byte the
/// request it would have been on its own.
///
/// The buffer is sized to the widest operation the channel has, which is why
/// it is not taken per call — a present, the hottest operation and one of the
/// shortest, would otherwise clear the whole of a declaration's width every
/// composited frame. Reuse is only safe because a frame is written and sent
/// at its own operation's length, and this is what holds that.
#[test]
fn reusing_the_encode_buffer_leaks_nothing_into_a_later_frame() {
    let fresh = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut first = WindowClient::new(Rc::clone(&fresh));
    let (window, _) = first
        .create(7, EVENTS_A, 1, &SURFACE, "Files", WindowSizing::Fixed)
        .expect("a window");
    first
        .present(window, 0, full_damage())
        .expect("the first present");
    let alone = fresh
        .borrow()
        .sent
        .last()
        .cloned()
        .expect("a present frame");

    // The same present, on a client that has since sent the widest operation
    // the channel has.
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let (window, _) = client
        .create(7, EVENTS_A, 1, &SURFACE, "Files", WindowSizing::Fixed)
        .expect("a window");
    client
        .set_app_bar(&sample_app_bar(EVENTS_A))
        .expect("declared");
    let widest = loopback
        .borrow()
        .sent
        .last()
        .cloned()
        .expect("a declaration frame");
    client
        .present(window, 0, full_damage())
        .expect("the second present");
    let after = loopback.borrow().sent.last().cloned().expect("a frame");

    assert!(
        widest.len() > alone.len(),
        "the declaration is the wider operation, or this proves nothing"
    );
    assert_eq!(after, alone, "a present carries only its own bytes");
}

#[test]
fn an_icon_bar_declaration_reaches_the_host_and_routes_its_events() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let bar = sample_app_bar(EVENTS_A);

    // The declaration needs no window: an application claims a slot under
    // its own attested identity, whether or not it has anything open.
    client.set_app_bar(&bar).expect("declared");
    assert_eq!(
        loopback.borrow().host.app_bars,
        alloc::vec![(proc_id(0xA1), bar)]
    );

    // Its events route to the endpoint the declaration named, and carry no
    // window id.
    let mut sink = QueueSink::default();
    for event in [
        WindowEvent::AppBarDefault,
        WindowEvent::AppBarMenu {
            item: AppMenuItemId::new(1).expect("a valid id"),
        },
    ] {
        loopback
            .borrow_mut()
            .server
            .deliver_app_event(&mut sink, proc_id(0xA1), &event)
            .expect("delivered");
    }
    assert_eq!(
        sink.delivered,
        alloc::vec![
            (EVENTS_A, WindowEvent::AppBarDefault.to_le_bytes()),
            (
                EVENTS_A,
                WindowEvent::AppBarMenu {
                    item: AppMenuItemId::new(1).expect("a valid id")
                }
                .to_le_bytes()
            )
        ]
    );

    // A re-declaration replaces the route whole rather than accumulating
    // one, so an application that moves its mailbox is not delivered to
    // both.
    let moved = sample_app_bar(EVENTS_B);
    client.set_app_bar(&moved).expect("re-declared");
    let mut sink = QueueSink::default();
    loopback
        .borrow_mut()
        .server
        .deliver_app_event(&mut sink, proc_id(0xA1), &WindowEvent::AppBarDefault)
        .expect("delivered");
    assert_eq!(
        sink.delivered,
        alloc::vec![(EVENTS_B, WindowEvent::AppBarDefault.to_le_bytes())]
    );
}

#[test]
fn icon_bar_delivery_fails_closed_without_a_declaration() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut sink = QueueSink::default();

    // An application that never declared a presence has nowhere for a bar
    // event to go, so the delivery is refused rather than guessed at.
    assert_eq!(
        loopback.borrow_mut().server.deliver_app_event(
            &mut sink,
            proc_id(0xA1),
            &WindowEvent::AppBarDefault
        ),
        Err(Errno::NotFound)
    );

    // A refused declaration records no route either: the engine remembers
    // one only once the host accepted it.
    loopback.borrow_mut().host.refuse_app_bar = Some(Errno::NotSupported);
    assert_eq!(
        client.set_app_bar(&sample_app_bar(EVENTS_A)),
        Err(Errno::NotSupported)
    );
    assert_eq!(
        loopback.borrow_mut().server.deliver_app_event(
            &mut sink,
            proc_id(0xA1),
            &WindowEvent::AppBarDefault
        ),
        Err(Errno::NotFound)
    );
    loopback.borrow_mut().host.refuse_app_bar = None;

    // The two delivery paths are not interchangeable: a window-scoped
    // event on the application path (and the reverse) is a routing bug and
    // is refused, never delivered to the wrong place.
    client
        .set_app_bar(&sample_app_bar(EVENTS_A))
        .expect("declared");
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    assert_eq!(
        loopback.borrow_mut().server.deliver_app_event(
            &mut sink,
            proc_id(0xA1),
            &WindowEvent::CloseRequested { window_id: window }
        ),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        deliver(&loopback, &mut sink, &WindowEvent::AppBarDefault),
        Err(Errno::OutOfRange)
    );
    assert!(sink.delivered.is_empty());
}

#[test]
fn a_dead_clients_icon_bar_presence_is_withdrawn() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    client
        .set_app_bar(&sample_app_bar(EVENTS_A))
        .expect("declared");

    let owner = proc_id(0xA1);
    {
        let mut borrowed = loopback.borrow_mut();
        let Loopback { server, host, .. } = &mut *borrowed;
        server.client_exited(host, owner);
    }
    assert_eq!(
        loopback.borrow().host.app_bars_withdrawn,
        alloc::vec![owner]
    );

    // The route is gone with it, so a late bar event has nowhere to land.
    let mut sink = QueueSink::default();
    assert_eq!(
        loopback.borrow_mut().server.deliver_app_event(
            &mut sink,
            owner,
            &WindowEvent::AppBarDefault
        ),
        Err(Errno::NotFound)
    );

    // A client that never declared one is not reported as withdrawing it.
    let other = proc_id(0xB2);
    {
        let mut borrowed = loopback.borrow_mut();
        let Loopback { server, host, .. } = &mut *borrowed;
        server.client_exited(host, other);
    }
    assert_eq!(
        loopback.borrow().host.app_bars_withdrawn,
        alloc::vec![owner]
    );
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

#[test]
fn a_wire_pointer_position_widens_saturating_into_window_geometry() {
    assert_eq!(pointer_point(0, 0), Point::new(0, 0));
    assert_eq!(pointer_point(7, 11), Point::new(7, 11));
    // Past the signed range the coordinate saturates rather than wrapping onto
    // a control: the point lands outside every laid-out rectangle.
    let far = pointer_point(u32::MAX, u32::MAX);
    assert_eq!(far, Point::new(i32::MAX, i32::MAX));
    assert!(!Rect::new(0, 0, 4096, 4096).contains(far));
}

#[test]
fn a_reported_rect_becomes_the_damage_it_covers() {
    let rect = Rect::new(1, 1, 2, 2);
    let damage = damage_in(&SURFACE, rect).expect("inside the surface");
    assert_eq!(
        (damage.x, damage.y, damage.width_px, damage.height_px),
        (1, 1, 2, 2)
    );
}

#[test]
fn a_reported_rect_is_clipped_to_the_window() {
    // A control drawn partly off the window — a popup at the edge — must not
    // present pixels the session would refuse.
    let damage = damage_in(&SURFACE, Rect::new(-4, 2, 100, 100)).expect("the part inside");
    assert_eq!(
        (damage.x, damage.y, damage.width_px, damage.height_px),
        (0, 2, 4, 1)
    );
}

#[test]
fn a_rect_wholly_outside_the_window_presents_nothing() {
    assert_eq!(damage_in(&SURFACE, Rect::new(9, 9, 4, 4)), None);
    assert_eq!(damage_in(&SURFACE, Rect::EMPTY), None);
}

#[test]
fn a_round_that_changed_nothing_presents_nothing() {
    let damage = Region::new();
    assert_eq!(present_damage(&SURFACE, Repaint::Nothing, &damage), None);
}

#[test]
fn a_round_presents_what_it_reported() {
    let mut damage = Region::new();
    damage.add(Rect::new(1, 0, 1, 1));
    damage.add(Rect::new(3, 2, 1, 1));
    let presented = present_damage(&SURFACE, Repaint::Reported, &damage).expect("a rectangle");
    // One present per frame carries one rectangle, so two reports present the
    // box around them — still far short of the window where they are close.
    assert_eq!(
        (
            presented.x,
            presented.y,
            presented.width_px,
            presented.height_px
        ),
        (1, 0, 3, 3)
    );
}

#[test]
fn a_round_that_changed_the_view_but_reported_nothing_presents_the_window() {
    // Under-covering would leave a stale frame on screen, so the fallback is
    // the whole window rather than nothing.
    let damage = Region::new();
    assert_eq!(
        present_damage(&SURFACE, Repaint::Reported, &damage),
        Some(DamageRect::full(&SURFACE))
    );
}

#[test]
fn a_whole_round_presents_the_window_whatever_was_reported() {
    let mut damage = Region::new();
    damage.add(Rect::new(1, 1, 1, 1));
    assert_eq!(
        present_damage(&SURFACE, Repaint::Whole, &damage),
        Some(DamageRect::full(&SURFACE))
    );
}
