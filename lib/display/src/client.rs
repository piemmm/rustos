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
//! and sends the frame *index* plus the damage rectangle — never pixel
//! bytes. With more than one frame the copy also refreshes the target
//! frame's **stale region** (the damage accumulated while other frames
//! were presented), the bookkeeping that keeps a double-buffered frame
//! current without ever copying the whole surface.

use rustos_abi::display_ipc::{
    decode_mode_reply, DisplayRequest, DISPLAY_MAX_FRAMES, DISPLAY_MODE_REPLY_LEN,
};
use rustos_abi::driver::display::{DamageRect, Display, DisplayMode};
use rustos_abi::reply::decode_status_reply;
use rustos_abi::{DriverError, Errno};

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
    /// The service's typed refusal, a transport failure, or a corrupt
    /// status frame.
    pub fn present(&mut self, frame_index: u32, damage: DamageRect) -> Result<(), Errno> {
        let request = DisplayRequest::Present {
            seat_id: self.seat_id,
            frame_index,
            damage,
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
    frames: &'f mut [u8],
    frame_len: usize,
    frame_count: u32,
    /// The frame the *next* present renders into.
    back: u32,
    /// Per frame: the region whose pixels predate the last present into
    /// that frame (`None` = current). Refreshed on every present so a
    /// re-used buffer never shows stale pixels.
    stale: [Option<DamageRect>; DISPLAY_MAX_FRAMES as usize],
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
        Ok(Self {
            client,
            mode,
            frames,
            frame_len,
            frame_count,
            back: 0,
            stale: [Some(DamageRect::full(&mode)); DISPLAY_MAX_FRAMES as usize],
        })
    }

    /// Copy `damage` (unioned with the back frame's stale region) from
    /// `frame` into the back frame, present it, and advance the buffer
    /// ring.
    fn push(&mut self, frame: &[u8], damage: DamageRect) -> Result<(), DriverError> {
        if frame.len() < self.frame_len {
            return Err(DriverError::BufferTooSmall);
        }
        let back = self.back as usize;
        let refresh = union(self.stale[back], damage);
        self.copy_region(frame, back, refresh);
        self.client
            .present(self.back, damage)
            .map_err(driver_error_from_errno)?;
        // The presented frame is now current; every other frame missed
        // this damage and must refresh it before its next present.
        for (index, stale) in self.stale.iter_mut().enumerate() {
            if index == back {
                *stale = None;
            } else {
                *stale = Some(union(*stale, damage));
            }
        }
        self.back = (self.back + 1) % self.frame_count;
        Ok(())
    }

    /// Copy `region`'s scanline spans from `frame` into frame `index`.
    fn copy_region(&mut self, frame: &[u8], index: usize, region: DamageRect) {
        let stride = self.mode.stride_bytes as usize;
        let bpp = self.mode.format.bytes_per_pixel() as usize;
        let x0 = region.x as usize * bpp;
        let span = region.width_px as usize * bpp;
        let base = index * self.frame_len;
        for row in 0..region.height_px as usize {
            let line = (region.y as usize + row) * stride + x0;
            self.frames[base + line..base + line + span].copy_from_slice(&frame[line..line + span]);
        }
    }
}

impl<T: DisplayTransport> Display for RemoteDisplay<'_, T> {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }

    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        let full = DamageRect::full(&self.mode);
        self.push(frame, full)
    }

    fn present_region(&mut self, frame: &[u8], damage: DamageRect) -> Result<(), DriverError> {
        damage.validate_in(&self.mode)?;
        self.push(frame, damage)
    }
}

/// The bounding rectangle of `a` (when present) and `b`.
///
/// Both inputs are validated against the same mode before they reach
/// here, so the union stays inside the mode and the arithmetic cannot
/// overflow `u32`.
fn union(a: Option<DamageRect>, b: DamageRect) -> DamageRect {
    let Some(a) = a else { return b };
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    let right = (a.x + a.width_px).max(b.x + b.width_px);
    let bottom = (a.y + a.height_px).max(b.y + b.height_px);
    DamageRect {
        x: left,
        y: top,
        width_px: right - left,
        height_px: bottom - top,
    }
}
