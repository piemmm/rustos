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
use tairix_abi::origin::{AppIdentity, ProcId, PROC_ID_LEN};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::window_ipc::{
    AppBar, AppBarClick, AppMenu, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuRow,
    AppMenuRowView, DocumentName, HandOverDocument, HandOverOutcome, LayerDepth, MenuOutcome,
    MenuRefusal, PointerAction, TerrainPlate, TooltipText, WindowEvent, WindowRegion,
    WindowRequest, APP_MENU_ENTRY_MAX, DESKTOP_LAYER_MAX_PER_CLIENT, HAND_OVER_RUN_PATH_MAX,
    WINDOW_MAX_OPEN_TARGETS, WINDOW_TITLE_MAX,
};
use tairix_abi::{BundleId, CapabilityId, Errno, PublisherId};
use tairix_display::{FrameRegion, ShmMapper};
use tairix_geometry::{Point, Rect, Region, Scale};

use crate::client::{
    damage_in, pointer_point, present_damage, retained_damage, EventDrain, EventError, EventSource,
    Parked, Repaint, Target, WindowClient, WindowEvents, WindowTransport,
};
use crate::desktop::Desktop;
use crate::server::{
    client_frame_budget_bytes, CallerIdentity, CursorSetName, EventSink, HandOverDesk, LayerSpec,
    OpenEntry, PopupSpec, WallpaperName, WindowHost, WindowServer, WindowSizeState, WindowSizing,
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
struct MockIdentity {
    /// Which tickets the kernel attests as holding `CAP_DESKTOP_LAYER`.
    /// Empty by default, so the gate is closed unless a test opens it.
    layer_holders: Vec<u64>,
    /// An attestation failure to surface instead of an answer.
    attest_error: Option<Errno>,
    /// The application each ticket is attested as running; absent means it
    /// runs no verified bundle.
    apps: Vec<(u64, AppIdentity)>,
}

fn proc_id(fill: u8) -> ProcId {
    ProcId::from_raw([fill; PROC_ID_LEN])
}

impl MockIdentity {
    /// An identity attesting `CAP_DESKTOP_LAYER` for each of `tickets`.
    fn holding_layer(tickets: &[u64]) -> Self {
        Self {
            layer_holders: tickets.to_vec(),
            attest_error: None,
            apps: Vec::new(),
        }
    }
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

    fn caller_holds(&mut self, ticket: u64, cap: CapabilityId) -> Result<bool, Errno> {
        if let Some(err) = self.attest_error {
            return Err(err);
        }
        Ok(cap == CapabilityId::DESKTOP_LAYER && self.layer_holders.contains(&ticket))
    }

    fn caller_app(&mut self, ticket: u64) -> Result<Option<AppIdentity>, Errno> {
        if let Some(err) = self.attest_error {
            return Err(err);
        }
        Ok(self
            .apps
            .iter()
            .find(|(held, _)| *held == ticket)
            .map(|(_, app)| *app))
    }
}

/// A host recording every bridge call, optionally refusing opens, picker
/// requests.
struct RecordingHost {
    opened: Vec<(ProcId, u64, DisplayMode, String, WindowSizing)>,
    popups: Vec<(u64, u64, i32, i32, DisplayMode)>,
    layers: Vec<(ProcId, u64, DisplayMode, i32, i32, LayerDepth)>,
    layer_places: Vec<(u64, i32, i32, LayerDepth)>,
    refuse_layer: Option<Errno>,
    /// The terrain the host reports, and the refusal it answers instead.
    terrain: Vec<TerrainPlate>,
    refuse_terrain: Option<Errno>,
    presented: Vec<(u64, Vec<u8>, DamageRect)>,
    resized: Vec<(u64, DisplayMode)>,
    closed: Vec<u64>,
    picks: Vec<u64>,
    menu_opens: Vec<(u64, u64, WindowRegion, AppMenu)>,
    tooltips: Vec<(u64, WindowRegion, String)>,
    refuse_tooltip: Option<Errno>,
    blur_sets: Vec<(u64, u16)>,
    retitled: Vec<(u64, String)>,
    resized_range: Vec<(u64, WindowSizing)>,
    size_states: Vec<(u64, WindowSizeState)>,
    refuse_size_state: Option<Errno>,
    app_bars: Vec<(ProcId, AppBar)>,
    app_bars_withdrawn: Vec<ProcId>,
    refuse_app_bar: Option<Errno>,
    refuse_open: bool,
    refuse_popup: bool,
    refuse_resize: Option<Errno>,
    refuse_retitle: Option<Errno>,
    refuse_sizing: Option<Errno>,
    refuse_pick: Option<Errno>,
    refuse_menu_open: Option<Errno>,
    hand_overs: Vec<(ProcId, String, Option<HandOverDocument>)>,
    /// What the host answers a hand-over with: `NotRunning` unless a test
    /// says otherwise, so the default is "nothing to reach".
    hand_over: Result<HandOverOutcome, Errno>,
    /// The desktop this host composites, or the refusal a host with no
    /// screen to describe answers with.
    desktop: Result<DesktopInfo, Errno>,
    /// The shipped wallpapers this host offers, and every render asked of
    /// it.
    wallpapers: Vec<WallpaperName>,
    renders: Vec<(u64, u64, u16, u16)>,
    refuse_render: Option<Errno>,
    /// The cursor sets this host offers.
    cursor_sets: Vec<CursorSetName>,
    /// The sources this host answers, and the application every
    /// notify-source query and lock request was attributed to.
    notify_sources: Vec<BundleId>,
    asked_by: Vec<Option<AppIdentity>>,
    locks: usize,
}

impl Default for RecordingHost {
    fn default() -> Self {
        Self {
            opened: Vec::new(),
            popups: Vec::new(),
            layers: Vec::new(),
            layer_places: Vec::new(),
            refuse_layer: None,
            terrain: Vec::new(),
            refuse_terrain: None,
            presented: Vec::new(),
            resized: Vec::new(),
            closed: Vec::new(),
            picks: Vec::new(),
            wallpapers: Vec::new(),
            renders: Vec::new(),
            refuse_render: None,
            cursor_sets: Vec::new(),
            notify_sources: Vec::new(),
            asked_by: Vec::new(),
            locks: 0,
            menu_opens: Vec::new(),
            tooltips: Vec::new(),
            refuse_tooltip: None,
            blur_sets: Vec::new(),
            retitled: Vec::new(),
            resized_range: Vec::new(),
            size_states: Vec::new(),
            refuse_size_state: None,
            app_bars: Vec::new(),
            app_bars_withdrawn: Vec::new(),
            refuse_app_bar: None,
            refuse_open: false,
            refuse_popup: false,
            refuse_resize: None,
            refuse_retitle: None,
            refuse_sizing: None,
            refuse_pick: None,
            refuse_menu_open: None,
            hand_overs: Vec::new(),
            hand_over: Ok(HandOverOutcome::NotRunning),
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

    fn layer_opened(
        &mut self,
        owner: ProcId,
        window_id: u64,
        surface: &DisplayMode,
        x: i32,
        y: i32,
        depth: LayerDepth,
    ) -> Result<(), Errno> {
        if let Some(err) = self.refuse_layer {
            return Err(err);
        }
        self.layers.push((owner, window_id, *surface, x, y, depth));
        Ok(())
    }

    fn layer_placed(
        &mut self,
        window_id: u64,
        x: i32,
        y: i32,
        depth: LayerDepth,
    ) -> Result<(), Errno> {
        if let Some(err) = self.refuse_layer {
            return Err(err);
        }
        self.layer_places.push((window_id, x, y, depth));
        Ok(())
    }

    fn layer_terrain(&mut self, window_id: u64, out: &mut [TerrainPlate]) -> Result<usize, Errno> {
        let _ = window_id;
        if let Some(err) = self.refuse_terrain {
            return Err(err);
        }
        let written = self.terrain.len().min(out.len());
        out[..written].copy_from_slice(&self.terrain[..written]);
        Ok(written)
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

    fn window_sizing_changed(&mut self, window_id: u64, sizing: WindowSizing) -> Result<(), Errno> {
        if let Some(err) = self.refuse_sizing {
            return Err(err);
        }
        self.resized_range.push((window_id, sizing));
        Ok(())
    }

    fn window_size_state_changed(
        &mut self,
        window_id: u64,
        state: WindowSizeState,
    ) -> Result<(), Errno> {
        if let Some(err) = self.refuse_size_state {
            return Err(err);
        }
        self.size_states.push((window_id, state));
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

    fn hand_over_requested(
        &mut self,
        desk: &mut dyn HandOverDesk,
        caller: ProcId,
        run_path: &str,
        document: Option<&HandOverDocument>,
    ) -> Result<HandOverOutcome, Errno> {
        self.hand_overs
            .push((caller, String::from(run_path), document.copied()));
        // A host that says it reached an instance really queues something,
        // so the engine's own half of the hand-over is exercised too.
        if self.hand_over == Ok(HandOverOutcome::Reached) {
            let entry = match document {
                Some(doc) => OpenEntry::Document {
                    name: String::from(doc.name.as_str()),
                    grant: doc.grant,
                },
                None => OpenEntry::Path(String::from(run_path)),
            };
            if !desk.hand_over(caller, entry) {
                return Ok(HandOverOutcome::NotRunning);
            }
        }
        self.hand_over
    }

    fn menu_open_requested(
        &mut self,
        window_id: u64,
        open_id: u64,
        anchor: WindowRegion,
        menu: &AppMenu,
    ) -> Result<(), Errno> {
        if let Some(err) = self.refuse_menu_open {
            return Err(err);
        }
        self.menu_opens.push((window_id, open_id, anchor, *menu));
        Ok(())
    }

    fn tooltip_declared(
        &mut self,
        window_id: u64,
        region: WindowRegion,
        text: &str,
    ) -> Result<(), Errno> {
        if let Some(err) = self.refuse_tooltip {
            return Err(err);
        }
        self.tooltips.push((window_id, region, String::from(text)));
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

    fn wallpaper_catalog(&mut self) -> &[WallpaperName] {
        &self.wallpapers
    }

    fn cursor_sets(&mut self) -> &[CursorSetName] {
        &self.cursor_sets
    }

    fn notify_sources(&mut self, caller: Option<&AppIdentity>) -> Result<&[BundleId], Errno> {
        self.asked_by.push(caller.copied());
        if caller.is_none() {
            return Err(Errno::PermissionDenied);
        }
        Ok(&self.notify_sources)
    }

    fn lock_screen(&mut self, caller: Option<&AppIdentity>) -> Result<(), Errno> {
        self.asked_by.push(caller.copied());
        if caller.is_none() {
            return Err(Errno::PermissionDenied);
        }
        self.locks += 1;
        Ok(())
    }

    fn wallpaper_render_requested(
        &mut self,
        window_id: u64,
        shm_handle: u64,
        index: u16,
        side: u16,
    ) -> Result<(), Errno> {
        if let Some(err) = self.refuse_render {
            return Err(err);
        }
        self.renders.push((window_id, shm_handle, index, side));
        Ok(())
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

    fn window_sizing_changed(
        &mut self,
        _window_id: u64,
        _sizing: WindowSizing,
    ) -> Result<(), Errno> {
        Ok(())
    }

    fn window_size_state_changed(
        &mut self,
        _window_id: u64,
        _state: WindowSizeState,
    ) -> Result<(), Errno> {
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
    /// Where an event the engine sends while serving a request lands — the
    /// hand-over's wake is the one that does.
    sink: QueueSink,
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
            identity: MockIdentity::holding_layer(&[TICKET_A, TICKET_B]),
            sink: QueueSink::default(),
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
            &mut inner.sink,
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
///
/// Nothing here can be woken, so the park stands in for one that never
/// returns: it refuses, which is what lets a test assert that the drain was
/// tried first and that a park happened at all.
struct QueueSource {
    queue: VecDeque<[u8; WindowEvent::WIRE_LEN]>,
    /// How many times the source has been parked.
    parked: usize,
}

impl QueueSource {
    /// A source over `frames`, in delivery order.
    fn new(frames: impl IntoIterator<Item = [u8; WindowEvent::WIRE_LEN]>) -> Self {
        Self {
            queue: frames.into_iter().collect(),
            parked: 0,
        }
    }
}

impl EventDrain for QueueSource {
    fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
        let Some(frame) = self.queue.pop_front() else {
            return Ok(false);
        };
        *event = frame;
        Ok(true)
    }
}

impl EventSource for QueueSource {
    fn park(&mut self) -> Result<Parked, Errno> {
        self.parked += 1;
        Err(Errno::WouldBlock)
    }
}

/// A source whose park is interrupted by something the loop owns — a worker's
/// answer — rather than by an event.
struct InterruptingSource {
    /// How many times the source has been parked.
    parked: usize,
}

impl EventDrain for InterruptingSource {
    fn try_next(&mut self, _event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
        Ok(false)
    }
}

impl EventSource for InterruptingSource {
    fn park(&mut self) -> Result<Parked, Errno> {
        self.parked += 1;
        Ok(Parked::Interrupted)
    }
}

/// The regression the answer type exists for. A worker's wake is level-
/// triggered, so a wait that treated it as "no event yet" would park again on
/// a source that is *still* ready — a spin, not a wait, and the answer would
/// never reach the loop. One interrupted park ends the wait with no event.
#[test]
fn an_interrupted_park_ends_the_wait_without_an_event() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut waiter = WindowEvents::new(InterruptingSource { parked: 0 });
    assert_eq!(waiter.wait(&mut client), Ok(None));
}

/// And it parks exactly once: the wait returns to the loop rather than
/// spinning round the drain-park pair.
#[test]
fn an_interrupted_wait_parks_once_rather_than_spinning() {
    let mut source = InterruptingSource { parked: 0 };
    let mut frame = [0u8; WindowEvent::WIRE_LEN];
    assert_eq!(source.next(&mut frame), Ok(false), "no event was delivered");
    assert_eq!(source.parked, 1);
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

/// The desktop belongs to the seat, not to one window, so an application
/// converges on the published state rather than being told per window: it
/// adopts a new record once and answers `false` for a re-publish of the same
/// one, so a session that re-states the current desktop costs no repaint.
#[test]
fn adopting_a_published_desktop_is_news_only_once() {
    let mut desktop = Desktop::new(sample_desktop()).expect("the sample scale is in range");
    let switched = DesktopInfo::new(1024, 768, 100, Appearance::Light)
        .expect("a 1024x768 screen at 100% is in range");

    assert_eq!(desktop.adopt(switched), Ok(true));
    assert_eq!(desktop.appearance(), Appearance::Light);
    assert_eq!(desktop.adopt(switched), Ok(false));
    assert_eq!(desktop.appearance(), Appearance::Light);

    // Back again is a change in its own right: an application must be told
    // it may return to the appearance it started in.
    assert_eq!(desktop.adopt(sample_desktop()), Ok(true));
    assert_eq!(desktop.appearance(), Appearance::Dark);
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
    assert_eq!(desktop.adopt(absurd), Err(Errno::OutOfRange));
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
        max_width_px: 0,
        max_height_px: 0,
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
        max_width_px: 0,
        max_height_px: 0,
    };
    let (window, _) = client
        .create(7, EVENTS_A, 1, &SURFACE, "Files", floor)
        .expect("create");

    let under = WindowEvent::Resized {
        window_id: window,
        width_px: 1,
        height_px: 1,
        state: WindowSizeState::Restored,
    };
    let mut waiter = WindowEvents::new(QueueSource::new([under.to_le_bytes()]));
    assert_eq!(waiter.wait(&mut client), Ok(Some(under)));

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
    assert_eq!(
        client.set_sizing(window, WindowSizing::Fixed),
        Err(Errno::NotFound)
    );
    {
        let inner = loopback.borrow();
        assert_eq!(inner.server.window_count(), 1);
        assert!(inner.host.closed.is_empty());
        assert!(inner.host.retitled.is_empty());
        assert!(inner.host.resized_range.is_empty());
    }

    // A still owns it.
    loopback.borrow_mut().ticket = TICKET_A;
    client
        .present(window, 0, full_damage())
        .expect("A presents");
}

#[test]
fn an_owner_restates_its_window_s_range_and_a_refusal_changes_nothing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = create_id(&mut client, 7, EVENTS_A, 1, "Sapper").expect("A creates");
    let range = WindowSizing::Resizable {
        min_width_px: 300,
        min_height_px: 200,
        max_width_px: 900,
        max_height_px: 700,
    };
    client
        .set_sizing(window, range)
        .expect("the owner restates its range");
    assert_eq!(loopback.borrow().host.resized_range, [(window, range)]);

    // A range naming no reachable size never reaches the session.
    assert_eq!(
        client.set_sizing(
            window,
            WindowSizing::Resizable {
                min_width_px: 300,
                min_height_px: 200,
                max_width_px: 100,
                max_height_px: 700,
            },
        ),
        Err(Errno::OutOfRange)
    );
    // An unknown window is refused, and a host refusal leaves the
    // previous range standing.
    assert_eq!(client.set_sizing(window + 1, range), Err(Errno::NotFound));
    loopback.borrow_mut().host.refuse_sizing = Some(Errno::NotSupported);
    assert_eq!(client.set_sizing(window, range), Err(Errno::NotSupported));
    assert_eq!(loopback.borrow().host.resized_range.len(), 1);
}

#[test]
fn an_owner_asks_for_a_size_state_and_a_refusal_changes_nothing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    let window = create_id(&mut client, 7, EVENTS_A, 1, "WinterSun").expect("A creates");
    client
        .set_size_state(window, WindowSizeState::Fullscreen)
        .expect("the owner asks for fullscreen");
    assert_eq!(
        loopback.borrow().host.size_states,
        [(window, WindowSizeState::Fullscreen)]
    );

    // A window the caller does not own never reaches the session: the id
    // is a name, not a credential.
    assert_eq!(
        client.set_size_state(window + 1, WindowSizeState::Fullscreen),
        Err(Errno::NotFound)
    );
    assert_eq!(loopback.borrow().host.size_states.len(), 1);

    // A host refusal is relayed and leaves the window where it is.
    loopback.borrow_mut().host.refuse_size_state = Some(Errno::NotSupported);
    assert_eq!(
        client.set_size_state(window, WindowSizeState::Fullscreen),
        Err(Errno::NotSupported)
    );
    assert_eq!(loopback.borrow().host.size_states.len(), 1);
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
        &mut inner.sink,
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
        &mut inner.sink,
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
    let mut waiter = WindowEvents::new(QueueSource::new(queue));
    assert_eq!(
        waiter.wait(&mut client),
        Ok(Some(WindowEvent::Focus {
            window_id: a,
            focused: true
        }))
    );
    assert_eq!(
        waiter.wait(&mut client),
        Ok(Some(WindowEvent::Key { window_id: a, key }))
    );
    assert_eq!(
        waiter.wait(&mut client),
        Ok(Some(WindowEvent::CloseRequested { window_id: a }))
    );
    // An empty queue surfaces the source's wait condition, never a
    // fabricated event — as a mailbox failure, which is what ends a channel.
    assert_eq!(
        waiter.wait(&mut client),
        Err(EventError::Mailbox(Errno::WouldBlock))
    );
}

/// The defaulted `next` drains before it parks: a loop that has queued input
/// must never be made to wait for a wake that has already been consumed.
///
/// The park is what a loop interleaving decode work with input has to avoid
/// entering while input is queued, so the ordering is asserted rather than
/// assumed.
#[test]
fn the_parked_wait_drains_before_it_parks() {
    let a = 11;
    let queued = WindowEvent::CloseRequested { window_id: a };
    let mut source = QueueSource::new([queued.to_le_bytes()]);

    let mut frame = [0u8; WindowEvent::WIRE_LEN];
    assert_eq!(source.next(&mut frame), Ok(true));
    assert_eq!(WindowEvent::from_bytes(&frame), Ok(queued));
    assert_eq!(source.parked, 0, "a queued event must not cost a park");

    // Nothing left: the drain reports empty and the park is entered, which is
    // how a genuinely idle loop stops running.
    assert_eq!(source.next(&mut frame), Err(Errno::WouldBlock));
    assert_eq!(source.parked, 1);
}

/// `try_wait` reports an empty mailbox as `None` without parking, and hands
/// back exactly the events `wait` would.
#[test]
fn the_drained_wait_answers_without_parking() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    let queued = WindowEvent::Focus {
        window_id: window,
        focused: true,
    };
    let mut waiter = WindowEvents::new(QueueSource::new([queued.to_le_bytes()]));
    assert_eq!(waiter.try_wait(&mut client), Ok(Some(queued)));
    assert_eq!(
        waiter.try_wait(&mut client),
        Ok(None),
        "an empty mailbox is answered at once, never parked on"
    );
}

/// A drain that fails with the one code a decode also produces, so a reader
/// that told the two apart by [`Errno`] alone could not.
struct FailingDrain;

impl EventDrain for FailingDrain {
    fn try_next(&mut self, _event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
        Err(Errno::LengthOutOfRange)
    }
}

/// A failed drain is answered as a *mailbox* failure even when its code is one
/// a decode refusal also uses.
///
/// The caller's two answers are opposites — read on past an undecodable frame,
/// end the channel on a dead mailbox — so an ambiguous one is a spin: the next
/// read meets the same failure and the loop never parks. `ipc_recv` really can
/// answer `LengthOutOfRange` (a received length the address width cannot
/// hold), so this is not a hypothetical code.
#[test]
fn a_failed_drain_is_never_mistaken_for_an_undecodable_frame() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut events = WindowEvents::new(FailingDrain);

    assert_eq!(
        events.try_wait(&mut client),
        Err(EventError::Mailbox(Errno::LengthOutOfRange))
    );
}

/// A frame the session sent that will not decode is answered as *undecodable*,
/// so the caller reads past it rather than tearing the channel down.
#[test]
fn a_frame_that_will_not_decode_is_answered_apart_from_a_dead_mailbox() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut events = WindowEvents::new(DrainOnlySource {
        queue: [[0u8; WindowEvent::WIRE_LEN]].into_iter().collect(),
    });

    assert!(matches!(
        events.try_wait(&mut client),
        Err(EventError::Undecodable(_))
    ));
    assert_eq!(
        events.try_wait(&mut client),
        Ok(None),
        "the refused frame is consumed, so the next read moves past it"
    );
}

/// A mailbox with no park of its own: the shape an app takes when its loop
/// dispatches several wake sources and a frame deadline itself, so parking is
/// the loop's business and only the drain is the mailbox's.
struct DrainOnlySource {
    queue: VecDeque<[u8; WindowEvent::WIRE_LEN]>,
}

impl EventDrain for DrainOnlySource {
    fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
        let Some(frame) = self.queue.pop_front() else {
            return Ok(false);
        };
        *event = frame;
        Ok(true)
    }
}

/// An app whose loop owns its park still reads through the shared stream, and
/// so still gets the fold.
///
/// This is the terminal emulator's shape, and the reason it matters: its
/// wait-set carries a shell stream and a child per window, a settings
/// worker, a pressure wake, and an animation deadline, so it dispatches its
/// own wakes and cannot hand the park to a source. When the stream was
/// reachable only *through* a park, that loop had to spell a raw mailbox
/// drain of its own — which folded nothing, so a resize-grab's queued samples
/// were each applied on release and walked the window back through the drag.
#[test]
fn a_loop_that_owns_its_park_still_folds_a_resize_run() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    let resized = |width_px, height_px| WindowEvent::Resized {
        window_id: window,
        width_px,
        height_px,
        state: WindowSizeState::Restored,
    };
    // What a drag out and back in leaves queued: every sample the pointer
    // produced, ending on the size the window actually settled at.
    let drag = [
        resized(400, 300),
        resized(520, 380),
        resized(640, 460),
        resized(520, 380),
        resized(430, 310),
    ];
    let mut events = WindowEvents::new(DrainOnlySource {
        queue: drag.iter().map(WindowEvent::to_le_bytes).collect(),
    });

    assert_eq!(
        events.try_wait(&mut client),
        Ok(Some(resized(430, 310))),
        "the whole drag collapses onto the size it ended at, so the window is \
         re-laid-out once instead of replaying every sample behind the pointer"
    );
    assert_eq!(
        events.try_wait(&mut client),
        Ok(None),
        "and nothing of the run is left to apply afterwards"
    );
}

