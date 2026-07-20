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
use crate::typography::{FontSpec, FontWeight, Fonts};
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
    #[must_use]
    pub fn dark() -> Self {
        Self::new(
            ThemeId::DARK,
            "TAIRiX Dark",
            Appearance::Dark,
            Palette {
                desktop: Rgba::rgb(0x12, 0x14, 0x18),
                surface: Rgba::rgb(0x1e, 0x22, 0x28),
                surface_raised: Rgba::rgb(0x2a, 0x2f, 0x37),
                on_surface: Rgba::rgb(0xe6, 0xe9, 0xef),
                on_surface_muted: Rgba::rgb(0x9a, 0xa1, 0xad),
                accent: Rgba::rgb(0x4c, 0x8d, 0xff),
                on_accent: Rgba::rgb(0x0b, 0x0d, 0x10),
                border: Rgba::rgb(0x3a, 0x40, 0x49),
                surface_pressed: Rgba::rgb(0x16, 0x19, 0x1e),
                rim: Rgba::rgb(0x44, 0x4b, 0x55),
                rim_active: Rgba::rgb(0x6f, 0xa4, 0xff),
                danger: Rgba::rgb(0xff, 0x5c, 0x5c),
                cpu_pressure: Rgba::rgb(0xf0, 0xa0, 0x30),
                memory_pressure: Rgba::rgb(0xb0, 0x6c, 0xf0),
                disk_pressure: Rgba::rgb(0x30, 0xc0, 0xb0),
                network_activity: Rgba::rgb(0x40, 0xb0, 0xff),
                power_pressure: Rgba::rgb(0x8b, 0xd4, 0x50),
                thermal_pressure: Rgba::rgb(0xff, 0x7a, 0x3c),
                recovery: Rgba::rgb(0xff, 0x6a, 0xb0),
                success: Rgba::rgb(0x4c, 0xd0, 0x7a),
                warning: Rgba::rgb(0xf5, 0xc5, 0x42),
                denied: Rgba::rgb(0xc8, 0x5a, 0x5a),
                scroll_track: Rgba::rgb(0x23, 0x28, 0x30),
                scroll_thumb: Rgba::rgb(0x4a, 0x51, 0x5c),
                frame_active: Rgba::rgb(0x4c, 0x8d, 0xff),
                frame_inactive: Rgba::rgb(0x3a, 0x40, 0x49),
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
    #[must_use]
    pub fn light() -> Self {
        Self::new(
            ThemeId::LIGHT,
            "TAIRiX Light",
            Appearance::Light,
            Palette {
                desktop: Rgba::rgb(0xd9, 0xdd, 0xe3),
                surface: Rgba::rgb(0xf7, 0xf8, 0xfa),
                surface_raised: Rgba::rgb(0xea, 0xec, 0xf0),
                on_surface: Rgba::rgb(0x1a, 0x1d, 0x22),
                on_surface_muted: Rgba::rgb(0x5b, 0x61, 0x6b),
                accent: Rgba::rgb(0x1f, 0x6f, 0xeb),
                on_accent: Rgba::rgb(0xff, 0xff, 0xff),
                border: Rgba::rgb(0xc4, 0xc9, 0xd1),
                surface_pressed: Rgba::rgb(0xdf, 0xe2, 0xe8),
                rim: Rgba::rgb(0xb9, 0xbf, 0xc9),
                rim_active: Rgba::rgb(0x1f, 0x6f, 0xeb),
                danger: Rgba::rgb(0xd8, 0x35, 0x35),
                cpu_pressure: Rgba::rgb(0xc0, 0x7a, 0x00),
                memory_pressure: Rgba::rgb(0x8b, 0x3f, 0xd0),
                disk_pressure: Rgba::rgb(0x0f, 0x8f, 0x80),
                network_activity: Rgba::rgb(0x14, 0x78, 0xd0),
                power_pressure: Rgba::rgb(0x4f, 0x9e, 0x20),
                thermal_pressure: Rgba::rgb(0xd8, 0x5a, 0x1c),
                recovery: Rgba::rgb(0xc8, 0x3a, 0x86),
                success: Rgba::rgb(0x1f, 0x9e, 0x52),
                warning: Rgba::rgb(0xb8, 0x86, 0x0b),
                denied: Rgba::rgb(0xb0, 0x30, 0x30),
                scroll_track: Rgba::rgb(0xe2, 0xe5, 0xea),
                scroll_thumb: Rgba::rgb(0xb0, 0xb6, 0xc0),
                frame_active: Rgba::rgb(0x1f, 0x6f, 0xeb),
                frame_inactive: Rgba::rgb(0xc4, 0xc9, 0xd1),
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
fn common_fonts() -> Fonts {
    Fonts {
        ui: FontSpec::new("TAIRiX Sans", 14, FontWeight::Regular),
        monospace: FontSpec::new("TAIRiX Mono", 14, FontWeight::Regular),
    }
}

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
