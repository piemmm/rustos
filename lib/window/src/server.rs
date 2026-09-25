//! The server half: the engine the desktop session composes.
//!
//! [`WindowServer::serve`] handles exactly one received request:
//! decode → caller attestation → owner/bounds validation → action →
//! encoded reply. The hosting session owns the transport (bind the
//! reserved `WINDOW_ENDPOINT`, park on its wait-set, `call_recv` /
//! `call_reply`); the engine owns every decision in between, so the
//! semantics are host-testable against mock seams and identical under
//! any host.
//!
//! # Security shape
//!
//! * **Identity is kernel-attested.** The engine never reads a claimed
//!   owner: [`CallerIdentity::caller`] asks the kernel who the
//!   *in-flight caller of this endpoint* is (`call_peer_origin`'s
//!   unforgeable `ProcId`), and every request is attributed to it
//!   before any state is read or mutated. A kernel-domain caller is
//!   refused outright — a window belongs to a user process instance.
//! * **Windows are keyed to their creator.** A `Present` or `Close`
//!   naming a window the caller does not own answers `NotFound`,
//!   indistinguishable from a window that never existed, so the id
//!   space leaks nothing about other apps' windows.
//! * **Every bound is checked before any pixel access.** The mapped
//!   region must hold every frame at create time, the frame index must
//!   name a created frame, and the damage rectangle must lie inside the
//!   window's surface; each failure is a typed refusal, never a clamp.
//! * **A hostile client is bounded.** At most
//!   [`client_frame_budget_bytes`] of mapped window frame per attested
//!   client — bytes, not windows, because that is the resource a window
//!   actually spends here — so one app cannot pin unbounded shared memory
//!   or flood the taskbar, however large or numerous its windows.
//! * **Teardown fails closed.** [`WindowServer::client_exited`] drops a
//!   dead client's windows (and their region mappings) and tells the
//!   host, so an exited app never leaks a mapped grant or a ghost
//!   taskbar entry.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::desktop::DesktopInfo;
use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_abi::origin::{AppIdentity, ProcId};
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::window_ipc::{
    encode_create_reply, encode_cursor_sets_reply, encode_desktop_reply, encode_hand_over_reply,
    encode_menu_text_reply, encode_minted_id_reply, encode_notify_sources_reply,
    encode_open_target_reply, encode_terrain_reply, encode_wallpapers_reply, AppBar, AppMenu,
    HandOverDocument, HandOverOutcome, LayerDepth, OpenTarget, TerrainPlate, WallpaperEntry,
    WindowEvent, WindowRegion, WindowRequest, WindowTitle, APP_MENU_ENTRY_MAX,
    DESKTOP_LAYER_MAX_PER_CLIENT, DESKTOP_LAYER_MAX_PER_SEAT, DESKTOP_LAYER_MAX_PLATES,
    WINDOW_CREATE_REPLY_LEN, WINDOW_CURSOR_SETS_REPLY_MAX, WINDOW_DESKTOP_REPLY_LEN,
    WINDOW_HAND_OVER_REPLY_LEN, WINDOW_MAX_OPEN_TARGETS, WINDOW_MENU_TEXT_REPLY_MAX,
    WINDOW_MINTED_ID_REPLY_LEN, WINDOW_NOTIFY_SOURCES_REPLY_MAX, WINDOW_OPEN_TARGET_REPLY_MAX,
    WINDOW_TERRAIN_REPLY_MAX, WINDOW_WALLPAPERS_REPLY_MAX,
};
pub use tairix_abi::window_ipc::{WindowSizeState, WindowSizing};
use tairix_abi::{BundleId, CapabilityId, Errno};
use tairix_display::{FrameRegion, ShmMapper};

/// Upper bound, in bytes, of any reply [`WindowServer::serve`] writes,
/// so one fixed buffer holds every outcome: the open-target frame (the
/// widest, since a path is), the create frame, the desktop frame, the
/// hand-over frame, or the status frame that fits inside any of them.
///
/// Derived from every reply the channel has rather than from the ones that
/// happen to be widest today, so an operation whose reply outgrew the buffer
/// could not slip past.
///
/// A caller holds this buffer **once** for the life of its serve loop rather
/// than taking one per request: it is sized to the widest reply the channel
/// has, and a per-request array would cost a present — the hottest operation
/// and one of the shortest — the whole of the widest one's clearing.
pub const WINDOW_REPLY_MAX: usize = {
    const fn wider(a: usize, b: usize) -> usize {
        if a > b {
            a
        } else {
            b
        }
    }
    wider(
        wider(WINDOW_CREATE_REPLY_LEN, WINDOW_DESKTOP_REPLY_LEN),
        wider(
            wider(WINDOW_OPEN_TARGET_REPLY_MAX, WINDOW_HAND_OVER_REPLY_LEN),
            wider(
                wider(WINDOW_MINTED_ID_REPLY_LEN, WINDOW_MENU_TEXT_REPLY_MAX),
                wider(
                    WINDOW_TERRAIN_REPLY_MAX,
                    wider(
                        WINDOW_WALLPAPERS_REPLY_MAX,
                        wider(
                            WINDOW_CURSOR_SETS_REPLY_MAX,
                            WINDOW_NOTIFY_SOURCES_REPLY_MAX,
                        ),
                    ),
                ),
            ),
        ),
    )
};

/// The minted-id reply a menu open answers with is the shortest of the three,
/// so the one buffer above already holds it.
const _: () = assert!(WINDOW_MINTED_ID_REPLY_LEN <= WINDOW_REPLY_MAX);

/// The share of the machine's RAM one attested client may hold mapped in the
/// session as window frames: a quarter of it.
///
/// The *physical* memory behind those frames is the client's own allocation,
/// already bounded by its address-space limit and by the machine; what this
/// bound protects is the session, which must not let one client's windows
/// crowd out every other client's ability to map and draw. A quarter is far
/// more than any application's windows measure — fifty terminal windows on a
/// 256 MiB board, and thirty full-screen ones on a 4 GiB machine driving a 4K
/// display — while still leaving three quarters for the session and every
/// other client. An eighth was tried first and refused the twenty-seventh
/// terminal window on that board, which is a bound tighter than the machine
/// rather than a defence.
const CLIENT_FRAME_RAM_DIVISOR: u64 = 4;

/// Frames of the session's own output one client may hold when the machine's
/// RAM total is unknown.
///
/// A window is clamped to the screen, so what a client can *show* is one
/// frame; the rest is stacked-under, minimised, and double-buffered windows.
/// Enough of those that no ordinary desktop meets the bound, and still a bound.
const CLIENT_FRAME_SCREENFULS: u64 = 32;

/// The bytes of window frame one attested client may hold mapped in the
/// session at once.
///
/// A **validation bound**, not a capacity, and deliberately measured in bytes
/// rather than windows: each window pins its shared frame region in the
/// session's address space, and a count says nothing about how much that is.
/// Thirty-two windows of a 4K frame is a gigabyte, while a hundred terminal
/// windows are a few tens of megabytes — a count generous enough for the
/// second is catastrophic for the first, and one tight enough for the first
/// refuses the second for no reason. Bounding the bytes bounds the resource
/// actually at stake, and the number of windows follows from how big the ones
/// a client opens really are. It also covers a resize, which a count cannot
/// see at all.
///
/// Both inputs are discovered, never hand-picked: `total_ram_bytes` is the
/// machine's usable physical RAM (the System Information API's total) and
/// `output_frame_bytes` is one frame of the session's own display mode. RAM is
/// the resource at stake so it decides wherever it is known; a total of zero
/// means the query went unanswered, and falling back to the display keeps the
/// desktop working under a bound instead of refusing every window.
#[must_use]
pub fn client_frame_budget_bytes(total_ram_bytes: u64, output_frame_bytes: usize) -> u64 {
    let from_ram = total_ram_bytes / CLIENT_FRAME_RAM_DIVISOR;
    if from_ram > 0 {
        return from_ram;
    }
    (output_frame_bytes as u64).saturating_mul(CLIENT_FRAME_SCREENFULS)
}

/// The caller-attestation seam — the kernel's `call_peer_origin` behind
/// a trait, so the engine is host-testable and never trusts a claimed
/// identity.
pub trait CallerIdentity {
    /// The kernel-attested [`ProcId`] of the in-flight caller identified
    /// by `ticket`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the attestation surfaces (a vanished caller, a
    /// transport fault); the engine refuses the request in that case.
    fn caller(&mut self, ticket: u64) -> Result<ProcId, Errno>;

    /// Whether the in-flight caller behind `ticket` holds `cap`, from the
    /// **kernel's** attestation of that caller rather than anything the
    /// caller said.
    ///
    /// The channel is bound unrestricted-sender, because a session is an
    /// ordinary user process and the kernel reserves a restricted-sender
    /// bind for holders of `CAP_IPC_BIND_PRIVILEGED` — a seat lease
    /// substitutes only for the reserved-id half of that gate. So the one
    /// gated operation on this channel is checked here instead, against the
    /// same unforgeable fact the kernel would have used.
    ///
    /// The default answers `false`: an identity source that cannot attest
    /// capabilities must not be taken to grant them, so a host without one
    /// refuses the privileged operations rather than opening them.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the attestation surfaces; the engine refuses the
    /// request in that case.
    fn caller_holds(&mut self, ticket: u64, cap: CapabilityId) -> Result<bool, Errno> {
        let _ = (ticket, cap);
        Ok(false)
    }

    /// The application the kernel attests the in-flight caller behind
    /// `ticket` is running, or `None` when it runs no verified bundle.
    ///
    /// The default answers `None`: an identity source that cannot attest an
    /// application must not be taken to name one, so a request reserved for
    /// one application is refused rather than opened.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the attestation surfaces; the engine refuses the
    /// request in that case.
    fn caller_app(&mut self, ticket: u64) -> Result<Option<AppIdentity>, Errno> {
        let _ = ticket;
        Ok(None)
    }
}

/// The session's compositor bridge: what the engine tells the desktop
/// about window lifecycle and presented pixels.
///
/// The engine has already validated everything it hands over — the
/// geometry, the ownership, the frame slice length
/// (`surface.stride_bytes * surface.height_px`), and the damage
/// rectangle's bounds — so an implementation only composites; it never
/// re-derives protocol policy.
pub trait WindowHost {
    /// A validated `Create` opened `window_id` with `surface` geometry,
    /// titled `title`, for the attested `owner`. `sizing` is what the app
    /// asks of the window manager's sizing: whether to present the window
    /// with a resize grabber and a live maximize/restore size toggle, and
    /// the smallest client size it may then be resized to. An error refuses
    /// the create: the engine unmaps the region and replies the refusal,
    /// keeping engine and host in lockstep.
    ///
    /// The host is the **enforcer** of a
    /// [`WindowSizing::Resizable`]'s declared minimum: it resizes no
    /// smaller than the larger of that and its own frame furniture's floor.
    /// The app states its minimum here once and lays out at whatever size
    /// it is given, so nothing resizes itself back.
    ///
    /// `owner` is the kernel-attested caller, not anything the client
    /// said — it is the only trustworthy answer to "which application is
    /// this window?", so a host that shows the owning application's
    /// identity (its icon) resolves it from this and never from `title`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot open a window for (e.g. the
    /// desktop is tearing down); the refusal is relayed to the client.
    fn window_opened(
        &mut self,
        owner: ProcId,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
        sizing: WindowSizing,
    ) -> Result<(), Errno>;

