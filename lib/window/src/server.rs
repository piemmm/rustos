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
//!   [`WINDOWS_PER_CLIENT_MAX`] windows per attested client, so one app
//!   cannot pin unbounded shared memory or flood the taskbar.
//! * **Teardown fails closed.** [`WindowServer::client_exited`] drops a
//!   dead client's windows (and their region mappings) and tells the
//!   host, so an exited app never leaks a mapped grant or a ghost
//!   taskbar entry.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tairix_abi::desktop::DesktopInfo;
use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_abi::origin::ProcId;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::window_ipc::{
    encode_create_reply, encode_desktop_reply, AppBar, WindowEvent, WindowRequest, WindowTitle,
    WINDOW_CREATE_REPLY_LEN, WINDOW_DESKTOP_REPLY_LEN,
};
use tairix_abi::Errno;
use tairix_display::{FrameRegion, ShmMapper};

/// Upper bound, in bytes, of any reply [`WindowServer::serve`] writes,
/// so one fixed buffer holds every outcome: the create frame, the desktop
/// frame, or the status frame that fits inside either.
pub const WINDOW_REPLY_MAX: usize = if WINDOW_CREATE_REPLY_LEN > WINDOW_DESKTOP_REPLY_LEN {
    WINDOW_CREATE_REPLY_LEN
} else {
    WINDOW_DESKTOP_REPLY_LEN
};

/// Most windows one attested client may hold open at once. A validation
/// bound, not a capacity: a desktop app opens a handful of windows, and
/// each one pins its shared frame region in the session, so an unbounded
/// count would let one hostile app exhaust the session's address space.
pub const WINDOWS_PER_CLIENT_MAX: usize = 16;

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
    /// The host is the **enforcer** of
    /// [`WindowSizing::min_width_px`]/[`WindowSizing::min_height_px`]: it
    /// resizes no smaller than the larger of that and its own frame
    /// furniture's floor. The app states its minimum here once and lays out
    /// at whatever size it is given, so nothing resizes itself back.
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

