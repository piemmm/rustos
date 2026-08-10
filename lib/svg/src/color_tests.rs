//! Unit tests for the CSS/SVG colour grammar.
//!
//! Colours arrive from untrusted assets, so the tests cover both halves of
//! the contract: every spelling CSS defines resolves to the value a browser
//! would compute, and everything else fails closed as an `InvalidColor`
//! without ever panicking.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_raster::Color;

use crate::error::SvgError;

use super::{compare_ascii_lowercase, parse_color, parse_fill, ColorSpec, NAMED_COLORS};

/// The colour `text` resolves to, failing the test if it does not resolve.
#[track_caller]
fn value(text: &str) -> Color {
    match parse_color(text) {
        Ok(ColorSpec::Value(color)) => color,
        other => panic!("expected a colour for {text:?}, got {other:?}"),
    }
}

/// Assert `text` is refused.
#[track_caller]
fn rejected(text: &str) {
    assert_eq!(parse_color(text), Err(SvgError::InvalidColor), "{text:?}");
}

// --- hex -----------------------------------------------------------------

#[test]
fn three_digit_hex_duplicates_each_nibble() {
    assert_eq!(value("#0f0"), Color::rgb(0x00, 0xff, 0x00));
    assert_eq!(value("#abc"), Color::rgb(0xaa, 0xbb, 0xcc));
}

#[test]
fn four_digit_hex_duplicates_the_alpha_nibble_too() {
    assert_eq!(value("#0f08"), Color::rgba(0x00, 0xff, 0x00, 0x88));
    assert_eq!(value("#1234"), Color::rgba(0x11, 0x22, 0x33, 0x44));
}

#[test]
fn six_digit_hex_is_opaque() {
    assert_eq!(value("#ff8800"), Color::rgba(0xff, 0x88, 0x00, 0xff));
}

#[test]
fn eight_digit_hex_carries_its_alpha() {
    assert_eq!(value("#11223380"), Color::rgba(0x11, 0x22, 0x33, 0x80));
}

#[test]
fn hex_digits_are_case_insensitive() {
    assert_eq!(value("#AbCdEf"), value("#abcdef"));
    assert_eq!(value("#FFF"), Color::rgb(0xff, 0xff, 0xff));
}

#[test]
fn malformed_hex_is_refused() {
    for text in [
        "#",
        "#1",
        "#12",
        "#12345",
        "#1234567",
        "#123456789",
        "#gg0000",
    ] {
        rejected(text);
    }
}

// --- keywords ------------------------------------------------------------

#[test]
fn none_and_transparent_paint_nothing() {
    for text in ["none", "NONE", "None", "transparent", "Transparent"] {
        assert_eq!(parse_color(text), Ok(ColorSpec::None), "{text:?}");
    }
}

#[test]
fn current_color_is_left_for_the_style_resolver() {
    for text in ["currentColor", "currentcolor", "CURRENTCOLOR"] {
        assert_eq!(parse_color(text), Ok(ColorSpec::Current), "{text:?}");
    }
}

#[test]
fn a_spread_of_named_colours_resolves() {
    assert_eq!(value("red"), Color::rgb(0xff, 0x00, 0x00));
    assert_eq!(value("rebeccapurple"), Color::rgb(0x66, 0x33, 0x99));
    assert_eq!(value("mediumaquamarine"), Color::rgb(0x66, 0xcd, 0xaa));
    assert_eq!(value("whitesmoke"), Color::rgb(0xf5, 0xf5, 0xf5));
    assert_eq!(value("chartreuse"), Color::rgb(0x7f, 0xff, 0x00));
    assert_eq!(parse_color("transparent"), Ok(ColorSpec::None));
}

