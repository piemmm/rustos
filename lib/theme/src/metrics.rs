//! Geometric theme metrics: the corner radii and line thicknesses that
//! shape the desktop.
//!
//! These are the *data* the window manager's single anti-aliased
//! rounded-corner path consumes: the theme says how
//! round a window or the taskbar is, and the compositor rounds it. A
//! radius of `0` means square corners.
//!
//! Every length here is in *logical* pixels at the reference density
//! (`tairix_geometry::REFERENCE_DPI`). The desktop's DPI / UI scale
//! (`tairix_geometry::Scale`) converts them to physical pixels at render
//! time, so the same theme stays a comfortable physical size across panel
//! densities.

/// Corner radii and border thickness, in logical pixels at the reference
/// density (scaled to physical pixels by `tairix_geometry::Scale`).
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
    /// Breadth (the short dimension) of a scrollbar's Scroll Channel — a
    /// vertical bar's width, a horizontal bar's height — in logical pixels.
    /// The window manager reserves a gutter of this breadth for a root
    /// viewport's bars, and it also sizes the square scroll corner at their
    /// junction. The scrollbar's long dimension is the track it runs along,
    /// so only the breadth is a metric.
    pub scrollbar_breadth: u32,
    /// The shortest a scrollbar thumb may be drawn, in logical pixels, so the
    /// thumb stays a grabbable target even when the viewport shows a tiny
    /// fraction of a very large content. The shared scroll geometry engine
    /// floors the proportional thumb length at this value (bounded by the
    /// track).
    pub min_thumb_length: u32,
}
