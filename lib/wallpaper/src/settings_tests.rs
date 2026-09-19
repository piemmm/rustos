//! Unit tests for the desktop settings registry.

use tairix_abi::desktop::CURSOR_SET_NAME_MAX;
use tairix_appconf::Document;

use super::*;
use crate::catalog;

/// The settings a document naming exactly `text` yields under the strict
/// reading over the defaults, or the refusal it raised.
fn read(text: &str) -> Result<DesktopSettings, DocumentRefusal> {
    merge(&DesktopSettings::default(), text)
}

/// The canonical rendered document of `settings`.
fn rendered(settings: &DesktopSettings) -> String {
    settings.document().render()
}

#[test]
fn an_empty_document_is_the_default_settings() {
    assert_eq!(
        read("").expect("empty document"),
        DesktopSettings::default()
    );
}

#[test]
fn defaults_match_the_documented_table() {
    let settings = DesktopSettings::default();
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
        DesktopSettings::default().wallpaper,
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
    let settings = DesktopSettings {
        wallpaper: WallpaperChoice::None,
        fit: WallpaperFit::Stretch,
        backdrop: Backdrop::Colour(Rgb::new(0xaa, 0xbb, 0xcc)),
        icons: IconFlow::Trailing,
        sort: IconSort::Size,
        appearance: Appearance::Light,
        contrast: Contrast::High,
        density: Density::Comfortable,
        motion: Motion::Reduced,
        scale: Scale::from_percent(150).expect("150% is a scale"),
        cursor_set: CursorSetId::new("High Visibility").expect("a legal set name"),
        cursor_size: CursorSize::Larger,
    };
    let text = rendered(&settings);
    assert_eq!(
        text,
        "wallpaper = none\n\
         fit = stretch\n\
         backdrop = aabbcc\n\
         icons = trailing\n\
         sort = size\n\
         appearance = light\n\
         contrast = high\n\
         density = comfortable\n\
         motion = reduced\n\
         scale = 150\n\
         cursor.set = High Visibility\n\
         cursor.size = larger\n"
    );
    assert_eq!(read(&text).expect("re-reads"), settings);
}

#[test]
fn the_cursor_keys_default_to_the_builtin_set_at_its_own_size() {
    let settings = DesktopSettings::default();
    assert_eq!(settings.cursor_set, CursorSetId::builtin());
    assert_eq!(settings.cursor_size, CursorSize::Normal);
    assert_eq!(settings.cursor_size.percent(), 100);
}

#[test]
fn a_cursor_set_name_no_set_could_carry_is_refused() {
    for value in ["", "..", "a/b", &"s".repeat(CURSOR_SET_NAME_MAX + 1)] {
        let document = alloc::format!("cursor.set = {value:?}\n");
        assert!(
            merge(&DesktopSettings::default(), &document).is_err(),
            "`{value}` must not be accepted as a cursor set"
        );
    }
}

/// A stored choice outlives the image that shipped it, so a set an update
/// removed is still a legal *value*: it is the desktop that falls back to
/// the built-in at activation, rather than the document losing every other
/// key it carries.
#[test]
fn a_set_the_store_no_longer_carries_is_still_a_legal_value() {
    let settings = read("cursor.set = \"Gone Away\"\nscale = 150\n").expect("a legal document");
    assert_eq!(settings.cursor_set.name(), "Gone Away");
    assert_eq!(settings.scale.percent(), 150);
}

#[test]
fn a_pointer_size_outside_the_ladder_is_refused() {
    assert!(merge(&DesktopSettings::default(), "cursor.size = huge\n").is_err());
    assert!(merge(&DesktopSettings::default(), "cursor.size = 150\n").is_err());
}

/// The size magnifies a logical side, so the one logical-to-physical
/// conversion still turns the answer into pixels.
#[test]
fn a_pointer_size_magnifies_the_reference_side() {
    assert_eq!(CursorSize::Normal.side(32), 32);
    assert_eq!(CursorSize::Large.side(32), 48);
    assert_eq!(CursorSize::Larger.side(32), 64);
    assert_eq!(CursorSize::Largest.side(32), 96);
    // No size collapses the pointer, and none of them wraps.
    for size in CursorSize::ALL {
        assert!(size.side(1) >= 1);
        assert!(size.side(u32::MAX) > 0);
    }
}