/// A run of resizes for one window folds to its newest: an interactive
/// resize-grab reports one per pointer sample, and an app that re-laid-out
/// for each would do the work once per queued sample to reach the size the
/// last one already named.
#[test]
fn a_run_of_resizes_folds_to_the_newest_extent() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    let resized = |width_px, height_px| WindowEvent::Resized {
        window_id: window,
        width_px,
        height_px,
        state: WindowSizeState::Restored,
    };
    let mut waiter = WindowEvents::new(QueueSource::new([
        resized(100, 50).to_le_bytes(),
        resized(120, 60).to_le_bytes(),
        resized(140, 70).to_le_bytes(),
    ]));
    assert_eq!(
        waiter.try_wait(&mut client),
        Ok(Some(resized(140, 70))),
        "the whole run collapses onto the size the last sample named"
    );
    assert_eq!(waiter.try_wait(&mut client), Ok(None));
}

/// Only a *consecutive* run folds, and only for the same window: the event
/// that ends the run is put back untouched, so folding can neither reorder
/// the queue nor lose anything from it.
#[test]
fn folding_a_resize_run_keeps_every_other_event_in_order() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    let resized = |window_id, width_px| WindowEvent::Resized {
        window_id,
        width_px,
        height_px: 50,
        state: WindowSizeState::Restored,
    };
    let key = WindowEvent::Focus {
        window_id: window,
        focused: true,
    };
    let mut waiter = WindowEvents::new(QueueSource::new([
        resized(window, 100).to_le_bytes(),
        resized(window, 120).to_le_bytes(),
        key.to_le_bytes(),
        resized(window, 140).to_le_bytes(),
        resized(window + 1, 160).to_le_bytes(),
    ]));
    assert_eq!(waiter.try_wait(&mut client), Ok(Some(resized(window, 120))));
    assert_eq!(waiter.try_wait(&mut client), Ok(Some(key)));
    assert_eq!(
        waiter.try_wait(&mut client),
        Ok(Some(resized(window, 140))),
        "a resize for another window does not join this one's run"
    );
    assert_eq!(
        waiter.try_wait(&mut client),
        Ok(Some(resized(window + 1, 160)))
    );
    assert_eq!(waiter.try_wait(&mut client), Ok(None));
}

