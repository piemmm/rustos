//! The app-side view of the desktop a window is displayed on.
//!
//! [`Desktop`] is what an application holds after asking the session
//! ([`WindowClient::desktop`](crate::WindowClient::desktop)): the screen
//! extent as geometry, the UI scale as a [`Scale`] rather than a bare
//! percentage, and the active [`Appearance`]. Feeding it every delivered
//! event keeps it current, so an application that follows a screen-mode
//! change or a light/dark switch does not repeat the same bookkeeping in
//! its own source.
//!
//! It holds only description — no capability, no handle, nothing another
//! principal owns — and every value in it is already validated: a
//! [`Desktop`] cannot exist with a zero-sized screen or a scale outside
//! the range [`Scale`] admits.

use tairix_abi::desktop::{Appearance, DesktopInfo};
use tairix_abi::window_ipc::WindowEvent;
use tairix_abi::Errno;
use tairix_geometry::{Rect, Scale};

/// The desktop an application's windows are displayed on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Desktop {
    info: DesktopInfo,
    scale: Scale,
}

impl Desktop {
    /// Adopt what the session reported.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if the reported scale is not one [`Scale`]
    /// admits. The percentage is refused rather than clamped: silently
    /// drawing at a density the session did not ask for would misplace
    /// every hit-test in the window.
    pub fn new(info: DesktopInfo) -> Result<Self, Errno> {
        let scale =
            Scale::from_percent(u32::from(info.scale_percent())).ok_or(Errno::OutOfRange)?;
        Ok(Self { info, scale })
    }

    /// The screen's width in physical pixels; never zero.
    #[must_use]
    pub const fn screen_width_px(&self) -> u32 {
        self.info.screen_width_px()
    }

    /// The screen's height in physical pixels; never zero.
    #[must_use]
    pub const fn screen_height_px(&self) -> u32 {
        self.info.screen_height_px()
    }

    /// The screen as a rectangle at the origin — the shape a preview or a
    /// wallpaper placement models.
    #[must_use]
    pub const fn screen(&self) -> Rect {
        Rect::new(0, 0, self.screen_width_px(), self.screen_height_px())
    }

    /// The desktop UI scale every logical length is resolved through.
    #[must_use]
    pub const fn scale(&self) -> Scale {
        self.scale
    }

    /// Which way round the active theme's colours run.
    #[must_use]
    pub const fn appearance(&self) -> Appearance {
        self.info.appearance()
    }

    /// The record exactly as the session reported it.
    #[must_use]
    pub const fn info(&self) -> DesktopInfo {
        self.info
    }

    /// Adopt what `event` reports, answering whether anything changed.
    ///
    /// Any other event answers `false`, so an application can hand its
    /// whole event stream through without first sorting it.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a change this crate cannot represent (see
    /// [`Self::new`]). The desktop keeps the last state it accepted, so a
    /// refused change leaves the window drawing correctly rather than at a
    /// nonsense density; the application reports the refusal.
    pub fn apply(&mut self, event: &WindowEvent) -> Result<bool, Errno> {
        let WindowEvent::DesktopChanged { desktop, .. } = *event else {
            return Ok(false);
        };
        if desktop == self.info {
            return Ok(false);
        }
        *self = Self::new(desktop)?;
        Ok(true)
    }

    /// The physical size to open a window of `logical_width` ×
    /// `logical_height` at.
    ///
    /// An application authors its preferred window size in *logical*
    /// pixels at the reference density, as every desktop length is, so
    /// two steps stand between that preference and a size the session can
    /// be asked for: resolve it at the desktop's density, then cap it to
    /// the screen so a window can never open larger than the display it
    /// must appear on. Both belong to every windowed application, so they
    /// are one call rather than a pair each app repeats.
    ///
    /// The cap is the screen itself, not a work area: how much of it the
    /// window furniture and the taskbar claim is the window manager's
    /// business, and it places the window accordingly. The result never
    /// exceeds the screen, and a request of nothing stays nothing — this
    /// only ever shrinks what the application asked for.
    #[must_use]
    pub fn window_size(&self, logical_width: u32, logical_height: u32) -> (u32, u32) {
        (
            self.scale
                .scale_length(logical_width)
                .min(self.screen_width_px()),
            self.scale
                .scale_length(logical_height)
                .min(self.screen_height_px()),
        )
    }
}
