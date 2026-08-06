//! Unit tests for the terminal profile document.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::effects::{Effects, FULL, MIN_OPACITY};
use crate::scheme::{ColorScheme, Rgb, Scheme, ANSI_COLORS};

use super::{
    parse, render, user_profile_path, ParseError, Profile, ProfileKey, DEFAULT_FONT_SIZE_PX,
    MAX_FONT_SIZE_PX, MAX_PROFILE_LEN, MIN_FONT_SIZE_PX,
};

// --- Profile::default -------------------------------------------------------

#[test]
fn default_profile_is_system_scheme_default_size_and_effects_off() {
    let profile = Profile::default();
    assert_eq!(profile.scheme, Scheme::System);
    assert_eq!(profile.font_size_px, DEFAULT_FONT_SIZE_PX);
    assert_eq!(profile.effects, Effects::default());
    assert_eq!(profile.effects.opacity, FULL);
}

// --- render / parse round trips ---------------------------------------------

#[test]
fn render_parse_round_trips_the_default_profile() {
    let profile = Profile::default();
    let text = render(&profile);
    assert_eq!(parse(&text), Ok(profile));
}

/// A profile with every field changed from its default, including a fully
/// custom ANSI palette — the round trip a real edited profile exercises.
fn customised_profile() -> Profile {
    let mut ansi = [Rgb::new(0, 0, 0); ANSI_COLORS];
    for (index, slot) in ansi.iter_mut().enumerate() {
        let value = u8::try_from(index * 15).unwrap_or(u8::MAX);
        *slot = Rgb::new(value, value.wrapping_add(1), value.wrapping_add(2));
    }
    Profile {
        scheme: Scheme::Custom,
        font_size_px: 22,
        effects: Effects {
            opacity: 450,
            blur: 600,
            scanlines: 700,
            fuzz: 100,
            phosphor: 900,
            wobble: 250,
        },
        custom: ColorScheme {
            background: Rgb::new(0x10, 0x20, 0x30),
            foreground: Rgb::new(0xe0, 0xd0, 0xc0),
            cursor: Rgb::new(0x40, 0x50, 0x60),
            cursor_text: Rgb::new(0x01, 0x02, 0x03),
            ansi,
        },
    }
}

#[test]
fn render_parse_round_trips_a_heavily_customised_profile() {
    let profile = customised_profile();
    let text = render(&profile);
    assert_eq!(parse(&text), Ok(profile));
}

#[test]
fn render_emits_every_key_exactly_once_in_order() {
    let text = render(&customised_profile());
    let keys: Vec<&str> = text
        .lines()
        .map(|line| line.split_whitespace().next().unwrap_or_default())
        .collect();
    let expected: Vec<&str> = ProfileKey::ALL.iter().map(|key| key.name()).collect();
    assert_eq!(keys, expected);
}

// --- parse: absent keys keep their defaults ---------------------------------

#[test]
fn parse_of_an_empty_document_yields_the_default_profile() {
    assert_eq!(parse(""), Ok(Profile::default()));
}

#[test]
fn parse_of_a_comment_only_document_yields_the_default_profile() {
    let text = "# a user's own note\n# and another\n";
    assert_eq!(parse(text), Ok(Profile::default()));
}

#[test]
fn parse_of_a_partial_document_leaves_the_rest_at_their_defaults() {
    let text = "scheme midnight\nfont-size 20\n";
    let parsed = parse(text).expect("valid document");
    assert_eq!(parsed.scheme, Scheme::Midnight);
    assert_eq!(parsed.font_size_px, 20);
    // Everything the document did not name stays at the documented default.
    let expected = Profile {
        scheme: Scheme::Midnight,
        font_size_px: 20,
        ..Profile::default()
    };
    assert_eq!(parsed, expected);
}

// --- parse: refusals ---------------------------------------------------------

