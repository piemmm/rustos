//! The client half: the session-side presenter.
//!
//! [`DisplayClient`] speaks the wire protocol over an injected
//! [`DisplayTransport`] (the `ipc_call` syscall in production, a mock in
//! tests). [`RemoteDisplay`] layers the *existing* [`Display`] trait
//! over it, so a compositor presents through a remote display exactly
//! as through a local driver — `Compositor::present` is unchanged.
//!
//! # Zero-copy shape
//!
//! The session owns the shared frame region (its own `shm_create`
//! mapping) and hands `RemoteDisplay` that mapping as a plain mutable
//! slice. A present copies only the changed pixels into the back frame
//! and sends the frame *index* plus the damage rectangles — never pixel
//! bytes. With more than one frame the copy also refreshes the target
//! frame's **stale region** (the damage accumulated while other frames
//! were presented), the bookkeeping that keeps a double-buffered frame
//! current without ever copying the whole surface.
//!
//! A frame is presented **once**, carrying every rectangle it changed: the
//! ring advances once per frame rather than once per rectangle, and each
//! rectangle is copied as itself. A frame that changed a menu and its
//! owner's title band therefore costs those two rectangles, not two copies
//! of the box that spans them.

use tairix_abi::display_ipc::{
    decode_mode_reply, DamageList, DisplayRequest, DISPLAY_MAX_FRAMES, DISPLAY_MODE_REPLY_LEN,
};
use tairix_abi::driver::display::{DamageRect, Display, DisplayMode, MAX_DAMAGE_RECTS};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::{DriverError, Errno};
use tairix_geometry::{Rect, Region};

use crate::driver_error_from_errno;

/// The one call the client issues: send one request frame, receive one
/// reply frame — the `ipc_call` syscall behind a seam, so the client is
/// host-testable.
pub trait DisplayTransport {
    /// Issue one synchronous call to the display endpoint.
    ///
    /// Returns the reply length written into `reply`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the transport surfaces (no such endpoint, a dead
    /// server); the caller treats a transport failure exactly like a
    /// refused request.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// A typed handle on the display service for one seat.
pub struct DisplayClient<T: DisplayTransport> {
    transport: T,
    seat_id: u64,
}

impl<T: DisplayTransport> DisplayClient<T> {
    /// A client acting for `seat_id` over `transport`.
    pub const fn new(transport: T, seat_id: u64) -> Self {
        Self { transport, seat_id }
    }

    /// The seat this client acts for.
    #[must_use]
    pub const fn seat_id(&self) -> u64 {
        self.seat_id
    }

    /// Query the active display mode.
    ///
    /// # Errors
    ///
    /// The service's typed refusal ([`Errno::SeatNotOwner`] /
    /// [`Errno::SeatRevoked`] / [`Errno::NotFound`]), a transport
    /// failure, or a malformed reply (fail closed, never a guessed
    /// mode).
    pub fn query(&mut self) -> Result<DisplayMode, Errno> {
        let request = DisplayRequest::Query {
            seat_id: self.seat_id,
        }
        .to_le_bytes();
        let mut reply = [0u8; DISPLAY_MODE_REPLY_LEN];
        let len = self.transport.call(&request, &mut reply)?;
        decode_mode_reply(&reply[..len])
    }

    /// Hand the service the granted frame region: `frame_count` frames
    /// laid out back-to-back, each shaped exactly as `mode`.
    ///
    /// # Errors
    ///
    /// The service's typed refusal, a transport failure, or a corrupt
    /// status frame.
    pub fn configure(
        &mut self,
        shm_handle: u64,
        frame_count: u32,
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        let request = DisplayRequest::Configure {
            seat_id: self.seat_id,
            shm_handle,
            frame_count,
            width_px: mode.width_px,
            height_px: mode.height_px,
            stride_bytes: mode.stride_bytes,
            format: mode.format,
        }
        .to_le_bytes();
        self.status_call(&request)
    }

