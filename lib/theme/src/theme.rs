//! A complete theme and the two built-in themes.
//!
//! A [`Theme`] bundles one [`Palette`], one set of [`Metrics`], one set of
//! [`Fonts`], and one [`CursorSet`] under a stable [`ThemeId`]. The charter
//! requires a default dark theme and a light theme switchable at runtime,
//! and that "adding a theme is data, not new code": a new
//! theme is just another [`Theme`] value registered with the
//! [`ThemeRegistry`](crate::ThemeRegistry).

use alloc::string::String;

use crate::cursor::CursorSet;
use crate::metrics::Metrics;
use crate::motion::{Contrast, Density, MotionTheme};
use crate::palette::Palette;
use crate::typography::{FamilyKey, Fonts};
use crate::Rgba;

/// A stable identifier for a theme.
///
/// The two built-in themes have reserved ids ([`ThemeId::DARK`] and
/// [`ThemeId::LIGHT`]); custom themes pick any other value.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ThemeId(pub u32);

impl ThemeId {
    /// The id of the built-in dark theme (the default).
    pub const DARK: Self = Self(1);
    /// The id of the built-in light theme.
    pub const LIGHT: Self = Self(2);
}

/// Whether a theme is dark-on-light or light-on-dark.
///
/// This is the axis a "switch to light/dark" control toggles; it is
/// independent of the concrete palette so a custom theme can declare which
/// side it belongs to.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Appearance {
    /// Light foreground on dark surfaces.
    Dark,
    /// Dark foreground on light surfaces.
    Light,
}

/// A complete, named theme: colours, metrics, fonts, and cursors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Theme {
    id: ThemeId,
    name: String,
    appearance: Appearance,
    palette: Palette,
    metrics: Metrics,
    fonts: Fonts,
    cursors: CursorSet,
    motion: MotionTheme,
    density: Density,
    contrast: Contrast,
}

