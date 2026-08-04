//! Unit tests for the pinboard settings document grammar.

use super::*;
use crate::catalog;

#[test]
fn an_empty_document_is_the_default_settings() {
    assert_eq!(
        parse("").expect("empty document"),
        PinboardSettings::default()
    );
}

#[test]
fn defaults_match_the_documented_table() {
    let settings = PinboardSettings::default();
    assert_eq!(
        settings.wallpaper,
        WallpaperChoice::Image(WallpaperPath::new(&catalog::default_wallpaper_path()).unwrap())
    );
    assert_eq!(settings.fit, WallpaperFit::Fill);
    assert_eq!(settings.backdrop, Backdrop::Theme);
    assert_eq!(settings.icons, IconFlow::Leading);
    assert_eq!(settings.sort, IconSort::Name);
}

#[test]
fn the_default_wallpaper_path_is_itself_a_valid_wallpaper_path() {
    // `WallpaperPath::shipped_default` bypasses `new`'s runtime validation
    // for the compile-time-fixed default; this pins that the bypass and the
    // validating constructor agree on the same value.
    let validated = WallpaperPath::new(&catalog::default_wallpaper_path()).expect("valid path");
    assert_eq!(
        PinboardSettings::default().wallpaper,
        WallpaperChoice::Image(validated)
    );
}

#[test]
fn a_full_document_parses_every_key() {
    let text = "\
# A full pinboard settings document.
wallpaper /Users/ada/Documents/sunset.png
fit centre
backdrop 112233
icons trailing
sort date
";
    let settings = parse(text).expect("well-formed document");
    assert_eq!(
        settings.wallpaper,
        WallpaperChoice::Image(WallpaperPath::new("/Users/ada/Documents/sunset.png").unwrap())
    );
    assert_eq!(settings.fit, WallpaperFit::Centre);
    assert_eq!(
        settings.backdrop,
        Backdrop::Colour(Rgb::new(0x11, 0x22, 0x33))
    );
    assert_eq!(settings.icons, IconFlow::Trailing);
    assert_eq!(settings.sort, IconSort::Date);
}

#[test]
fn a_partial_document_leaves_the_rest_at_default() {
    let settings = parse("fit tile\n").expect("partial document");
    assert_eq!(settings.fit, WallpaperFit::Tile);
    assert_eq!(settings.backdrop, Backdrop::Theme);
    assert_eq!(settings.icons, IconFlow::Leading);
    assert_eq!(settings.sort, IconSort::Name);
}

#[test]
fn wallpaper_none_is_accepted() {
    let settings = parse("wallpaper none\n").expect("none wallpaper");
    assert_eq!(settings.wallpaper, WallpaperChoice::None);
}

#[test]
fn the_render_is_canonical_and_round_trips() {
    let settings = PinboardSettings {
        wallpaper: WallpaperChoice::None,
        fit: WallpaperFit::Stretch,
        backdrop: Backdrop::Colour(Rgb::new(0xaa, 0xbb, 0xcc)),
        icons: IconFlow::Trailing,
        sort: IconSort::Size,
    };
    let rendered = render(&settings);
    assert_eq!(
        rendered,
        "wallpaper none\n\
         fit stretch\n\
         backdrop aabbcc\n\
         icons trailing\n\
         sort size\n"
    );
    assert_eq!(parse(&rendered).expect("re-parses"), settings);
}

#[test]
fn default_settings_render_and_reparse_exactly() {
    let settings = PinboardSettings::default();
    let rendered = render(&settings);
    assert_eq!(parse(&rendered).expect("re-parses"), settings);
}

#[test]
fn every_fit_value_round_trips() {
    for fit in [
        WallpaperFit::Fill,
        WallpaperFit::Fit,
        WallpaperFit::Stretch,
        WallpaperFit::Centre,
        WallpaperFit::Tile,
    ] {
        let settings = PinboardSettings {
            fit,
            ..PinboardSettings::default()
        };
        assert_eq!(parse(&render(&settings)).unwrap().fit, fit);
    }
}