/// The subset the decoder supported before the full CSS grammar landed.
#[test]
fn the_original_named_subset_keeps_its_values() {
    assert_eq!(value("black"), Color::rgb(0, 0, 0));
    assert_eq!(value("white"), Color::rgb(255, 255, 255));
    assert_eq!(value("red"), Color::rgb(255, 0, 0));
    assert_eq!(value("green"), Color::rgb(0, 128, 0));
    assert_eq!(value("blue"), Color::rgb(0, 0, 255));
    assert_eq!(value("gray"), Color::rgb(128, 128, 128));
    assert_eq!(value("grey"), Color::rgb(128, 128, 128));
    assert_eq!(value("yellow"), Color::rgb(255, 255, 0));
}

#[test]
fn named_colours_are_case_insensitive() {
    assert_eq!(value("RED"), value("red"));
    assert_eq!(value("RebeccaPurple"), value("rebeccapurple"));
    assert_eq!(value("WhiteSmoke"), value("whitesmoke"));
}

/// The binary search is only sound over a sorted table, so the order is
/// checked rather than trusted to the eye.
#[test]
fn the_named_colour_table_is_sorted_and_lowercase() {
    for pair in NAMED_COLORS.windows(2) {
        let [(before, _), (after, _)] = pair else {
            unreachable!("windows(2) yields pairs")
        };
        assert!(before < after, "{before} must sort before {after}");
        assert!(before.bytes().all(|b| b.is_ascii_lowercase()), "{before}");
    }
}

#[test]
fn every_named_colour_is_reachable_through_the_search() {
    for (name, expected) in NAMED_COLORS {
        assert_eq!(value(name), *expected, "{name}");
        assert_eq!(value(&name.to_ascii_uppercase()), *expected, "{name}");
    }
}

#[test]
fn the_probe_orders_against_a_lowercased_name() {
    assert!(compare_ascii_lowercase("red", "RED").is_eq());
    assert!(compare_ascii_lowercase("red", "REDDISH").is_lt());
    assert!(compare_ascii_lowercase("red", "RE").is_gt());
    assert!(compare_ascii_lowercase("blue", "RED").is_lt());
}

// --- rgb() / rgba() ------------------------------------------------------

#[test]
fn rgb_takes_integer_channels() {
    assert_eq!(value("rgb(1, 2, 3)"), Color::rgb(1, 2, 3));
    assert_eq!(value("rgb(0,0,0)"), Color::rgb(0, 0, 0));
    assert_eq!(value("rgb(255, 255, 255)"), Color::rgb(255, 255, 255));
}

#[test]
fn rgb_takes_percentage_channels() {
    assert_eq!(value("rgb(0%, 50%, 100%)"), Color::rgb(0, 128, 255));
    assert_eq!(value("rgb(100%,0%,0%)"), value("red"));
}

#[test]
fn rgb_takes_the_space_and_slash_spelling() {
    assert_eq!(value("rgb(1 2 3)"), Color::rgb(1, 2, 3));
    assert_eq!(value("rgb(1 2 3 / 0.5)"), Color::rgba(1, 2, 3, 128));
    assert_eq!(
        value("rgb(10% 20% 30%/50%)"),
        value("rgba(10%,20%,30%,0.5)")
    );
}

#[test]
fn rgb_clamps_out_of_range_channels() {
    assert_eq!(value("rgb(300, -20, 3)"), Color::rgb(255, 0, 3));
    assert_eq!(value("rgb(120%, -10%, 50%)"), Color::rgb(255, 0, 128));
}

#[test]
fn rgb_rounds_a_fractional_channel_to_the_nearest_byte() {
    assert_eq!(value("rgb(1.4, 1.6, 2.5)"), Color::rgb(1, 2, 3));
}

#[test]
fn rgba_takes_its_alpha_as_a_number_or_a_percentage() {
    assert_eq!(value("rgba(1,2,3,0.5)"), Color::rgba(1, 2, 3, 128));
    assert_eq!(value("rgba(1,2,3,50%)"), Color::rgba(1, 2, 3, 128));
    assert_eq!(value("rgba(1,2,3,0)"), Color::rgba(1, 2, 3, 0));
    assert_eq!(value("rgba(1,2,3,1)"), Color::rgba(1, 2, 3, 255));
}