    /// A validated `CreatePopup` opened undecorated popup `window_id` of
    /// `surface` geometry, owned by and stacked directly above the
    /// caller's own window `parent_window_id`, at `offset_x`/`offset_y`
    /// physical pixels from that parent's client origin. The host resolves
    /// the parent's current screen position, adds the offset, clamps the
    /// whole popup onto the screen, opens it **undecorated** (no title
    /// bar, no frame furniture — the popup is a transient, not a taskbar
    /// entry), and records the parent→popup link so the popup stays glued
    /// above its parent and is torn down with it.
    ///
    /// An error refuses the popup: the engine unmaps the region and
    /// replies the refusal, keeping engine and host in lockstep exactly as
    /// [`window_opened`](Self::window_opened) does. The default is an
    /// infallible no-op: a host with no compositor to stack a popup on
    /// (a test double) accepts it without drawing anything.
    fn popup_opened(
        &mut self,
        window_id: u64,
        parent_window_id: u64,
        offset_x: i32,
        offset_y: i32,
        surface: &DisplayMode,
    ) -> Result<(), Errno> {
        let _ = (window_id, parent_window_id, offset_x, offset_y, surface);
        Ok(())
    }

    /// A validated, capability-gated `OpenLayer` opened desktop layer
    /// surface `window_id` of `surface` geometry for the attested `owner`,
    /// at screen point `x`/`y` in stacking layer `depth`.
    ///
    /// The engine has checked the caller's kernel-attested
    /// `CAP_DESKTOP_LAYER`, the geometry, and the per-client and per-seat
    /// surface counts. The host owns what is left, because only it knows
    /// the screen: clamping the surface onto the work area, re-checking the
    /// extent against the **live** UI scale (the wire bound is the ceiling
    /// at a scale of one), stacking it under the session's own chrome,
    /// keeping it out of the focus rotation and off the taskbar, and
    /// hit-testing it against its own content alpha.
    ///
    /// Unlike [`popup_opened`](Self::popup_opened) the default **refuses**:
    /// this is privileged authority, and a host that has not implemented it
    /// must not appear to grant it.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host refuses the surface with; the engine unmaps
    /// the region and relays the refusal, so engine and host stay in
    /// lockstep.
    fn layer_opened(
        &mut self,
        owner: ProcId,
        window_id: u64,
        surface: &DisplayMode,
        x: i32,
        y: i32,
        depth: LayerDepth,
    ) -> Result<(), Errno> {
        let _ = (owner, window_id, surface, x, y, depth);
        Err(Errno::NotSupported)
    }

    /// The engine refused a layer operation from `owner` at its capability
    /// gate, before any state was touched.
    ///
    /// Reported because the gate is enforced *here* rather than by the
    /// kernel — a session cannot bind a restricted-sender endpoint — so
    /// without this the most security-relevant refusal on the channel would
    /// be the one nothing recorded. The default is a no-op: a host with no
    /// audit log to write to is not obliged to invent one.
    fn layer_refused(&mut self, owner: ProcId, reason: Errno) {
        let _ = (owner, reason);
    }

    /// A validated `PlaceLayer` moved layer surface `window_id` to screen
    /// point `x`/`y` in stacking layer `depth`. Clamping is the host's, as
    /// at open.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host refuses the move with.
    fn layer_placed(
        &mut self,
        window_id: u64,
        x: i32,
        y: i32,
        depth: LayerDepth,
    ) -> Result<(), Errno> {
        let _ = (window_id, x, y, depth);
        Err(Errno::NotSupported)
    }

    /// Fill `out` with the visible windows' screen rectangles, back-to-front,
    /// as layer surface `window_id` sees them, and return how many were
    /// written.
    ///
    /// The asking surface is never its own terrain. A desktop with more
    /// visible windows than `out` holds is reported truncated to the
    /// frontmost, which are the ones a surface can actually meet.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host refuses the query with.
    fn layer_terrain(&mut self, window_id: u64, out: &mut [TerrainPlate]) -> Result<usize, Errno> {
        let _ = (window_id, out);
        Err(Errno::NotSupported)
    }

    /// A validated `Present`: `frame` is exactly one frame of
    /// `window_id`'s region shaped as `surface`, of which `damage`
    /// changed.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] compositing fails with; the refusal is relayed to
    /// the client.
    fn window_presented(
        &mut self,
        window_id: u64,
        surface: &DisplayMode,
        frame: &[u8],
        damage: DamageRect,
    ) -> Result<(), Errno>;

    /// A validated `Resize` re-mapped live `window_id` onto a fresh frame
    /// region of `surface` geometry, keeping the same window id, owner,
    /// event route, and taskbar entry. The host resizes the window's
    /// presented surface to match; the next `Present` shapes its frame
    /// against the new `surface`. An error refuses the resize: the engine
    /// keeps the previous mapping and replies the refusal, so engine and
    /// host never disagree about a window's geometry.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot resize a window for (a surface it
    /// cannot reallocate, the desktop is tearing down); the refusal is
    /// relayed to the client and the old geometry stands.
    fn window_resized(&mut self, window_id: u64, surface: &DisplayMode) -> Result<(), Errno>;

    /// A validated `SetTitle`: the attested owner of live `window_id`
    /// retitled it to `title`, already bounded and control-character-free
    /// by the engine's ABI decode. The host applies it to the window's
    /// chrome and its taskbar entry from this one call, so the two can
    /// never disagree.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot retitle for (it is tearing down, its
    /// compositor no longer holds the window); the refusal is relayed to
    /// the client and the previous title stands.
    fn window_retitled(&mut self, window_id: u64, title: &str) -> Result<(), Errno>;

    /// A validated `SetSizing`: the attested owner of live `window_id`
    /// restated the range a *user* may resize it within. The host adopts it
    /// as the range it enforces from here on, in place of the one the
    /// `Create` declared.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot adopt the range for — it is tearing
    /// down, its compositor no longer holds the window, or the sizing
    /// contradicts how the window was decorated (a resizable window handed
    /// a fixed sizing, or the reverse: what may change is the range, not
    /// whether the window has a grabber at all). The refusal is relayed to
    /// the client and the previous range stands.
    fn window_sizing_changed(&mut self, window_id: u64, sizing: WindowSizing) -> Result<(), Errno>;

    /// A validated `SetSizeState`: the attested owner of live `window_id`
    /// asked for it to be put into `state`. The host decides, and reports
    /// the state it actually applied — with the resulting client extent —
    /// as a `Resized` event.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot apply the state for — it is tearing
    /// down, its compositor no longer holds the window, or the window
    /// cannot take the state at all (a fixed-size window has no state but
    /// the one it was created at). The refusal is relayed to the client
    /// and the window stays where it is.
    fn window_size_state_changed(
        &mut self,
        window_id: u64,
        state: WindowSizeState,
    ) -> Result<(), Errno>;

    /// `window_id` is gone — closed by its owner or torn down after the
    /// owner exited. Infallible: the window is already unmapped and
    /// forgotten by the engine, and the host must not resurrect it.
    fn window_closed(&mut self, window_id: u64);

    /// A validated `PickFile`: the attested owner of live window
    /// `window_id` (which has no pick pending) asked for the session's
    /// trusted file picker. The host opens its picker UI and, when the
    /// user concludes, routes the outcome back through
    /// [`WindowServer::deliver_event`] as a `FilePicked` or
    /// `PickCancelled` — the engine tracks the pending pick and enforces
    /// that exactly one conclusion follows each acceptance.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot run a picker for (its one picker
    /// slot is taken by another window's pick, it holds no filesystem
    /// authority, the desktop is tearing down); the refusal is relayed
    /// to the client and no pick is recorded.
    fn pick_requested(&mut self, window_id: u64) -> Result<(), Errno>;

    /// A validated `HandOverLaunch`: attested `caller` asked the session to
    /// reach the live instance of the bundle whose entry binary is
    /// `run_path`, handing it `document` if one is named.
    ///
    /// The host owns the decision, because only it knows which bundles are
    /// running and what their manifests attest: it resolves the launch
    /// through its own funnel and answers [`HandOverOutcome::NotRunning`]
    /// when there was no instance to reach, which is what tells the caller to
    /// launch the bundle itself. `desk` is the one capability the *engine*
    /// owns and lends for the occasion — queueing a target and waking its
    /// owner — so the launch rule stays in one place rather than being
    /// re-derived here.
    ///
    /// `document`'s grant is one `caller` minted **to the session**, from a
    /// descriptor the caller opened under its own authority. The host redeems
    /// it only as `caller`'s own — bound to its attested instance, so a caller
    /// naming a delegation somebody else minted to the session gets nothing —
    /// and hands it on to the instance it resolved; it never opens a path on a
    /// caller's behalf, which would lend the session's own larger reach.
    ///
    /// The default refuses: a host with no launch table cannot say whether
    /// anything is running, and telling the caller so is more honest than
    /// answering "not running" for a bundle it never looked for.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host could not act on the request with — a grant it
    /// could not redeem, a re-grant the kernel refused. Nothing is delegated
    /// on a refusal.
    fn hand_over_requested(
        &mut self,
        desk: &mut dyn HandOverDesk,
        caller: ProcId,
        run_path: &str,
        document: Option<&HandOverDocument>,
    ) -> Result<HandOverOutcome, Errno> {
        let _ = (desk, caller, run_path, document);
        Err(Errno::NotSupported)
    }

    /// A validated `OpenMenu`: the attested owner of live window
    /// `window_id` (which has no open unanswered) asked for a menu chain,
    /// anchored at `anchor` in that window's own client pixels, over
    /// `menu`'s rows — already bounded and shape-checked by the engine's
    /// ABI decode, and carrying its root plate's title.
    ///
    /// The host places, draws, grabs and routes the chain, and when it
    /// closes routes the answer back through
    /// [`WindowServer::deliver_event`] as a `MenuClosed` naming `open_id`
    /// — the engine tracks the open and enforces that exactly one outcome
    /// follows each acceptance.
    ///
    /// Accepting must not depend on the requesting application: nothing
    /// about bringing a chain up may wait on a client.
    ///
    /// The default refuses: a host that composes no menu service cannot
    /// honour a chain, and telling the application so is more honest than
    /// accepting an open nothing will ever answer.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot open a chain for; the refusal is
    /// relayed to the client, which reports it and carries on, and no open
    /// is recorded.
    fn menu_open_requested(
        &mut self,
        window_id: u64,
        open_id: u64,
        anchor: WindowRegion,
        menu: &AppMenu,
    ) -> Result<(), Errno> {
        let _ = (window_id, open_id, anchor, menu);
        Err(Errno::NotSupported)
    }

    /// A validated `SetTooltip`: the attested owner of live window
    /// `window_id` declared that `region` of its own client pixels is
    /// explained by `text`, or — with `text` empty — withdrew whatever it
    /// had declared.
    ///
    /// Everything a tooltip *does* is the host's: the dwell before it
    /// appears, where the plate goes so it stays on screen, and every
    /// reason it comes down again. The engine only validates the window and
    /// the bounds and relays the declaration.
    ///
    /// The default refuses: a host with no seat to hover on cannot honour a
    /// tip, and saying so is more honest than accepting one nothing draws.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot hold a declaration for; the refusal is
    /// relayed to the client, which reports it and carries on.
    fn tooltip_declared(
        &mut self,
        window_id: u64,
        region: WindowRegion,
        text: &str,
    ) -> Result<(), Errno> {
        let _ = (window_id, region, text);
        Err(Errno::NotSupported)
    }

