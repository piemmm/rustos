//! The server half: the engine a display driver's `Run` binary hosts.
//!
//! [`DisplayServer::serve`] handles exactly one received request:
//! decode → live-lease check → validation → action → encoded reply. The
//! hosting binary owns the transport (bind the reserved
//! `DISPLAY_ENDPOINT`, park on its wait-set, `call_recv` /
//! `call_reply`); the engine owns every decision in between, so the
//! semantics are host-testable against mock seams and identical under
//! any host.
//!
//! # Security shape
//!
//! * **Identity is kernel-attested.** The engine never reads a claimed
//!   lease: [`PeerFacts::live_generation`] asks the kernel whether the
//!   *in-flight caller of this endpoint* holds the seat's live lease
//!   (`call_peer_seat`), and every request that acts for a seat — `Query`
//!   included — is gated on it before any state is read or mutated.
//! * **The device read carries its own authority.** `QueryStats` acts for no
//!   seat, so a lease is the wrong question about it: it is gated on the
//!   caller's kernel-attested `CAP_SYSINFO_HW`
//!   ([`PeerFacts::holds_capability`]) instead. A monitor holds no lease and
//!   never will, and hardware utilisation is what that capability governs
//!   everywhere else in the system.
//! * **The occupancy the read serves is measured, not claimed.** `busy_ns`
//!   accumulates only around the driver's own present call, and the window it
//!   is a share of opens at that same instant — so a refused present (a stale
//!   lease, a bad frame index, a rectangle outside the mode) adds nothing
//!   because the device was never driven, and no request an unauthorised
//!   caller can send moves any of it.
//! * **Frames are bound to the lease that configured them.** `Configure`
//!   records the granting lease's generation; a `Present` whose caller
//!   holds any *other* generation (the seat was revoked and re-acquired)
//!   is refused `NotFound` until the new owner reconfigures, and a
//!   caller that lost the lease outright drops the stale configuration —
//!   one owner's frames can never scan out under another's lease.
//! * **Every bound is checked before any pixel access.** The configured
//!   geometry must equal the active mode exactly, the mapped region must
//!   hold every frame, the frame index must name a configured frame, and
//!   *every* damage rectangle must lie inside the mode — the whole list is
//!   checked before the first is blitted, so one bad rectangle refuses the
//!   present rather than half-applying it. Each failure is a typed
//!   refusal, never a clamp.

use tairix_abi::display_ipc::{
    encode_mode_reply, encode_stats_reply, DamageList, DisplayRequest, DisplayStats,
    DISPLAY_MODE_REPLY_LEN, DISPLAY_STATS_REPLY_LEN,
};
use tairix_abi::driver::display::{DamageRect, Display, DisplayFormat, DisplayMode};
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::time::MonotonicClock;
use tairix_abi::{CapabilityId, Errno};

/// Upper bound, in bytes, of any reply [`DisplayServer::serve`] writes:
/// the statistics reply is the longest frame, and the mode and status frames
/// both fit inside it.
pub const DISPLAY_REPLY_MAX: usize = if DISPLAY_STATS_REPLY_LEN > DISPLAY_MODE_REPLY_LEN {
    DISPLAY_STATS_REPLY_LEN
} else {
    DISPLAY_MODE_REPLY_LEN
};

/// The kernel's own facts about the in-flight caller, behind a seam — so the
/// engine is host-testable and never trusts a claim the caller made about
/// itself.
///
/// Both questions are about the *caller of this endpoint identified by
/// `ticket`*, never about a task named in a request frame.
pub trait PeerFacts {
    /// The live lease generation of `seat_id` **if and only if** the
    /// in-flight caller identified by `ticket` holds it.
    ///
    /// # Errors
    ///
    /// * [`Errno::SeatNotOwner`] — the caller does not hold the live
    ///   lease.
    /// * [`Errno::SeatRevoked`] — the caller's lease was forcibly
    ///   revoked (the distinct signal a compositor tears down on).
    /// * [`Errno::NotFound`] — no such seat.
    fn live_generation(&mut self, ticket: u64, seat_id: u64) -> Result<u64, Errno>;