#[test]
fn rgba_clamps_an_out_of_range_alpha() {
    assert_eq!(value("rgba(0,0,0,2)"), Color::rgba(0, 0, 0, 255));
    assert_eq!(value("rgba(0,0,0,-1)"), Color::rgba(0, 0, 0, 0));
}

/// `rgb()` and `rgba()` are synonyms in CSS Color 4: either takes an alpha.
#[test]
fn rgb_and_rgba_are_interchangeable() {
    assert_eq!(value("rgb(1,2,3,0.5)"), value("rgba(1 2 3 / 0.5)"));
    assert_eq!(value("rgba(1,2,3)"), value("rgb(1,2,3)"));
}

#[test]
fn function_names_are_case_insensitive() {
    assert_eq!(value("RGB(1,2,3)"), Color::rgb(1, 2, 3));
    assert_eq!(value("RgBa(1,2,3,1)"), Color::rgb(1, 2, 3));
    assert_eq!(value("HSL(0, 100%, 50%)"), value("red"));
}

#[test]
fn surrounding_and_inner_whitespace_is_tolerated() {
    assert_eq!(value("  \t#0f0\n "), Color::rgb(0, 255, 0));
    assert_eq!(value(" rgb( 1 , 2 , 3 ) "), Color::rgb(1, 2, 3));
    assert_eq!(value("  red  "), Color::rgb(255, 0, 0));
}

#[test]
fn a_malformed_rgb_argument_list_is_refused() {
    for text in [
        "rgb(1,2)",
        "rgb(1,2,3,4,5)",
        "rgb(1 2)",
        "rgb(1 2 3 4)",
        "rgb(1,2,3,)",
        "rgb(,1,2)",
        "rgb()",
        "rgb(",
        "rgb(1,2,3",
        "rgb(1, 2, 3 / 0.5)",
        "rgb(1 2 3 / )",
        "rgb(1 2 3 / 0.5 / 0.5)",
        "rgb(255, 50%, 0)",
        "rgb(1,2,3))",
        "rgb (1,2,3)",
    ] {
        rejected(text);
    }
}

// --- hsl() / hsla() ------------------------------------------------------

#[test]
fn hsl_matches_the_reference_conversion() {
    assert_eq!(value("hsl(0, 100%, 50%)"), Color::rgb(255, 0, 0));
    assert_eq!(value("hsl(120, 100%, 50%)"), Color::rgb(0, 255, 0));
    assert_eq!(value("hsl(240, 100%, 50%)"), Color::rgb(0, 0, 255));
    assert_eq!(value("hsl(210, 50%, 40%)"), Color::rgb(0x33, 0x66, 0x99));
    assert_eq!(value("hsl(0, 100%, 100%)"), Color::rgb(255, 255, 255));
    assert_eq!(value("hsl(0, 100%, 0%)"), Color::rgb(0, 0, 0));
}

/// Zero saturation is grey at every hue, the branch a sector-based
/// conversion most often gets wrong.
#[test]
fn hsl_with_no_saturation_is_grey() {
    assert_eq!(value("hsl(0, 0%, 50%)"), Color::rgb(128, 128, 128));
    assert_eq!(value("hsl(200, 0%, 50%)"), Color::rgb(128, 128, 128));
    assert_eq!(value("hsl(0, 0%, 0%)"), Color::rgb(0, 0, 0));
    assert_eq!(value("hsl(0, 0%, 100%)"), Color::rgb(255, 255, 255));
}

#[test]
fn hsl_wraps_the_hue_rather_than_refusing_it() {
    assert_eq!(value("hsl(480, 100%, 50%)"), value("hsl(120, 100%, 50%)"));
    assert_eq!(value("hsl(-120, 100%, 50%)"), value("hsl(240, 100%, 50%)"));
    assert_eq!(value("hsl(3600, 100%, 50%)"), value("hsl(0, 100%, 50%)"));
}