    /// A validated `SetAppBar`: the attested `owner` declared (or
    /// re-declared) its presence on the desktop's icon bar — its event
    /// route, whether it handles the primary click, and its menu, all
    /// already bounded and shape-checked by the engine's ABI decode.
    ///
    /// A re-declaration replaces the previous one whole; that is how an
    /// application changes a row's enablement or its mark.
    ///
    /// The default refuses: a host that composes no icon bar cannot honour
    /// a slot, and telling the application so is more honest than
    /// accepting a declaration nothing will ever draw.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot list the application under; the
    /// refusal is relayed to the client, which reports it and carries on.
    fn app_bar_declared(&mut self, owner: ProcId, bar: &AppBar) -> Result<(), Errno> {
        let _ = (owner, bar);
        Err(Errno::NotSupported)
    }

    /// The attested `owner` is gone; drop the icon-bar presence it
    /// declared, if any. Infallible: the engine has already forgotten the
    /// route, and the host must not resurrect the slot.
    ///
    /// Called from [`WindowServer::client_exited`] only for a client that
    /// actually held a declaration, so a host need not filter.
    fn app_bar_withdrawn(&mut self, owner: ProcId) {
        let _ = owner;
    }

    /// A validated `SetBackdropBlur`: the attested owner of live `window`
    /// set its backdrop-blur radius to `radius_px` logical pixels, already
    /// bounded by the engine's ABI decode to `WINDOW_BACKDROP_BLUR_MAX_PX`.
    /// Infallible: it only changes how the host recomposites the window's
    /// own rectangle, never another principal's state, so there is nothing
    /// for a host to refuse; the default does nothing.
    fn backdrop_blur_set(&mut self, window: u64, radius_px: u16) {
        let _ = (window, radius_px);
    }

    /// Describe the desktop this host composites: the screen extent, the
    /// UI scale, and the active appearance.
    ///
    /// The host is the authority for all three — it owns the compositor
    /// and the theme registry — so the answer is read straight from it
    /// rather than cached in the engine, and an application can never see
    /// a desktop the session has already left behind.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host has no desktop to describe (it is tearing
    /// down, or its screen extent is not one the record admits); the
    /// refusal is relayed to the client, which then knows it does not
    /// know rather than drawing to a guess.
    fn desktop(&mut self) -> Result<DesktopInfo, Errno>;

    /// The shipped wallpaper catalog this host offers, in catalog order.
    ///
    /// The host is the authority because it is the only party that may
    /// read the store; it lists that store once — `/System` is read-only,
    /// so the catalog is fixed for the life of the boot — and holds the
    /// result, so answering a query costs no I/O on the serve loop.
    ///
    /// The default is empty: a host that has listed no store has none to
    /// describe, which is the honest "this desktop offers no shipped
    /// pictures" rather than a refusal.
    fn wallpaper_catalog(&mut self) -> &[WallpaperName] {
        &[]
    }

    /// The cursor sets this host offers, in the order a chooser lists
    /// them.
    ///
    /// The host is the authority because it is the only party that may read
    /// the store; it lists that store once at bring-up — `/System` is
    /// read-only, so the choice space is fixed for the life of the boot —
    /// and holds the result, so answering a query costs no I/O on the serve
    /// loop.
    ///
    /// The default is empty: a host that has listed no store offers no sets
    /// of its own, which is the honest answer rather than a refusal.
    fn cursor_sets(&mut self) -> &[CursorSetName] {
        &[]
    }

    /// A validated `RenderWallpaper`: render catalog entry `index` as a
    /// `side`x`side` straight-alpha RGBA8 picture into the region granted
    /// as `shm_handle`, concluding to `window_id`.
    ///
    /// The engine has checked that the caller owns the window, that no
    /// render is already pending on it, and that the side is within the
    /// ABI bound. The host owns what is left, because only it holds the
    /// catalog and the parser sandbox: resolving the index against its own
    /// listing, mapping the region and checking it holds `side * side * 4`
    /// bytes, and doing the read and the decode **off** its compositing
    /// loop. It concludes by delivering exactly one
    /// [`WindowEvent::WallpaperRendered`] naming the same `index` and
    /// `side`.
    ///
    /// The default refuses: a host with no store to read cannot render a
    /// picture, and saying so is more honest than accepting a request that
    /// would never conclude.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host refuses the render with — a catalog position
    /// that does not exist ([`Errno::NotFound`]), a region too small or
    /// ungranted ([`Errno::LengthOutOfRange`], [`Errno::NotFound`]). A
    /// refusal leaves no render pending, so the caller may ask again.
    ///
    /// [`WindowEvent::WallpaperRendered`]: tairix_abi::window_ipc::WindowEvent::WallpaperRendered
    fn wallpaper_render_requested(
        &mut self,
        window_id: u64,
        shm_handle: u64,
        index: u16,
        side: u16,
    ) -> Result<(), Errno> {
        let _ = (window_id, shm_handle, index, side);
        Err(Errno::NotSupported)
    }

    /// The sources that have posted a notification since the desktop started,
    /// answered to `caller`, the application the kernel attests is asking.
    ///
    /// The host decides who may learn this; the default refuses, because a
    /// host that keeps no such record has nothing honest to answer.
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] for a caller the host does not answer, or
    /// [`Errno::NotSupported`] from a host that keeps no record.
    fn notify_sources(&mut self, caller: Option<&AppIdentity>) -> Result<&[BundleId], Errno> {
        let _ = caller;
        Err(Errno::NotSupported)
    }

    /// Lock the screen now, at the request of `caller`, the application the
    /// kernel attests is asking.
    ///
    /// The host decides who may ask; the default refuses, because a host with
    /// no lock cannot honour one.
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] for a caller the host does not honour, or
    /// the host's own refusal to lock.
    fn lock_screen(&mut self, caller: Option<&AppIdentity>) -> Result<(), Errno> {
        let _ = caller;
        Err(Errno::NotSupported)
    }
}

/// The event-delivery seam — the session's app-ward send (`ipc_send` to
/// the owning app's event endpoint in production) behind a trait, so
/// event routing is host-testable.
pub trait EventSink {
    /// Deliver one [`WindowEvent`] to `endpoint`, encoding it on the way.
    ///
    /// The sink takes the typed event rather than its wire form because
    /// only the sink knows whether the event goes out now: one that holds
    /// an event back against a full mailbox folds it into what it already
    /// holds by *kind*, and encodes once, when it finally goes.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the delivery surfaces (a full queue, a dead
    /// receiver). The sink must never block the session on a slow
    /// receiver: it either accepts responsibility for the event or
    /// refuses it.
    fn deliver(&mut self, endpoint: u64, event: &WindowEvent) -> Result<(), Errno>;
}

/// The four geometry fields a `Create`, `CreatePopup`, or `Resize` carries
/// on the wire, as the one surface value every spec holds.
const fn surface_of(
    width_px: u32,
    height_px: u32,
    stride_bytes: u32,
    format: DisplayFormat,
) -> DisplayMode {
    DisplayMode {
        width_px,
        height_px,
        stride_bytes,
        format,
    }
}

/// Everything one validated `Create` asks for, in one place, so the
/// engine's create path takes the request as a unit.
#[derive(Copy, Clone)]
struct CreateSpec {
    shm_handle: u64,
    event_endpoint: u64,
    frame_count: u32,
    surface: DisplayMode,
    title: WindowTitle,
    sizing: WindowSizing,
}

/// Everything one validated `Resize` asks for, in one place, so the
/// engine's resize path takes the request as a unit.
#[derive(Copy, Clone)]
struct ResizeSpec {
    window_id: u64,
    shm_handle: u64,
    frame_count: u32,
    surface: DisplayMode,
}

/// Everything one `CreatePopup` asks for, in one place: the app half
/// fills it in for [`WindowClient::create_popup`], the engine's popup
/// path receives the decoded request as the same unit, so both halves
/// describe a popup once.
///
/// [`WindowClient::create_popup`]: crate::client::WindowClient::create_popup
#[derive(Copy, Clone)]
pub struct PopupSpec {
    /// The caller's own top-level window the popup is anchored to and
    /// stacked above; a foreign or unknown parent is refused.
    pub parent_window_id: u64,
    /// The `shm_grant`ed region holding the popup's frames, mapped once.
    pub shm_handle: u64,
    /// The endpoint the popup's own events are delivered to.
    pub event_endpoint: u64,
    /// How many frames the region holds, back to back.
    pub frame_count: u32,
    /// The geometry of one frame, which must hold for every frame.
    pub surface: DisplayMode,
    /// Physical pixels right of the parent window's client origin.
    pub offset_x: i32,
    /// Physical pixels below the parent window's client origin.
    pub offset_y: i32,
}

/// Everything a validated [`WindowRequest::OpenLayer`] carries.
pub struct LayerSpec {
    /// The `shm_grant`ed region holding the surface's frames, mapped once.
    pub shm_handle: u64,
    /// The endpoint the surface's own events are delivered to.
    pub event_endpoint: u64,
    /// How many frames the region holds, back to back.
    pub frame_count: u32,
    /// The geometry of one frame, which must hold for every frame.
    pub surface: DisplayMode,
    /// Left edge in physical screen pixels, before the host clamps it.
    pub x: i32,
    /// Top edge in physical screen pixels, before the host clamps it.
    pub y: i32,
    /// Which desktop stacking layer the surface opens in.
    pub depth: LayerDepth,
}

/// The engine's own [`HandOverDesk`]: its queue and the sink the wake goes
/// out on, bound for the length of one hand-over.
struct EngineDesk<'a, M: ShmMapper> {
    server: &'a mut WindowServer<M>,
    sink: &'a mut dyn EventSink,
}

impl<M: ShmMapper> HandOverDesk for EngineDesk<'_, M> {
    fn hand_over(&mut self, app: ProcId, entry: OpenEntry) -> bool {
        self.server
            .hand_over_open_target(self.sink, app, entry)
            .is_ok()
    }

    fn ask_default(&mut self, app: ProcId) -> bool {
        self.server
            .deliver_app_event(self.sink, app, &WindowEvent::AppBarDefault)
            .is_ok()
    }

    fn recent_window(&self, app: ProcId) -> Option<u64> {
        self.server
            .windows
            .iter()
            .rev()
            .find(|(_, record)| record.owner == app)
            .map(|(&id, _)| id)
    }
}

/// The engine's half of reaching a live instance, lent to the host for the
/// length of one hand-over.
///
/// Whether to reach an instance is the host's decision — only it knows what
/// is running and what a manifest attests — while every *way* of reaching
/// one is a fact or an action the engine owns: the target queue and its
/// bound, the application's event route, and which window it most recently
/// opened. So the engine hands these over rather than either party
/// re-deriving the other's half.
pub trait HandOverDesk {
    /// Queue `entry` for `app` and wake it, answering whether it was taken.
    ///
    /// `false` is an unreachable instance: nothing is left queued, so the
    /// caller is free to read it as "start a fresh process instead".
    fn hand_over(&mut self, app: ProcId, entry: OpenEntry) -> bool;

    /// Ask `app` for its icon-bar default action. `false` when it declared no
    /// icon-bar presence, so it has no default to be asked for.
    fn ask_default(&mut self, app: ProcId) -> bool;

    /// The window `app` most recently opened, as its channel id, for a host
    /// that means to raise it. `None` when it owns none.
    fn recent_window(&self, app: ProcId) -> Option<u64>;
}

/// One target queued for an application to open — the session's owned form
/// of [`tairix_abi::window_ipc::OpenTarget`].
///
/// The wire type borrows from the frame it is encoded into, which a queue
/// cannot hold; this is what the session queues and the engine hands back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenEntry {
    /// A path the user named. Confers no access.
    Path(String),
    /// A document already opened, reachable through a one-shot delegation the
    /// session minted to the application this is queued for.
    Document {
        /// Its own file name, for a title. Empty when unknown.
        name: String,
        /// The `fd_redeem` handle. Never zero.
        grant: u64,
    },
    /// A place inside the application, resolved against its own closed set
    /// of places. Confers nothing.
    Pane(String),
}