/// The drained path answers a redraw request on the app's behalf exactly as
/// the parked one does, so a loop that interleaves work cannot leave a window
/// blank where a parked loop would have re-presented it.
#[test]
fn the_drained_wait_answers_a_redraw_request_like_the_parked_one() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 2, "a").expect("a");
    client
        .present(window, 1, full_damage())
        .expect("the first present");
    assert_eq!(loopback.borrow().host.presented.len(), 1);

    let mut waiter = WindowEvents::new(redraw_source(window));
    assert_eq!(
        waiter.try_wait(&mut client),
        Ok(Some(WindowEvent::RedrawRequested { window_id: window }))
    );
    let inner = loopback.borrow();
    assert_eq!(inner.host.presented.len(), 2);
    let (id, _, seen) = inner.host.presented.last().expect("the re-present");
    assert_eq!(*id, window);
    assert_eq!(*seen, full_damage());
}

/// A source holding one queued redraw request for `window`, ready for the
/// typed wait.
fn redraw_source(window: u64) -> QueueSource {
    QueueSource::new([WindowEvent::RedrawRequested { window_id: window }.to_le_bytes()])
}

#[test]
fn a_redraw_request_re_presents_the_last_frame_without_the_app_acting() {
    let loopback = Loopback::with_regions(&[(7, 2 * FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 2, "a").expect("a");

    // A client that has never presented has no frame to re-send: the
    // request is a no-op, not an error.
    let mut waiter = WindowEvents::new(redraw_source(window));
    assert_eq!(
        waiter.wait(&mut client),
        Ok(Some(WindowEvent::RedrawRequested { window_id: window }))
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
    let mut waiter = WindowEvents::new(redraw_source(window));
    assert_eq!(
        waiter.wait(&mut client),
        Ok(Some(WindowEvent::RedrawRequested { window_id: window }))
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

/// The catalog is the host's, paged into a bounded reply, and a caller
/// asking past its end gets an honest empty page rather than a refusal.
#[test]
fn the_wallpaper_catalog_is_answered_as_pages_of_the_hosts_own_listing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    loopback.borrow_mut().host.wallpapers = alloc::vec![
        WallpaperName {
            category: String::from("TAIRiX"),
            file: String::from("a.jpg"),
        },
        WallpaperName {
            category: String::from("Space"),
            file: String::from("b.png"),
        },
    ];
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut page = [0u8; tairix_abi::window_ipc::WINDOW_WALLPAPERS_REPLY_MAX];

    let answered = client.wallpapers(0, &mut page).expect("a catalog page");
    assert_eq!(answered.total, 2);
    let entries: Vec<(String, String)> = answered
        .entries()
        .map(|entry| {
            (
                String::from_utf8_lossy(entry.category).into_owned(),
                String::from_utf8_lossy(entry.file).into_owned(),
            )
        })
        .collect();
    assert_eq!(
        entries,
        alloc::vec![
            (String::from("TAIRiX"), String::from("a.jpg")),
            (String::from("Space"), String::from("b.png")),
        ]
    );

    // Asking from the second entry answers only the remainder, and asking
    // past the end answers nothing rather than refusing.
    let answered = client.wallpapers(1, &mut page).expect("a catalog page");
    assert_eq!(answered.len(), 1);
    let answered = client.wallpapers(9, &mut page).expect("a catalog page");
    assert_eq!(answered.total, 2);
    assert!(answered.is_empty());
}

/// The choice space is the host's, answered whole rather than paged: it
/// fits one frame by construction, so a chooser learns every set in one
/// call.
#[test]
fn the_cursor_sets_are_answered_whole_from_the_hosts_own_listing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    loopback.borrow_mut().host.cursor_sets = alloc::vec![
        CursorSetName(String::from("Standard")),
        CursorSetName(String::from("High Visibility")),
    ];
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut frame = [0u8; tairix_abi::window_ipc::WINDOW_CURSOR_SETS_REPLY_MAX];

    let answered = client.cursor_sets(&mut frame).expect("the choice space");
    assert_eq!(answered.len(), 2);
    let names: Vec<String> = answered
        .names()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    assert_eq!(
        names,
        alloc::vec![String::from("Standard"), String::from("High Visibility")]
    );
}

/// A desktop that listed no store offers no sets of its own, which is an
/// answer rather than a failure: the built-in set is the client's to add.
#[test]
fn a_host_with_no_cursor_store_answers_an_empty_choice_space() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut frame = [0u8; tairix_abi::window_ipc::WINDOW_CURSOR_SETS_REPLY_MAX];
    let answered = client.cursor_sets(&mut frame).expect("the choice space");
    assert!(answered.is_empty());
    assert_eq!(answered.names().count(), 0);
}

/// The application a reserved request is decided against is the one the
/// kernel attests for that very call, handed to the host verbatim.
#[test]
fn a_notify_source_query_reaches_the_host_with_the_attested_application() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let settings = AppIdentity::new("os.tairix.settings", PublisherId::from_raw([7; 32]))
        .expect("a well-formed identity");
    loopback.borrow_mut().identity.apps = alloc::vec![(TICKET_A, settings)];
    loopback.borrow_mut().host.notify_sources =
        alloc::vec![BundleId::new("os.tairix.netstack").expect("a bounded identity")];
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut frame = [0u8; tairix_abi::window_ipc::WINDOW_NOTIFY_SOURCES_REPLY_MAX];

    let answered = client.notify_sources(&mut frame).expect("the host answers");
    assert!(answered.names().eq([b"os.tairix.netstack".as_slice()]));
    assert_eq!(loopback.borrow().host.asked_by, alloc::vec![Some(settings)]);

    assert_eq!(client.lock_screen(), Ok(()));
    assert_eq!(loopback.borrow().host.locks, 1);
}

/// A caller running no verified bundle is handed to the host as exactly that,
/// and the host's refusal reaches the caller whole.
#[test]
fn a_caller_with_no_application_is_refused_by_the_host() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut frame = [0u8; tairix_abi::window_ipc::WINDOW_NOTIFY_SOURCES_REPLY_MAX];
    assert_eq!(
        client.notify_sources(&mut frame).map(|list| list.len()),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(client.lock_screen(), Err(Errno::PermissionDenied));
    assert_eq!(loopback.borrow().host.asked_by, alloc::vec![None, None]);
    assert_eq!(loopback.borrow().host.locks, 0);
}

/// An attestation that fails refuses the request before the host is asked.
#[test]
fn a_failed_attestation_refuses_a_reserved_request_before_the_host() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    loopback.borrow_mut().identity.attest_error = Some(Errno::NotFound);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    assert_eq!(client.lock_screen(), Err(Errno::NotFound));
    assert!(loopback.borrow().host.asked_by.is_empty());
}

