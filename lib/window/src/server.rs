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

use tairix_abi::driver::display::{DamageRect, DisplayMode};
use tairix_abi::origin::ProcId;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::window_ipc::{
    encode_create_reply, WindowEvent, WindowRequest, WindowTitle, WINDOW_CREATE_REPLY_LEN,
};
use tairix_abi::Errno;
use tairix_display::{FrameRegion, ShmMapper};

/// Upper bound, in bytes, of any reply [`WindowServer::serve`] writes:
/// the create reply is the longest frame, and the status frame always
/// fits inside it.
pub const WINDOW_REPLY_MAX: usize = WINDOW_CREATE_REPLY_LEN;

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
    /// titled `title`. An error refuses the create: the engine unmaps
    /// the region and replies the refusal, keeping engine and host in
    /// lockstep.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the host cannot open a window for (e.g. the
    /// desktop is tearing down); the refusal is relayed to the client.
    fn window_opened(
        &mut self,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
    ) -> Result<(), Errno>;

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
}

/// The event-delivery seam — the session's app-ward send (`ipc_send` to
/// the owning app's event endpoint in production) behind a trait, so
/// event routing is host-testable.
pub trait EventSink {
    /// Deliver one encoded [`WindowEvent`] to `endpoint`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the delivery surfaces (a full queue, a dead
    /// receiver); events are advisory, so the caller may drop the event
    /// but must never block the session on it.
    fn deliver(&mut self, endpoint: u64, event: &[u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno>;
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
                    WindowRequest::Create { .. } => create_reply(reply, Err(err), self.server),
                    _ => status(reply, Err(err)),
                }
            }
        };
        match decoded {
            WindowRequest::Create {
                shm_handle,
                event_endpoint,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
                title,
            } => {
                let spec = CreateSpec {
                    shm_handle,
                    event_endpoint,
                    frame_count,
                    surface: DisplayMode {
                        width_px,
                        height_px,
                        stride_bytes,
                        format,
                    },
                    title,
                };
                create_reply(reply, self.create(host, caller, spec), self.server)
            }
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
                    surface: DisplayMode {
                        width_px,
                        height_px,
                        stride_bytes,
                        format,
                    },
                };
                status(reply, self.resize(host, caller, spec))
            }
        }
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
        host.window_opened(window_id, &spec.surface, spec.title.as_str())?;
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

    /// Close `caller`'s window `window_id`, dropping its region mapping.
    fn close(
        &mut self,
        host: &mut dyn WindowHost,
        caller: ProcId,
        window_id: u64,
    ) -> Result<(), Errno> {
        owned_window(&self.windows, caller, window_id)?;
        self.windows.remove(&window_id);
        host.window_closed(window_id);
        Ok(())
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
    ///   surface, or a pick conclusion with no pick pending (a routing
    ///   bug, refused rather than delivered).
    /// * Any [`Errno`] the sink surfaces; a refused delivery leaves a
    ///   pending pick pending (the session decides whether to retry or
    ///   tear the client down).
    pub fn deliver_event(
        &mut self,
        sink: &mut dyn EventSink,
        event: &WindowEvent,
    ) -> Result<(), Errno> {
        let record = self
            .windows
            .get_mut(&event.window_id())
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
        sink.deliver(record.event_endpoint, &event.to_le_bytes())?;
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