    /// Whether the in-flight caller identified by `ticket` holds `cap`.
    ///
    /// # Errors
    ///
    /// Any error the kernel's attestation reports. The engine treats one as a
    /// refusal, so an unanswerable question never widens authority.
    fn holds_capability(&mut self, ticket: u64, cap: CapabilityId) -> Result<bool, Errno>;
}

/// A mapped shared-frame region the engine reads presented frames from.
pub trait FrameRegion {
    /// The mapped bytes. The length is fixed at map time.
    fn bytes(&self) -> &[u8];
}

/// The map-once seam over the kernel's `shm_map` of a granted handle.
///
/// Mapping happens exactly once per `Configure`; the present hot path
/// only indexes the returned region. Dropping the returned region (a
/// reconfigure, a lost lease) is the implementation's cue to unmap.
pub trait ShmMapper {
    /// The region type a successful map yields.
    type Region: FrameRegion;

    /// Map the granted region `handle`, requiring at least `min_len`
    /// bytes.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`] — the handle names no grant for this task
    ///   (the kernel's owner check at `shm_map`).
    /// * [`Errno::LengthOutOfRange`] — the region is smaller than
    ///   `min_len`.
    fn map(&mut self, handle: u64, min_len: usize) -> Result<Self::Region, Errno>;
}

/// One lease's adopted frame region.
struct Configured<R> {
    /// The seat the frames belong to.
    seat_id: u64,
    /// The lease generation that configured them.
    generation: u64,
    /// The mode the geometry was validated against at configure time.
    mode: DisplayMode,
    /// Frames laid out back-to-back in `region`.
    frame_count: u32,
    /// Bytes per frame (`mode.stride_bytes * mode.height_px`).
    frame_len: usize,
    /// The once-mapped shared region.
    region: R,
}

/// The display-service engine: one instance serves one `Display`.
pub struct DisplayServer<M: ShmMapper, C: MonotonicClock> {
    mapper: M,
    clock: C,
    configured: Option<Configured<M::Region>>,
    /// Latched by the first present that reaches the driver: the instant the
    /// device was first driven, which `busy_ns` and `idle_ns` partition.
    /// [`None`] until then, so a device nothing has presented to reports no
    /// window rather than one starting at an epoch the engine invented.
    window_start_ns: Option<u64>,
    /// Cumulative nanoseconds spent inside the driver's present call.
    busy_ns: u64,
    /// Latched by the first successful [`Self::present`]: a client frame
    /// has reached the scan-out surface at least once in this engine's
    /// lifetime. Observability only — no serving decision reads it.
    presented: bool,
}

impl<M: ShmMapper, C: MonotonicClock> DisplayServer<M, C> {
    /// An engine with no configured frames, mapping through `mapper` and
    /// timing the device it drives against `clock`.
    pub const fn new(mapper: M, clock: C) -> Self {
        Self {
            mapper,
            clock,
            configured: None,
            window_start_ns: None,
            busy_ns: 0,
            presented: false,
        }
    }

    /// Serve one received request and encode its reply into `reply`,
    /// returning the reply length (`<= DISPLAY_REPLY_MAX`).
    ///
    /// `ticket` is the in-flight call's receive ticket, threaded to the
    /// [`PeerFacts`] so every authority fact is about the actual caller.
    /// Every outcome — including a malformed request — is a well-formed
    /// reply; the engine never leaves a call unanswered.
    pub fn serve(
        &mut self,
        display: &mut dyn Display,
        peer: &mut dyn PeerFacts,
        ticket: u64,
        request: &[u8],
        reply: &mut [u8; DISPLAY_REPLY_MAX],
    ) -> usize {
        let request = match DisplayRequest::from_bytes(request) {
            Ok(request) => request,
            Err(err) => return status(reply, Err(err)),
        };
        match request {
            // A device read acts for no seat, so a lease is the wrong
            // question about it: it is gated on the caller's own
            // hardware-inventory authority, and an unanswerable attestation
            // is a refusal. Nothing — not even the clock — is read before
            // that answer, so a caller without the authority cannot move any
            // state this engine holds.
            DisplayRequest::QueryStats => {
                let result = match peer.holds_capability(ticket, CapabilityId::SYSINFO_HW) {
                    Ok(true) => self.device_stats(display),
                    Ok(false) => Err(Errno::PermissionDenied),
                    Err(err) => Err(err),
                };
                stats(reply, result)
            }
            DisplayRequest::Query { seat_id } => {
                let result = self.lease(peer, ticket, seat_id).and_then(|_| {
                    display
                        .mode_info()
                        .map_err(tairix_abi::DriverError::as_errno)
                });
                mode(reply, result)
            }
            DisplayRequest::Configure {
                seat_id,
                shm_handle,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
            } => {
                let result = match self.lease(peer, ticket, seat_id) {
                    Ok(generation) => self.configure(
                        display,
                        seat_id,
                        generation,
                        shm_handle,
                        frame_count,
                        width_px,
                        height_px,
                        stride_bytes,
                        format,
                    ),
                    Err(err) => Err(err),
                };
                status(reply, result)
            }
            DisplayRequest::Present {
                seat_id,
                frame_index,
                damage,
            } => {
                let result = match self.lease(peer, ticket, seat_id) {
                    Ok(generation) => {
                        self.present(display, seat_id, generation, frame_index, damage)
                    }
                    Err(err) => Err(err),
                };
                status(reply, result)
            }
        }
    }

