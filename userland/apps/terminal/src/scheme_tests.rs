//! Unit tests for the terminal's colour schemes.

use alloc::string::String;

use tairix_theme::Theme;
use tairix_vt::{Attributes, BasicColor, Color as VtColor};

use super::{ColorScheme, Painted, Rgb, Scheme, ANSI_COLORS};

// --- Rgb::from_hex / write_hex --------------------------------------------

#[test]
fn from_hex_accepts_lowercase_and_uppercase_six_digit_hex() {
    assert_eq!(Rgb::from_hex("00ff80"), Some(Rgb::new(0x00, 0xff, 0x80)));
    assert_eq!(Rgb::from_hex("00FF80"), Some(Rgb::new(0x00, 0xff, 0x80)));
    assert_eq!(Rgb::from_hex("AaBbCc"), Some(Rgb::new(0xaa, 0xbb, 0xcc)));
}

#[test]
fn from_hex_rejects_malformed_input() {
    // Wrong length, either shorter or longer.
    assert_eq!(Rgb::from_hex("fff"), None);
    assert_eq!(Rgb::from_hex("0011223"), None);
    assert_eq!(Rgb::from_hex(""), None);
    // Non-hex digits.
    assert_eq!(Rgb::from_hex("zzzzzz"), None);
    assert_eq!(Rgb::from_hex("00gg00"), None);
    // A leading `#` is not the canonical bare spelling.
    assert_eq!(Rgb::from_hex("#00ff80"), None);
    // Non-ASCII of the right byte length must still be refused.
    assert_eq!(Rgb::from_hex("00ff8é"), None);
}

#[test]
fn write_hex_round_trips_from_hex() {
    for color in [
        Rgb::new(0, 0, 0),
        Rgb::new(255, 255, 255),
        Rgb::new(0x12, 0x34, 0x56),
        Rgb::new(0xab, 0xcd, 0xef),
    ] {
        let mut text = String::new();
        color.write_hex(&mut text);
        assert_eq!(Rgb::from_hex(&text), Some(color));
    }
}

// --- Scheme::from_name / name / label -------------------------------------

#[test]
fn from_name_and_name_round_trip_for_every_scheme() {
    for scheme in Scheme::ALL {
        assert_eq!(Scheme::from_name(scheme.name()), Some(scheme));
    }
}

#[test]
fn from_name_rejects_an_unknown_spelling() {
    assert_eq!(Scheme::from_name("not-a-scheme"), None);
    assert_eq!(Scheme::from_name(""), None);
    // Case matters: the registry is case-sensitive.
    assert_eq!(Scheme::from_name("Midnight"), None);
}

#[test]
fn every_scheme_label_is_non_empty_and_distinct() {
    let labels: alloc::vec::Vec<&str> = Scheme::ALL.iter().map(|s| s.label()).collect();
    for label in &labels {
        assert!(!label.is_empty());
    }
    for (index, label) in labels.iter().enumerate() {
        for other in labels.iter().skip(index + 1) {
            assert_ne!(label, other, "labels must be distinct");
        }
    }
}

// --- Scheme::palette -------------------------------------------------------

#[test]
fn system_and_custom_have_no_fixed_palette() {
    assert_eq!(Scheme::System.palette(), None);
    assert_eq!(Scheme::Custom.palette(), None);
}

#[test]
fn every_other_scheme_has_a_fixed_palette() {
    // Every variant but `System` and `Custom` carries a palette of its own.
    for scheme in Scheme::ALL {
        if matches!(scheme, Scheme::System | Scheme::Custom) {
            continue;
        }
        assert!(
            scheme.palette().is_some(),
            "{scheme:?} should carry a palette"
        );
    }
}

// --- Painted::resolve -------------------------------------------------------

#[test]
fn resolve_system_takes_the_theme_surface_and_accent_colors() {
    let theme = Theme::dark();
    let custom = ColorScheme::from_theme(&Theme::light());
    let painted = Painted::resolve(Scheme::System, &custom, &theme, 255);
    let palette = theme.palette();
    assert_eq!(painted.scheme.background, Rgb::from(palette.surface));
    assert_eq!(painted.scheme.foreground, Rgb::from(palette.on_surface));
    assert_eq!(painted.scheme.cursor, Rgb::from(palette.accent));
    assert_eq!(painted.scheme.cursor_text, Rgb::from(palette.on_accent));
}

#[test]
fn resolve_custom_takes_the_passed_in_custom_scheme() {
    let theme = Theme::dark();
    let custom = ColorScheme {
        background: Rgb::new(1, 2, 3),
        foreground: Rgb::new(4, 5, 6),
        cursor: Rgb::new(7, 8, 9),
        cursor_text: Rgb::new(10, 11, 12),
        ansi: [Rgb::new(1, 1, 1); ANSI_COLORS],
    };
    let painted = Painted::resolve(Scheme::Custom, &custom, &theme, 255);
    assert_eq!(painted.scheme, custom);
}

#[test]
fn resolve_builtin_takes_its_own_fixed_palette() {
    let theme = Theme::dark();
    let custom = ColorScheme::from_theme(&Theme::light());
    for scheme in Scheme::BUILTINS {
        let painted = Painted::resolve(scheme, &custom, &theme, 255);
        assert_eq!(
            painted.scheme,
            scheme.palette().expect("builtin has a palette")
        );
    }
}

// --- Painted::cell_colors ---------------------------------------------------

/// A [`Painted`] over a fixed built-in scheme, fully opaque unless a test
/// asks for translucency.
fn painted(background_alpha: u8) -> Painted {
    Painted {
        scheme: Scheme::Contrast.palette().expect("contrast has a palette"),
        background_alpha,
    }
}