/// A desktop that listed no store answers an empty catalog, not an error:
/// offering no shipped pictures is a fact, not a failure.
#[test]
fn a_host_with_no_catalog_answers_an_empty_page() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let mut page = [0u8; tairix_abi::window_ipc::WINDOW_WALLPAPERS_REPLY_MAX];
    let answered = client.wallpapers(0, &mut page).expect("a catalog page");
    assert_eq!(answered.total, 0);
    assert!(answered.is_empty());
}

/// The same discipline a pick has: owner-bound, one pending per window,
/// and concluded exactly once by its own event.
#[test]
fn a_wallpaper_render_is_owner_bound_single_pending_and_concluded_by_delivery() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.render_wallpaper(window, 0x99, 0, 64),
        Err(Errno::NotFound)
    );
    loopback.borrow_mut().ticket = TICKET_A;

    client
        .render_wallpaper(window, 0x99, 0, 64)
        .expect("render accepted");
    assert_eq!(
        loopback.borrow().host.renders,
        alloc::vec![(window, 0x99, 0, 64)]
    );
    assert_eq!(
        client.render_wallpaper(window, 0x99, 1, 64),
        Err(Errno::AlreadyExists)
    );
    assert_eq!(loopback.borrow().host.renders.len(), 1);

    let mut sink = QueueSink::default();
    let concluded = WindowEvent::WallpaperRendered {
        window_id: window,
        index: 0,
        side: 64,
        rendered: true,
    };
    deliver(&loopback, &mut sink, &concluded).expect("conclusion delivered");
    assert_eq!(sink.delivered.len(), 1);
    // Exactly one conclusion per acceptance.
    assert_eq!(
        deliver(&loopback, &mut sink, &concluded),
        Err(Errno::OutOfRange)
    );
    client
        .render_wallpaper(window, 0x99, 1, 64)
        .expect("a fresh render is accepted");
}