#[test]
fn parse_rejects_an_unknown_key() {
    let err = parse("bogus-key value\n").unwrap_err();
    assert_eq!(err.kind(), ParseError::UnknownKey);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn parse_rejects_a_key_with_no_value() {
    let err = parse("scheme\n").unwrap_err();
    assert_eq!(err.kind(), ParseError::MissingValue);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn parse_rejects_a_duplicate_key() {
    let err = parse("scheme system\nscheme phosphor\n").unwrap_err();
    assert_eq!(err.kind(), ParseError::DuplicateKey);
    assert_eq!(err.line(), Some(2));
}

#[test]
fn parse_rejects_opacity_below_the_minimum() {
    let below = MIN_OPACITY - 1;
    let err = parse(&format!("opacity {below}\n")).unwrap_err();
    assert_eq!(err.kind(), ParseError::InvalidValue);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn parse_rejects_a_font_size_above_the_maximum() {
    let above = MAX_FONT_SIZE_PX + 1;
    let err = parse(&format!("font-size {above}\n")).unwrap_err();
    assert_eq!(err.kind(), ParseError::InvalidValue);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn parse_rejects_a_colour_of_the_wrong_length() {
    let err = parse("custom-background ffff\n").unwrap_err();
    assert_eq!(err.kind(), ParseError::InvalidValue);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn a_hash_prefixed_colour_is_swallowed_as_a_comment_rather_than_parsed() {
    // `#` begins a comment on every line, so a `#rrggbb` spelling never
    // reaches the colour parser at all: the comment-stripped line carries no
    // value, which is `MissingValue`, not `InvalidValue`.
    let err = parse("custom-background #ffffff\n").unwrap_err();
    assert_eq!(err.kind(), ParseError::MissingValue);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn parse_rejects_a_custom_ansi_list_with_fewer_than_sixteen_colors() {
    let fifteen = "000000 111111 222222 333333 444444 555555 666666 777777 \
        888888 999999 aaaaaa bbbbbb cccccc dddddd eeeeee";
    let err = parse(&format!("custom-ansi {fifteen}\n")).unwrap_err();
    assert_eq!(err.kind(), ParseError::InvalidValue);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn parse_rejects_a_custom_ansi_list_with_more_than_sixteen_colors() {
    let seventeen = "000000 111111 222222 333333 444444 555555 666666 777777 \
        888888 999999 aaaaaa bbbbbb cccccc dddddd eeeeee ffffff 000001";
    let err = parse(&format!("custom-ansi {seventeen}\n")).unwrap_err();
    assert_eq!(err.kind(), ParseError::InvalidValue);
    assert_eq!(err.line(), Some(1));
}

#[test]
fn parse_rejects_a_document_longer_than_the_maximum_length() {
    let text = "a".repeat(MAX_PROFILE_LEN + 1);
    let err = parse(&text).unwrap_err();
    assert_eq!(err.kind(), ParseError::DocumentTooLong);
    // A whole-document refusal belongs to no single line.
    assert_eq!(err.line(), None);
}

#[test]
fn a_trailing_comment_after_a_value_is_stripped() {
    let parsed = parse("font-size 20 # a user's own note\n").expect("valid document");
    assert_eq!(parsed.font_size_px, 20);
}

// --- user_profile_path -------------------------------------------------------

#[test]
fn user_profile_path_builds_the_canonical_settings_path() {
    assert_eq!(
        user_profile_path("/Users/ada"),
        Some(String::from("/Users/ada/Settings/Terminal/terminal.conf"))
    );
}

#[test]
fn user_profile_path_normalises_a_trailing_slash() {
    assert_eq!(
        user_profile_path("/Users/ada/"),
        Some(String::from("/Users/ada/Settings/Terminal/terminal.conf"))
    );
}

#[test]
fn user_profile_path_returns_none_for_an_empty_or_root_home() {
    assert_eq!(user_profile_path(""), None);
    assert_eq!(user_profile_path("/"), None);
}

// --- Profile::clamp / enlarge / reduce --------------------------------------

#[test]
fn clamp_bounds_font_size_and_every_effect() {
    let mut profile = Profile {
        scheme: Scheme::System,
        font_size_px: MIN_FONT_SIZE_PX - 1,
        effects: Effects {
            opacity: MIN_OPACITY - 1,
            blur: FULL + 1,
            scanlines: FULL + 1,
            fuzz: FULL + 1,
            phosphor: FULL + 1,
            wobble: FULL + 1,
        },
        custom: Scheme::Contrast.palette().expect("contrast has a palette"),
    };
    profile.clamp();
    assert_eq!(profile.font_size_px, MIN_FONT_SIZE_PX);
    assert_eq!(profile.effects.opacity, MIN_OPACITY);
    assert_eq!(profile.effects.blur, FULL);
    assert_eq!(profile.effects.scanlines, FULL);
    assert_eq!(profile.effects.fuzz, FULL);
    assert_eq!(profile.effects.phosphor, FULL);
    assert_eq!(profile.effects.wobble, FULL);

    profile.font_size_px = MAX_FONT_SIZE_PX + 1;
    profile.clamp();
    assert_eq!(profile.font_size_px, MAX_FONT_SIZE_PX);
}

#[test]
fn enlarge_and_reduce_respect_their_bounds_and_never_wrap() {
    let mut profile = Profile {
        font_size_px: MAX_FONT_SIZE_PX,
        ..Profile::default()
    };
    profile.enlarge();
    assert_eq!(profile.font_size_px, MAX_FONT_SIZE_PX);

    profile.font_size_px = MAX_FONT_SIZE_PX - 1;
    profile.enlarge();
    assert_eq!(profile.font_size_px, MAX_FONT_SIZE_PX);

    profile.font_size_px = MIN_FONT_SIZE_PX;
    profile.reduce();
    assert_eq!(profile.font_size_px, MIN_FONT_SIZE_PX);

    profile.font_size_px = MIN_FONT_SIZE_PX + 1;
    profile.reduce();
    assert_eq!(profile.font_size_px, MIN_FONT_SIZE_PX);
}
