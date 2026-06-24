//! The colour roles a theme defines.
//!
//! A [`Palette`] is a fixed set of *semantic* colour roles, not a free-form
//! map: every role is a named field, so a theme cannot omit one and a
//! consumer cannot ask for a role that does not exist (illegal states are
//! unrepresentable). The window manager, the taskbar,
//! and the default apps all read these same roles, which is what makes a
//! theme switch apply consistently everywhere from one definition.

use crate::color::Rgba;

/// The semantic colours every theme provides.
///
/// Roles are intentionally generic ("surface", "on-surface") rather than
/// per-widget ("button", "menu") so that adding a widget needs no new role
/// and adding a theme needs no new code — a theme is data.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Palette {
    /// The empty desktop behind every window (the compositor root).
    pub desktop: Rgba,
    /// The base fill of a window or app content area.
    pub surface: Rgba,
    /// A raised/alternate fill (the taskbar, menus, headers) that must
    /// read as distinct from [`surface`](Self::surface).
    pub surface_raised: Rgba,
    /// Primary foreground (text, icons) drawn on
    /// [`surface`](Self::surface) and
    /// [`surface_raised`](Self::surface_raised).
    pub on_surface: Rgba,
    /// Secondary foreground for less prominent text on a surface.
    pub on_surface_muted: Rgba,
    /// The accent used for selection, focus, and the active task.
    pub accent: Rgba,
    /// Foreground drawn on top of [`accent`](Self::accent).
    pub on_accent: Rgba,
    /// Window and control borders / separators.
    pub border: Rgba,
}