    /// The caller's live lease generation on `seat_id`, gating every
    /// seat-scoped operation — `Query` included — before any state is read or
    /// mutated.
    ///
    /// A refusal also drops a configuration whose seat the caller no longer
    /// holds: stale frames are released the moment the loss is observed,
    /// never scanned out.
    fn lease(&mut self, peer: &mut dyn PeerFacts, ticket: u64, seat_id: u64) -> Result<u64, Errno> {
        match peer.live_generation(ticket, seat_id) {
            Ok(generation) => Ok(generation),
            Err(err) => {
                if self
                    .configured
                    .as_ref()
                    .is_some_and(|c| c.seat_id == seat_id)
                {
                    self.configured = None;
                }
                Err(err)
            }
        }
    }

    /// The device's own statistics: the occupancy this engine measured, what
    /// the driver reports about the device, and the mode it scans out.
    ///
    /// `busy_ns` and `idle_ns` partition the window since the device was
    /// first driven, so the two always sum to a real elapsed span rather than
    /// to a total the engine chose. A device that has never been presented to
    /// has no window at all and reports neither, which is what leaves the
    /// reader with no share instead of a fabricated idle one.
    fn device_stats(&self, display: &dyn Display) -> Result<DisplayStats, Errno> {
        let mode = display
            .mode_info()
            .map_err(tairix_abi::DriverError::as_errno)?;
        let window_ns = self
            .window_start_ns
            .map_or(0, |start| self.clock.now_ns().saturating_sub(start));
        Ok(DisplayStats {
            seat_id: self.configured.as_ref().map_or(0, |c| c.seat_id),
            busy_ns: self.busy_ns,
            idle_ns: window_ns.saturating_sub(self.busy_ns),
            device: display.device_report(),
            mode,
        })
    }

