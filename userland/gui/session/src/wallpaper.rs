//! Preparing the desktop's wallpaper off the session's event loop
//! (`plans/FIX-DESKTOP.md` DESK-4).
//!
//! Painting the backdrop means reading a user-chosen image file — up to
//! `tairix_wallpaper::MAX_WALLPAPER_BYTES` of untrusted bytes — and decoding and
//! fitting it in a capability-empty sandbox worker. Run on the session's own
//! task that is a visible stall at login and again on every settings change: a
//! file read from a slow disk, then a round trip through another process.
//!
//! [`WallpaperDesk`] is the arrangement's whole policy, and it holds no lock, no
//! thread, and no syscall: what the desktop wants painted, what has come back,
//! and the staleness rule that discards a picture prepared for a screen or a
//! choice the desktop has since moved on from. The `Run` binary wraps it in the
//! runtime's futex mutex and parks a worker on a condition variable over it.
//!
//! # Its own sandbox worker, deliberately
//!
//! The icon rasteriser's sandbox worker stays where it is, driven from the
//! session's own task through the handle it has always used. The wallpaper's
//! worker thread owns a **second** one, created inside the thread, so no sandbox
//! handle ever has to cross a thread boundary and the icon path is not changed
//! by any of this. The cost is one more capability-empty process per session; it
//! buys a desktop that comes up without waiting for a picture.
//!
//! # The gallery's previews share the same worker
//!
//! The Settings application browses the shipped store through the desktop
//! rather than reading it itself, so a tile's picture is prepared here too:
//! the same read, the same sandbox, the same thread. One preview is in
//! flight at a time across the whole desktop — a bound on how much decoding
//! any set of clients can queue, and the reason the backdrop is always taken
//! first: the picture the user is actually looking at never waits behind a
//! thumbnail.
//!
//! Nothing is ever recalled. A render already taken cannot be, and every
//! accepted one answers exactly once, so a window that closes mid-render
//! costs one wasted decode into a region only the desktop still maps, and
//! the slot frees itself. Recalling it would mean a second record of which
//! preview is in flight, and two records of one fact are a fact that can
//! disagree with itself.
//!
//! # A wallpaper is never load-bearing
//!
//! Every refusal — an unreadable file, one larger than any wallpaper, a
//! malformed image, a crashed worker, a reply that does not fill the screen —
//! answers "no surface", and the desktop paints its backdrop colour. The reason
//! travels *with* the answer rather than being written where it was noticed:
//! `stderr` is one descriptor and a formatted line reaches it in several writes,
//! so two threads stating something at once would interleave into an unreadable
//! diagnosis. The session states it, once, on its own thread. The desktop never
//! fails over a picture.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::Rect;
use tairix_raster::Surface;
use tairix_wallpaper::{DesktopSettings, WallpaperChoice, WallpaperFit};

/// Everything a prepared wallpaper depends on: the chosen file, how it is
/// placed, and the screen it was placed on.
///
/// Preparing one reads a file and runs a sandboxed decode, so it happens only
/// when one of these really changed — never on a frame path. Comparing the whole
/// value is what makes that decision exact, and what lets a picture prepared for
/// a screen size the session has since left be discarded rather than stretched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WallpaperSource {
    /// The user's choice: a colour-only backdrop, or an image file.
    pub choice: WallpaperChoice,
    /// How the image is placed on the screen.
    pub fit: WallpaperFit,
    /// Screen width in physical pixels.
    pub width: u32,
    /// Screen height in physical pixels.
    pub height: u32,
}

impl WallpaperSource {
    /// What `settings` ask for on a `screen`-sized display.
    ///
    /// The one place the desktop's settings and its output become a wallpaper
    /// request, so the comparison that decides whether to prepare and the
    /// request that is prepared cannot disagree.
    #[must_use]
    pub fn wanted(settings: &DesktopSettings, screen: Rect) -> Self {
        Self {
            choice: settings.wallpaper.clone(),
            fit: settings.fit,
            width: screen.width,
            height: screen.height,
        }
    }

