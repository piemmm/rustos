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

use tairix_geometry::Rect;
use tairix_raster::Surface;
use tairix_wallpaper::{PinboardSettings, WallpaperChoice, WallpaperFit};

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
    pub fn wanted(settings: &PinboardSettings, screen: Rect) -> Self {
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
        !self.stopping && self.wanted.is_some() && !self.preparing
    }

    /// Take the wallpaper to prepare, or `None` when there is nothing to do.
    pub fn next_job(&mut self) -> Option<WallpaperSource> {
        if !self.has_work() {
            return None;
        }
        self.preparing = true;
        self.wanted.clone()
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

#[cfg(test)]
#[path = "wallpaper_tests.rs"]
mod tests;
