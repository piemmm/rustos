//! The application's half of a window's pixels: the shared frame region it
//! presents from.
//!
//! Every windowed app needs the same three-step dance — create a region,
//! grant it to the reserved window endpoint, hand the grant to `Create` — and
//! the same `unsafe` slice over the mapping it got back. It lives here so
//! there is one of each rather than one per app, and so the *release* path
//! below exists once.
//!
//! # Releasing, and coming back
//!
//! A window's pixels exist three times over: the app's render target, this
//! region, and the session's own converted copy. When nobody can see the
//! window and the machine is short of memory the session gives its copy back
//! and unmaps this region — but the pages only go when *both* sides let go,
//! so it tells the app ([`tairix_abi::window_ipc::WindowEvent::ContentReleased`])
//! and the app answers
//! with [`WindowFrames::release`]. A hidden 4K window then costs nothing
//! instead of sixty megabytes.
//!
//! Coming back is [`WindowClient::frame_pixels`](crate::WindowClient::frame_pixels):
//! the paint that follows the session's next redraw request finds the region
//! released, makes a fresh one, and re-attaches it with the ordinary resize
//! request. The app's paint code does not change.

use tairix_abi::window_ipc::WINDOW_ENDPOINT;
use tairix_rt::shm::SharedRegion;

/// One live mapping of a window's frame region, with the grant the session
/// maps it through.
struct Mapped {
    region: SharedRegion,
    grant: u64,
}

/// A window's shared frame region, or nothing while released.
///
/// Dropping it unmaps, so a window's frames never outlive the value that owns
/// them.
pub struct WindowFrames {
    mapped: Option<Mapped>,
    len: usize,
}

impl WindowFrames {
    /// Create a `len`-byte region and grant it to the window endpoint.
    ///
    /// `None` on any refusal, unmapping a region that mapped but could not be
    /// granted, so a refused allocation never leaves pinned memory behind.
    #[must_use]
    pub fn create(len: usize) -> Option<Self> {
        Some(Self {
            mapped: Some(Self::map(len)?),
            len,
        })
    }

    /// The grant handle a `Create`, `CreatePopup`, or `Resize` request names,
    /// or `None` while released.
    #[must_use]
    pub const fn grant(&self) -> Option<u64> {
        match &self.mapped {
            Some(mapped) => Some(mapped.grant),
            None => None,
        }
    }

    /// Whether the region holds no mapping, because the app released it after
    /// the session did.
    #[must_use]
    pub const fn is_released(&self) -> bool {
        self.mapped.is_none()
    }

    /// The region's bytes, or `None` while released.
    pub fn pixels(&mut self) -> Option<&mut [u8]> {
        Some(self.mapped.as_mut()?.region.bytes_mut())
    }

    /// Give the region back, because the session has released its side.
    ///
    /// Both halves have to go for the pages to be freed at all, which is the
    /// whole point: a mapping either side keeps holds every page of it.
    pub fn release(&mut self) {
        self.mapped = None;
    }

    /// Re-create the region at the size it was and grant it again, reporting
    /// the fresh grant handle for the caller to re-attach with.
    ///
    /// `None` if the region could not be re-created, which leaves it released
    /// rather than half-attached: the caller draws nothing this frame and the
    /// window shows through, exactly as it did while released.
    pub fn reattach(&mut self) -> Option<u64> {
        self.release();
        let mapped = Self::map(self.len)?;
        let grant = mapped.grant;
        self.mapped = Some(mapped);
        Some(grant)
    }

    fn map(len: usize) -> Option<Mapped> {
        let region = SharedRegion::create(len)?;
        let grant = tairix_rt::shm_grant(region.id(), WINDOW_ENDPOINT);
        if grant < 1 {
            // Dropping the region unmaps it, so a grant refusal never leaves
            // pinned memory behind.
            return None;
        }
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        Some(Mapped {
            region,
            grant: grant as u64,
        })
    }
}