#[test]
fn hsl_reads_every_css_angle_unit() {
    let cyan = Color::rgb(0, 255, 255);
    assert_eq!(value("hsl(180, 100%, 50%)"), cyan);
    assert_eq!(value("hsl(180deg, 100%, 50%)"), cyan);
    assert_eq!(value("hsl(200grad, 100%, 50%)"), cyan);
    assert_eq!(value("hsl(3.141592653589793rad, 100%, 50%)"), cyan);
    assert_eq!(value("hsl(0.5turn, 100%, 50%)"), cyan);
    assert_eq!(value("hsl(180DEG, 100%, 50%)"), cyan);
}

#[test]
fn hsl_clamps_saturation_and_lightness() {
    assert_eq!(value("hsl(0, 200%, 50%)"), Color::rgb(255, 0, 0));
    assert_eq!(value("hsl(0, -50%, 50%)"), Color::rgb(128, 128, 128));
    assert_eq!(value("hsl(0, 100%, 150%)"), Color::rgb(255, 255, 255));
    assert_eq!(value("hsl(0, 100%, -50%)"), Color::rgb(0, 0, 0));
}

#[test]
fn hsla_carries_an_alpha_in_either_spelling() {
    assert_eq!(
        value("hsla(120, 100%, 50%, 0.5)"),
        Color::rgba(0, 255, 0, 128)
    );
    assert_eq!(
        value("hsl(120 100% 50% / 50%)"),
        Color::rgba(0, 255, 0, 128)
    );
    assert_eq!(value("hsl(120 100% 50%)"), Color::rgb(0, 255, 0));
}

/// CSS Color 3 spells saturation and lightness as percentages; bare numbers
/// are a different (and here unsupported) grammar, not a value to guess at.
#[test]
fn hsl_without_percent_signs_is_refused() {
    for text in [
        "hsl(1,2,3)",
        "hsl(120, 100, 50%)",
        "hsl(120, 100%, 50)",
        "hsl(120 100 50)",
    ] {
        rejected(text);
    }
}

#[test]
fn a_malformed_hsl_argument_list_is_refused() {
    for text in [
        "hsl(120, 100%)",
        "hsl(120, 100%, 50%, 1, 2)",
        "hsl(",
        "hsl()",
        "hsl(120deg 100% 50% / )",
        "hsl(120grad, 100%, 50%, abc)",
    ] {
        rejected(text);
    }
}

// --- everything else -----------------------------------------------------

#[test]
fn an_unknown_word_or_function_is_refused() {
    for text in [
        "",
        "   ",
        "notacolour",
        "reddish",
        "re",
        "transparentish",
        "currentColour",
        "hwb(0 0% 0%)",
        "lab(50% 0 0)",
        "url(#gradient)",
        "0f0",
    ] {
        rejected(text);
    }
}

// --- the `parse_fill` compatibility shim ---------------------------------

#[test]
fn fill_resolves_a_colour_and_scales_it_by_its_opacity() {
    assert_eq!(parse_fill("#0f0", None), Ok(Some(Color::rgb(0, 255, 0))));
    assert_eq!(parse_fill("black", None), Ok(Some(Color::rgb(0, 0, 0))));
    assert_eq!(
        parse_fill("#ffffff", Some("0.5")),
        Ok(Some(Color::rgba(255, 255, 255, 128)))
    );
    assert_eq!(
        parse_fill("#11223380", None),
        Ok(Some(Color::rgba(0x11, 0x22, 0x33, 0x80)))
    );
    assert_eq!(
        parse_fill("  #fff  ", Some("  0.5  ")),
        Ok(Some(Color::rgba(255, 255, 255, 128)))
    );
}

