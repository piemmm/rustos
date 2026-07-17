//! The closed set of built-in status/notification icon glyphs.
//!
//! [`IconKind`] is the vocabulary of glyphs the taskbar's notification area
//! draws. A theme asset id resolves to a kind through [`IconKind::for_asset`];
//! an unrecognised id falls back to [`IconKind::Generic`] rather than failing,
//! so an unknown notification still shows a placeholder dot instead of nothing. [`builtin_icon`] turns a kind plus a single theme colour
//! into a [`VectorIcon`]; the glyphs are monochrome silhouettes tinted by the
//! caller, so re-theming is data, not new code.

use alloc::vec;

use tairix_raster::Color;

use crate::vector::{IconLayer, VectorIcon};

/// The design-grid side every built-in glyph is authored on.
const DESIGN: u32 = 24;

/// A status/notification glyph the taskbar can draw.
///
/// A closed set: adding a glyph is a new variant plus its coordinate table,
/// never an open-ended string lookup at the draw site.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum IconKind {
    /// Network / signal-strength bars.
    Network,
    /// A speaker, for volume / audio status.
    Volume,
    /// A battery body, for power status.
    Battery,
    /// A bell, for pending notifications.
    Bell,
    /// The fallback glyph for an unrecognised asset id: a filled diamond.
    Generic,
}

impl IconKind {
    /// Resolve a theme asset identifier to a glyph, falling back to
    /// [`Generic`](Self::Generic) for an unknown id so an unexpected
    /// notification still draws a placeholder.
    #[must_use]
    pub fn for_asset(asset: &str) -> Self {
        match asset {
            "network" => Self::Network,
            "volume" => Self::Volume,
            "battery" => Self::Battery,
            "bell" => Self::Bell,
            _ => Self::Generic,
        }
    }

    /// The canonical asset identifier for this kind — the inverse of
    /// [`for_asset`](Self::for_asset).
    ///
    /// A desktop loader names a kind's on-disk SVG asset by this id, so the
    /// id↔kind mapping lives in one place rather than being restated at the
    /// load site. The round trip holds for every kind:
    /// `IconKind::for_asset(kind.asset_id()) == kind`.
    #[must_use]
    pub fn asset_id(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Volume => "volume",
            Self::Battery => "battery",
            Self::Bell => "bell",
            Self::Generic => "generic",
        }
    }
}

/// Build the built-in glyph for `kind`, tinted with `color`.
///
/// The returned [`VectorIcon`] is authored on a fixed square design grid; the
/// caller rasterises it to whatever pixel size the notification slot needs.
#[must_use]
pub fn builtin_icon(kind: IconKind, color: Color) -> VectorIcon {
    let layers = match kind {
        IconKind::Network => network(color),
        IconKind::Volume => volume(color),
        IconKind::Battery => battery(color),
        IconKind::Bell => bell(color),
        IconKind::Generic => generic(color),
    };
    VectorIcon::new(DESIGN, layers)
}

/// Three rising signal bars.
fn network(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SHORT: &[(i32, i32)] = &[(3, 15), (7, 15), (7, 20), (3, 20)];
    const MID: &[(i32, i32)] = &[(10, 10), (14, 10), (14, 20), (10, 20)];
    const TALL: &[(i32, i32)] = &[(17, 5), (21, 5), (21, 20), (17, 20)];
    vec![
        IconLayer::from_points(color, SHORT),
        IconLayer::from_points(color, MID),
        IconLayer::from_points(color, TALL),
    ]
}

/// A speaker cone (a rectangle joined to a triangular horn).
fn volume(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SPEAKER: &[(i32, i32)] = &[(3, 9), (7, 9), (12, 4), (12, 20), (7, 15), (3, 15)];
    vec![IconLayer::from_points(color, SPEAKER)]
}

/// A battery body with a small terminal nub on the right.
fn battery(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BODY: &[(i32, i32)] = &[(3, 8), (18, 8), (18, 17), (3, 17)];
    const TERMINAL: &[(i32, i32)] = &[(18, 11), (21, 11), (21, 14), (18, 14)];
    vec![
        IconLayer::from_points(color, BODY),
        IconLayer::from_points(color, TERMINAL),
    ]
}

/// A bell with a clapper beneath it.
fn bell(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BODY: &[(i32, i32)] = &[
        (12, 2),
        (16, 5),
        (17, 15),
        (20, 18),
        (4, 18),
        (7, 15),
        (8, 5),
    ];
    const CLAPPER: &[(i32, i32)] = &[(10, 18), (14, 18), (12, 22)];
    vec![
        IconLayer::from_points(color, BODY),
        IconLayer::from_points(color, CLAPPER),
    ]
}

/// The fallback placeholder: a filled diamond.
fn generic(color: Color) -> alloc::vec::Vec<IconLayer> {
    const DIAMOND: &[(i32, i32)] = &[(12, 4), (20, 12), (12, 20), (4, 12)];
    vec![IconLayer::from_points(color, DIAMOND)]
}
