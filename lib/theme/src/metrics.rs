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

    // --- Reactive Alloy control metrics ---------------------------------
    //
    // The logical extents a control's anatomy is laid out from. Every
    // Reactive Alloy control resolves its size from these rather than
    // carrying a private constant, so a density change is data.
    /// The standard interactive height of a control (button, field, menu
    /// row), in logical pixels. Also the minimum interactive target height.
    pub control_height: u32,
    /// The padding between a control's edge and its content group, in logical
    /// pixels.
    pub control_inset: u32,
    /// The gap between adjacent controls in a group or toolbar, in logical
    /// pixels.
    pub control_gap: u32,
    /// The corner radius of an ordinary control plate (the Alloy Plate), in
    /// logical pixels. `0` is square.
    pub control_corner_radius: u32,
    /// The blur radius of the halo behind a selected item
    /// ([`Palette::selection_glow`](crate::Palette::selection_glow)), in
    /// logical pixels. `0` leaves the halo crisp.
    ///
    /// Thirty percent of
    /// [`WINDOW_BACKDROP_BLUR_MAX_PX`](tairix_abi::window_ipc::WINDOW_BACKDROP_BLUR_MAX_PX),
    /// the widest backdrop blur a window may ask the compositor for (64
    /// logical pixels): a selection halo is a soft mark on one item, not the
    /// frosted glass a whole window sits on, so it takes a fraction of the
    /// strongest blur the desktop draws and stays recognisably the same
    /// effect.
    pub selection_glow_blur: u32,
    /// The thickness of a Heat Seam (an activity/progress line on an edge),
    /// in logical pixels.
    pub seam_thickness: u32,
    /// The thickness of a Pressure Rail (a side resource-pressure indicator),
    /// in logical pixels.
    pub rail_thickness: u32,
    /// The diameter of a Signal Bead (a compact count/alert lamp), in logical
    /// pixels.
    pub bead_size: u32,
    /// The breadth (short dimension) of a *measured* value track the user
    /// drives — a slider's groove — in logical pixels.
    ///
    /// A measured track is an instrument line, not a plate: it is deliberately
    /// much thinner than [`control_height`](Self::control_height) and is
    /// centred within whatever row the owner lays it out in, so a slider never
    /// reads as a button-sized block.
    pub measured_thickness: u32,
    /// The breadth (short dimension) of a progress trace's bar, in logical
    /// pixels.
    ///
    /// A progress bar is read, never dragged, so it carries a little more
    /// breadth than the [`measured_thickness`](Self::measured_thickness)
    /// groove a slider's thumb rides in: the fill has to be legible at a
    /// glance across a long run without a thumb to mark it. It stays an
    /// instrument line well under [`control_height`](Self::control_height).
    pub progress_thickness: u32,
    /// The height of a history chart's plot box, in logical pixels.
    ///
    /// A chart is the one measured instrument that is a *box* rather than a
    /// line: a trend has to rise and fall to be read, so it needs vertical
    /// room that a
    /// [`progress_thickness`](Self::progress_thickness) track cannot give it.
    /// It is therefore several times that breadth — a series confined to an
    /// instrument groove cannot rise more than a pixel or two whatever its
    /// values are, which is a graph that cannot report its own data.
    pub chart_height: u32,
    /// The square extent of a boolean selector's glyph — a checkbox box, a
    /// radio circle — and the breadth of a toggle's track, in logical pixels.
    ///
    /// Smaller than [`control_height`](Self::control_height): the glyph is a
    /// compact mark centred in the control's row beside its label, so the row
    /// keeps a full-height hit target while the mark stays small.
    pub selector_extent: u32,
    /// The length (long dimension) of a toggle's track, in logical pixels.
    /// Together with [`selector_extent`](Self::selector_extent) this fixes the
    /// pill's proportions from theme data rather than a ratio buried in the
    /// renderer.
    pub toggle_track_length: u32,

    // --- Window-furniture metrics ---------------------------------------
    /// The height of a window title bar, in logical pixels.
    pub title_bar_height: u32,
    /// The inset of the client viewport from the outer frame edge, in logical
    /// pixels.
    pub frame_inset: u32,
    /// The square extent of one window-control furniture button (close,
    /// minimize, …), in logical pixels.
    pub window_control_extent: u32,
    /// The square extent of the resize grabber's visible affordance, in
    /// logical pixels.
    pub resize_grabber_extent: u32,
    /// The invisible slop added around a furniture hit target so it stays
    /// grabbable, in logical pixels. Never extends over another control. On a
    /// resizable window's frame this is also how far the resize-edge hit zone
    /// reaches into the client's own outer pixels, trading a few unclickable
    /// app pixels for a border that costs no visible space.
    pub hit_slop: u32,
}