/// One entry of the shipped wallpaper catalog, owned by the host that
/// listed the store — the session's owned form of
/// [`tairix_abi::window_ipc::WallpaperEntry`], which
/// borrows from the frame it is encoded into.
///
/// Two names rather than a path, because the *caller* builds the path from
/// them through the one shared spelling, so the catalog and the settings
/// document a chosen wallpaper produces cannot disagree about where it
/// lives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WallpaperName {
    /// The store category directory this wallpaper is filed under.
    pub category: String,
    /// The wallpaper's own file name inside that category.
    pub file: String,
}

/// One cursor set a host offers, named by the directory it occupies in the
/// shipped store — which is also the label a chooser draws.
///
/// A `String` rather than the typed `CursorSetId`: the engine relays the
/// name and judges only its length, so it needs no cursor vocabulary and
/// this crate takes on no dependency to carry one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorSetName(pub String);

impl OpenEntry {
    /// This entry as the wire type, borrowing its text.
    fn as_wire(&self) -> OpenTarget<'_> {
        match self {
            Self::Path(path) => OpenTarget::Path(path.as_bytes()),
            Self::Document { name, grant } => OpenTarget::Document {
                name: name.as_bytes(),
                grant: *grant,
            },
            Self::Pane(pane) => OpenTarget::Pane(pane.as_bytes()),
        }
    }
}

/// One live window: its attested owner, its event route, its
/// once-mapped frame region, and whether a trusted-picker request is
/// awaiting its conclusion.
struct WindowRecord<R> {
    owner: ProcId,
    event_endpoint: u64,
    surface: DisplayMode,
    frame_count: u32,
    frame_len: usize,
    /// The mapped client frame region, or `None` while released: the session
    /// unmaps a hidden window's frames under memory pressure so the physical
    /// pages can actually go, which needs both sides to let go. The geometry
    /// stays, so the window still lays out, hit-tests, and re-attaches.
    region: Option<R>,
    /// A `PickFile` was accepted and neither conclusion
    /// (`FilePicked`/`PickCancelled`) has been delivered yet. At most one
    /// pick is pending per window; the engine sets it on acceptance and
    /// clears it when the conclusion is delivered, so the protocol's
    /// one-conclusion-per-acceptance shape is enforced in one place.
    pick_pending: bool,
    /// A `RenderWallpaper` was accepted and its conclusion is still owed.
    /// One at a time, as a pick is, so a client cannot queue the session's
    /// sandbox full of decodes.
    render_pending: bool,
    /// The id of an accepted `OpenMenu` whose outcome has not been
    /// delivered yet, or `None`. At most one open is unanswered per window;
    /// the engine mints the id on acceptance and clears it when the
    /// outcome is delivered, so the protocol's one-outcome-per-acceptance
    /// shape is enforced in one place and an application can tell one
    /// gesture's answer from the next's.
    menu_open: Option<u64>,
    /// The text the user committed into a menu chain's quick-entry field,
    /// with the open it belongs to, until the owning application pulls it.
    ///
    /// One slot, not a queue: a window has at most one unanswered open, so it
    /// has at most one commit to hand back, and the next open on this window
    /// clears it — a commit nobody pulled can never answer a later gesture.
    /// It is held here rather than sent in the event because an event is one
    /// fixed 40-byte frame and a name is wider than that.
    menu_text: Option<CommittedText>,
    /// The top-level window this record is a **transient** of — the parent
    /// of a popup — or `None` for an ordinary top-level window. A transient
    /// is closed when the window it hangs from closes, so the link lives
    /// beside the window it binds.
    parent: Option<u64>,
    /// The desktop stacking layer this surface sits in, or `None` for an
    /// ordinary window.
    ///
    /// One field answers both "is this a layer surface?" and "which layer?",
    /// so the two can never disagree: an operation reserved for a layer
    /// surface refuses any window whose answer here is `None`.
    layer: Option<LayerDepth>,
}

/// One committed quick-entry text, with the open whose answer it belongs to.
struct CommittedText {
    open_id: u64,
    text: String,
}

impl<R> WindowRecord<R> {
    /// Bytes of client frame region this window holds mapped in the
    /// session: every frame of it, which is what the mapping validated.
    fn mapped_bytes(&self) -> u64 {
        if self.region.is_none() {
            return 0;
        }
        (self.frame_len as u64).saturating_mul(u64::from(self.frame_count))
    }
}

/// The window-channel engine: one instance serves one desktop session.
pub struct WindowServer<M: ShmMapper> {
    mapper: M,
    /// The serving session's own kernel-attested identity, stamped into
    /// every successful create reply so an app can authenticate the
    /// sender of each later event against it (the reply channel is the
    /// squat-protected rendezvous, so the stamp is trustworthy).
    server: ProcId,
    windows: BTreeMap<u64, WindowRecord<M::Region>>,
    /// The next window id to mint. Ids start at 1 and are never reused,
    /// so a stale id held by an app can never name a newer window.
    next_id: u64,
    /// Where each application that declared an icon-bar presence receives
    /// its bar events. The declaration's *content* is the host's — the
    /// engine keeps only the route, so an application-scoped event can be
    /// delivered without asking the host where it goes.
    app_bars: BTreeMap<ProcId, u64>,
    /// Targets queued for each application to open, oldest first.
    ///
    /// Keyed on the application rather than on one of its windows, because
    /// the instance a hand-over most needs to reach is the one with nothing
    /// open: a resident single-instance viewer sitting on the icon bar has no
    /// window to queue against. Bounded per application by
    /// [`WINDOW_MAX_OPEN_TARGETS`], and the whole queue dies with the client,
    /// so a target nobody drained is reachable by nothing and is dropped.
    open_targets: BTreeMap<ProcId, VecDeque<OpenEntry>>,
    /// The next menu-open id to mint. Its own sequence rather than the
    /// window ids': an open names a gesture, not a window, and the two are
    /// never interchangeable. Ids start at 1 and are never reused, so an
    /// outcome can only ever answer the open it names.
    next_menu_open: u64,
    /// Bytes of window frame one client may hold mapped here at once
    /// ([`client_frame_budget_bytes`]).
    client_frame_max: u64,
}

impl<M: ShmMapper> WindowServer<M> {
    /// An engine with no windows, mapping through `mapper`, replying as
    /// `server` (the session's own kernel-attested `ProcId`, e.g. from
    /// `self_origin`), bounding each client to `client_frame_max` bytes of
    /// mapped window frame ([`client_frame_budget_bytes`]).
    pub const fn new(mapper: M, server: ProcId, client_frame_max: u64) -> Self {
        Self {
            mapper,
            server,
            windows: BTreeMap::new(),
            next_id: 1,
            next_menu_open: 1,
            app_bars: BTreeMap::new(),
            open_targets: BTreeMap::new(),
            client_frame_max,
        }
    }

    /// Bytes of window frame `owner` currently holds mapped here, ignoring
    /// window `except` (the one a resize is replacing).
    ///
    /// A pass over the live windows rather than a running per-client tally:
    /// a create, a resize, and a close are each one mapping operation on a
    /// map of at most a few hundred entries, so the sum costs nothing
    /// measurable there, while a denormalised total is one missed update
    /// away from admitting whatever it has lost track of.
    fn client_frame_bytes(&self, owner: ProcId, except: Option<u64>) -> u64 {
        self.windows
            .iter()
            .filter(|(id, record)| record.owner == owner && Some(**id) != except)
            .map(|(_, record)| record.mapped_bytes())
            .fold(0u64, u64::saturating_add)
    }

    /// Whether `owner` may hold `bytes` more frame, with `except` excluded
    /// from what it already holds.
    fn client_frames_fit(&self, owner: ProcId, bytes: u64, except: Option<u64>) -> bool {
        self.client_frame_bytes(owner, except).saturating_add(bytes) <= self.client_frame_max
    }

    /// Number of live windows across every client.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// The attested owner of live window `window_id`, if any.
    ///
    /// The session uses this to turn a failed event delivery into the
    /// owner's [`client_exited`](Self::client_exited) teardown: the
    /// kernel reclaims a dead task's event port, so a delivery that
    /// finds no port is the kernel-backed fact that the owner is gone.
    #[must_use]
    pub fn owner_of(&self, window_id: u64) -> Option<ProcId> {
        self.windows.get(&window_id).map(|record| record.owner)
    }

    /// Handle one received request: decode `request`, attest the caller
    /// behind `ticket`, act through `host`, and write the encoded reply
    /// into `reply`, returning its length.
    ///
    /// Every outcome — including a malformed request — is a well-formed
    /// reply frame; the engine never leaves a caller without an answer.
    pub fn serve(
        &mut self,
        host: &mut dyn WindowHost,
        sink: &mut dyn EventSink,
        identity: &mut dyn CallerIdentity,
        ticket: u64,
        request: &[u8],
        reply: &mut [u8; WINDOW_REPLY_MAX],
    ) -> usize {
        let decoded = match WindowRequest::from_bytes(request) {
            Ok(decoded) => decoded,
            Err(err) => return status(reply, Err(err)),
        };
        let caller = match identity.caller(ticket) {
            Ok(caller) => caller,
            Err(err) => {
                return match decoded {
                    // A request that mints an id answers with the frame that
                    // carries it, so an attestation failure must too.
                    WindowRequest::Create { .. }
                    | WindowRequest::CreatePopup { .. }
                    | WindowRequest::OpenLayer { .. } => create_reply(reply, Err(err), self.server),
                    WindowRequest::OpenMenu { .. } => minted_id_reply(reply, Err(err)),
                    _ => status(reply, Err(err)),
                };
            }
        };
        // The one gated group on this channel, checked before dispatch
        // touches any state and re-checked on every layer operation rather
        // than only at open, so a revoked grant stops the surface at its
        // next request instead of lasting as long as the process does.
        if is_layer_op(&decoded) {
            let refusal = match identity.caller_holds(ticket, CapabilityId::DESKTOP_LAYER) {
                Ok(true) => None,
                Ok(false) => Some(Errno::PermissionDenied),
                Err(err) => Some(err),
            };
            if let Some(err) = refusal {
                host.layer_refused(caller, err);
                return layer_refusal(&decoded, reply, err, self.server);
            }
        }
        // Reserved for one application: the host decides against the
        // kernel-attested caller, a second attestation only these pay.
        match decoded {
            WindowRequest::QueryNotifySources => {
                let answered = identity
                    .caller_app(ticket)
                    .and_then(|app| host.notify_sources(app.as_ref()));
                return notify_sources_reply(reply, answered);
            }
            WindowRequest::LockScreen => {
                let locked = identity
                    .caller_app(ticket)
                    .and_then(|app| host.lock_screen(app.as_ref()));
                return status(reply, locked);
            }
            _ => {}
        }
        self.dispatch(host, sink, caller, &decoded, reply)
    }