/// What an app asks of the window manager's sizing of one window: whether
/// it may be resized at all, and the smallest client it may be resized to.
///
/// The pair travels as one value because the minimum only means something
/// for a resizable window — a minimum declared without `resizable` is
/// refused at decode, since a window that is never resized can never be
/// measured against a floor.
///
/// One definition serves both halves: the app fills it in for
/// [`WindowClient::create`] and the engine hands the host the same value,
/// so the two cannot drift.
///
/// [`WindowClient::create`]: crate::client::WindowClient::create
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowSizing {
    /// Whether the window manager presents the window with a resize
    /// grabber and a live maximize/restore size toggle. A fixed-size app
    /// leaves this `false` and is offered neither affordance (and never
    /// receives a `WindowEvent::Resized`).
    pub resizable: bool,
    /// Smallest client width, in physical pixels, the window manager may
    /// resize the window to; `0` declares no minimum of the app's own,
    /// leaving the frame furniture's floor to stand alone.
    ///
    /// The window manager enforces it, never the app: an app states the
    /// floor here once and then lays out at exactly the size it is told.
    /// An app that resized itself back up instead would fight the drag,
    /// frame by frame.
    pub min_width_px: u32,
    /// Smallest client height, in physical pixels, on the same terms as
    /// [`min_width_px`](Self::min_width_px).
    pub min_height_px: u32,
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

/// One live window: its attested owner, its event route, its
/// once-mapped frame region, and whether a trusted-picker request is
/// awaiting its conclusion.
struct WindowRecord<R> {
    owner: ProcId,
    event_endpoint: u64,
    surface: DisplayMode,
    frame_count: u32,
    frame_len: usize,
    region: R,
    /// A `PickFile` was accepted and neither conclusion
    /// (`FilePicked`/`PickCancelled`) has been delivered yet. At most one
    /// pick is pending per window; the engine sets it on acceptance and
    /// clears it when the conclusion is delivered, so the protocol's
    /// one-conclusion-per-acceptance shape is enforced in one place.
    pick_pending: bool,
    /// The parent top-level window this record is a popup of, or `None`
    /// for an ordinary top-level window. A popup is closed when its
    /// parent closes, so the link lives beside the window it binds.
    parent: Option<u64>,
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
}

impl<M: ShmMapper> WindowServer<M> {
    /// An engine with no windows, mapping through `mapper`, replying as
    /// `server` (the session's own kernel-attested `ProcId`, e.g. from
    /// `self_origin`).
    pub const fn new(mapper: M, server: ProcId) -> Self {
        Self {
            mapper,
            server,
            windows: BTreeMap::new(),
            next_id: 1,
            app_bars: BTreeMap::new(),
        }
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
                    // Both create requests answer with the create-reply
                    // frame, so an attestation failure must too.
                    WindowRequest::Create { .. } | WindowRequest::CreatePopup { .. } => {
                        create_reply(reply, Err(err), self.server)
                    }
                    _ => status(reply, Err(err)),
                };
            }
        };
        self.dispatch(host, caller, &decoded, reply)
    }

    /// Act on one decoded request from the attested `caller`, writing the
    /// encoded outcome into `reply` and returning its length.
    fn dispatch(
        &mut self,
        host: &mut dyn WindowHost,
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
                resizable,
                min_width_px,
                min_height_px,
            } => {
                let spec = CreateSpec {
                    shm_handle,
                    event_endpoint,
                    frame_count,
                    surface: surface_of(width_px, height_px, stride_bytes, format),
                    title,
                    sizing: WindowSizing {
                        resizable,
                        min_width_px,
                        min_height_px,
                    },
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
            WindowRequest::SetAppBar(ref bar) => status(reply, self.set_app_bar(host, caller, bar)),
            WindowRequest::SetBackdropBlur {
                window_id,
                radius_px,
            } => status(
                reply,
                self.set_backdrop_blur(host, caller, window_id, radius_px),
            ),
            // Read-only and ungated: the reply describes the caller's own
            // seat, holding nothing another principal owns and granting
            // no authority, so every client on the desktop may ask.
            WindowRequest::QueryDesktop => desktop_reply(reply, host.desktop()),
            // A create mints a window id and is answered by the caller with
            // the create-reply frame; refusing it here keeps this total
            // without a second copy of that path, and refuses rather than
            // opening a window down a route that never validated one.
            WindowRequest::Create { .. } | WindowRequest::CreatePopup { .. } => {
                create_reply(reply, Err(Errno::NotSupported), self.server)
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
        let owned = self
            .windows
            .values()
            .filter(|record| record.owner == caller)
            .count();
        if owned >= WINDOWS_PER_CLIENT_MAX {
            return Err(Errno::NoSpace);
        }
        let frame_len = frame_bytes(&spec.surface)?;
        let total = frame_len
            .checked_mul(spec.frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
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
                region,
                pick_pending: false,
                parent: None,
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
    /// or unknown parent answers `NotFound`, leaking nothing). It counts
    /// against the **same** per-client budget as a top-level window, so a
    /// popup can never be used to exceed [`WINDOWS_PER_CLIENT_MAX`]. The
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
        // A popup counts against the same per-client budget as a top-level
        // window (the parent it needs is itself one of those windows), so
        // "popup" cannot be used to hold open more than the cap allows.
        let owned = self
            .windows
            .values()
            .filter(|record| record.owner == caller)
            .count();
        if owned >= WINDOWS_PER_CLIENT_MAX {
            return Err(Errno::NoSpace);
        }
        let frame_len = frame_bytes(&spec.surface)?;
        let total = frame_len
            .checked_mul(spec.frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
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
                region,
                pick_pending: false,
                parent: Some(spec.parent_window_id),
            },
        );
        Ok(window_id)
    }

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
            record.region = region;
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
        let base = frame_index as usize * record.frame_len;
        let bytes = record.region.bytes();
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
        self.remove_with_popups(host, window_id);
        Ok(())
    }

    /// Remove `window_id` and every popup keyed to it, telling the host of
    /// each teardown. The parent is dropped first, then its popups; a
    /// popup (which owns no popups) drops alone.
    fn remove_with_popups(&mut self, host: &mut dyn WindowHost, window_id: u64) {
        let popups: alloc::vec::Vec<u64> = self
            .windows
            .iter()
            .filter(|(_, record)| record.parent == Some(window_id))
            .map(|(&id, _)| id)
            .collect();
        self.windows.remove(&window_id);
        host.window_closed(window_id);
        for popup in popups {
            self.windows.remove(&popup);
            host.window_closed(popup);
        }
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
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — no such window (it was closed, or never
    ///   existed); the session drops the event.
    /// * [`Errno::OutOfRange`] — a pointer event outside the window's
    ///   surface, a pick conclusion with no pick pending (a routing bug,
    ///   refused rather than delivered), or an application-scoped event
    ///   (those go through [`Self::deliver_app_event`]).
    /// * Any [`Errno`] the sink surfaces; a refused delivery leaves a
    ///   pending pick pending (the session decides whether to retry or
    ///   tear the client down). A sink that *accepts* the conclusion is
    ///   answering for it, whether it goes out now or from a hold-back.
    pub fn deliver_event(
        &mut self,
        sink: &mut dyn EventSink,
        event: &WindowEvent,
    ) -> Result<(), Errno> {
        let record = self
            .windows
            .get_mut(&event.window_id().ok_or(Errno::OutOfRange)?)
            .ok_or(Errno::NotFound)?;
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
        sink.deliver(record.event_endpoint, event)?;
        if concludes_pick {
            record.pick_pending = false;
        }
        Ok(())
    }
}

/// Look up `window_id` **as owned by** `caller`. A window owned by
/// someone else answers exactly like a window that does not exist, so
/// the reply leaks nothing about other clients.
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
fn desktop_reply(reply: &mut [u8; WINDOW_REPLY_MAX], result: Result<DesktopInfo, Errno>) -> usize {
    reply[..WINDOW_DESKTOP_REPLY_LEN].copy_from_slice(&encode_desktop_reply(result));
    WINDOW_DESKTOP_REPLY_LEN
}
