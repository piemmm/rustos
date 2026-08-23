//! Unit tests for the pinboard settings registry.

use tairix_appconf::Document;

use super::*;
use crate::catalog;

/// The settings a document naming exactly `text` yields under the strict
/// reading, or the refusal it raised.
fn read(text: &str) -> Result<PinboardSettings, DocumentRefusal> {
    decode(text)
}

/// The canonical rendered document of `settings`.
fn rendered(settings: &PinboardSettings) -> String {
    settings.document().render()
}

#[test]
fn an_empty_document_is_the_default_settings() {
    assert_eq!(
        read("").expect("empty document"),
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
wallpaper = /Users/ada/Documents/sunset.png
fit = centre
backdrop = 112233
icons = trailing
sort = date
";
    let settings = read(text).expect("well-formed document");
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
    let settings = read("fit = tile\n").expect("partial document");
    assert_eq!(settings.fit, WallpaperFit::Tile);
    assert_eq!(settings.backdrop, Backdrop::Theme);
    assert_eq!(settings.icons, IconFlow::Leading);
    assert_eq!(settings.sort, IconSort::Name);
}

#[test]
fn wallpaper_none_is_accepted() {
    let settings = read("wallpaper = none\n").expect("none wallpaper");
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
    let text = rendered(&settings);
    assert_eq!(
        text,
        "wallpaper = none\n\
         fit = stretch\n\
         backdrop = aabbcc\n\
         icons = trailing\n\
         sort = size\n"
    );
    assert_eq!(read(&text).expect("re-reads"), settings);
}

#[test]
fn default_settings_render_and_reread_exactly() {
    let settings = PinboardSettings::default();
    assert_eq!(
        read(&rendered(&settings)).expect("re-reads"),
        settings,
        "the canonical document of the defaults reads back as the defaults"
    );
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
        assert_eq!(read(&rendered(&settings)).unwrap().fit, fit);
    }
}

#[test]
fn every_icon_flow_and_sort_round_trips() {
    for icons in [IconFlow::Leading, IconFlow::Trailing] {
        let settings = PinboardSettings {
            icons,
            ..PinboardSettings::default()
        };
        assert_eq!(read(&rendered(&settings)).unwrap().icons, icons);
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
        assert_eq!(read(&rendered(&settings)).unwrap().sort, sort);
    }
}

#[test]
fn comments_and_blank_lines_are_ignored() {
    let text = "\
# comment
   # indented comment

fit = tile

   
icons = trailing
";
    let settings = read(text).expect("ignored whitespace");
    assert_eq!(settings.fit, WallpaperFit::Tile);
    assert_eq!(settings.icons, IconFlow::Trailing);
}

#[test]
fn an_unknown_key_is_refused_on_the_wire() {
    assert_eq!(
        read("bogus = value\n"),
        Err(DocumentRefusal::UnknownKey(String::from("bogus")))
    );
}

#[test]
fn a_line_that_is_not_a_setting_is_refused_on_the_wire() {
    // The engine keeps a line it could not read as a setting rather than
    // aborting the document; a *sender* emitting one is describing a desktop
    // this build cannot show, so the strict reading refuses it and names the
    // line.
    assert_eq!(read("fit\n"), Err(DocumentRefusal::Unparsed(1)));
    assert_eq!(
        read("fit = tile\nnonsense\n"),
        Err(DocumentRefusal::Unparsed(2))
    );
}

#[test]
fn a_repeated_key_takes_the_last_setting() {
    // The format engine defines what a duplicate means — the last line wins,
    // so appending overrides — and the registry does not get a second
    // opinion about it.
    assert_eq!(
        read("fit = tile\nfit = fill\n")
            .expect("a repeated key is not a refusal")
            .fit,
        WallpaperFit::Fill
    );
}

#[test]
fn an_invalid_value_is_refused_for_every_key() {
    for (text, key) in [
        ("wallpaper = relative/path.png", SettingsKey::Wallpaper),
        ("fit = sideways", SettingsKey::Fit),
        ("backdrop = notacolour", SettingsKey::Backdrop),
        ("icons = upward", SettingsKey::Icons),
        ("sort = alphabetical", SettingsKey::Sort),
    ] {
        let document = alloc::format!("{text}\n");
        assert_eq!(
            read(&document),
            Err(DocumentRefusal::InvalidValue(key)),
            "{text}"
        );
    }
}

#[test]
fn an_oversized_document_is_refused() {
    let mut text = String::from("fit = tile\n");
    while text.len() <= tairix_appconf::MAX_DOCUMENT_LEN {
        text.push_str("# padding comment line to grow the document\n");
    }
    assert!(matches!(read(&text), Err(DocumentRefusal::Malformed(_))));
}

