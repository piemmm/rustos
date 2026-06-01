//! Geometric theme metrics: the corner radii and line thicknesses that
//! shape the desktop.
//!
//! These are the *data* the window manager's single anti-aliased
//! rounded-corner path consumes (`AGENTS.md` §2.2): the theme says how
//! round a window or the taskbar is, and the compositor rounds it. A
//! radius of `0` means square corners.

/// Corner radii and border thickness, in physical pixels.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Metrics {
    /// Corner radius applied to ordinary top-level windows. `0` is
    /// square.
    pub window_corner_radius: u32,
    /// Corner radius applied to the taskbar, rounded through the same
    /// compositor path as windows (`AGENTS.md` §2.2).
    pub taskbar_corner_radius: u32,
    /// Corner radius applied to transient surfaces (menus, popups,
    /// tooltips).
    pub popup_corner_radius: u32,
    /// Thickness of window and control borders/separators.
    pub border_thickness: u32,
}