/// A fill opacity multiplies the colour's own alpha rather than replacing it.
#[test]
fn fill_opacity_compounds_with_the_colours_own_alpha() {
    assert_eq!(
        parse_fill("#ffffff80", Some("0.5")),
        Ok(Some(Color::rgba(255, 255, 255, 64)))
    );
}

/// However the alpha reaches zero, the shape paints nothing and its caller
/// gets no layer for it.
#[test]
fn a_fill_that_paints_nothing_is_no_colour_at_all() {
    assert_eq!(parse_fill("none", None), Ok(None));
    assert_eq!(parse_fill("transparent", None), Ok(None));
    assert_eq!(parse_fill("#fff", Some("0")), Ok(None));
    assert_eq!(parse_fill("#ffffff00", None), Ok(None));
    assert_eq!(parse_fill("rgba(255,255,255,0)", None), Ok(None));
    assert_eq!(parse_fill("hsla(0, 0%, 0%, 0%)", None), Ok(None));
}

#[test]
fn fill_reaches_the_whole_colour_grammar() {
    assert_eq!(
        parse_fill("chartreuse", None),
        Ok(Some(Color::rgb(0x7f, 0xff, 0x00)))
    );
    assert_eq!(
        parse_fill("hsl(120, 100%, 50%)", None),
        Ok(Some(Color::rgb(0, 255, 0)))
    );
}

/// The shim has no inherited `color` to substitute, so it fails closed
/// instead of guessing one.
#[test]
fn fill_refuses_current_color() {
    assert_eq!(
        parse_fill("currentColor", None),
        Err(SvgError::InvalidColor)
    );
}

#[test]
fn a_malformed_fill_or_opacity_is_refused() {
    assert_eq!(parse_fill("notacolour", None), Err(SvgError::InvalidColor));
    assert_eq!(parse_fill("#gg0000", None), Err(SvgError::InvalidColor));
    assert_eq!(
        parse_fill("#fff", Some("abc")),
        Err(SvgError::InvalidNumber)
    );
    assert_eq!(parse_fill("#fff", Some("")), Err(SvgError::InvalidNumber));
}

// --- totality ------------------------------------------------------------

/// Hostile spellings: partial functions, absurd numbers, control and
/// non-ASCII bytes. The only assertion is that nothing panics — whatever
/// each one returns, it returns it.
#[test]
fn no_input_panics() {
    let mut cases: Vec<String> = [
        "",
        " ",
        "#",
        "#\u{0}\u{0}\u{0}",
        "#ÿÿÿ",
        "###",
        "rgb(",
        "rgb()",
        "rgb(,,)",
        "rgb(///)",
        "rgb(1,2,3",
        "rgb((1,2,3))",
        "rgba(1e999, -1e999, 1e999, 1e999)",
        "rgb(nan, nan, nan)",
        "rgb(inf, inf, inf)",
        "rgb(é, é, é)",
        "hsl(",
        "hsl(1e308deg, 1e308%, 1e308%)",
        "hsl(-1e308turn, 100%, 50%)",
        "hsl(0/0/0)",
        "hsl(rad, rad, rad)",
        "hsl(grad,%,%)",
        "(((((((((",
        ")))))))))",
        "/",
        "%",
        "50%",
        "-",
        "e",
        "currentColor(1,2,3)",
        "\u{0}",
        "\u{7f}\u{1b}[0m",
        "🎨",
        "rgb(🎨,🎨,🎨)",
        "الأحمر",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // Every byte value in turn, both as a whole value and inside an
    // otherwise well-formed function argument.
    for byte in 0..=u8::MAX {
        let raw = char::from(byte);
        cases.push(format!("{raw}"));
        cases.push(format!("#{raw}{raw}{raw}"));
        cases.push(format!("rgb(1,2,{raw})"));
        cases.push(format!("hsl(1{raw}, 2%, 3%)"));
        cases.push(format!("re{raw}"));
    }

    for text in &cases {
        let _ = parse_color(text);
        let _ = parse_fill(text, Some(text));
    }
}