    /// Present configured frame `frame_index`, of which `damage`
    /// changed.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `damage` is empty or holds more than
    /// [`MAX_DAMAGE_RECTS`] rectangles, the service's typed refusal, a
    /// transport failure, or a corrupt status frame.
    pub fn present(&mut self, frame_index: u32, damage: &[DamageRect]) -> Result<(), Errno> {
        let request = DisplayRequest::Present {
            seat_id: self.seat_id,
            frame_index,
            damage: DamageList::new(damage)?,
        }
        .to_le_bytes();
        self.status_call(&request)
    }

    /// Issue `request` and decode the shared status reply.
    fn status_call(&mut self, request: &[u8]) -> Result<(), Errno> {
        let mut reply = [0u8; DISPLAY_MODE_REPLY_LEN];
        let len = self.transport.call(request, &mut reply)?;
        decode_status_reply(&reply[..len])
    }
}

/// A remote display: the [`Display`] trait served over the display
/// protocol, presenting through the shared frame region.
///
/// Build one with [`RemoteDisplay::new`] after the bring-up handshake
/// (query → create/grant the region → configure) has succeeded; the
/// mapping handed in is the client's own view of the region it granted
/// to the service.
pub struct RemoteDisplay<'f, T: DisplayTransport> {
    client: DisplayClient<T>,
    mode: DisplayMode,
    /// The whole surface, as the rectangle every stale set is clipped to.
    /// Clipping is what makes a stale rectangle's copy total: every edge is
    /// inside the frame, so no span can be negative or overrun a row.
    surface: Rect,
    frames: &'f mut [u8],
    frame_len: usize,
    frame_count: u32,
    /// The frame the *next* present renders into.
    back: u32,
    /// Per frame: the pixels that predate the last present into that frame
    /// (empty = current). Refreshed on every present so a re-used buffer
    /// never shows stale pixels.
    ///
    /// A set, not a bounding box: two far-apart updates stay two small
    /// copies across frames as well as within one.
    stale: [Region; DISPLAY_MAX_FRAMES as usize],
}

impl<'f, T: DisplayTransport> RemoteDisplay<'f, T> {
    /// Wrap a configured session: `frames` is the client's mapping of
    /// the granted region, holding `frame_count` frames shaped as
    /// `mode`.
    ///
    /// Every frame starts marked wholly stale, so the first present into
    /// each buffer copies the full surface once and only the damage
    /// thereafter.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — `frame_count` is outside
    ///   `1..=DISPLAY_MAX_FRAMES`, the mode's frame size overflows, or
    ///   `frames` cannot hold every frame.
    pub fn new(
        client: DisplayClient<T>,
        mode: DisplayMode,
        frames: &'f mut [u8],
        frame_count: u32,
    ) -> Result<Self, Errno> {
        if frame_count == 0 || frame_count > DISPLAY_MAX_FRAMES {
            return Err(Errno::LengthOutOfRange);
        }
        let frame_len = u64::from(mode.stride_bytes)
            .checked_mul(u64::from(mode.height_px))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(Errno::LengthOutOfRange)?;
        let total = frame_len
            .checked_mul(frame_count as usize)
            .ok_or(Errno::LengthOutOfRange)?;
        if frame_len == 0 || frames.len() < total {
            return Err(Errno::LengthOutOfRange);
        }
        // Every stale rectangle is a screen rectangle, so a mode whose
        // extent cannot be expressed as one is refused rather than
        // truncated into a wrong copy.
        if i32::try_from(mode.width_px).is_err() || i32::try_from(mode.height_px).is_err() {
            return Err(Errno::LengthOutOfRange);
        }
        let surface = Rect::new(0, 0, mode.width_px, mode.height_px);
        let mut stale: [Region; DISPLAY_MAX_FRAMES as usize] =
            core::array::from_fn(|_| Region::with_budget(stale_budget(frame_count)));
        for region in stale.iter_mut().take(frame_count as usize) {
            region.add(surface);
        }
        Ok(Self {
            client,
            mode,
            surface,
            frames,
            frame_len,
            frame_count,
            back: 0,
            stale,
        })
    }