#[test]
fn the_canonical_document_holds_every_registry_key() {
    // `PinboardSettings::document` drops a key the format engine refuses, so
    // this is what pins that it never has to: every registry key is inside
    // the key grammar and every rendered value inside the value grammar.
    let document = PinboardSettings::default().document();
    for key in SettingsKey::ALL {
        assert_eq!(tairix_appconf::validate_key(key.name()), Ok(()));
        assert_eq!(
            document.get(key.name()),
            Some(key.value_of(&PinboardSettings::default()).as_str()),
            "{key}"
        );
    }
    assert_eq!(document.settings().count(), SettingsKey::ALL.len());
}

// --- The tolerant reading a stored document gets ---------------------------

#[test]
fn a_stored_value_the_registry_refuses_costs_only_itself() {
    let document =
        Document::parse("fit = sideways\nicons = trailing\n").expect("a well-formed document");
    let (settings, refused) = PinboardSettings::load(&document);
    assert_eq!(refused, alloc::vec![SettingsKey::Fit]);
    assert_eq!(
        settings.fit,
        WallpaperFit::default(),
        "the refused setting keeps its documented default"
    );
    assert_eq!(
        settings.icons,
        IconFlow::Trailing,
        "the sound setting beside it still applies"
    );
}

#[test]
fn an_absent_stored_document_is_the_defaults_with_nothing_refused() {
    let (settings, refused) = PinboardSettings::load(&Document::new());
    assert_eq!(settings, PinboardSettings::default());
    assert!(refused.is_empty());
}

#[test]
fn a_stored_line_the_grammar_refused_is_ignored_rather_than_fatal() {
    // The opposite rule to the wire's, and deliberately: a stored document
    // may predate this build, and a desktop must never be blanked by one
    // line the registry cannot place.
    let document = Document::parse("nonsense\nsort = size\n").expect("tolerant parse");
    let (settings, refused) = PinboardSettings::load(&document);
    assert!(refused.is_empty());
    assert_eq!(settings.sort, IconSort::Size);
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
fn wallpaper_path_accepts_a_hash_character() {
    // The refusal this replaced existed only because the hand-rolled grammar
    // cut a line at its first `#`. The format engine quotes such a value, so
    // the path grammar is the only thing that judges a path now.
    assert_eq!(
        WallpaperPath::new("/Users/ada/sun#set.png")
            .expect("a legal path")
            .as_str(),
        "/Users/ada/sun#set.png"
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
fn a_backdrop_value_carrying_a_hash_is_refused_by_the_registry() {
    // The format engine would quote `#112233` and carry it perfectly well,
    // so this is the *registry* keeping one spelling per colour rather than
    // the grammar truncating a line — and it is refused, never accepted on
    // one path and lost on another.
    assert_eq!(
        read("backdrop = \"#112233\"\n"),
        Err(DocumentRefusal::InvalidValue(SettingsKey::Backdrop))
    );
}

#[test]
fn a_bare_hex_backdrop_value_round_trips_without_a_hash() {
    let settings = read("backdrop = 112233\n").expect("bare hex backdrop");
    assert_eq!(
        settings.backdrop,
        Backdrop::Colour(Rgb::new(0x11, 0x22, 0x33))
    );
    assert!(rendered(&settings).contains("backdrop = 112233\n"));
}

#[test]
fn a_wallpaper_path_carrying_a_hash_survives_the_round_trip() {
    // The hand-rolled grammar this replaced had to refuse such a path to
    // stay unambiguous; the format engine quotes it instead, so a file the
    // user really named this way is choosable.
    let settings = PinboardSettings {
        wallpaper: WallpaperChoice::Image(
            WallpaperPath::new("/Users/ada/Pictures/sunset#2.png").expect("a legal path"),
        ),
        ..PinboardSettings::default()
    };
    let text = rendered(&settings);
    assert!(
        text.contains("\"/Users/ada/Pictures/sunset#2.png\""),
        "{text}"
    );
    assert_eq!(read(&text).expect("re-reads"), settings);
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
    for refusal in [
        DocumentRefusal::Malformed(tairix_appconf::ConfError::DocumentTooLarge),
        DocumentRefusal::Unparsed(3),
        DocumentRefusal::UnknownKey(String::from("bogus")),
        DocumentRefusal::InvalidValue(SettingsKey::Fit),
    ] {
        assert!(!alloc::format!("{refusal}").is_empty());
    }
    for error in [WallpaperPathError::TooLong, WallpaperPathError::Malformed] {
        assert!(!alloc::format!("{error}").is_empty());
    }
}

#[test]
fn a_refusal_names_what_was_wrong() {
    assert_eq!(
        alloc::format!("{}", read("bogus = value\n").unwrap_err()),
        "unknown pinboard settings key `bogus`"
    );
    assert!(
        alloc::format!("{}", read("fit = tile\nnonsense\n").unwrap_err()).starts_with("line 2:")
    );
}