impl Theme {
    /// Assemble a theme from its parts.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ThemeId,
        name: impl Into<String>,
        appearance: Appearance,
        palette: Palette,
        metrics: Metrics,
        fonts: Fonts,
        cursors: CursorSet,
        motion: MotionTheme,
        density: Density,
        contrast: Contrast,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            appearance,
            palette,
            metrics,
            fonts,
            cursors,
            motion,
            density,
            contrast,
        }
    }

    /// The theme's stable identifier.
    #[must_use]
    pub fn id(&self) -> ThemeId {
        self.id
    }

    /// The theme's human-readable name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the theme is [`Appearance::Dark`] or [`Appearance::Light`].
    #[must_use]
    pub fn appearance(&self) -> Appearance {
        self.appearance
    }

    /// The theme's colour roles.
    #[must_use]
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// The theme's geometric metrics.
    #[must_use]
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// The theme's fonts.
    #[must_use]
    pub fn fonts(&self) -> &Fonts {
        &self.fonts
    }

    /// The theme's cursors.
    #[must_use]
    pub fn cursors(&self) -> &CursorSet {
        &self.cursors
    }

    /// The theme's motion timings and reduced-motion policy.
    #[must_use]
    pub fn motion(&self) -> MotionTheme {
        self.motion
    }

    /// The theme's information density.
    #[must_use]
    pub fn density(&self) -> Density {
        self.density
    }

    /// The theme's contrast policy.
    #[must_use]
    pub fn contrast(&self) -> Contrast {
        self.contrast
    }

    /// The built-in **dark** theme — TAIRiX's default.
    ///
    /// The tokens are the Reactive Alloy design boards (`plans/desktop1.png`,
    /// `plans/desktop2a.png`) measured rather than invented: near-black cool
    /// surfaces, one alloy-orange accent family (a burnt-orange plate fill
    /// under a bright signal edge), the semantic signal hues the boards' own
    /// legend fixes, and a hover plate one measured step above the bar fill —
    /// the boards draw a hovered icon on the taskbar as a bare wash roughly
    /// nine levels lighter than the bar behind it.
    #[must_use]
    pub fn dark() -> Self {
        Self::new(
            ThemeId::DARK,
            "TAIRiX Dark",
            Appearance::Dark,
            Palette {
                desktop: Rgba::rgb(0x0b, 0x0e, 0x10),
                surface: Rgba::rgb(0x0f, 0x13, 0x16),
                surface_raised: Rgba::rgb(0x15, 0x1b, 0x1f),
                on_surface: Rgba::rgb(0xe8, 0xeb, 0xed),
                on_surface_muted: Rgba::rgb(0x8e, 0x97, 0x9c),
                accent: Rgba::rgb(0xd1, 0x55, 0x0f),
                on_accent: ON_ACCENT,
                border: Rgba::rgb(0x1c, 0x23, 0x27),
                surface_hover: Rgba::rgb(0x1e, 0x25, 0x2a),
                surface_pressed: Rgba::rgb(0x0b, 0x0f, 0x11),
                rim: Rgba::rgb(0x23, 0x2b, 0x30),
                rim_active: Rgba::rgb(0xff, 0x60, 0x00),
                danger: Rgba::rgb(0xe4, 0x1d, 0x21),
                cpu_pressure: Rgba::rgb(0xf7, 0x62, 0x02),
                memory_pressure: Rgba::rgb(0x8b, 0x43, 0xd6),
                disk_pressure: Rgba::rgb(0xf8, 0xa3, 0x30),
                network_activity: Rgba::rgb(0x0b, 0x88, 0xd9),
                power_pressure: Rgba::rgb(0xa8, 0xd8, 0x4f),
                thermal_pressure: Rgba::rgb(0xff, 0x8a, 0x5c),
                recovery: Rgba::rgb(0xe8, 0x48, 0x4c),
                success: Rgba::rgb(0x6f, 0xb2, 0x3a),
                warning: Rgba::rgb(0xe8, 0xb1, 0x3a),
                denied: Rgba::rgb(0xb0, 0x3a, 0x3d),
                scroll_track: Rgba::rgb(0x13, 0x1a, 0x1d),
                scroll_thumb: Rgba::rgb(0x2f, 0x3a, 0x3f),
                frame: Rgba::rgb(0x4b, 0x52, 0x57),
            },
            common_metrics(),
            common_fonts(),
            common_cursors(),
            common_motion(),
            Density::Normal,
            Contrast::Normal,
        )
    }

    /// The built-in **light** theme.
    ///
    /// The light board (`plans/desktop1-light.png`) keeps the dark variant's
    /// alloy-orange accent family and semantic vocabulary and re-tunes it for
    /// warm off-white surfaces: every signal hue is darkened until it carries
    /// on paper-white, and the accent deepens to the burnt end of the family
    /// so orange-on-white text and rims stay legible.
    #[must_use]
    pub fn light() -> Self {
        Self::new(
            ThemeId::LIGHT,
            "TAIRiX Light",
            Appearance::Light,
            Palette {
                desktop: Rgba::rgb(0xe8, 0xe3, 0xdd),
                surface: Rgba::rgb(0xfd, 0xfc, 0xfa),
                surface_raised: Rgba::rgb(0xf4, 0xf0, 0xec),
                on_surface: Rgba::rgb(0x1b, 0x1d, 0x20),
                on_surface_muted: Rgba::rgb(0x6a, 0x6f, 0x75),
                accent: Rgba::rgb(0xc8, 0x50, 0x0c),
                on_accent: ON_ACCENT,
                border: Rgba::rgb(0xdd, 0xd6, 0xce),
                surface_hover: Rgba::rgb(0xee, 0xea, 0xe5),
                surface_pressed: Rgba::rgb(0xe9, 0xe4, 0xde),
                rim: Rgba::rgb(0xcf, 0xc7, 0xbf),
                rim_active: Rgba::rgb(0xd2, 0x54, 0x0b),
                danger: Rgba::rgb(0xc4, 0x16, 0x1a),
                cpu_pressure: Rgba::rgb(0xd8, 0x54, 0x0a),
                memory_pressure: Rgba::rgb(0x74, 0x33, 0xbd),
                disk_pressure: Rgba::rgb(0xc4, 0x7a, 0x12),
                network_activity: Rgba::rgb(0x0b, 0x6f, 0xb0),
                power_pressure: Rgba::rgb(0x64, 0x8f, 0x1e),
                thermal_pressure: Rgba::rgb(0xc2, 0x50, 0x1a),
                recovery: Rgba::rgb(0xc9, 0x33, 0x37),
                success: Rgba::rgb(0x4b, 0x7d, 0x22),
                warning: Rgba::rgb(0xa9, 0x74, 0x1a),
                denied: Rgba::rgb(0x8f, 0x25, 0x28),
                scroll_track: Rgba::rgb(0xec, 0xe7, 0xe1),
                scroll_thumb: Rgba::rgb(0xbd, 0xb5, 0xac),
                frame: Rgba::rgb(0xc2, 0xbb, 0xb4),
            },
            common_metrics(),
            common_fonts(),
            common_cursors(),
            common_motion(),
            Density::Normal,
            Contrast::Normal,
        )
    }
}