    /// The image file this source names, or `None` for a colour-only backdrop
    /// that needs no preparation at all.
    #[must_use]
    pub fn image_path(&self) -> Option<&str> {
        match &self.choice {
            WallpaperChoice::None => None,
            WallpaperChoice::Image(path) => Some(path.as_str()),
        }
    }
}

/// One gallery preview the desktop has been asked to render: which
/// catalog entry, at what square side, for which window.
///
/// The picture is named by its **catalog position** rather than by a path,
/// because the asking application named it that way: it browses a store it
/// cannot read, so it can only point at what the desktop listed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewRequest {
    /// The asking window, which the conclusion is delivered to.
    pub window_id: u64,
    /// The catalog position asked for.
    pub index: u16,
    /// The square side, in physical pixels, to render at.
    pub side: u16,
}

/// A preview the worker has taken: the request, and the file to read for
/// it, resolved against the catalog before it left the serve loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewJob {
    /// What was asked for.
    pub request: PreviewRequest,
    /// The absolute path of the shipped master to read.
    pub path: String,
}

/// A rendered preview: the request it answers, and the straight-alpha
/// RGBA8 pixels, or `None` for a refusal the asking window is told about
/// rather than left waiting on.
pub struct PreviewDone {
    /// What was asked for.
    pub request: PreviewRequest,
    /// `side * side * 4` bytes, or `None` when the picture could not be
    /// read, decoded, or placed.
    pub pixels: Option<Vec<u8>>,
}

/// One unit of work a wallpaper preparer takes.
pub enum WallpaperJob {
    /// The desktop's own backdrop, which is always taken first.
    Backdrop(WallpaperSource),
    /// One picture of the screensaver's slideshow.
    Slide(WallpaperSource),
    /// One gallery tile for a browsing application.
    Preview(PreviewJob),
}

/// What the desk has for a wallpaper request right now.
pub enum Prepared {
    /// The preparation finished.
    Ready {
        /// The surface to paint the desktop layer over, or `None` for "paint the
        /// backdrop colour" — the answer both for a colour-only choice and for
        /// every refusal, since a wallpaper is never load-bearing.
        surface: Option<Surface>,
        /// Why there is no surface, for the session to state once on its own
        /// thread. `None` when nothing went wrong.
        refusal: Option<String>,
    },
    /// The preparation is under way somewhere else. The desktop keeps whatever
    /// it is painting until the answer arrives.
    Pending,
}

/// The wallpaper arrangement's policy: what is wanted, what has been prepared,
/// and whether a preparer is already working on it.
///
/// Deliberately free of locks, threads, and syscalls, so every rule below is a
/// host test rather than an argument.
#[derive(Default)]
pub struct WallpaperDesk {
    /// What the desktop wants painted, cleared when its answer is stored.
    wanted: Option<WallpaperSource>,
    /// Whether a preparer has taken [`WallpaperDesk::wanted`] and not yet
    /// answered it, so the same picture is never prepared twice at once.
    preparing: bool,
    /// The prepared surface (or the reason there is none), kept until the
    /// desktop asks for that same source.
    done: Option<(WallpaperSource, Result<Surface, String>)>,
    /// The one preview asked for and not yet taken by a preparer.
    wanted_preview: Option<PreviewJob>,
    /// The preview a preparer has taken and not yet answered.
    rendering: Option<PreviewRequest>,
    /// The rendered preview waiting for the serve loop to hand it over.
    preview_done: Option<PreviewDone>,
    /// The slideshow picture asked for and not yet taken by a preparer.
    wanted_slide: Option<WallpaperSource>,
    /// The slideshow picture a preparer has taken and not yet answered.
    preparing_slide: Option<WallpaperSource>,
    /// The prepared slideshow picture waiting for the serve loop.
    slide_done: Option<Result<Surface, String>>,
    /// Set once the embedder is tearing down, so a parked preparer leaves.
    stopping: bool,
}