#[test]
fn default_settings_render_and_reread_exactly() {
    let settings = DesktopSettings::default();
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
        let settings = DesktopSettings {
            fit,
            ..DesktopSettings::default()
        };
        assert_eq!(read(&rendered(&settings)).unwrap().fit, fit);
    }
}

#[test]
fn every_icon_flow_and_sort_round_trips() {
    for icons in [IconFlow::Leading, IconFlow::Trailing] {
        let settings = DesktopSettings {
            icons,
            ..DesktopSettings::default()
        };
        assert_eq!(read(&rendered(&settings)).unwrap().icons, icons);
    }
    for sort in [
        IconSort::Name,
        IconSort::Kind,
        IconSort::Size,
        IconSort::Date,
    ] {
        let settings = DesktopSettings {
            sort,
            ..DesktopSettings::default()
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
    // `DesktopSettings::document` drops a key the format engine refuses, so
    // this is what pins that it never has to: every registry key is inside
    // the key grammar and every rendered value inside the value grammar.
    let document = DesktopSettings::default().document();
    for key in SettingsKey::ALL {
        assert_eq!(tairix_appconf::validate_key(key.name()), Ok(()));
        assert_eq!(
            document.get(key.name()),
            Some(key.value_of(&DesktopSettings::default()).as_str()),
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
    let (settings, refused) = DesktopSettings::load(&document);
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
    let (settings, refused) = DesktopSettings::load(&Document::new());
    assert_eq!(settings, DesktopSettings::default());
    assert!(refused.is_empty());
}

#[test]
fn a_stored_line_the_grammar_refused_is_ignored_rather_than_fatal() {
    // The opposite rule to the wire's, and deliberately: a stored document
    // may predate this build, and a desktop must never be blanked by one
    // line the registry cannot place.
    let document = Document::parse("nonsense\nsort = size\n").expect("tolerant parse");
    let (settings, refused) = DesktopSettings::load(&document);
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
    let settings = DesktopSettings {
        wallpaper: WallpaperChoice::Image(
            WallpaperPath::new("/Users/ada/Pictures/sunset#2.png").expect("a legal path"),
        ),
        ..DesktopSettings::default()
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

#[test]
fn every_appearance_key_reads_its_closed_set() {
    let settings = read(
        "appearance = light\n\
         contrast = monochrome\n\
         density = compact\n\
         motion = reduced\n\
         scale = 175\n",
    )
    .expect("every value is in its set");
    assert_eq!(settings.appearance, Appearance::Light);
    assert_eq!(settings.contrast, Contrast::Monochrome);
    assert_eq!(settings.density, Density::Compact);
    assert_eq!(settings.motion, Motion::Reduced);
    assert_eq!(settings.scale.percent(), 175);
}

#[test]
fn an_appearance_value_outside_its_set_is_refused_whole() {
    for (key, bad) in [
        ("appearance", "sepia"),
        ("contrast", "Normal"),
        ("density", "dense"),
        ("motion", "none"),
    ] {
        let text = alloc::format!("{key} = {bad}\n");
        let refusal = read(&text).expect_err("outside the closed set");
        assert!(matches!(refusal, DocumentRefusal::InvalidValue(_)), "{key}");
    }
}

#[test]
fn a_scale_outside_what_the_desktop_can_draw_is_refused() {
    // The bound is `Scale`'s own, so a percentage this registry accepts is
    // always one the geometry can resolve.
    let below = alloc::format!("scale = {}\n", Scale::MIN_PERCENT - 1);
    let above = alloc::format!("scale = {}\n", Scale::MAX_PERCENT + 1);
    for text in [below, above, String::from("scale = 0\n")] {
        assert!(matches!(
            read(&text),
            Err(DocumentRefusal::InvalidValue(SettingsKey::Scale))
        ));
    }
    // One spelling only: a sign, a space, or a radix prefix is a second way
    // to write a value the document already has one way to write.
    for text in ["scale = +150\n", "scale = 1 5 0\n", "scale = 0x96\n"] {
        assert!(matches!(
            read(text),
            Err(DocumentRefusal::InvalidValue(SettingsKey::Scale))
        ));
    }
}

#[test]
fn a_scale_that_would_overflow_the_accumulator_is_refused_not_wrapped() {
    let text = alloc::format!("scale = {}\n", u64::from(u32::MAX) + 1);
    assert!(matches!(
        read(&text),
        Err(DocumentRefusal::InvalidValue(SettingsKey::Scale))
    ));
}

#[test]
fn a_tolerant_read_leaves_exactly_the_refused_appearance_key_at_its_default() {
    let mut document = Document::new();
    let _ = document.set("appearance", "light");
    let _ = document.set("contrast", "sepia");
    let _ = document.set("density", "compact");
    let (settings, refused) = DesktopSettings::load(&document);
    assert_eq!(refused, alloc::vec![SettingsKey::Contrast]);
    assert_eq!(settings.appearance, Appearance::Light);
    assert_eq!(settings.contrast, Contrast::Normal);
    assert_eq!(settings.density, Density::Compact);
}

#[test]
fn a_merge_leaves_every_key_the_sender_did_not_name() {
    // The defect this forecloses: choosing a wallpaper must not reimpose
    // the appearance the surface happened to open on.
    let in_effect = DesktopSettings {
        appearance: Appearance::Light,
        contrast: Contrast::High,
        density: Density::Comfortable,
        motion: Motion::Reduced,
        scale: Scale::from_percent(150).expect("150% is a scale"),
        ..DesktopSettings::default()
    };
    let posted = DesktopSettings {
        fit: WallpaperFit::Tile,
        sort: IconSort::Date,
        ..DesktopSettings::default()
    }
    .document_of(&SettingsKey::PINBOARD)
    .render();

    let merged = merge(&in_effect, &posted).expect("the pinboard keys are valid");
    assert_eq!(merged.fit, WallpaperFit::Tile);
    assert_eq!(merged.sort, IconSort::Date);
    assert_eq!(merged.appearance, Appearance::Light);
    assert_eq!(merged.contrast, Contrast::High);
    assert_eq!(merged.density, Density::Comfortable);
    assert_eq!(merged.motion, Motion::Reduced);
    assert_eq!(merged.scale.percent(), 150);
}

#[test]
fn a_refused_merge_changes_nothing_at_all() {
    let in_effect = DesktopSettings {
        appearance: Appearance::Light,
        fit: WallpaperFit::Tile,
        ..DesktopSettings::default()
    };
    // The valid key precedes the invalid one, so a half-applying merge
    // would show `Centre` here.
    let refusal = merge(&in_effect, "fit = centre\ncontrast = sepia\n")
        .expect_err("the second value is outside its set");
    assert_eq!(
        refusal,
        DocumentRefusal::InvalidValue(SettingsKey::Contrast)
    );
    assert_eq!(in_effect.fit, WallpaperFit::Tile);
}

#[test]
fn the_two_key_groups_partition_the_registry() {
    // Every key belongs to exactly one group, so a surface that renders
    // its group can never leave a key with no owner or post one twice.
    for key in SettingsKey::ALL {
        let pinboard = SettingsKey::PINBOARD.contains(&key);
        let appearance = SettingsKey::APPEARANCE.contains(&key);
        assert!(pinboard ^ appearance, "{key} is in neither group or both");
    }
    assert_eq!(
        SettingsKey::PINBOARD.len() + SettingsKey::APPEARANCE.len(),
        SettingsKey::ALL.len()
    );
}

#[test]
fn a_group_document_names_only_its_own_keys() {
    let rendered = DesktopSettings::default()
        .document_of(&SettingsKey::APPEARANCE)
        .render();
    for key in SettingsKey::APPEARANCE {
        assert!(rendered.contains(key.name()), "{key} missing");
    }
    for key in SettingsKey::PINBOARD {
        assert!(!rendered.contains(key.name()), "{key} should not be here");
    }
}