    /// Act on one decoded request from the attested `caller`, writing the
    /// encoded outcome into `reply` and returning its length.
    fn dispatch(
        &mut self,
        host: &mut dyn WindowHost,
        sink: &mut dyn EventSink,
        caller: ProcId,
        decoded: &WindowRequest,
        reply: &mut [u8; WINDOW_REPLY_MAX],
    ) -> usize {
        match *decoded {
            WindowRequest::Create {
                shm_handle,
                event_endpoint,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
                title,
                sizing,
            } => {
                let spec = CreateSpec {
                    shm_handle,
                    event_endpoint,
                    frame_count,
                    surface: surface_of(width_px, height_px, stride_bytes, format),
                    title,
                    sizing,
                };
                create_reply(reply, self.create(host, caller, spec), self.server)
            }
            WindowRequest::CreatePopup {
                parent_window_id,
                shm_handle,
                event_endpoint,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
                offset_x,
                offset_y,
            } => {
                let spec = PopupSpec {
                    parent_window_id,
                    shm_handle,
                    event_endpoint,
                    frame_count,
                    surface: surface_of(width_px, height_px, stride_bytes, format),
                    offset_x,
                    offset_y,
                };
                create_reply(reply, self.create_popup(host, caller, spec), self.server)
            }
            WindowRequest::OpenLayer { .. } => {
                let opened = match layer_spec(decoded) {
                    Some(spec) => self.open_layer(host, caller, &spec),
                    None => Err(Errno::NotSupported),
                };
                create_reply(reply, opened, self.server)
            }
            WindowRequest::OpenMenu {
                window_id,
                anchor,
                ref menu,
            } => minted_id_reply(reply, self.open_menu(host, caller, window_id, anchor, menu)),
            WindowRequest::TakeOpenTarget => {
                let taken = self.take_open_target(caller);
                open_target_reply(reply, Ok(taken.as_ref().map(OpenEntry::as_wire)))
            }
            WindowRequest::QueryWallpapers { from } => wallpapers_reply(reply, host, from),
            WindowRequest::QueryCursorSets => cursor_sets_reply(reply, host),
            WindowRequest::TakeMenuText { window_id, open_id } => {
                let taken = self.take_menu_text(caller, window_id, open_id);
                menu_text_reply(
                    reply,
                    match taken {
                        Ok(ref held) => Ok(held.as_deref()),
                        Err(err) => Err(err),
                    },
                )
            }
            WindowRequest::HandOverLaunch {
                ref run_path,
                ref document,
            } => {
                let mut desk = EngineDesk { server: self, sink };
                let outcome = host.hand_over_requested(
                    &mut desk,
                    caller,
                    run_path.as_str(),
                    document.as_ref(),
                );
                hand_over_reply(reply, outcome)
            }
            ref other => self.dispatch_status_op(host, caller, other, reply),
        }
    }

    /// Act on one decoded request that answers with a plain status frame —
    /// everything but the two create requests, which mint a window id and so
    /// answer with the create-reply frame instead.
    fn dispatch_status_op(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        decoded: &WindowRequest,
        reply: &mut [u8; WINDOW_REPLY_MAX],
    ) -> usize {
        match *decoded {
            WindowRequest::Present {
                window_id,
                frame_index,
                damage,
            } => status(
                reply,
                self.present(host, caller, window_id, frame_index, damage),
            ),
            WindowRequest::Close { window_id } => {
                status(reply, self.close(host, caller, window_id))
            }
            WindowRequest::PickFile { window_id } => {
                status(reply, self.pick_file(host, caller, window_id))
            }
            WindowRequest::PlaceLayer {
                window_id,
                x,
                y,
                depth,
            } => status(
                reply,
                self.place_layer(host, caller, window_id, x, y, depth),
            ),
            WindowRequest::TakeTerrain { window_id } => {
                self.take_terrain(host, caller, window_id, reply)
            }
            WindowRequest::SetTooltip {
                window_id,
                region,
                text,
            } => status(
                reply,
                self.set_tooltip(host, caller, window_id, region, text.as_str()),
            ),
            WindowRequest::Resize {
                window_id,
                shm_handle,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
            } => {
                let spec = ResizeSpec {
                    window_id,
                    shm_handle,
                    frame_count,
                    surface: surface_of(width_px, height_px, stride_bytes, format),
                };
                status(reply, self.resize(host, caller, spec))
            }
            WindowRequest::SetTitle { window_id, title } => status(
                reply,
                self.set_title(host, caller, window_id, title.as_str()),
            ),
            WindowRequest::SetSizing { window_id, sizing } => {
                status(reply, self.set_sizing(host, caller, window_id, sizing))
            }
            WindowRequest::SetSizeState { window_id, state } => {
                status(reply, self.set_size_state(host, caller, window_id, state))
            }
            WindowRequest::SetAppBar(ref bar) => status(reply, self.set_app_bar(host, caller, bar)),
            WindowRequest::SetBackdropBlur {
                window_id,
                radius_px,
            } => status(
                reply,
                self.set_backdrop_blur(host, caller, window_id, radius_px),
            ),
            WindowRequest::RenderWallpaper {
                window_id,
                shm_handle,
                index,
                side,
            } => status(
                reply,
                self.render_wallpaper(host, caller, window_id, shm_handle, index, side),
            ),
            // Read-only and ungated: the reply describes the caller's own
            // seat, holding nothing another principal owns and granting
            // no authority, so every client on the desktop may ask.
            WindowRequest::QueryDesktop => desktop_reply(reply, host.desktop(), self.server),
            // A create mints a window id and is answered by the caller with
            // the create-reply frame; refusing it here keeps this total
            // without a second copy of that path, and refuses rather than
            // opening a window down a route that never validated one.
            WindowRequest::Create { .. }
            | WindowRequest::CreatePopup { .. }
            | WindowRequest::OpenLayer { .. } => {
                create_reply(reply, Err(Errno::NotSupported), self.server)
            }
            // Likewise a menu open, which mints an open id.
            WindowRequest::OpenMenu { .. } => minted_id_reply(reply, Err(Errno::NotSupported)),
            // ...and a target pull, which answers with its own frame.
            WindowRequest::TakeOpenTarget => open_target_reply(reply, Err(Errno::NotSupported)),
            // ...and a catalog page, likewise.
            WindowRequest::QueryWallpapers { .. } => wallpapers_refusal(reply, Errno::NotSupported),
            WindowRequest::QueryCursorSets => cursor_sets_refusal(reply, Errno::NotSupported),
            // ...and the two requests `serve` decides against the attested
            // application before dispatch is reached.
            WindowRequest::QueryNotifySources => {
                notify_sources_reply(reply, Err(Errno::NotSupported))
            }
            WindowRequest::LockScreen => status(reply, Err(Errno::NotSupported)),
            // ...and a committed-text pull, likewise.
            WindowRequest::TakeMenuText { .. } => menu_text_reply(reply, Err(Errno::NotSupported)),
            // ...and a hand-over, likewise.
            WindowRequest::HandOverLaunch { .. } => {
                hand_over_reply(reply, Err(Errno::NotSupported))
            }
        }
    }

    /// Every live window id, across every client, in ascending order.
    ///
    /// The session uses this to reach each window when it must tell every
    /// application something about the desktop they share — a light/dark
    /// switch, a scale or mode change — routing each one through its own
    /// delivery path so a client that has died is torn down exactly as it
    /// would be for any other event.
    #[must_use]
    pub fn window_ids(&self) -> Vec<u64> {
        self.windows.keys().copied().collect()
    }