impl WallpaperDesk {
    /// A desk with nothing wanted and nothing prepared.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer the desktop's request for `source`, recording it if this desk does
    /// not already hold the answer.
    ///
    /// [`Prepared::Ready`] once — the desktop installs the surface, so a later
    /// ask for the same source means it genuinely wants it prepared again. A
    /// surface prepared for a *different* source is dropped as stale rather than
    /// stretched onto the wrong screen.
    ///
    /// A colour-only choice needs nothing prepared and is answered at once, so
    /// the common case never reaches a worker at all.
    pub fn take(&mut self, source: &WallpaperSource) -> Prepared {
        if source.image_path().is_none() {
            self.wanted = None;
            self.done = None;
            return Prepared::Ready {
                surface: None,
                refusal: None,
            };
        }
        match self.done.take() {
            Some((prepared, outcome)) if prepared == *source => {
                return match outcome {
                    Ok(surface) => Prepared::Ready {
                        surface: Some(surface),
                        refusal: None,
                    },
                    Err(refusal) => Prepared::Ready {
                        surface: None,
                        refusal: Some(refusal),
                    },
                };
            }
            Some(_) | None => {}
        }
        if self.wanted.as_ref() != Some(source) {
            self.wanted = Some(source.clone());
        }
        Prepared::Pending
    }

    /// Whether a wallpaper is wanted that no preparer has taken.
    #[must_use]
    pub const fn has_work(&self) -> bool {
        !self.stopping
            && ((self.wanted.is_some() && !self.preparing)
                || self.has_slide_work()
                || self.has_preview_work())
    }

    /// Whether a slideshow picture is wanted that no preparer has taken.
    const fn has_slide_work(&self) -> bool {
        self.wanted_slide.is_some() && self.preparing_slide.is_none()
    }

    /// Whether a preview is wanted that no preparer has taken.
    const fn has_preview_work(&self) -> bool {
        self.wanted_preview.is_some() && self.rendering.is_none()
    }

    /// Take the next thing to prepare, or `None` when there is nothing to
    /// do.
    ///
    /// The desktop's own backdrop is always taken first: it is the picture
    /// the user is looking at, and a gallery of thumbnails must never make
    /// it wait.
    pub fn next_job(&mut self) -> Option<WallpaperJob> {
        if self.stopping {
            return None;
        }
        if self.wanted.is_some() && !self.preparing {
            self.preparing = true;
            return self.wanted.clone().map(WallpaperJob::Backdrop);
        }
        if self.has_slide_work() {
            let source = self.wanted_slide.take()?;
            self.preparing_slide = Some(source.clone());
            return Some(WallpaperJob::Slide(source));
        }
        if !self.has_preview_work() {
            return None;
        }
        let job = self.wanted_preview.take()?;
        self.rendering = Some(job.request.clone());
        Some(WallpaperJob::Preview(job))
    }

    /// Record a wanted preview, answering whether the desk took it.
    ///
    /// `false` is "one is already in flight": the desktop renders one
    /// preview at a time, so a second ask is refused rather than queued —
    /// a bound on how much sandboxed decoding a browsing application can
    /// set going, and the caller asks again once its answer arrives.
    pub fn want_preview(&mut self, job: PreviewJob) -> bool {
        if self.stopping || self.wanted_preview.is_some() || self.rendering.is_some() {
            return false;
        }
        self.wanted_preview = Some(job);
        true
    }

    /// Record the result of rendering a preview, answering whether the desk
    /// kept it (and so owes the serve loop a wake).
    ///
    /// An answer to a request the desk is no longer rendering is dropped:
    /// the asking window has gone, or the desk was stopped under it.
    pub fn deliver_preview(&mut self, done: PreviewDone) -> bool {
        if self.rendering.as_ref() != Some(&done.request) {
            return false;
        }
        self.rendering = None;
        self.preview_done = Some(done);
        true
    }

    /// Take the rendered preview waiting to be handed over, if any.
    pub fn take_preview(&mut self) -> Option<PreviewDone> {
        self.preview_done.take()
    }

