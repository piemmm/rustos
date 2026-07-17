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
//!   lease: [`SeatCheck::live_generation`] asks the kernel whether the
//!   *in-flight caller of this endpoint* holds the seat's live lease
//!   (`call_peer_seat`), and every request — `Query` included — is gated
//!   on it before any state is read or mutated.
//! * **Frames are bound to the lease that configured them.** `Configure`
//!   records the granting lease's generation; a `Present` whose caller
//!   holds any *other* generation (the seat was revoked and re-acquired)
//!   is refused `NotFound` until the new owner reconfigures, and a
//!   caller that lost the lease outright drops the stale configuration —
//!   one owner's frames can never scan out under another's lease.
//! * **Every bound is checked before any pixel access.** The configured
//!   geometry must equal the active mode exactly, the mapped region must
//!   hold every frame, the frame index must name a configured frame, and
//!   the damage rectangle must lie inside the mode; each failure is a
//!   typed refusal, never a clamp.

use tairix_abi::display_ipc::{encode_mode_reply, DisplayRequest, DISPLAY_MODE_REPLY_LEN};
use tairix_abi::driver::display::{DamageRect, Display, DisplayFormat, DisplayMode};
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::Errno;

/// Upper bound, in bytes, of any reply [`DisplayServer::serve`] writes:
/// the mode reply is the longest frame, and the status frame always fits
/// inside it.
pub const DISPLAY_REPLY_MAX: usize = DISPLAY_MODE_REPLY_LEN;

/// The live-lease check — the kernel's `call_peer_seat` behind a seam,
/// so the engine is host-testable and never trusts a claimed lease.
pub trait SeatCheck {
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
pub struct DisplayServer<M: ShmMapper> {
    mapper: M,
    configured: Option<Configured<M::Region>>,
    /// Latched by the first successful [`Self::present`]: a client frame
    /// has reached the scan-out surface at least once in this engine's
    /// lifetime. Observability only — no serving decision reads it.
    presented: bool,
}

impl<M: ShmMapper> DisplayServer<M> {
    /// An engine with no configured frames, mapping through `mapper`.
    pub const fn new(mapper: M) -> Self {
        Self {
            mapper,
            configured: None,
            presented: false,
        }
    }

    /// Serve one received request and encode its reply into `reply`,
    /// returning the reply length (`<= DISPLAY_REPLY_MAX`).
    ///
    /// `ticket` is the in-flight call's receive ticket, threaded to the
    /// [`SeatCheck`] so the lease fact is about the actual caller.
    /// Every outcome — including a malformed request — is a well-formed
    /// reply; the engine never leaves a call unanswered.
    pub fn serve(
        &mut self,
        display: &mut dyn Display,
        seat: &mut dyn SeatCheck,
        ticket: u64,
        request: &[u8],
        reply: &mut [u8; DISPLAY_REPLY_MAX],
    ) -> usize {
        let request = match DisplayRequest::from_bytes(request) {
            Ok(request) => request,
            Err(err) => return status(reply, Err(err)),
        };
        let seat_id = match request {
            DisplayRequest::Query { seat_id }
            | DisplayRequest::Configure { seat_id, .. }
            | DisplayRequest::Present { seat_id, .. } => seat_id,
        };
        // Gate every operation — Query included — on the caller's live
        // lease before touching any state, and drop a configuration whose
        // seat the caller no longer holds: stale frames are released the
        // moment the loss is observed, never scanned out.
        let generation = match seat.live_generation(ticket, seat_id) {
            Ok(generation) => generation,
            Err(err) => {
                if self
                    .configured
                    .as_ref()
                    .is_some_and(|c| c.seat_id == seat_id)
                {
                    self.configured = None;
                }
                return match request {
                    DisplayRequest::Query { .. } => mode(reply, Err(err)),
                    _ => status(reply, Err(err)),
                };
            }
        };
        match request {
            DisplayRequest::Query { .. } => {
                let result = display
                    .mode_info()
                    .map_err(tairix_abi::DriverError::as_errno);
                mode(reply, result)
            }
            DisplayRequest::Configure {
                shm_handle,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
                ..
            } => {
                let result = self.configure(
                    display,
                    seat_id,
                    generation,
                    shm_handle,
                    frame_count,
                    width_px,
                    height_px,
                    stride_bytes,
                    format,
                );
                status(reply, result)
            }
            DisplayRequest::Present {
                frame_index,
                damage,
                ..
            } => {
                let result = self.present(display, seat_id, generation, frame_index, damage);
                status(reply, result)
            }
        }
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
        damage: DamageRect,
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
        damage
            .validate_in(&configured.mode)
            .map_err(tairix_abi::DriverError::as_errno)?;
        let offset = configured.frame_len * frame_index as usize;
        let bytes = configured.region.bytes();
        let frame = bytes
            .get(offset..offset + configured.frame_len)
            .ok_or(Errno::LengthOutOfRange)?;
        let result = if damage.covers(&configured.mode) {
            display.present(frame)
        } else {
            display.present_region(frame, damage)
        };
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