    /// Whether the engine currently holds a configured frame region.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.configured.is_some()
    }

    /// Whether at least one client frame has been successfully presented
    /// to the scan-out surface in this engine's lifetime.
    ///
    /// The hosting service reads this after each served request to emit
    /// its one-shot first-present log record — the operational witness
    /// that the session → service → surface path is live end to end. It
    /// is a fact about served work, never an authority: no request
    /// outcome depends on it.
    #[must_use]
    pub const fn has_presented(&self) -> bool {
        self.presented
    }

    /// Adopt the caller's granted frame region for `seat_id` under the
    /// live `generation`, replacing any earlier configuration.
    #[allow(clippy::too_many_arguments)]
    fn configure(
        &mut self,
        display: &mut dyn Display,
        seat_id: u64,
        generation: u64,
        shm_handle: u64,
        frame_count: u32,
        width_px: u32,
        height_px: u32,
        stride_bytes: u32,
        format: DisplayFormat,
    ) -> Result<(), Errno> {
        let mode = display
            .mode_info()
            .map_err(tairix_abi::DriverError::as_errno)?;
        // The frames must be bit-compatible with the scan-out surface:
        // an exact-mode match, so a present is a bounded copy with no
        // conversion on the hot path.
        if width_px != mode.width_px
            || height_px != mode.height_px
            || stride_bytes != mode.stride_bytes
            || format != mode.format
        {
            return Err(Errno::LengthOutOfRange);
        }
        let frame_len = frame_bytes(&mode)?;
        let total = frame_len
            .checked_mul(frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
        // Replace-then-map: the old region (if any) is dropped first so
        // the mapper may release it before mapping the new grant.
        self.configured = None;
        let region = self.mapper.map(shm_handle, total)?;
        if region.bytes().len() < total {
            return Err(Errno::LengthOutOfRange);
        }
        self.configured = Some(Configured {
            seat_id,
            generation,
            mode,
            frame_count,
            frame_len,
            region,
        });
        Ok(())
    }

    /// Scan out configured frame `frame_index` for the caller holding
    /// the live `generation` on `seat_id`.
    fn present(
        &mut self,
        display: &mut dyn Display,
        seat_id: u64,
        generation: u64,
        frame_index: u32,
        damage: DamageList,
    ) -> Result<(), Errno> {
        let configured = self.configured.as_ref().ok_or(Errno::NotFound)?;
        // Frames are bound to the lease that granted them: a caller on a
        // different seat, or holding a newer lease than the one that
        // configured, must reconfigure before it can present.
        if configured.seat_id != seat_id || configured.generation != generation {
            return Err(Errno::NotFound);
        }
        if frame_index >= configured.frame_count {
            return Err(Errno::OutOfRange);
        }
        let rects = damage.rects();
        DamageRect::validate_list(rects, &configured.mode)
            .map_err(tairix_abi::DriverError::as_errno)?;
        let offset = configured.frame_len * frame_index as usize;
        let bytes = configured.region.bytes();
        let frame = bytes
            .get(offset..offset + configured.frame_len)
            .ok_or(Errno::LengthOutOfRange)?;
        // A rectangle covering the surface makes the rest of the list
        // redundant: the full blit already writes every pixel they name.
        // The driver call is bracketed because the time the device had this
        // frame in flight is what utilisation is a share of; nothing above
        // or below the call is device-busy time. This is also where the
        // measurement window opens — the instant the device was first
        // driven — so no unauthorised request can move its epoch.
        let started_ns = self.clock.now_ns();
        self.window_start_ns.get_or_insert(started_ns);
        let result = if rects.iter().any(|rect| rect.covers(&configured.mode)) {
            display.present(frame)
        } else {
            display.present_rects(frame, rects)
        };
        self.busy_ns = self
            .busy_ns
            .saturating_add(self.clock.now_ns().saturating_sub(started_ns));
        let result = result.map_err(tairix_abi::DriverError::as_errno);
        if result.is_ok() {
            self.presented = true;
        }
        result
    }
}

/// Bytes one frame occupies under `mode`, checked against the host's
/// address width.
fn frame_bytes(mode: &DisplayMode) -> Result<usize, Errno> {
    let bytes = u64::from(mode.stride_bytes)
        .checked_mul(u64::from(mode.height_px))
        .ok_or(Errno::LengthOutOfRange)?;
    usize::try_from(bytes).map_err(|_| Errno::LengthOutOfRange)
}

/// Encode a status outcome into `reply`, returning the frame length.
fn status(reply: &mut [u8; DISPLAY_REPLY_MAX], result: Result<(), Errno>) -> usize {
    reply[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(result));
    STATUS_REPLY_LEN
}

/// Encode a mode outcome into `reply`, returning the frame length.
fn mode(reply: &mut [u8; DISPLAY_REPLY_MAX], result: Result<DisplayMode, Errno>) -> usize {
    reply[..DISPLAY_MODE_REPLY_LEN].copy_from_slice(&encode_mode_reply(result));
    DISPLAY_MODE_REPLY_LEN
}

/// Encode a device-statistics outcome into `reply`, returning the frame
/// length.
fn stats(reply: &mut [u8; DISPLAY_REPLY_MAX], result: Result<DisplayStats, Errno>) -> usize {
    reply[..DISPLAY_STATS_REPLY_LEN].copy_from_slice(&encode_stats_reply(result));
    DISPLAY_STATS_REPLY_LEN
}