    /// Record a wanted slideshow picture, replacing one not yet taken:
    /// only the newest slide is worth showing.
    pub fn want_slide(&mut self, source: WallpaperSource) {
        if !self.stopping {
            self.wanted_slide = Some(source);
        }
    }

    /// Record the result of preparing slide `source`, answering whether the
    /// desk kept it (and so owes the serve loop a wake).
    ///
    /// An answer for a slide no longer being prepared is dropped: the
    /// screensaver went down, or moved on to a newer picture.
    pub fn deliver_slide(
        &mut self,
        source: &WallpaperSource,
        outcome: Result<Surface, String>,
    ) -> bool {
        if self.preparing_slide.as_ref() != Some(source) {
            return false;
        }
        self.preparing_slide = None;
        self.slide_done = Some(outcome);
        true
    }

    /// Take the prepared slideshow picture, if one is waiting.
    pub fn take_slide(&mut self) -> Option<Result<Surface, String>> {
        self.slide_done.take()
    }

    /// Forget every slide wanted, in preparation, or prepared: the
    /// screensaver has gone.
    pub fn forget_slides(&mut self) {
        self.wanted_slide = None;
        self.preparing_slide = None;
        self.slide_done = None;
    }

    /// Record the result of preparing `source`.
    ///
    /// Answers `false` — and keeps nothing — when the desktop has since asked
    /// for something else, so an abandoned preparation owes the session no wake
    /// and its pixels are dropped rather than painted.
    ///
    /// An accepted answer **clears the request it answers**. Leaving it standing
    /// made the desk workable again the instant it was answered, so a preparer
    /// handed itself the same picture forever — a decode loop, and this one
    /// reads and rasterises a whole screen's worth each time round.
    pub fn deliver(&mut self, source: WallpaperSource, outcome: Result<Surface, String>) -> bool {
        self.preparing = false;
        if self.wanted.as_ref() != Some(&source) {
            return false;
        }
        self.wanted = None;
        self.done = Some((source, outcome));
        true
    }

    /// Stop handing out work, so a parked preparer leaves its loop.
    pub fn stop(&mut self) {
        self.stopping = true;
    }

    /// Whether the embedder has asked preparers to leave.
    #[must_use]
    pub const fn stopping(&self) -> bool {
        self.stopping
    }
}

/// The desktop's shipped-wallpaper service as the window channel reaches
/// it: the catalog a browsing application may list, and the render it may
/// ask for.
///
/// A seam because the two answers need the session's own filesystem reach,
/// its parser sandbox, and its shared-memory mapping — none of which the
/// host-testable bridge has — while the rules around them (who owns the
/// catalog, one render at a time, a closed window owes nothing) are policy
/// worth testing without any of it.
pub trait WallpaperService {
    /// The flat catalog of shipped wallpapers this desktop offers, in
    /// catalog order. Empty for a desktop whose store could not be listed.
    fn catalog(&self) -> &[tairix_window::WallpaperName];

    /// Render catalog entry `index` at `side` square into the region
    /// granted as `shm_handle`, concluding to `window_id`.
    ///
    /// Accepting is all this does: the read and the decode happen off the
    /// compositing loop, and the conclusion is delivered later.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotFound`](tairix_abi::Errno::NotFound) — no such catalog
    ///   entry, or the granted handle names no region for this task.
    /// * [`Errno::LengthOutOfRange`](tairix_abi::Errno::LengthOutOfRange) —
    ///   the region is too small for the side asked for.
    /// * [`Errno::AlreadyExists`](tairix_abi::Errno::AlreadyExists) — the
    ///   desktop is already rendering a preview; the caller asks again once
    ///   its answer arrives.
    fn render(
        &mut self,
        window_id: u64,
        shm_handle: u64,
        index: u16,
        side: u16,
    ) -> Result<(), tairix_abi::Errno>;
}

#[cfg(test)]
#[path = "wallpaper_tests.rs"]
mod tests;