/// The foreground both built-in themes draw on an accent fill.
///
/// A primary action is one treatment in the design boards — a warm white
/// label on the alloy-orange plate — and it reads identically on either
/// appearance, so the two variants share the token instead of restating it.
const ON_ACCENT: Rgba = Rgba::rgb(0xff, 0xf5, 0xee);

/// The metrics shared by both built-in themes. Corner radii and border
/// thickness are an appearance-independent house style, so the dark and
/// light themes share them rather than restate identical numbers.
fn common_metrics() -> Metrics {
    Metrics {
        window_corner_radius: 8,
        taskbar_corner_radius: 12,
        popup_corner_radius: 6,
        border_thickness: 1,
        scrollbar_breadth: 14,
        min_thumb_length: 24,
        control_height: 28,
        control_inset: 10,
        control_gap: 8,
        control_corner_radius: 6,
        seam_thickness: 2,
        rail_thickness: 3,
        bead_size: 8,
        measured_thickness: 4,
        progress_thickness: 6,
        chart_height: 40,
        selector_extent: 16,
        toggle_track_length: 28,
        title_bar_height: 28,
        frame_inset: 1,
        window_control_extent: 20,
        resize_grabber_extent: 16,
        hit_slop: 4,
    }
}

/// The motion timings shared by both built-in themes, tuned to the middle of
/// the spec §9 targets. Reduced motion is derived from this by a consumer
/// (or a variant theme) via [`MotionTheme::with_reduced_motion`].
fn common_motion() -> MotionTheme {
    MotionTheme::new(
        100, // hover enter
        95,  // hover exit
        75,  // press compress
        110, // release settle
        210, // panel open
        150, // menu open
        150, // job progress pulse
        220, // recovery latch reveal
        115, // window activate
        200, // window size transition
        95,  // scrollbar wake
    )
}

/// The fonts shared by both built-in themes.
///
/// One authored base size drives the whole role ladder, and both appearances
/// use the same type — the boards change colour between light and dark, never
/// the type scale. The base is measured from the reference boards, where body
/// text fills a little under two thirds of a control's height.
fn common_fonts() -> Fonts {
    Fonts::ladder(UI_FAMILY, MONOSPACE_FAMILY, BASE_TEXT_SIZE_PX)
}

/// The logical-pixel body size both built-in themes author their ladder at.
const BASE_TEXT_SIZE_PX: u16 = 18;

/// The proportional family the shipped themes draw interface text in: the
/// humanist sans the design boards are set in, installed as `/System/Fonts`
/// `inter`. A user's own choice replaces it through
/// [`Fonts::with_ui_family`](crate::Fonts::with_ui_family).
///
/// A spelling the key grammar refuses would leave the desktop with no UI
/// family at all, so the fallback is the fixed-pitch family every image
/// ships; the crate's tests assert the shipped spelling resolves.
const UI_FAMILY: FamilyKey = match FamilyKey::new("inter") {
    Ok(key) => key,
    Err(_) => FamilyKey::MONO,
};

/// The fixed-pitch family the shipped themes draw terminal and code text in.
const MONOSPACE_FAMILY: FamilyKey = FamilyKey::MONO;

/// The cursor set shared by both built-in themes.
fn common_cursors() -> CursorSet {
    CursorSet {
        arrow: String::from("cursor.arrow"),
        text: String::from("cursor.text"),
        pointer: String::from("cursor.pointer"),
        move_: String::from("cursor.move"),
        busy: String::from("cursor.busy"),
    }
}
