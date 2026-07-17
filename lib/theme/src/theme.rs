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
}

impl Theme {
    /// Assemble a theme from its parts.
    #[must_use]
    pub fn new(
        id: ThemeId,
        name: impl Into<String>,
        appearance: Appearance,
        palette: Palette,
        metrics: Metrics,
        fonts: Fonts,
        cursors: CursorSet,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            appearance,
            palette,
            metrics,
            fonts,
            cursors,
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
            },
            common_metrics(),
            common_fonts(),
            common_cursors(),
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
            },
            common_metrics(),
            common_fonts(),
            common_cursors(),
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
    }
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