#[test]
fn default_attributes_take_the_scheme_foreground_and_background() {
    let p = painted(200);
    let (fg, bg) = p.cell_colors(Attributes::PLAIN);
    assert_eq!(fg, p.scheme.foreground.opaque());
    // The default background carries the translucent alpha in force, while
    // the foreground text stays fully opaque.
    assert_eq!(bg, p.scheme.background.with_alpha(200));
    assert_eq!(fg.a, 255);
    assert_eq!(bg.a, 200);
}

#[test]
fn an_explicit_basic_color_maps_to_its_ansi_slot() {
    let p = painted(255);
    let attrs = Attributes {
        foreground: VtColor::Basic(BasicColor::Green),
        ..Attributes::PLAIN
    };
    let (fg, _bg) = p.cell_colors(attrs);
    assert_eq!(fg, p.scheme.ansi(BasicColor::Green.index()).opaque());
}

#[test]
fn bold_brightens_a_non_bright_basic_color() {
    let p = painted(255);
    let attrs = Attributes {
        foreground: VtColor::Basic(BasicColor::Red),
        bold: true,
        ..Attributes::PLAIN
    };
    let (fg, _bg) = p.cell_colors(attrs);
    assert_eq!(fg, p.scheme.ansi(BasicColor::BrightRed.index()).opaque());
}

#[test]
fn bold_leaves_an_already_bright_basic_color_unchanged() {
    let p = painted(255);
    let attrs = Attributes {
        foreground: VtColor::Basic(BasicColor::BrightGreen),
        bold: true,
        ..Attributes::PLAIN
    };
    let (fg, _bg) = p.cell_colors(attrs);
    assert_eq!(fg, p.scheme.ansi(BasicColor::BrightGreen.index()).opaque());
}

#[test]
fn indexed_below_sixteen_uses_the_schemes_own_slot() {
    let p = painted(255);
    for index in 0u8..16 {
        let attrs = Attributes {
            foreground: VtColor::Indexed(index),
            ..Attributes::PLAIN
        };
        let (fg, _bg) = p.cell_colors(attrs);
        assert_eq!(fg, p.scheme.ansi(index).opaque());
    }
}

#[test]
fn indexed_in_the_cube_range_uses_the_fixed_6x6x6_arithmetic() {
    let p = painted(255);
    // Index 67 = 16 + 51; offset 51 decomposes to cube coordinates (1, 2, 3).
    let attrs = Attributes {
        foreground: VtColor::Indexed(67),
        ..Attributes::PLAIN
    };
    let (fg, _bg) = p.cell_colors(attrs);
    assert_eq!(fg, tairix_raster::Color::rgb(95, 135, 175));

    // Index 16 is the cube's own black corner.
    let attrs = Attributes {
        foreground: VtColor::Indexed(16),
        ..Attributes::PLAIN
    };
    let (fg, _bg) = p.cell_colors(attrs);
    assert_eq!(fg, tairix_raster::Color::rgb(0, 0, 0));
}

#[test]
fn indexed_in_the_greyscale_range_uses_the_fixed_ramp() {
    let p = painted(255);
    let attrs = Attributes {
        foreground: VtColor::Indexed(232),
        ..Attributes::PLAIN
    };
    let (fg, _bg) = p.cell_colors(attrs);
    assert_eq!(fg, tairix_raster::Color::rgb(8, 8, 8));

    let attrs = Attributes {
        foreground: VtColor::Indexed(255),
        ..Attributes::PLAIN
    };
    let (fg, _bg) = p.cell_colors(attrs);
    assert_eq!(fg, tairix_raster::Color::rgb(238, 238, 238));
}

#[test]
fn truecolor_is_used_verbatim() {
    let p = painted(255);
    let attrs = Attributes {
        foreground: VtColor::Rgb(10, 20, 30),
        background: VtColor::Rgb(40, 50, 60),
        ..Attributes::PLAIN
    };
    let (fg, bg) = p.cell_colors(attrs);
    assert_eq!(fg, tairix_raster::Color::rgb(10, 20, 30));
    // An explicit background colour is opaque, unlike the translucent default.
    assert_eq!(bg, tairix_raster::Color::rgb(40, 50, 60));
}

#[test]
fn reverse_swaps_the_pair_and_forces_both_opaque() {
    let p = painted(120);
    let attrs = Attributes {
        reverse: true,
        ..Attributes::PLAIN
    };
    let (fg, bg) = p.cell_colors(attrs);
    // Reversed: the (translucent) background colour comes out as the first
    // element and the (opaque) foreground colour as the second, and both are
    // forced fully opaque regardless of the window's own translucency.
    assert_eq!(fg.a, 255);
    assert_eq!(bg.a, 255);
    assert_eq!(
        (fg.r, fg.g, fg.b),
        (
            p.scheme.background.r,
            p.scheme.background.g,
            p.scheme.background.b
        )
    );
    assert_eq!(
        (bg.r, bg.g, bg.b),
        (
            p.scheme.foreground.r,
            p.scheme.foreground.g,
            p.scheme.foreground.b
        )
    );
}

// --- Legibility ------------------------------------------------------------

#[test]
fn every_builtin_scheme_has_visibly_distinct_foreground_and_background() {
    for scheme in Scheme::BUILTINS.into_iter().chain([Scheme::Paper]) {
        let palette = scheme.palette().expect("builtin has a palette");
        let fg_luma = i32::from(palette.foreground.luminance());
        let bg_luma = i32::from(palette.background.luminance());
        assert!(
            (fg_luma - bg_luma).abs() > 60,
            "{scheme:?} text is too close in luminance to its background"
        );
    }
}
