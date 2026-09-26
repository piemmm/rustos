//! Unit tests for the settings categories' colour badges.

extern crate std;

use tairix_raster::{Color, Pixel, Surface};

use super::BadgeHue;
use crate::artwork::{builtin_picture, glyph_mask, IconPicture};
use crate::glyph::IconKind;
use crate::load::ICON_KINDS;
use crate::symbol::marks;

/// The sides a badge is drawn at on the desktop: every sidebar size from a
/// compact density through a 300% scale, and the sizes between.
const SIDES: core::ops::RangeInclusive<u32> = 12..=66;

/// The categories whose symbols are mirror images of themselves.
const SYMMETRIC: [IconKind; 9] = [
    IconKind::Settings,
    IconKind::Display,
    IconKind::LockScreen,
    IconKind::Power,
    IconKind::Notifications,
    IconKind::Mouse,
    IconKind::Trackpad,
    IconKind::Touchscreen,
    IconKind::Accessibility,
];

fn picture(kind: IconKind, side: u32) -> Surface {
    builtin_picture(kind, side).unwrap_or_else(|| panic!("{kind:?} draws at {side}"))
}

fn pixel(surface: &Surface, x: u32, y: u32) -> Pixel {
    surface.get(x, y).expect("inside the surface")
}

#[test]
fn every_settings_category_is_a_badge_with_its_own_symbol() {
    let badges: alloc::vec::Vec<IconKind> = ICON_KINDS
        .into_iter()
        .filter(|kind| kind.badge().is_some())
        .collect();
    assert_eq!(badges.len(), 21, "one badge per settings category");
    for kind in ICON_KINDS {
        assert_eq!(
            kind.badge().is_some(),
            marks(kind).is_some(),
            "{kind:?}: a badge and its symbol come together"
        );
        assert_eq!(
            IconPicture::builtin(kind, &Surface::new(1, 1).expect("a surface"))
                .artwork()
                .is_some(),
            kind.badge().is_some(),
            "{kind:?}: only a badge is ready-coloured"
        );
    }
}

#[test]
fn a_badge_draws_at_every_side_and_nothing_at_zero() {
    for kind in ICON_KINDS.into_iter().filter(|kind| kind.badge().is_some()) {
        assert!(builtin_picture(kind, 0).is_none(), "{kind:?}");
        for side in [1, 2, 7, 22, 44, 128] {
            let drawn = picture(kind, side);
            assert_eq!((drawn.width(), drawn.height()), (side, side), "{kind:?}");
        }
    }
}

/// The plate spans the whole slot, so its flat edges fall on the slot's own
/// pixel boundaries at every side: the middle of each edge is solid plate,
/// never a half-covered pixel.
#[test]
fn a_badge_plate_meets_the_pixel_grid_at_every_side() {
    for kind in ICON_KINDS.into_iter().filter(|kind| kind.badge().is_some()) {
        for side in SIDES {
            let drawn = picture(kind, side);
            let (mid, last) = (side / 2, side - 1);
            for (x, y) in [(mid, 0), (mid, last), (0, mid), (last, mid)] {
                assert_eq!(pixel(&drawn, x, y).a, 255, "{kind:?} at {side}: {x},{y}");
            }
        }
    }
}

/// A symmetric shape whose two edges land on different sub-pixel phases
/// renders one edge solid and its mirror grey. Centred vector art drawn at the
/// exact side cannot, at any side.
#[test]
fn a_symmetric_badge_renders_mirror_symmetric_at_every_side() {
    for kind in SYMMETRIC {
        for side in SIDES {
            let drawn = picture(kind, side);
            for y in 0..side {
                for x in 0..side / 2 {
                    let left = pixel(&drawn, x, y);
                    let right = pixel(&drawn, side - 1 - x, y);
                    for (l, r) in [
                        (left.r, right.r),
                        (left.g, right.g),
                        (left.b, right.b),
                        (left.a, right.a),
                    ] {
                        assert!(
                            l.abs_diff(r) <= 6,
                            "{kind:?} at {side}: {x},{y} is {left:?}, its mirror {right:?}"
                        );
                    }
                }
            }
        }
    }
}

/// A category's glyph — what a control drawing only in its own colour shows —
/// is the symbol alone, never a tinted plate.
#[test]
fn a_badge_kinds_mask_is_its_symbol() {
    for kind in ICON_KINDS.into_iter().filter(|kind| kind.badge().is_some()) {
        let badge = picture(kind, 44);
        let mask = glyph_mask(kind, 44).expect("a mask");
        assert_eq!(
            pixel(&badge, 22, 0).a,
            255,
            "{kind:?}: the plate reaches its edge"
        );
        assert_eq!(
            pixel(&mask, 0, 0).a,
            0,
            "{kind:?}: a symbol leaves its corner clear"
        );
        assert!(
            mask.pixels().iter().any(|p| p.a == 255),
            "{kind:?}: the symbol draws solid somewhere"
        );
    }
}

/// A channel's linear-light share, as WCAG 2.1 defines relative luminance.
fn channel_luminance(value: u8) -> f64 {
    let c = f64::from(value) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(color: Color) -> f64 {
    0.2126 * channel_luminance(color.r)
        + 0.7152 * channel_luminance(color.g)
        + 0.0722 * channel_luminance(color.b)
}

/// The white symbol stands at least 3:1 clear of its plate where the plate is
/// at its middle shade, which is the contrast a non-text mark needs.
#[test]
fn every_symbol_stands_clear_of_its_plate() {
    let hues = [
        BadgeHue::Grey,
        BadgeHue::Graphite,
        BadgeHue::Blue,
        BadgeHue::Sky,
        BadgeHue::Indigo,
        BadgeHue::Purple,
        BadgeHue::Teal,
        BadgeHue::Green,
        BadgeHue::Pink,
        BadgeHue::Red,
    ];
    for hue in hues {
        let (top, bottom) = hue.ramp();
        let mid = Color::rgb(
            top.r.midpoint(bottom.r),
            top.g.midpoint(bottom.g),
            top.b.midpoint(bottom.b),
        );
        let ratio = (1.0 + 0.05) / (luminance(mid) + 0.05);
        assert!(
            ratio >= 3.0,
            "{hue:?}: white on {mid:?} is only {ratio:.2}:1"
        );
    }
}

/// Kin categories share a hue and the rest stand apart, so a reader who knows
/// one grey badge is a device knows them all.
#[test]
fn a_category_wears_its_kins_hue() {
    for kind in [
        IconKind::Keyboard,
        IconKind::Mouse,
        IconKind::Trackpad,
        IconKind::Touchscreen,
        IconKind::Printer,
    ] {
        assert_eq!(kind.badge(), Some(BadgeHue::Grey), "{kind:?}");
    }
    assert_eq!(IconKind::Notifications.badge(), Some(BadgeHue::Red));
    assert_eq!(
        IconKind::Network.badge(),
        None,
        "the tray's reading is a glyph"
    );
    assert_eq!(IconKind::Folder.badge(), None);
}