/// A refused render leaves nothing pending, so the caller may ask again.
#[test]
fn a_refused_render_leaves_no_pending_conclusion() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    loopback.borrow_mut().host.refuse_render = Some(Errno::LengthOutOfRange);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    assert_eq!(
        client.render_wallpaper(window, 0x99, 0, 64),
        Err(Errno::LengthOutOfRange)
    );
    loopback.borrow_mut().host.refuse_render = None;
    client
        .render_wallpaper(window, 0x99, 0, 64)
        .expect("the window was left free to ask again");
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
    let mut identity = MockIdentity::holding_layer(&[]);
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
    let len = server.serve(
        &mut host,
        &mut QueueSink::default(),
        &mut identity,
        TICKET_A,
        &create,
        &mut reply,
    );
    let (window, _) = tairix_abi::window_ipc::decode_create_reply(&reply[..len]).expect("created");

    // Setting the backdrop blur is infallible for an owned window: the
    // default handler has no compositor to tell.
    let blur = request_frame(&WindowRequest::SetBackdropBlur {
        window_id: window,
        radius_px: 8,
    });
    let len = server.serve(
        &mut host,
        &mut QueueSink::default(),
        &mut identity,
        TICKET_A,
        &blur,
        &mut reply,
    );
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
fn sample_menu_anchor() -> WindowRegion {
    WindowRegion::new(12, 30, 0, 0).expect("a representable anchor")
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

/// The text the user committed into a chain's quick-entry field is held for
/// the owning window alone, answered once, and cleared by the next open.
///
/// The commit is pulled rather than delivered because an event is one fixed
/// frame and a name is wider than that, so the rules that make an outcome
/// unmistakable have to hold for the pull too.
#[test]
fn committed_menu_text_is_owner_bound_taken_once_and_cleared_by_the_next_open() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    let other = create_id(&mut client, 8, EVENTS_A, 1, "b").expect("b");
    let menu = sample_open_menu();
    let anchor = sample_menu_anchor();
    let open = client
        .open_menu(window, anchor, &menu)
        .expect("the open is accepted");

    // With nothing recorded the pull answers the honest empty rather than a
    // fabricated name.
    assert_eq!(client.take_menu_text(window, open), Ok(None));

    // A text may only be recorded against the open that window is still
    // waiting on: any other names a gesture whose answer has gone.
    assert_eq!(
        loopback
            .borrow_mut()
            .server
            .record_menu_text(window, open + 1, "stale.txt"),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        loopback
            .borrow_mut()
            .server
            .record_menu_text(other, open, "elsewhere.txt"),
        Err(Errno::OutOfRange)
    );
    // And never wider than a field can hold.
    let over = "n".repeat(APP_MENU_ENTRY_MAX + 1);
    assert_eq!(
        loopback
            .borrow_mut()
            .server
            .record_menu_text(window, open, &over),
        Err(Errno::LengthOutOfRange)
    );

    loopback
        .borrow_mut()
        .server
        .record_menu_text(window, open, "report.txt")
        .expect("the commit is recorded against its own open");

    // A window the caller does not own answers exactly like one that never
    // existed, and reads nothing.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.take_menu_text(window, open),
        Err(Errno::NotFound),
        "a foreign pull is refused"
    );
    loopback.borrow_mut().ticket = TICKET_A;

    // A pull naming another open answers nothing and leaves the held text
    // for the pull that names it.
    assert_eq!(client.take_menu_text(window, open + 1), Ok(None));
    assert_eq!(
        client.take_menu_text(window, open),
        Ok(Some(String::from("report.txt")))
    );
    // Taken once: two readers cannot both act on one commit.
    assert_eq!(client.take_menu_text(window, open), Ok(None));

    // A commit the application never pulled belongs to the gesture that is
    // over, so the next open on that window clears it.
    let mut sink = QueueSink::default();
    deliver(
        &loopback,
        &mut sink,
        &WindowEvent::MenuClosed {
            window_id: window,
            open_id: open,
            outcome: MenuOutcome::Entered(AppMenuItemId::new(90).expect("a valid id")),
        },
    )
    .expect("the outcome delivers");
    let again = client
        .open_menu(window, anchor, &menu)
        .expect("a fresh open is accepted");
    loopback
        .borrow_mut()
        .server
        .record_menu_text(window, again, "unpulled.txt")
        .expect("recorded against the fresh open");
    deliver(
        &loopback,
        &mut sink,
        &WindowEvent::MenuClosed {
            window_id: window,
            open_id: again,
            outcome: MenuOutcome::Dismissed,
        },
    )
    .expect("the outcome delivers");
    let third = client
        .open_menu(window, anchor, &menu)
        .expect("a third open is accepted");
    assert_eq!(
        client.take_menu_text(window, again),
        Ok(None),
        "the next open cleared the commit nobody pulled"
    );
    assert_eq!(client.take_menu_text(window, third), Ok(None));
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
    let mut identity = MockIdentity::holding_layer(&[]);
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
    let len = server.serve(
        &mut host,
        &mut QueueSink::default(),
        &mut identity,
        TICKET_A,
        &create,
        &mut reply,
    );
    let (window, _) = tairix_abi::window_ipc::decode_create_reply(&reply[..len]).expect("created");

    let open = request_frame(&WindowRequest::OpenMenu {
        window_id: window,
        anchor: sample_menu_anchor(),
        menu: sample_open_menu(),
    });
    let len = server.serve(
        &mut host,
        &mut QueueSink::default(),
        &mut identity,
        TICKET_A,
        &open,
        &mut reply,
    );
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

fn rect(x: u32, y: u32, width_px: u32, height_px: u32) -> DamageRect {
    DamageRect {
        x,
        y,
        width_px,
        height_px,
    }
}

#[test]
fn a_retained_present_resends_what_an_earlier_one_left_torn() {
    let torn = Some(rect(0, 0, 1, 1));
    assert_eq!(
        retained_damage(&SURFACE, false, torn, rect(2, 1, 1, 1)),
        Some(rect(0, 0, 3, 2))
    );
    assert_eq!(
        retained_damage(&SURFACE, false, None, rect(2, 1, 1, 1)),
        Some(rect(2, 1, 1, 1))
    );
}

#[test]
fn a_released_window_is_repainted_whole() {
    assert_eq!(
        retained_damage(&SURFACE, true, Some(rect(0, 0, 1, 1)), rect(2, 1, 1, 1)),
        Some(DamageRect::full(&SURFACE))
    );
}