#[test]
fn every_icon_flow_and_sort_round_trips() {
    for icons in [IconFlow::Leading, IconFlow::Trailing] {
        let settings = PinboardSettings {
            icons,
            ..PinboardSettings::default()
        };
        assert_eq!(parse(&render(&settings)).unwrap().icons, icons);
    }
    for sort in [
        IconSort::Name,
        IconSort::Kind,
        IconSort::Size,
        IconSort::Date,
    ] {
        let settings = PinboardSettings {
            sort,
            ..PinboardSettings::default()
        };
        assert_eq!(parse(&render(&settings)).unwrap().sort, sort);
    }
}

#[test]
fn comments_and_blank_lines_are_ignored() {
    let text = "\
# comment
   # indented comment

fit tile

   
icons trailing
";
    let settings = parse(text).expect("ignored whitespace");
    assert_eq!(settings.fit, WallpaperFit::Tile);
    assert_eq!(settings.icons, IconFlow::Trailing);
}

#[test]
fn unknown_key_is_refused() {
    let err = parse("bogus value\n").unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::UnknownKey);
}

#[test]
fn missing_value_is_refused() {
    let err = parse("fit\n").unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::MissingValue);

    let err = parse("fit   \n").unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::MissingValue);
}

#[test]
fn duplicate_key_is_refused() {
    let err = parse("fit tile\nfit fill\n").unwrap_err();
    assert_eq!(err.line(), Some(2));
    assert_eq!(err.kind(), ParseError::DuplicateKey);
}

#[test]
fn invalid_value_is_refused_for_every_key() {
    for (line, expected_line) in [
        ("wallpaper relative/path.png", 1),
        ("fit sideways", 1),
        ("backdrop notacolour", 1),
        ("icons upward", 1),
        ("sort alphabetical", 1),
    ] {
        let text = alloc::format!("{line}\n");
        let err = parse(&text).unwrap_err();
        assert_eq!(err.line(), Some(expected_line), "{line}");
        assert_eq!(err.kind(), ParseError::InvalidValue, "{line}");
    }
}

#[test]
fn an_oversized_document_is_refused() {
    let mut text = String::from("fit tile\n");
    while text.len() <= MAX_SETTINGS_LEN {
        text.push_str("# padding comment line to grow the document\n");
    }
    let err = parse(&text).unwrap_err();
    assert_eq!(err.line(), None);
    assert_eq!(err.kind(), ParseError::DocumentTooLong);
}

#[test]
fn wallpaper_path_rejects_relative_paths() {
    assert_eq!(
        WallpaperPath::new("Documents/sunset.png"),
        Err(WallpaperPathError::Malformed)
    );
    assert_eq!(
        WallpaperPath::new("../sunset.png"),
        Err(WallpaperPathError::Malformed)
    );
}

#[test]
fn wallpaper_path_rejects_empty_paths() {
    assert_eq!(WallpaperPath::new(""), Err(WallpaperPathError::Malformed));
}

#[test]
fn wallpaper_path_rejects_control_characters() {
    assert_eq!(
        WallpaperPath::new("/Users/ada/sun\u{0}set.png"),
        Err(WallpaperPathError::Malformed)
    );
}

#[test]
fn wallpaper_path_rejects_a_hash_character() {
    // A `#` in a value would begin a comment, breaking the round trip.
    assert_eq!(
        WallpaperPath::new("/Users/ada/sun#set.png"),
        Err(WallpaperPathError::Malformed)
    );
}

#[test]
fn wallpaper_path_rejects_an_alias_or_volume_id_root() {
    assert_eq!(
        WallpaperPath::new("Home:/sunset.png"),
        Err(WallpaperPathError::Malformed)
    );
}

#[test]
fn wallpaper_path_rejects_over_long_paths() {
    let long = alloc::format!("/{}", "a".repeat(MAX_WALLPAPER_PATH_LEN));
    assert_eq!(WallpaperPath::new(&long), Err(WallpaperPathError::TooLong));
}