    /// Copy `damage` (with whatever the back frame had still to catch up
    /// on) from `frame` into the back frame, present the whole list in one
    /// call, and advance the buffer ring.
    ///
    /// The copy leaves the back frame wholly current, which is what lets a
    /// driver scan the buffer out in full — a page flip, a hardware layer —
    /// and not only the rectangles this present named.
    fn push(&mut self, frame: &[u8], damage: &[DamageRect]) -> Result<(), DriverError> {
        if frame.len() < self.frame_len {
            return Err(DriverError::BufferTooSmall);
        }
        let back = self.back as usize;
        let surface = self.surface;
        // Taken out so the copy below can borrow the frames: the region
        // comes back cleared, keeping its allocation and its budget.
        let mut refresh = core::mem::take(&mut self.stale[back]);
        mark(&mut refresh, damage, surface);
        for &rect in refresh.rects() {
            self.copy_rect(frame, back, rect);
        }
        // Settled before the present, because it holds either way: the copy
        // made this buffer current, and the composed frame has moved on for
        // every other buffer whatever the service does with this one.
        refresh.clear();
        self.stale[back] = refresh;
        let frames = self.frame_count as usize;
        for (index, stale) in self.stale.iter_mut().take(frames).enumerate() {
            if index != back {
                mark(stale, damage, surface);
            }
        }
        self.client
            .present(self.back, damage)
            .map_err(driver_error_from_errno)?;
        self.back = (self.back + 1) % self.frame_count;
        Ok(())
    }

    /// Copy `rect`'s scanline spans from `frame` into frame `index`.
    ///
    /// The clip is what makes the offsets below inside the frame by
    /// construction; a rectangle outside the surface names none of its
    /// pixels and there is nothing to copy.
    fn copy_rect(&mut self, frame: &[u8], index: usize, rect: Rect) {
        let rect = rect.intersection(&self.surface);
        if rect.is_empty() {
            return;
        }
        let (Ok(left), Ok(top)) = (usize::try_from(rect.left()), usize::try_from(rect.top()))
        else {
            return;
        };
        let stride = self.mode.stride_bytes as usize;
        let bpp = self.mode.format.bytes_per_pixel() as usize;
        let x0 = left * bpp;
        let span = rect.width as usize * bpp;
        let base = index * self.frame_len;
        for row in 0..rect.height as usize {
            let line = (top + row) * stride + x0;
            self.frames[base + line..base + line + span].copy_from_slice(&frame[line..line + span]);
        }
    }
}

/// The rectangle budget of one frame's stale set.
///
/// A buffer waits out the other frames before its turn comes round, and
/// each of those presents named at most [`MAX_DAMAGE_RECTS`] rectangles — as
/// does the present that finally consumes the set. Sizing the budget from
/// the ring depth the session configured is therefore what keeps a
/// double-buffered desktop's scattered damage *scattered*: degrade earlier
/// and the catch-up copy becomes the bounding box this crate exists to
/// avoid. Past the budget a region covers its bounding box, which costs
/// pixels but never correctness.
fn stale_budget(frame_count: u32) -> usize {
    (frame_count as usize).saturating_mul(MAX_DAMAGE_RECTS)
}

/// Add `damage` to `region`, in screen rectangles clipped to `surface`.
///
/// A rectangle that cannot be expressed as a screen rectangle marks the
/// whole surface instead: over-covering costs a copy, losing a rectangle
/// would show stale pixels. Validation against the mode makes that
/// unreachable — an accepted rectangle lies inside a surface that fits the
/// coordinate space — but the safe answer is the cheap one to write.
fn mark(region: &mut Region, damage: &[DamageRect], surface: Rect) {
    for rect in damage {
        match (i32::try_from(rect.x), i32::try_from(rect.y)) {
            (Ok(x), Ok(y)) => {
                region.add(Rect::new(x, y, rect.width_px, rect.height_px).intersection(&surface));
            }
            _ => region.add(surface),
        }
    }
}

impl<T: DisplayTransport> Display for RemoteDisplay<'_, T> {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }

    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        self.push(frame, &[DamageRect::full(&self.mode)])
    }

    fn present_rects(&mut self, frame: &[u8], damage: &[DamageRect]) -> Result<(), DriverError> {
        DamageRect::validate_list(damage, &self.mode)?;
        self.push(frame, damage)
    }
}