/// A rectangle past the surface was once left torn as named when the frame
/// codec refused it, and every later present, widened over it, was refused
/// too until a resize: a window that ignored present errors froze silently.
#[test]
fn a_rectangle_past_the_surface_is_sent_clipped_so_it_cannot_poison_later_presents() {
    let first = retained_damage(&SURFACE, false, None, rect(2, 1, 50, 50));
    assert_eq!(first, Some(rect(2, 1, 2, 2)));
    let next = retained_damage(&SURFACE, false, first, rect(0, 0, 1, 1));
    assert_eq!(next, Some(rect(0, 0, 4, 3)));
    let edges = [0, 1, 3, 4, 5, u32::MAX - 1, u32::MAX];
    let mut named = Vec::new();
    for x in edges {
        for y in edges {
            for width in edges {
                named.extend(edges.map(|height| rect(x, y, width, height)));
            }
        }
    }
    for damage in named {
        let torn = retained_damage(&SURFACE, false, None, damage);
        for sent in [
            torn,
            retained_damage(&SURFACE, false, torn, rect(1, 1, 1, 1)),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(sent.validate_in(&SURFACE), Ok(()), "{damage:?}");
        }
    }
}

#[test]
fn a_rectangle_wholly_outside_the_window_repaints_nothing() {
    assert_eq!(
        retained_damage(&SURFACE, false, None, rect(4, 0, 2, 2)),
        None
    );
    assert_eq!(
        retained_damage(&SURFACE, false, None, rect(0, 0, 0, 3)),
        None
    );
    assert_eq!(
        retained_damage(&SURFACE, false, None, rect(u32::MAX, 0, 8, 1)),
        None
    );
}

// ---- the open-target channel and the tooltip declaration ---------------

/// Client A's attested identity, which its own targets are queued under.
fn app_a() -> ProcId {
    proc_id(0xA1)
}

/// A path entry for `path`.
fn path_entry(path: &str) -> OpenEntry {
    OpenEntry::Path(String::from(path))
}

#[test]
fn an_open_target_is_queued_by_the_session_and_pulled_once_by_its_owner() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    // Nothing queued is the honest empty answer, not an error.
    assert_eq!(client.take_open_target(), Ok(None));

    loopback
        .borrow_mut()
        .server
        .hand_over_open_target(&mut sink, app_a(), path_entry("Users:/ada/Documents"))
        .expect("the application takes it");
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Path(String::from("Users:/ada/Documents"))))
    );
    assert_eq!(
        client.take_open_target(),
        Ok(None),
        "popping is what makes a target one-shot"
    );
}

#[test]
fn a_document_hand_over_carries_its_delegation_and_its_name() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    loopback
        .borrow_mut()
        .server
        .hand_over_open_target(
            &mut sink,
            app_a(),
            OpenEntry::Document {
                name: String::from("holiday.png"),
                grant: 42,
            },
        )
        .expect("the application takes it");
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Document {
            name: String::from("holiday.png"),
            grant: 42,
        })),
        "a document is the form an application with no filesystem reach can open"
    );

    // A document with no delegation is nothing to open, so it never queues.
    assert_eq!(
        loopback.borrow_mut().server.hand_over_open_target(
            &mut sink,
            app_a(),
            OpenEntry::Document {
                name: String::from("holiday.png"),
                grant: 0,
            },
        ),
        Err(Errno::OutOfRange)
    );
    assert_eq!(client.take_open_target(), Ok(None));
}

#[test]
fn one_delegation_handle_is_queued_once_however_often_it_is_handed_over() {
    // A grant handle is one-shot, and the kernel hands the *same* handle back
    // when the same authority is granted to the same process twice. Queueing
    // it twice would promise a second document the first pull consumes, so
    // the repeat is the answer rather than a second entry.
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    let document = OpenEntry::Document {
        name: String::from("holiday.png"),
        grant: 42,
    };
    for _ in 0..3 {
        loopback
            .borrow_mut()
            .server
            .hand_over_open_target(&mut sink, app_a(), document.clone())
            .expect("a repeat is taken");
    }
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Document {
            name: String::from("holiday.png"),
            grant: 42,
        }))
    );
    assert_eq!(client.take_open_target(), Ok(None), "one handle, one entry");
}

#[test]
fn handing_over_a_target_wakes_its_owner() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    loopback
        .borrow_mut()
        .server
        .hand_over_open_target(&mut sink, app_a(), path_entry("Users:/ada/report"))
        .expect("the application takes it");
    let (endpoint, bytes) = sink.delivered.pop_front().expect("a wake was announced");
    assert_eq!(endpoint, EVENTS_A);
    assert_eq!(
        WindowEvent::from_bytes(&bytes),
        Ok(WindowEvent::OpenRequested),
        "the wake names the application and carries no target"
    );
    assert!(sink.delivered.is_empty(), "one target, one wake");
}

#[test]
fn a_windowless_application_is_reached_through_its_icon_bar_route() {
    // The instance a hand-over most needs to reach: a resident
    // single-instance application sitting on the icon bar with nothing open.
    // A window-scoped queue would leave it unreachable.
    let loopback = Loopback::with_regions(&[]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    client
        .set_app_bar(&sample_app_bar(EVENTS_A))
        .expect("the bar declaration is accepted");

    loopback
        .borrow_mut()
        .server
        .hand_over_open_target(&mut sink, app_a(), path_entry("Users:/ada/report"))
        .expect("an application with no window still has a route");
    let (endpoint, bytes) = sink.delivered.pop_front().expect("a wake was announced");
    assert_eq!(endpoint, EVENTS_A);
    assert_eq!(
        WindowEvent::from_bytes(&bytes),
        Ok(WindowEvent::OpenRequested)
    );
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Path(String::from("Users:/ada/report"))))
    );
}

#[test]
fn an_application_with_no_route_at_all_takes_nothing() {
    // No window and no icon-bar presence is no live instance to hand
    // anything to: the session is told so and starts a fresh process
    // instead of stranding a target.
    let loopback = Loopback::with_regions(&[]);
    let mut sink = QueueSink::default();
    assert_eq!(
        loopback.borrow_mut().server.hand_over_open_target(
            &mut sink,
            app_a(),
            path_entry("Users:/ada/report")
        ),
        Err(Errno::NotFound)
    );
    assert!(sink.delivered.is_empty());
}

#[test]
fn a_refused_wake_takes_the_target_back_off_the_queue() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");

    // Nothing was announced, so nothing may be left queued: a target the
    // owner was never woken for would sit unreachable, and the caller must
    // be free to read the refusal as "the instance does not have it".
    assert_eq!(
        loopback.borrow_mut().server.hand_over_open_target(
            &mut FullSink,
            app_a(),
            path_entry("Users:/ada/report")
        ),
        Err(Errno::WouldBlock)
    );
    assert_eq!(
        client.take_open_target(),
        Ok(None),
        "a refused hand-over strands no target"
    );
}

#[test]
fn queued_targets_are_pulled_oldest_first() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    for path in ["Users:/one", "Users:/two", "Users:/three"] {
        loopback
            .borrow_mut()
            .server
            .hand_over_open_target(&mut sink, app_a(), path_entry(path))
            .expect("room");
    }
    for path in ["Users:/one", "Users:/two", "Users:/three"] {
        assert_eq!(
            client.take_open_target(),
            Ok(Some(Target::Path(String::from(path)))),
            "the ordering is the protocol"
        );
    }
    assert_eq!(client.take_open_target(), Ok(None));
}

#[test]
fn a_pull_reaches_only_the_callers_own_queue() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    loopback
        .borrow_mut()
        .server
        .hand_over_open_target(&mut sink, app_a(), path_entry("Users:/ada/secret"))
        .expect("room");

    // Another client's pull answers like a drained queue: the identity the
    // kernel attests is the scope, so the reply says nothing about who else
    // has something waiting.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(client.take_open_target(), Ok(None));
    loopback.borrow_mut().ticket = TICKET_A;

    // And that pull took nothing: the owner still finds its target.
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Path(String::from("Users:/ada/secret"))))
    );
}

