//! Geometric theme metrics: the corner radii and line thicknesses that
//! shape the desktop.
//!
//! These are the *data* the window manager's single anti-aliased
//! rounded-corner path consumes: the theme says how
//! round a window or the taskbar is, and the compositor rounds it. A
//! radius of `0` means square corners.
//!
//! Every length here is in *logical* pixels at the reference density
//! (`rustos_geometry::REFERENCE_DPI`). The desktop's DPI / UI scale
//! (`rustos_geometry::Scale`) converts them to physical pixels at render
//! time, so the same theme stays a comfortable physical size across panel
//! densities.

/// Corner radii and border thickness, in logical pixels at the reference
/// density (scaled to physical pixels by `rustos_geometry::Scale`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Metrics {
    /// Corner radius applied to ordinary top-level windows, in logical
    /// pixels. `0` is square.
    pub window_corner_radius: u32,
    /// Corner radius applied to the taskbar, rounded through the same
    /// compositor path as windows.
    pub taskbar_corner_radius: u32,
    /// Corner radius applied to transient surfaces (menus, popups,
    /// tooltips).
    pub popup_corner_radius: u32,
    /// Thickness of window and control borders/separators.
    pub border_thickness: u32,
}