#[test]
fn wallpaper_path_accepts_a_long_but_within_bound_path() {
    // Several components, each within `tairix_path`'s own per-component
    // bound, whose combined length sits close to the wallpaper path bound.
    let component = "a".repeat(200);
    let mut path = String::new();
    while path.len() + 1 + component.len() <= MAX_WALLPAPER_PATH_LEN {
        path.push('/');
        path.push_str(&component);
    }
    assert!(
        path.len() > 500,
        "the constructed path should be substantial"
    );
    assert!(path.len() <= MAX_WALLPAPER_PATH_LEN);
    WallpaperPath::new(&path).expect("a long path within bound is accepted");
}

#[test]
fn wallpaper_path_canonicalises_dot_segments() {
    let path = WallpaperPath::new("/Users/ada/./Documents/../Documents/sunset.png").unwrap();
    assert_eq!(path.as_str(), "/Users/ada/Documents/sunset.png");
}

#[test]
fn a_backdrop_value_may_not_carry_a_hash_the_document_grammar_reserves() {
    // A stored backdrop value is bare `rrggbb` digits, never `#rrggbb`: the
    // document's own `#`-comment grammar would otherwise cut the line at
    // the very character the value needs, silently truncating it away
    // instead of refusing it.
    let err = parse("backdrop #112233\n").unwrap_err();
    assert_eq!(err.line(), Some(1));
    assert_eq!(err.kind(), ParseError::MissingValue);
}

#[test]
fn a_bare_hex_backdrop_value_round_trips_without_a_hash() {
    let settings = parse("backdrop 112233\n").expect("bare hex backdrop");
    assert_eq!(
        settings.backdrop,
        Backdrop::Colour(Rgb::new(0x11, 0x22, 0x33))
    );
    assert!(render(&settings).contains("backdrop 112233\n"));
}

#[test]
fn rgb_hex_round_trips() {
    let rgb = Rgb::new(0x0a, 0xbc, 0xde);
    assert_eq!(rgb.to_hex(), "0abcde");
    assert_eq!(Rgb::from_hex("0abcde"), Some(rgb));
    assert_eq!(Rgb::from_hex("0ABCDE"), Some(rgb));
}

#[test]
fn rgb_hex_rejects_malformed_text() {
    assert_eq!(Rgb::from_hex("theme"), None);
    assert_eq!(Rgb::from_hex("abc"), None);
    assert_eq!(Rgb::from_hex("gggggg"), None);
    assert_eq!(Rgb::from_hex(""), None);
    assert_eq!(Rgb::from_hex("1122334"), None);
    // The one spelling carries no `#`, so the `#`-prefixed wording the
    // document grammar could never hold is refused rather than accepted on
    // a second path.
    assert_eq!(Rgb::from_hex("#11223"), None);
    assert_eq!(Rgb::from_hex("#112233"), None);
    // A multi-byte character must not be sliced mid-scalar.
    assert_eq!(Rgb::from_hex("11223\u{e9}"), None);
}

#[test]
fn settings_key_registry_round_trips_names() {
    for key in SettingsKey::ALL {
        assert_eq!(SettingsKey::from_name(key.name()), Some(key));
    }
    assert_eq!(SettingsKey::from_name("Wallpaper"), None, "case-sensitive");
    assert_eq!(SettingsKey::from_name(""), None);
}

#[test]
fn error_display_is_nonempty() {
    for kind in [
        ParseError::DocumentTooLong,
        ParseError::UnknownKey,
        ParseError::MissingValue,
        ParseError::DuplicateKey,
        ParseError::InvalidValue,
    ] {
        assert!(!alloc::format!("{kind}").is_empty());
    }
    for error in [WallpaperPathError::TooLong, WallpaperPathError::Malformed] {
        assert!(!alloc::format!("{error}").is_empty());
    }
}

#[test]
fn settings_error_display_carries_the_line_when_present() {
    let err = parse("bogus value\n").unwrap_err();
    assert_eq!(
        alloc::format!("{err}"),
        "line 1: unknown pinboard settings key"
    );
    let err = parse("fit tile\nfit fill\n").unwrap_err();
    assert!(alloc::format!("{err}").starts_with("line 2:"));
}