#[test]
fn the_open_target_queue_refuses_rather_than_dropping_or_growing() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    for index in 0..WINDOW_MAX_OPEN_TARGETS {
        loopback
            .borrow_mut()
            .server
            .hand_over_open_target(
                &mut sink,
                app_a(),
                path_entry(&alloc::format!("Users:/{index}")),
            )
            .expect("within the bound");
    }
    assert_eq!(
        loopback.borrow_mut().server.hand_over_open_target(
            &mut sink,
            app_a(),
            path_entry("Users:/one-too-many")
        ),
        Err(Errno::NoSpace),
        "the newest is refused rather than an older one dropped silently"
    );
    // The oldest is still first: a refusal at the far end disturbs nothing.
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Path(String::from("Users:/0"))))
    );

    // An empty or over-long path is refused too.
    assert_eq!(
        loopback
            .borrow_mut()
            .server
            .hand_over_open_target(&mut sink, app_a(), path_entry("")),
        Err(Errno::LengthOutOfRange)
    );
    let long = "p".repeat(tairix_abi::FS_PATH_MAX + 1);
    assert_eq!(
        loopback
            .borrow_mut()
            .server
            .hand_over_open_target(&mut sink, app_a(), path_entry(&long)),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn an_applications_queued_targets_die_with_the_client() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut sink = QueueSink::default();
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    client
        .set_app_bar(&sample_app_bar(EVENTS_A))
        .expect("the bar declaration is accepted");
    loopback
        .borrow_mut()
        .server
        .hand_over_open_target(&mut sink, app_a(), path_entry("Users:/ada/report"))
        .expect("room");

    // Closing the window leaves the queue alone: the application is still
    // there, still on the icon bar it declared, and the target is still its
    // to take.
    client.close(window).expect("the owner closes it");
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Path(String::from("Users:/ada/report")))),
        "a target outlives the window that happened to be open"
    );

    // The client going is what drops it: a target queued for a process that
    // has gone is reachable by nothing, and its delegation dies with it.
    loopback
        .borrow_mut()
        .server
        .hand_over_open_target(&mut sink, app_a(), path_entry("Users:/ada/report"))
        .expect("room");
    let mut host = RecordingHost::default();
    loopback
        .borrow_mut()
        .server
        .client_exited(&mut host, app_a());
    assert_eq!(
        loopback.borrow_mut().server.hand_over_open_target(
            &mut sink,
            app_a(),
            path_entry("Users:/ada/report")
        ),
        Err(Errno::NotFound),
        "a client with no windows and no bar has no route left"
    );
    assert_eq!(
        client.take_open_target(),
        Ok(None),
        "and the queue went with it"
    );
}

#[test]
fn a_hand_over_reaches_the_host_with_the_callers_attested_identity() {
    let loopback = Loopback::with_regions(&[]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    // The host answers "nothing to reach" by default, which is the caller's
    // cue to launch the bundle itself rather than a refusal.
    assert_eq!(
        client.hand_over_launch("/System/Applications/view.app/Run", None),
        Ok(HandOverOutcome::NotRunning)
    );
    let document = HandOverDocument {
        name: DocumentName::new("holiday.png").expect("a valid name"),
        grant: 31,
    };
    // A host that reaches an instance queues through the desk the engine
    // lends it, so the instance needs a route for the wake to reach.
    client
        .set_app_bar(&sample_app_bar(EVENTS_A))
        .expect("the bar declaration is accepted");
    loopback.borrow_mut().host.hand_over = Ok(HandOverOutcome::Reached);
    assert_eq!(
        client.hand_over_launch("/System/Applications/view.app/Run", Some(document)),
        Ok(HandOverOutcome::Reached)
    );
    assert_eq!(
        client.take_open_target(),
        Ok(Some(Target::Document {
            name: String::from("holiday.png"),
            grant: 31,
        })),
        "the engine's half of the hand-over really queued it"
    );

    // An instance the desk cannot reach is answered `NotRunning`, whatever
    // the host believed: a launch that reached nothing must fall back to
    // starting a process rather than being reported as delivered.
    let mut host = RecordingHost {
        hand_over: Ok(HandOverOutcome::Reached),
        ..RecordingHost::default()
    };
    let mut server = WindowServer::new(MockMapper::with_regions(&[]), SERVER, CLIENT_FRAME_MAX);
    let mut reply = [0u8; WINDOW_REPLY_MAX];
    let frame = request_frame(&WindowRequest::HandOverLaunch {
        run_path: tairix_abi::window_ipc::BundleRunPath::new("/System/Applications/view.app/Run")
            .expect("a valid bundle path"),
        document: None,
    });
    let len = server.serve(
        &mut host,
        &mut QueueSink::default(),
        &mut MockIdentity::holding_layer(&[]),
        TICKET_A,
        &frame,
        &mut reply,
    );
    assert_eq!(
        tairix_abi::window_ipc::decode_hand_over_reply(&reply[..len]),
        Ok(HandOverOutcome::NotRunning),
        "an unreachable instance is not a reached one"
    );
    assert_eq!(
        loopback.borrow().host.hand_overs,
        alloc::vec![
            (
                proc_id(0xA1),
                String::from("/System/Applications/view.app/Run"),
                None
            ),
            (
                proc_id(0xA1),
                String::from("/System/Applications/view.app/Run"),
                Some(document)
            ),
        ],
        "the host sees who asked, from the kernel and not from the wire"
    );

    // A refusal is relayed as itself, and a path longer than a hand-over may
    // name never reaches the channel at all.
    loopback.borrow_mut().host.hand_over = Err(Errno::NoSpace);
    assert_eq!(
        client.hand_over_launch("/System/Applications/view.app/Run", None),
        Err(Errno::NoSpace)
    );
    let long = "p".repeat(HAND_OVER_RUN_PATH_MAX + 1);
    assert_eq!(
        client.hand_over_launch(&long, None),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        loopback.borrow().host.hand_overs.len(),
        3,
        "an over-long path is refused before the session is asked"
    );
}

#[test]
fn a_tooltip_declaration_reaches_the_host_and_replaces_rather_than_appends() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    let region = sample_menu_anchor();
    let other = WindowRegion::new(4, 8, 20, 12).expect("a second region");

    // A window the caller does not own answers like one that never existed,
    // and the host is never told.
    loopback.borrow_mut().ticket = TICKET_B;
    assert_eq!(
        client.set_tooltip(window, region, tip("Copy")),
        Err(Errno::NotFound)
    );
    assert!(loopback.borrow().host.tooltips.is_empty());
    loopback.borrow_mut().ticket = TICKET_A;

    client
        .set_tooltip(window, region, tip("Copy"))
        .expect("the owner may declare one");
    client
        .set_tooltip(window, other, tip("Paste"))
        .expect("and re-declare it");
    client
        .set_tooltip(window, other, tip(""))
        .expect("and withdraw it");
    {
        let host = &loopback.borrow().host;
        assert_eq!(
            host.tooltips.len(),
            3,
            "each declaration is relayed; the host holds at most one at a time"
        );
        assert_eq!(host.tooltips[0], (window, region, String::from("Copy")));
        assert_eq!(host.tooltips[1], (window, other, String::from("Paste")));
        assert_eq!(
            host.tooltips[2],
            (window, other, String::new()),
            "empty text is the withdrawal"
        );
    }
}

#[test]
fn a_host_that_shows_no_tooltip_refuses_and_the_app_carries_on() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let window = create_id(&mut client, 7, EVENTS_A, 1, "a").expect("a");
    loopback.borrow_mut().host.refuse_tooltip = Some(Errno::NotSupported);
    assert_eq!(
        client.set_tooltip(window, sample_menu_anchor(), tip("Copy")),
        Err(Errno::NotSupported),
        "a refused tip is an answer the app reports and carries on from"
    );
    assert!(loopback.borrow().host.tooltips.is_empty());
}

/// A tooltip's text, which the tests state as a plain literal.
fn tip(text: &str) -> TooltipText {
    TooltipText::new(text).expect("a valid tip")
}

/// A one-frame SURFACE-shaped layer surface granted as `shm`, its events
/// routed to `events`, opening at `(x, y)` in `depth`.
fn layer_spec(shm: u64, events: u64, x: i32, y: i32, depth: LayerDepth) -> LayerSpec {
    LayerSpec {
        shm_handle: shm,
        event_endpoint: events,
        frame_count: 1,
        surface: SURFACE,
        x,
        y,
        depth,
    }
}