    /// Open a window for `caller`: bound the client, map the granted
    /// region once, validate it holds every frame, tell the host, and
    /// mint the id.
    fn create(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        spec: CreateSpec,
    ) -> Result<u64, Errno> {
        // A window belongs to a user process instance; the kernel
        // sentinel is not a client and would alias every in-kernel
        // caller onto one "owner".
        if caller.is_kernel() {
            return Err(Errno::PermissionDenied);
        }
        let frame_len = frame_bytes(&spec.surface)?;
        let total = frame_len
            .checked_mul(spec.frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
        if !self.client_frames_fit(caller, total as u64, None) {
            return Err(Errno::NoSpace);
        }
        let region = self.mapper.map(spec.shm_handle, total)?;
        let window_id = self.next_id;
        // Minting never wraps in practice (2^64 creates); refuse rather
        // than reuse an id if it ever would.
        let next = window_id.checked_add(1).ok_or(Errno::NoSpace)?;
        // Tell the host before committing: a refused open leaves no
        // record and drops the mapping (the mapper's cue to unmap).
        host.window_opened(
            caller,
            window_id,
            &spec.surface,
            spec.title.as_str(),
            spec.sizing,
        )?;
        self.next_id = next;
        self.windows.insert(
            window_id,
            WindowRecord {
                owner: caller,
                event_endpoint: spec.event_endpoint,
                surface: spec.surface,
                frame_count: spec.frame_count,
                frame_len,
                region: Some(region),
                pick_pending: false,
                render_pending: false,
                menu_open: None,
                menu_text: None,
                parent: None,
                layer: None,
            },
        );
        Ok(window_id)
    }

    /// Open an undecorated popup for `caller`, stacked directly above the
    /// caller's own window `spec.parent_window_id`.
    ///
    /// A popup is validated exactly like a top-level [`Self::create`] —
    /// no kernel caller, the geometry holds every frame — and additionally
    /// requires that the parent window is one the caller owns (a foreign
    /// or unknown parent answers `NotFound`, leaking nothing). Its frames
    /// are charged against the **same** per-client budget as a top-level
    /// window's, so a popup can never be used to hold more than the client
    /// is allowed ([`client_frame_budget_bytes`]). The
    /// host is told before committing, so a refused popup leaves no record
    /// and drops the mapping (fail closed).
    fn create_popup(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        spec: PopupSpec,
    ) -> Result<u64, Errno> {
        if caller.is_kernel() {
            return Err(Errno::PermissionDenied);
        }
        // The parent must be a live window the caller owns; a foreign or
        // unknown parent is refused before anything is mapped.
        owned_window(&self.windows, caller, spec.parent_window_id)?;
        let frame_len = frame_bytes(&spec.surface)?;
        let total = frame_len
            .checked_mul(spec.frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
        // A popup's frames are charged against the same per-client budget as
        // a top-level window's, so "popup" cannot be used to hold more than
        // the client is allowed.
        if !self.client_frames_fit(caller, total as u64, None) {
            return Err(Errno::NoSpace);
        }
        let region = self.mapper.map(spec.shm_handle, total)?;
        let window_id = self.next_id;
        let next = window_id.checked_add(1).ok_or(Errno::NoSpace)?;
        // Tell the host before committing: a refused popup leaves no record
        // and drops the mapping (the mapper's cue to unmap).
        host.popup_opened(
            window_id,
            spec.parent_window_id,
            spec.offset_x,
            spec.offset_y,
            &spec.surface,
        )?;
        self.next_id = next;
        self.windows.insert(
            window_id,
            WindowRecord {
                owner: caller,
                event_endpoint: spec.event_endpoint,
                surface: spec.surface,
                frame_count: spec.frame_count,
                frame_len,
                region: Some(region),
                pick_pending: false,
                render_pending: false,
                menu_open: None,
                menu_text: None,
                parent: Some(spec.parent_window_id),
                layer: None,
            },
        );
        Ok(window_id)
    }

    /// Open a desktop layer surface for `caller`.
    ///
    /// The caller's `CAP_DESKTOP_LAYER` was checked before this was
    /// reached. What is left is containment: a layer surface is counted
    /// per client and per seat, so neither one holder nor a crowd of them
    /// can occupy more of the desktop than the bounds allow, and its frames
    /// are charged against the same per-client budget every window's are.
    /// The host is told before committing, so a refused surface leaves no
    /// record and drops the mapping.
    fn open_layer(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        spec: &LayerSpec,
    ) -> Result<u64, Errno> {
        if caller.is_kernel() {
            return Err(Errno::PermissionDenied);
        }
        if self.layer_count(Some(caller)) >= DESKTOP_LAYER_MAX_PER_CLIENT
            || self.layer_count(None) >= DESKTOP_LAYER_MAX_PER_SEAT
        {
            return Err(Errno::LimitExceeded);
        }
        let frame_len = frame_bytes(&spec.surface)?;
        let total = frame_len
            .checked_mul(spec.frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
        if !self.client_frames_fit(caller, total as u64, None) {
            return Err(Errno::NoSpace);
        }
        let region = self.mapper.map(spec.shm_handle, total)?;
        let window_id = self.next_id;
        let next = window_id.checked_add(1).ok_or(Errno::NoSpace)?;
        host.layer_opened(caller, window_id, &spec.surface, spec.x, spec.y, spec.depth)?;
        self.next_id = next;
        self.windows.insert(
            window_id,
            WindowRecord {
                owner: caller,
                event_endpoint: spec.event_endpoint,
                surface: spec.surface,
                frame_count: spec.frame_count,
                frame_len,
                region: Some(region),
                pick_pending: false,
                render_pending: false,
                menu_open: None,
                menu_text: None,
                parent: None,
                layer: Some(spec.depth),
            },
        );
        Ok(window_id)
    }

    /// How many live layer surfaces there are, for one `owner` or across
    /// every client when `owner` is `None`.
    fn layer_count(&self, owner: Option<ProcId>) -> usize {
        self.windows
            .values()
            .filter(|record| {
                record.layer.is_some() && owner.is_none_or(|caller| record.owner == caller)
            })
            .count()
    }

    /// Move `caller`'s own live layer surface to a new screen point and
    /// stacking layer.
    ///
    /// Refuses any window the caller does not own, and any window of the
    /// caller's that is not a layer surface — an ordinary window is placed
    /// by the window manager, and letting this reposition one would hand
    /// every `CAP_DESKTOP_LAYER` holder the authority to move its own
    /// windows around the screen, which is not what the capability is for.
    fn place_layer(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        x: i32,
        y: i32,
        depth: LayerDepth,
    ) -> Result<(), Errno> {
        let record = owned_window(&self.windows, caller, window_id)?;
        if record.layer.is_none() {
            return Err(Errno::NotSupported);
        }
        host.layer_placed(window_id, x, y, depth)?;
        // Recorded only once the host accepted, so the engine's depth is
        // always the one the compositor actually stacked.
        if let Some(record) = self.windows.get_mut(&window_id) {
            record.layer = Some(depth);
        }
        Ok(())
    }

    /// Answer `caller`'s terrain pull for its own live layer surface.
    fn take_terrain(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        reply: &mut [u8; WINDOW_REPLY_MAX],
    ) -> usize {
        let plates = match owned_window(&self.windows, caller, window_id) {
            Ok(record) if record.layer.is_none() => Err(Errno::NotSupported),
            Ok(_) => Ok(()),
            Err(err) => Err(err),
        };
        if let Err(err) = plates {
            return status(reply, Err(err));
        }
        let mut out = [TerrainPlate {
            x: 0,
            y: 0,
            width_px: 1,
            height_px: 1,
        }; DESKTOP_LAYER_MAX_PLATES as usize];
        let written = match host.layer_terrain(window_id, &mut out) {
            Ok(written) => written.min(out.len()),
            Err(err) => return status(reply, Err(err)),
        };
        match encode_terrain_reply(&out[..written], reply) {
            Ok(len) => len,
            Err(err) => status(reply, Err(err)),
        }
    }

    /// Hang an attached window from a panel row of `caller`'s live chain,
    /// returning the minted window id.
    ///
    /// The chain is the scope: the named window must be the caller's own and
    /// must hold `open_id` unanswered, so a surface for a chain that has
    /// already closed is unrepresentable rather than merely refused. One
    /// panel hangs per chain — a chain's deepest child is a plate or a
    /// panel, never both — and a second while one is live is refused. The
    /// host decides whether the pointer is still on the row and must accept
    /// before anything is recorded, so a late or refused panel leaves no
    /// window, no mapping, and no id spent.
    /// Re-map `caller`'s window `window_id` onto a fresh frame region of
    /// the new geometry, keeping its id, owner, event route, and pending
    /// pick state.
    ///
    /// The new region is mapped and validated to hold every frame before
    /// anything is committed; the host is told before the swap, so a
    /// refused resize leaves the old mapping intact (fail closed). On
    /// success the old region is dropped (the mapper's cue to unmap) as
    /// the record adopts the new one.
    fn resize(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        spec: ResizeSpec,
    ) -> Result<(), Errno> {
        // Ownership first: a window the caller does not own answers
        // exactly like one that never existed, leaking nothing.
        owned_window(&self.windows, caller, spec.window_id)?;
        let frame_len = frame_bytes(&spec.surface)?;
        let total = frame_len
            .checked_mul(spec.frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
        // The new geometry replaces this window's own frames, so it is the
        // one excluded from what the client already holds. A resize is how a
        // client grows its mapped bytes without opening a window, which is
        // exactly what a per-client *count* could never bound.
        if !self.client_frames_fit(caller, total as u64, Some(spec.window_id)) {
            return Err(Errno::NoSpace);
        }
        let region = self.mapper.map(spec.shm_handle, total)?;
        // Tell the host before committing: a refused resize drops the
        // freshly mapped region and leaves the record's old geometry.
        host.window_resized(spec.window_id, &spec.surface)?;
        // `owned_window` proved the record exists and the borrow above is
        // released, so this re-lookup cannot miss.
        if let Some(record) = self.windows.get_mut(&spec.window_id) {
            record.surface = spec.surface;
            record.frame_count = spec.frame_count;
            record.frame_len = frame_len;
            record.region = Some(region);
        }
        Ok(())
    }

    /// Present one frame of `caller`'s window `window_id`.
    fn present(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        frame_index: u32,
        damage: DamageRect,
    ) -> Result<(), Errno> {
        let record = owned_window(&self.windows, caller, window_id)?;
        if frame_index >= record.frame_count {
            return Err(Errno::OutOfRange);
        }
        damage
            .validate_in(&record.surface)
            .map_err(|_| Errno::LengthOutOfRange)?;
        // A window whose frames the session released holds no pixels to
        // present. Refusing typed is what lets a client re-attach on its next
        // paint instead of writing into a mapping neither side has.
        let bytes = record.region.as_ref().ok_or(Errno::NotAttached)?.bytes();
        let base = frame_index as usize * record.frame_len;
        // The region was validated to hold every frame at create time; a
        // region that shrank underneath us is a fault, not a clamp.
        let frame = bytes
            .get(base..base + record.frame_len)
            .ok_or(Errno::DeviceFault)?;
        host.window_presented(window_id, &record.surface, frame, damage)
    }

    /// Accept a trusted-picker request for `caller`'s window `window_id`:
    /// at most one pick pends per window, and the host must be able to
    /// run its picker before anything is recorded (fail closed — a
    /// refused request leaves no pending state).
    fn pick_file(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
    ) -> Result<(), Errno> {
        // Owned-window check first: a window the caller does not own
        // answers exactly like one that never existed.
        let record = self
            .windows
            .get_mut(&window_id)
            .filter(|record| record.owner == caller)
            .ok_or(Errno::NotFound)?;
        if record.pick_pending {
            return Err(Errno::AlreadyExists);
        }
        // Tell the host before committing: a refused picker (slot taken,
        // no filesystem authority) leaves no pending pick behind.
        host.pick_requested(window_id)?;
        record.pick_pending = true;
        Ok(())
    }

    /// Accept a wallpaper-render request for `caller`'s window
    /// `window_id`: at most one render pends per window, and the host must
    /// accept it before anything is recorded (fail closed — a refused
    /// request leaves no pending state, so the caller may ask again).
    fn render_wallpaper(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        shm_handle: u64,
        index: u16,
        side: u16,
    ) -> Result<(), Errno> {
        let record = self
            .windows
            .get_mut(&window_id)
            .filter(|record| record.owner == caller)
            .ok_or(Errno::NotFound)?;
        if record.render_pending {
            return Err(Errno::AlreadyExists);
        }
        host.wallpaper_render_requested(window_id, shm_handle, index, side)?;
        record.render_pending = true;
        Ok(())
    }

    /// Relay `caller`'s tooltip declaration for its own window `window_id`.
    ///
    /// The window and the bounds are the engine's to check; the declaration
    /// itself is the host's to hold and act on. A window the caller does not
    /// own answers exactly like one that never existed, so a client learns
    /// nothing about another application's windows.
    fn set_tooltip(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        region: WindowRegion,
        text: &str,
    ) -> Result<(), Errno> {
        if !self.owns(caller, window_id) {
            return Err(Errno::NotFound);
        }
        host.tooltip_declared(window_id, region, text)
    }

    /// Pop the oldest target queued for `caller`, or `None` once the queue is
    /// drained.
    ///
    /// Popping is what makes a target one-shot: there is no id to mint or
    /// validate, and the ordering is the protocol. An application with no
    /// queue at all — nothing was ever handed to it — answers like one whose
    /// queue is drained, so a pull leaks nothing about other clients.
    fn take_open_target(&mut self, caller: ProcId) -> Option<OpenEntry> {
        let queue = self.open_targets.get_mut(&caller)?;
        let entry = queue.pop_front();
        if queue.is_empty() {
            self.open_targets.remove(&caller);
        }
        entry
    }

    /// Where an application-scoped event reaches `app`: its declared
    /// icon-bar route, else the event endpoint of its most recent window.
    ///
    /// The bar route is preferred because an application that declared one
    /// receives every application-scoped event there, and it is the only
    /// route an instance with no window has. Falling back to a window keeps
    /// an application that opted out of the icon bar reachable — it is a
    /// window-owning client like any other, and a hand-over is not a bar
    /// gesture.
    fn app_event_endpoint(&self, app: ProcId) -> Option<u64> {
        if let Some(&endpoint) = self.app_bars.get(&app) {
            return Some(endpoint);
        }
        self.windows
            .iter()
            .rev()
            .find(|(_, record)| record.owner == app)
            .map(|(_, record)| record.event_endpoint)
    }

    /// Hand `entry` to application `app` to open: queue it, then wake the
    /// application with a [`WindowEvent::OpenRequested`].
    ///
    /// The session's side of the channel, and **one** operation because the
    /// two halves are one invariant: a queued target the application was
    /// never woken for would sit unreachable, so a refused wake takes the
    /// target back off the queue rather than leaving it stranded. The caller
    /// may therefore read the answer as "the instance has it", and fall back
    /// to starting a fresh process when it does not.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — `app` has no route an event can reach it by,
    ///   so there is no live instance to hand anything to.
    /// * [`Errno::LengthOutOfRange`] — an empty path, or one longer than the
    ///   filesystem admits.
    /// * [`Errno::OutOfRange`] — a document naming no delegation.
    /// * [`Errno::NoSpace`] — the application already holds
    ///   [`WINDOW_MAX_OPEN_TARGETS`] targets. The newest is refused with the
    ///   refusal stated rather than an older one dropped silently, or the
    ///   queue grown without bound.
    /// * Whatever the wake's delivery refused with.
    pub fn hand_over_open_target(
        &mut self,
        sink: &mut dyn EventSink,
        app: ProcId,
        entry: OpenEntry,
    ) -> Result<(), Errno> {
        match &entry {
            OpenEntry::Path(path) => {
                if path.is_empty() || path.len() > tairix_abi::FS_PATH_MAX {
                    return Err(Errno::LengthOutOfRange);
                }
            }
            OpenEntry::Document { grant, .. } => {
                if *grant == 0 {
                    return Err(Errno::OutOfRange);
                }
            }
            OpenEntry::Pane(pane) => {
                if pane.is_empty() || pane.len() > tairix_abi::window_ipc::WINDOW_PANE_NAME_MAX {
                    return Err(Errno::LengthOutOfRange);
                }
            }
        }
        let endpoint = self.app_event_endpoint(app).ok_or(Errno::NotFound)?;
        let queue = self.open_targets.entry(app).or_default();
        if queue.len() >= WINDOW_MAX_OPEN_TARGETS {
            self.prune_empty_queue(app);
            return Err(Errno::NoSpace);
        }
        // A delegation handle is one-shot, and the kernel hands the *same*
        // handle back when the same authority is granted to the same process
        // twice. Queueing it twice would therefore promise a second document
        // the first pull consumes, so an entry already waiting is the answer.
        if let OpenEntry::Document { grant, .. } = entry {
            if queue.iter().any(
                |held| matches!(held, OpenEntry::Document { grant: held, .. } if *held == grant),
            ) {
                return Ok(());
            }
        }
        queue.push_back(entry);
        if let Err(err) = sink.deliver(endpoint, &WindowEvent::OpenRequested) {
            // Nothing was announced, so nothing may be left queued.
            if let Some(queue) = self.open_targets.get_mut(&app) {
                queue.pop_back();
            }
            self.prune_empty_queue(app);
            return Err(err);
        }
        Ok(())
    }

    /// Drop `app`'s queue entry once it holds nothing, so a refused hand-over
    /// leaves no record behind for an application that has none.
    fn prune_empty_queue(&mut self, app: ProcId) {
        if self
            .open_targets
            .get(&app)
            .is_some_and(alloc::collections::VecDeque::is_empty)
        {
            self.open_targets.remove(&app);
        }
    }

    /// Whether `caller` is the attested owner of live window `window_id`.
    fn owns(&self, caller: ProcId, window_id: u64) -> bool {
        self.windows
            .get(&window_id)
            .is_some_and(|record| record.owner == caller)
    }

    /// Accept a menu open for `caller`'s window `window_id`, returning the
    /// minted open id its one outcome will name.
    ///
    /// At most one open is unanswered per window: while one is, a second is
    /// refused, which a well-behaved application cannot reach — its chain
    /// holds the seat's grab, so the press that would open another is
    /// consumed there. The host must accept before anything is recorded, so
    /// a refused open leaves no state and no spent id.
    fn open_menu(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        anchor: WindowRegion,
        menu: &AppMenu,
    ) -> Result<u64, Errno> {
        let open_id = self.next_menu_open;
        // Minting never wraps in practice; refuse rather than reuse an id
        // that an outcome could then answer twice.
        let next = open_id.checked_add(1).ok_or(Errno::NoSpace)?;
        // Owned-window check first: a window the caller does not own
        // answers exactly like one that never existed.
        let record = self
            .windows
            .get_mut(&window_id)
            .filter(|record| record.owner == caller)
            .ok_or(Errno::NotFound)?;
        if record.menu_open.is_some() {
            return Err(Errno::AlreadyExists);
        }
        host.menu_open_requested(window_id, open_id, anchor, menu)?;
        record.menu_open = Some(open_id);
        // A commit the application never pulled belongs to the gesture that
        // is now over, so it goes with the open that replaces it.
        record.menu_text = None;
        self.next_menu_open = next;
        Ok(open_id)
    }

    /// Record the text the user committed into the quick-entry field of the
    /// chain opened as `open_id` on window `window_id`, for its application
    /// to pull.
    ///
    /// Called by the embedder as it settles the chain, *before* it delivers
    /// the `Entered` outcome, so the answer and the text it refers to are
    /// never out of order. The open must be the one that window is still
    /// waiting on: a text recorded against any other names a gesture whose
    /// answer has already gone, and is refused rather than held for a puller
    /// to find.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no such live window.
    /// * [`Errno::OutOfRange`] — `open_id` is not the window's unanswered
    ///   open.
    /// * [`Errno::LengthOutOfRange`] — longer than a quick-entry field can
    ///   hold.
    pub fn record_menu_text(
        &mut self,
        window_id: u64,
        open_id: u64,
        text: &str,
    ) -> Result<(), Errno> {
        if text.len() > APP_MENU_ENTRY_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let record = self.windows.get_mut(&window_id).ok_or(Errno::NotFound)?;
        if record.menu_open != Some(open_id) {
            return Err(Errno::OutOfRange);
        }
        record.menu_text = Some(CommittedText {
            open_id,
            text: String::from(text),
        });
        Ok(())
    }

    /// Take the text committed for `caller`'s window `window_id` under
    /// `open_id`, if that is the one it holds.
    ///
    /// Taken once: the slot is emptied, so a second pull answers nothing and
    /// two readers cannot both act on one commit. A window the caller does
    /// not own answers exactly like one that never existed.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] — no such live window owned by `caller`.
    fn take_menu_text(
        &mut self,
        caller: ProcId,
        window_id: u64,
        open_id: u64,
    ) -> Result<Option<String>, Errno> {
        let record = self
            .windows
            .get_mut(&window_id)
            .filter(|record| record.owner == caller)
            .ok_or(Errno::NotFound)?;
        if record.menu_text.as_ref().map(|held| held.open_id) != Some(open_id) {
            return Ok(None);
        }
        Ok(record.menu_text.take().map(|held| held.text))
    }

    /// Retitle `caller`'s window `window_id` to `title`.
    ///
    /// Ownership is checked before the host is told anything, so a
    /// retitle aimed at another client's window (or issued by a kernel
    /// caller, which owns none) answers `NotFound` and changes nothing.
    /// A host refusal leaves the previous title standing.
    fn set_title(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        title: &str,
    ) -> Result<(), Errno> {
        owned_window(&self.windows, caller, window_id)?;
        host.window_retitled(window_id, title)
    }

    /// Restate `caller`'s window `window_id`'s resize range through the
    /// host.
    ///
    /// Ownership is checked before the host is told anything, exactly as a
    /// retitle is, so a range aimed at another client's window answers
    /// `NotFound` and changes nothing. A host refusal leaves the previous
    /// range standing.
    fn set_sizing(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        sizing: WindowSizing,
    ) -> Result<(), Errno> {
        owned_window(&self.windows, caller, window_id)?;
        host.window_sizing_changed(window_id, sizing)
    }

    /// Ask the host to put `caller`'s window `window_id` into `state`.
    ///
    /// Ownership is checked before the host is told anything, exactly as a
    /// retitle is, so a state aimed at another client's window answers
    /// `NotFound` and changes nothing.
    fn set_size_state(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        state: WindowSizeState,
    ) -> Result<(), Errno> {
        owned_window(&self.windows, caller, window_id)?;
        host.window_size_state_changed(window_id, state)
    }

    /// Record `caller`'s icon-bar declaration and hand it to the host.
    ///
    /// The route is remembered only once the host accepted the
    /// declaration, so a refused one leaves nothing behind to deliver a
    /// bar event to (fail closed), and a re-declaration replaces the
    /// previous route rather than accumulating one.
    fn set_app_bar(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        bar: &AppBar,
    ) -> Result<(), Errno> {
        host.app_bar_declared(caller, bar)?;
        self.app_bars.insert(caller, bar.event_endpoint);
        Ok(())
    }

    /// Set `caller`'s window `window_id`'s backdrop-blur radius. Always
    /// succeeds for an owned window: the radius was already bounded by the
    /// engine's ABI decode, so there is nothing left to fail once
    /// ownership holds.
    fn set_backdrop_blur(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
        radius_px: u16,
    ) -> Result<(), Errno> {
        owned_window(&self.windows, caller, window_id)?;
        host.backdrop_blur_set(window_id, radius_px);
        Ok(())
    }

    /// Close `caller`'s window `window_id`, dropping its region mapping.
    ///
    /// Closing a top-level window also closes every popup anchored to it,
    /// so a menu or sheet can never outlive the window it belongs to.
    /// Closing a popup's own id tears down only that popup — a popup has
    /// no popups of its own.
    fn close(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
    ) -> Result<(), Errno> {
        owned_window(&self.windows, caller, window_id)?;
        self.remove_with_transients(host, window_id);
        Ok(())
    }

    /// Remove `window_id` and every transient keyed to it — its popups and
    /// any attached menu panel — telling the host of each teardown. The
    /// window is dropped first, then its transients.
    fn remove_with_transients(&mut self, host: &mut dyn WindowHost, window_id: u64) {
        let transients: alloc::vec::Vec<u64> = self
            .windows
            .iter()
            .filter(|(_, record)| record.parent == Some(window_id))
            .map(|(&id, _)| id)
            .collect();
        self.windows.remove(&window_id);
        host.window_closed(window_id);
        for transient in transients {
            self.windows.remove(&transient);
            host.window_closed(transient);
        }
    }

    /// Unmap window `window_id`'s frame region, keeping the window itself:
    /// the bytes given back, or zero if it holds none (or the id is unknown).
    ///
    /// The session calls this for a window whose content it released while
    /// nobody could see it, and tells the client the same
    /// ([`WindowEvent::ContentReleased`]). Both sides have to let go for the
    /// physical pages to be freed at all — a mapping either side keeps holds
    /// every page of them — so releasing here without telling the client
    /// frees only address space, and telling the client without releasing
    /// here frees nothing.
    ///
    /// Everything else about the window survives: its geometry, owner, event
    /// route, title, and place in the stack. The client re-attaches a fresh
    /// region with an ordinary [`WindowRequest::Resize`], which is what its
    /// next paint does; until then a present is refused
    /// ([`Errno::NotAttached`]) rather than reading a mapping nobody has.
    pub fn release_frames(&mut self, window_id: u64) -> u64 {
        let Some(record) = self.windows.get_mut(&window_id) else {
            return 0;
        };
        let bytes = record.mapped_bytes();
        // Dropping the region is the unmap: the mapper owns that side.
        record.region = None;
        bytes
    }

    /// Tear down every window `client` owns: the session calls this when
    /// the owning app exits, so no mapping or taskbar entry outlives its
    /// process.
    pub fn client_exited(&mut self, host: &mut dyn WindowHost, client: ProcId) {
        let closed: alloc::vec::Vec<u64> = self
            .windows
            .iter()
            .filter(|(_, record)| record.owner == client)
            .map(|(&id, _)| id)
            .collect();
        for id in closed {
            self.windows.remove(&id);
            host.window_closed(id);
        }
        if self.app_bars.remove(&client).is_some() {
            host.app_bar_withdrawn(client);
        }
        // A target queued for a client that has gone is reachable by
        // nothing; its delegation dies with the process it was minted to.
        self.open_targets.remove(&client);
    }

    /// Route one application-scoped event — an icon-bar click or menu
    /// outcome — to the application that declared the bar presence it
    /// belongs to.
    ///
    /// The destination comes from the declaration the engine recorded, not
    /// from the event, so the session cannot address a bar event to a
    /// process that never asked to be on the bar.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `event` is window-scoped; those go
    ///   through [`Self::deliver_event`].
    /// * [`Errno::NotFound`] — `app` declared no icon-bar presence (it
    ///   never did, or it has exited); the session drops the event.
    /// * Any [`Errno`] the sink surfaces.
    pub fn deliver_app_event(
        &mut self,
        sink: &mut dyn EventSink,
        app: ProcId,
        event: &WindowEvent,
    ) -> Result<(), Errno> {
        if event.window_id().is_some() {
            return Err(Errno::OutOfRange);
        }
        let endpoint = *self.app_bars.get(&app).ok_or(Errno::NotFound)?;
        sink.deliver(endpoint, event)
    }

    /// Route one event to the owning app of the window it addresses:
    /// validate it against the live window, encode it, and hand it to
    /// `sink` for the window's event endpoint.
    ///
    /// A `FilePicked`/`PickCancelled` conclusion additionally requires a
    /// pending pick on the window and clears it once the sink accepted
    /// the delivery, so exactly one conclusion follows each accepted
    /// `PickFile` — the engine enforces the protocol shape in one place
    /// and a session bug (a conclusion no one asked for) is refused, not
    /// delivered.
    ///
    /// A `MenuClosed` outcome must name the window's *own* unanswered open:
    /// an open that was never accepted, one already answered, or another
    /// window's, is refused. The outcome concludes it, which is what makes
    /// exactly-once a property rather than a convention: a second outcome
    /// for one open cannot be delivered, and one gesture's dismissal can
    /// never arrive as another's answer.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no such window (it was closed, or never
    ///   existed); the session drops the event.
    /// * [`Errno::OutOfRange`] — a pointer event outside the window's
    ///   surface, a pick conclusion with no pick pending, an event naming
    ///   anything but the window's unanswered open (both routing bugs,
    ///   refused rather than delivered), or an application-scoped event
    ///   (those go through [`Self::deliver_app_event`]).
    /// * Any [`Errno`] the sink surfaces; a refused delivery leaves a
    ///   pending pick or open still owed (the session decides whether to
    ///   retry or tear the client down). A sink that *accepts* it is
    ///   answering for it, whether it goes out now or from a hold-back.
    pub fn deliver_event(
        &mut self,
        sink: &mut dyn EventSink,
        event: &WindowEvent,
    ) -> Result<(), Errno> {
        let window_id = event.window_id().ok_or(Errno::OutOfRange)?;
        let record = self.windows.get(&window_id).ok_or(Errno::NotFound)?;
        if let WindowEvent::Pointer { x, y, .. } = *event {
            if x >= record.surface.width_px || y >= record.surface.height_px {
                return Err(Errno::OutOfRange);
            }
        }
        let concludes_pick = matches!(
            event,
            WindowEvent::FilePicked { .. } | WindowEvent::PickCancelled { .. }
        );
        if concludes_pick && !record.pick_pending {
            return Err(Errno::OutOfRange);
        }
        let concludes_render = matches!(event, WindowEvent::WallpaperRendered { .. });
        if concludes_render && !record.render_pending {
            return Err(Errno::OutOfRange);
        }
        let names_open = match *event {
            WindowEvent::MenuClosed { open_id, .. } => Some(open_id),
            _ => None,
        };
        if names_open.is_some() && names_open != record.menu_open {
            return Err(Errno::OutOfRange);
        }
        let concludes_open = matches!(*event, WindowEvent::MenuClosed { .. });
        let endpoint = record.event_endpoint;
        sink.deliver(endpoint, event)?;
        if let Some(record) = self.windows.get_mut(&window_id) {
            if concludes_pick {
                record.pick_pending = false;
            }
            if concludes_render {
                record.render_pending = false;
            }
            if concludes_open {
                record.menu_open = None;
            }
        }
        Ok(())
    }
}

/// Look up `window_id` **as owned by** `caller`. A window owned by
/// someone else answers exactly like a window that does not exist, so
/// the reply leaks nothing about other clients.
/// The [`LayerSpec`] a decoded [`WindowRequest::OpenLayer`] describes, or
/// `None` for any other request.
fn layer_spec(request: &WindowRequest) -> Option<LayerSpec> {
    let WindowRequest::OpenLayer {
        shm_handle,
        event_endpoint,
        frame_count,
        width_px,
        height_px,
        stride_bytes,
        format,
        x,
        y,
        depth,
    } = *request
    else {
        return None;
    };
    Some(LayerSpec {
        shm_handle,
        event_endpoint,
        frame_count,
        surface: surface_of(width_px, height_px, stride_bytes, format),
        x,
        y,
        depth,
    })
}

/// Whether `request` is one of the operations `CAP_DESKTOP_LAYER` gates.
///
/// Named once, so the gate in `serve` and the refusal frame it writes can
/// never disagree about which operations are privileged.
fn is_layer_op(request: &WindowRequest) -> bool {
    matches!(
        request,
        WindowRequest::OpenLayer { .. }
            | WindowRequest::PlaceLayer { .. }
            | WindowRequest::TakeTerrain { .. }
    )
}

/// Write the refusal frame `request`'s own reply shape requires.
///
/// An open mints an id and so answers with the create frame; the other two
/// answer with a plain status word, which a terrain decode reads as the
/// refusal it is rather than as an empty page.
fn layer_refusal(
    request: &WindowRequest,
    reply: &mut [u8; WINDOW_REPLY_MAX],
    err: Errno,
    server: ProcId,
) -> usize {
    match request {
        WindowRequest::OpenLayer { .. } => create_reply(reply, Err(err), server),
        _ => status(reply, Err(err)),
    }
}

fn owned_window<R>(
    windows: &BTreeMap<u64, WindowRecord<R>>,
    caller: ProcId,
    window_id: u64,
) -> Result<&WindowRecord<R>, Errno> {
    windows
        .get(&window_id)
        .filter(|record| record.owner == caller)
        .ok_or(Errno::NotFound)
}

/// Bytes one frame of `surface` occupies (`stride_bytes * height_px`),
/// refused if it overflows the host's address width.
fn frame_bytes(surface: &DisplayMode) -> Result<usize, Errno> {
    u64::from(surface.stride_bytes)
        .checked_mul(u64::from(surface.height_px))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(Errno::LengthOutOfRange)
}

/// Write a status-only reply into `reply`, returning its length.
fn status(reply: &mut [u8; WINDOW_REPLY_MAX], result: Result<(), Errno>) -> usize {
    reply[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(result));
    STATUS_REPLY_LEN
}

/// Write a minted-id reply into `reply`, returning its length.
/// Write a `HandOverLaunch` outcome into `reply`, answering its length.
fn hand_over_reply(
    reply: &mut [u8; WINDOW_REPLY_MAX],
    result: Result<HandOverOutcome, Errno>,
) -> usize {
    let frame = encode_hand_over_reply(result);
    reply[..frame.len()].copy_from_slice(&frame);
    frame.len()
}

/// Write a `TakeMenuText` outcome into `reply`, answering its length.
///
/// The frame is only as long as the answer: "nothing held" costs its header
/// rather than the widest name.
fn menu_text_reply(
    reply: &mut [u8; WINDOW_REPLY_MAX],
    result: Result<Option<&str>, Errno>,
) -> usize {
    let mut frame = [0u8; WINDOW_MENU_TEXT_REPLY_MAX];
    let len = encode_menu_text_reply(&mut frame, result);
    reply[..len].copy_from_slice(&frame[..len]);
    len
}

/// Write a `TakeOpenTarget` outcome into `reply`, answering its length.
///
/// The frame is only as long as the answer: the drained queue costs its
/// header, not the widest path.
fn open_target_reply(
    reply: &mut [u8; WINDOW_REPLY_MAX],
    result: Result<Option<OpenTarget<'_>>, Errno>,
) -> usize {
    let mut frame = [0u8; WINDOW_OPEN_TARGET_REPLY_MAX];
    let len = encode_open_target_reply(&mut frame, result);
    reply[..len].copy_from_slice(&frame[..len]);
    len
}

/// Write one page of `host`'s wallpaper catalog from `from` into `reply`,
/// answering its length.
///
/// The page is as long as the entries it carried, not the widest one the
/// channel admits, and the total lets the caller decide whether to ask
/// again.
fn wallpapers_reply(
    reply: &mut [u8; WINDOW_REPLY_MAX],
    host: &mut dyn WindowHost,
    from: u16,
) -> usize {
    let catalog = host.wallpaper_catalog();
    let total = u16::try_from(catalog.len()).unwrap_or(u16::MAX);
    let page = catalog.get(usize::from(from)..).unwrap_or(&[]);
    let mut frame = [0u8; WINDOW_WALLPAPERS_REPLY_MAX];
    let len = encode_wallpapers_reply(
        &mut frame,
        Ok((
            total,
            page.iter().map(|name| WallpaperEntry {
                category: name.category.as_bytes(),
                file: name.file.as_bytes(),
            }),
        )),
    );
    reply[..len].copy_from_slice(&frame[..len]);
    len
}

/// Write `host`'s cursor-set choice space into `reply`, answering its
/// length.
///
/// The whole choice space, never a page: a store offers at most what one
/// reply frame holds, so a chooser learns every set it may offer in one
/// call.
fn cursor_sets_reply(reply: &mut [u8; WINDOW_REPLY_MAX], host: &mut dyn WindowHost) -> usize {
    let sets = host.cursor_sets();
    let mut frame = [0u8; WINDOW_CURSOR_SETS_REPLY_MAX];
    let len = encode_cursor_sets_reply(&mut frame, Ok(sets.iter().map(|set| set.0.as_bytes())));
    reply[..len].copy_from_slice(&frame[..len]);
    len
}

/// Write a cursor-set refusal into `reply`, answering its length.
fn cursor_sets_refusal(reply: &mut [u8; WINDOW_REPLY_MAX], err: Errno) -> usize {
    let mut frame = [0u8; WINDOW_CURSOR_SETS_REPLY_MAX];
    let len = encode_cursor_sets_reply(&mut frame, Err::<core::iter::Empty<&[u8]>, Errno>(err));
    reply[..len].copy_from_slice(&frame[..len]);
    len
}

/// Write the sources the host answered, or its refusal, into `reply`,
/// answering its length.
fn notify_sources_reply(
    reply: &mut [u8; WINDOW_REPLY_MAX],
    answered: Result<&[BundleId], Errno>,
) -> usize {
    let mut frame = [0u8; WINDOW_NOTIFY_SOURCES_REPLY_MAX];
    let len = encode_notify_sources_reply(
        &mut frame,
        answered.map(|sources| sources.iter().map(|source| source.as_str().as_bytes())),
    );
    reply[..len].copy_from_slice(&frame[..len]);
    len
}

/// Write a wallpaper-catalog refusal into `reply`, answering its length.
fn wallpapers_refusal(reply: &mut [u8; WINDOW_REPLY_MAX], err: Errno) -> usize {
    let mut frame = [0u8; WINDOW_WALLPAPERS_REPLY_MAX];
    let len = encode_wallpapers_reply(
        &mut frame,
        Err::<(u16, core::iter::Empty<WallpaperEntry<'_>>), Errno>(err),
    );
    reply[..len].copy_from_slice(&frame[..len]);
    len
}

fn minted_id_reply(reply: &mut [u8; WINDOW_REPLY_MAX], result: Result<u64, Errno>) -> usize {
    reply[..WINDOW_MINTED_ID_REPLY_LEN].copy_from_slice(&encode_minted_id_reply(result));
    WINDOW_MINTED_ID_REPLY_LEN
}

/// Write a create reply into `reply`, returning its length.
fn create_reply(
    reply: &mut [u8; WINDOW_REPLY_MAX],
    result: Result<u64, Errno>,
    server: ProcId,
) -> usize {
    reply[..WINDOW_CREATE_REPLY_LEN].copy_from_slice(&encode_create_reply(result, server));
    WINDOW_CREATE_REPLY_LEN
}

/// Write a desktop reply into `reply`, returning its length.
///
/// It carries this session's own attested identity for the same reason the
/// create reply does — it is what an app requires of every event's sender —
/// and on this reply too, because an app may declare an icon-bar presence
/// before it owns any window.
fn desktop_reply(
    reply: &mut [u8; WINDOW_REPLY_MAX],
    result: Result<DesktopInfo, Errno>,
    server: ProcId,
) -> usize {
    reply[..WINDOW_DESKTOP_REPLY_LEN].copy_from_slice(&encode_desktop_reply(result, server));
    WINDOW_DESKTOP_REPLY_LEN
}