#[test]
fn opening_a_layer_surface_needs_the_capability_and_reaches_the_host() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));

    // The gate is the kernel's attestation of the caller, not anything the
    // caller said: with the capability withheld the open is refused before
    // the host is told anything at all.
    loopback.borrow_mut().identity = MockIdentity::holding_layer(&[]);
    assert_eq!(
        client
            .open_layer(&layer_spec(7, EVENTS_A, 10, 20, LayerDepth::Above))
            .map(|(id, _)| id),
        Err(Errno::PermissionDenied)
    );
    assert!(
        loopback.borrow().host.layers.is_empty(),
        "a refused open must not reach the host"
    );

    // Granted, the open carries the attested owner, the geometry, the
    // screen point, and the depth through to the host.
    loopback.borrow_mut().identity = MockIdentity::holding_layer(&[TICKET_A]);
    let (id, _) = client
        .open_layer(&layer_spec(7, EVENTS_A, 10, 20, LayerDepth::Above))
        .expect("a held capability opens the surface");
    let host = &loopback.borrow().host;
    assert_eq!(
        host.layers,
        [(proc_id(0xA1), id, SURFACE, 10, 20, LayerDepth::Above)]
    );
}

#[test]
fn a_failed_capability_attestation_refuses_rather_than_grants() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    loopback.borrow_mut().identity.attest_error = Some(Errno::NotFound);
    assert_eq!(
        client
            .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
            .map(|(id, _)| id),
        Err(Errno::NotFound),
        "an attestation the kernel could not answer fails closed"
    );
    assert!(loopback.borrow().host.layers.is_empty());
}

#[test]
fn every_layer_operation_is_gated_not_only_the_open() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let (id, _) = client
        .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("opened while held");

    // Revoking the grant stops the surface at its next request rather than
    // letting it run as long as the process does.
    loopback.borrow_mut().identity = MockIdentity::holding_layer(&[]);
    assert_eq!(
        client.place_layer(id, 5, 5, LayerDepth::Above),
        Err(Errno::PermissionDenied)
    );
    let mut plates = [TerrainPlate {
        x: 0,
        y: 0,
        width_px: 1,
        height_px: 1,
    }; 4];
    assert_eq!(
        client.take_terrain(id, &mut plates).map(<[_]>::len),
        Err(Errno::PermissionDenied)
    );
    assert!(loopback.borrow().host.layer_places.is_empty());
}

#[test]
fn a_layer_surface_is_one_per_client() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    client
        .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("the first is allowed");
    assert_eq!(DESKTOP_LAYER_MAX_PER_CLIENT, 1);
    assert_eq!(
        client
            .open_layer(&layer_spec(8, EVENTS_A, 0, 0, LayerDepth::Below))
            .map(|(id, _)| id),
        Err(Errno::LimitExceeded),
        "a second would multiply the screen area one holder occupies"
    );
}

#[test]
fn closing_a_layer_surface_frees_its_slot() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let (id, _) = client
        .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("the first is allowed");

    // Retiring is the ordinary `Close`: a layer surface is a window in the
    // one registry, so it needs no teardown path of its own.
    client.close(id).expect("closed");
    assert!(loopback.borrow().host.closed.contains(&id));
    client
        .open_layer(&layer_spec(8, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("the slot is free again");
}

#[test]
fn place_layer_is_owner_bound_and_refuses_an_ordinary_window() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let (layer, _) = client
        .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("opened");
    let window = create_id(&mut client, 8, EVENTS_A, 1, "a").expect("a window");

    // An ordinary window is placed by the window manager; letting this move
    // one would hand every holder authority over its own windows' positions.
    assert_eq!(
        client.place_layer(window, 1, 2, LayerDepth::Above),
        Err(Errno::NotSupported)
    );
    // A window nobody owns here leaks nothing beyond "not found".
    assert_eq!(
        client.place_layer(9_999, 1, 2, LayerDepth::Above),
        Err(Errno::NotFound)
    );
    client
        .place_layer(layer, 3, 4, LayerDepth::Above)
        .expect("its own surface moves");
    assert_eq!(
        loopback.borrow().host.layer_places,
        [(layer, 3, 4, LayerDepth::Above)]
    );
}

#[test]
fn a_refused_place_leaves_the_recorded_depth_alone() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let (layer, _) = client
        .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("opened");

    // The engine records the depth only once the host accepted it, so the
    // two can never disagree about what is actually stacked.
    loopback.borrow_mut().host.refuse_layer = Some(Errno::WouldBlock);
    assert_eq!(
        client.place_layer(layer, 1, 1, LayerDepth::Above),
        Err(Errno::WouldBlock)
    );
    loopback.borrow_mut().host.refuse_layer = None;
    client
        .place_layer(layer, 1, 1, LayerDepth::Above)
        .expect("accepted once the host can");
}

#[test]
fn terrain_is_answered_for_a_layer_surface_and_refused_for_anything_else() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN), (8, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let (layer, _) = client
        .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("opened");
    let window = create_id(&mut client, 8, EVENTS_A, 1, "a").expect("a window");

    let reported = [
        TerrainPlate {
            x: 0,
            y: 0,
            width_px: 100,
            height_px: 40,
        },
        TerrainPlate {
            x: 30,
            y: 60,
            width_px: 20,
            height_px: 20,
        },
    ];
    loopback.borrow_mut().host.terrain = reported.to_vec();

    let mut plates = [TerrainPlate {
        x: 0,
        y: 0,
        width_px: 1,
        height_px: 1,
    }; 8];
    assert_eq!(client.take_terrain(layer, &mut plates), Ok(&reported[..]));

    // An ordinary window has no terrain to ask about: the desktop's shape
    // is what the layer authority buys, not something every window may read.
    assert_eq!(
        client.take_terrain(window, &mut plates).map(<[_]>::len),
        Err(Errno::NotSupported)
    );
}

#[test]
fn a_refused_terrain_query_reaches_the_caller_as_a_refusal() {
    let loopback = Loopback::with_regions(&[(7, FRAME_LEN)]);
    let mut client = WindowClient::new(Rc::clone(&loopback));
    let (layer, _) = client
        .open_layer(&layer_spec(7, EVENTS_A, 0, 0, LayerDepth::Below))
        .expect("opened");
    loopback.borrow_mut().host.refuse_terrain = Some(Errno::SeatRevoked);
    let mut plates = [TerrainPlate {
        x: 0,
        y: 0,
        width_px: 1,
        height_px: 1,
    }; 4];
    assert_eq!(
        client.take_terrain(layer, &mut plates).map(<[_]>::len),
        Err(Errno::SeatRevoked),
        "a refusal must not read as an empty desktop"
    );
}

#[test]
fn a_host_that_has_not_implemented_the_layer_refuses_it() {
    // The default host bridge answers `NotSupported` rather than silently
    // accepting: privileged authority a host never implemented must not
    // appear to be granted.
    struct BareHost;
    impl WindowHost for BareHost {
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

        fn window_sizing_changed(
            &mut self,
            _window_id: u64,
            _sizing: WindowSizing,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn window_size_state_changed(
            &mut self,
            _window_id: u64,
            _state: WindowSizeState,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn window_retitled(&mut self, _window_id: u64, _title: &str) -> Result<(), Errno> {
            Ok(())
        }

        fn pick_requested(&mut self, _window_id: u64) -> Result<(), Errno> {
            Ok(())
        }

        fn window_closed(&mut self, _window_id: u64) {}

        fn desktop(&mut self) -> Result<DesktopInfo, Errno> {
            Ok(sample_desktop())
        }
    }

    let mut server = WindowServer::new(
        MockMapper::with_regions(&[(7, FRAME_LEN)]),
        SERVER,
        CLIENT_FRAME_MAX,
    );
    let mut reply = [0u8; WINDOW_REPLY_MAX];
    let request = WindowRequest::OpenLayer {
        shm_handle: 7,
        event_endpoint: EVENTS_A,
        frame_count: 1,
        width_px: SURFACE.width_px,
        height_px: SURFACE.height_px,
        stride_bytes: SURFACE.stride_bytes,
        format: SURFACE.format,
        x: 0,
        y: 0,
        depth: LayerDepth::Above,
    };
    let mut frame = [0u8; 128];
    let len = request.encode(&mut frame).expect("encodes");
    let written = server.serve(
        &mut BareHost,
        &mut QueueSink::default(),
        &mut MockIdentity::holding_layer(&[TICKET_A]),
        TICKET_A,
        &frame[..len],
        &mut reply,
    );
    assert_eq!(
        decode_status_reply(&reply[..written.min(4)]),
        Err(Errno::NotSupported)
    );
}
