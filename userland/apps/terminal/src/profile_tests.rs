//! Unit tests for the terminal profile document.

use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_appdata::fake::FakeService;
use tairix_appdata::Settings;

use crate::effects::{Effects, FULL, MIN_OPACITY};
use crate::scheme::{Rgb, Scheme, ANSI_COLORS};

use super::{Profile, ProfileKey, DEFAULT_FONT_SIZE_PX, MAX_FONT_SIZE_PX, MIN_FONT_SIZE_PX};

/// The command word the terminal's bundle is installed under.
const OWN_WORD: &str = "terminal";

/// The bundle directory the resolution order finds it at.
const BUNDLE: &str = "/System/Applications/terminal.app";

/// A fake app-data service with an empty store and a bundle shipping no
/// defaults — a fresh account of the shipped terminal.
///
/// The fake speaks the real `appdata-v1` codec and is the one every consumer
/// of the store tests against, so these tests exercise the wire the service
/// actually answers rather than a private idea of it.
fn service() -> FakeService {
    FakeService::for_word(OWN_WORD).with_bundle(BUNDLE)
}

/// A fake service whose committed document holds `text`.
fn holding(text: &str) -> FakeService {
    service().with_store(text)
}

/// The committed value of `key`, if the store holds one.
fn stored<'a>(host: &'a FakeService, key: &str) -> Option<&'a str> {
    host.committed().get(key)
}

/// How many settings the committed document holds.
fn stored_len(host: &FakeService) -> usize {
    host.committed().settings().count()
}

/// A profile with every field moved off its default, so a save has something
/// to write for every key in the registry.
fn edited_profile() -> Profile {
    let mut custom = Scheme::Contrast.palette().expect("contrast has a palette");
    custom.background = Rgb::new(0x10, 0x20, 0x30);
    custom.foreground = Rgb::new(0xf0, 0xe0, 0xd0);
    custom.cursor = Rgb::new(0x01, 0x02, 0x03);
    custom.cursor_text = Rgb::new(0x04, 0x05, 0x06);
    custom.ansi = [Rgb::new(0x7f, 0x00, 0x11); ANSI_COLORS];
    Profile {
        scheme: Scheme::Contrast,
        font_size_px: DEFAULT_FONT_SIZE_PX + 2,
        effects: Effects {
            opacity: MIN_OPACITY,
            blur: 250,
            scanlines: 125,
            fuzz: 375,
            phosphor: 625,
            wobble: 875,
        },
        custom,
    }
}

// --- Profile::default -------------------------------------------------------

#[test]
fn default_profile_is_system_scheme_default_size_and_effects_off() {
    let profile = Profile::default();
    assert_eq!(profile.scheme, Scheme::System);
    assert_eq!(profile.font_size_px, DEFAULT_FONT_SIZE_PX);
    assert_eq!(profile.effects, Effects::default());
    assert_eq!(profile.effects.opacity, FULL);
}

// --- Profile::load / save / clear -------------------------------------------

#[test]
fn an_empty_store_loads_the_documented_defaults() {
    // A fresh account, and a store the user cleared, are the same thing: every
    // key falls back to its documented default.
    let mut host = service();
    let settings = Settings::open(&mut host, OWN_WORD);
    let (profile, refused) = Profile::load(&settings);
    assert_eq!(profile, Profile::default());
    assert!(refused.is_empty());
}

#[test]
fn a_saved_profile_loads_back_identically() {
    let mut host = service();
    let edited = edited_profile();
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        edited.save(&mut settings).expect("publishes");
    }
    let settings = Settings::open(&mut host, OWN_WORD);
    let (loaded, refused) = Profile::load(&settings);
    assert_eq!(loaded, edited);
    assert!(refused.is_empty());
}

#[test]
fn a_save_writes_only_what_the_layers_do_not_already_imply() {
    // The rule that keeps a user's own document holding what the user changed:
    // a key already at its effective value is not written.
    let mut host = service();
    let profile = Profile {
        font_size_px: DEFAULT_FONT_SIZE_PX + 3,
        ..Profile::default()
    };
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        profile.save(&mut settings).expect("publishes");
    }
    assert_eq!(stored_len(&host), 1, "one key changed, one key written");
    assert_eq!(stored(&host, ProfileKey::FontSize.name()), Some("17"));
    assert_eq!(stored(&host, ProfileKey::Scheme.name()), None);
}

#[test]
fn saving_an_unchanged_profile_writes_nothing_at_all() {
    let mut host = holding("scheme = contrast\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (loaded, _) = Profile::load(&settings);
    drop(settings);

    let mut settings = Settings::open(&mut host, OWN_WORD);
    loaded.save(&mut settings).expect("a no-op save");
    assert!(!settings.is_dirty());
    drop(settings);
    assert_eq!(
        stored(&host, ProfileKey::Scheme.name()),
        Some("contrast"),
        "and the one stored key is untouched"
    );
    assert_eq!(stored_len(&host), 1);
}

#[test]
fn a_stored_value_the_registry_refuses_is_named_and_costs_only_itself() {
    // One broken setting must not cost the user every other one, and must not
    // silently become a value they cannot account for.
    let mut host = holding("scheme = contrast\nfont.size = enormous\neffects.blur = 250\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (profile, refused) = Profile::load(&settings);
    assert_eq!(refused, [ProfileKey::FontSize]);
    assert_eq!(
        profile.font_size_px, DEFAULT_FONT_SIZE_PX,
        "the refused field is at its default"
    );
    assert_eq!(profile.scheme, Scheme::Contrast, "and the rest applied");
    assert_eq!(profile.effects.blur, 250);
}

#[test]
fn an_out_of_range_value_is_refused_rather_than_clamped() {
    // Clamping would hide the mistake from whoever wrote it.
    let mut host = holding(&alloc::format!("font.size = {}\n", MAX_FONT_SIZE_PX + 1));
    let settings = Settings::open(&mut host, OWN_WORD);
    let (profile, refused) = Profile::load(&settings);
    assert_eq!(refused, [ProfileKey::FontSize]);
    assert_eq!(profile.font_size_px, DEFAULT_FONT_SIZE_PX);
}

#[test]
fn a_malformed_colour_is_refused_and_named() {
    let mut host = holding("custom.background = #112233\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (_, refused) = Profile::load(&settings);
    assert_eq!(
        refused,
        [ProfileKey::CustomBackground],
        "a colour is bare rrggbb, because `#` begins a comment in the format"
    );
}

#[test]
fn a_custom_palette_needs_exactly_sixteen_colours() {
    for text in [
        "custom.ansi = 000000\n",
        &alloc::format!("custom.ansi = {}\n", "000000 ".repeat(ANSI_COLORS + 1)),
    ] {
        let mut host = holding(text);
        let settings = Settings::open(&mut host, OWN_WORD);
        let (_, refused) = Profile::load(&settings);
        assert_eq!(refused, [ProfileKey::CustomAnsi], "for `{text}`");
    }
}

#[test]
fn clear_removes_the_users_opinions_so_the_layers_beneath_apply() {
    // *Restore defaults* means "stop having an opinion", not "write the
    // shipped values into my document".
    let mut host = service();
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        edited_profile().save(&mut settings).expect("publishes");
    }
    assert!(stored_len(&host) > 1);

    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        Profile::clear(&mut settings).expect("clears");
    }
    assert_eq!(stored_len(&host), 0, "the user's document holds nothing");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (profile, refused) = Profile::load(&settings);
    assert_eq!(profile, Profile::default());
    assert!(refused.is_empty());
}

#[test]
fn the_bundles_shipped_defaults_fill_what_the_user_has_not_set() {
    // The layer this application does not own: the bundle's own
    // `DefaultSettings/`, beneath the user's document and the machine's policy.
    let mut host = FakeService::for_word(OWN_WORD)
        .with_defaults(BUNDLE, "scheme = contrast\neffects.blur = 750\n")
        .with_store("effects.blur = 250\n");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (profile, refused) = Profile::load(&settings);
    assert!(refused.is_empty());
    assert_eq!(
        profile.effects.blur, 250,
        "the user's own choice outranks the shipped default"
    );
    assert_eq!(
        profile.scheme,
        Scheme::Contrast,
        "and the shipped default fills what the user never set"
    );
}

#[test]
fn clearing_falls_back_to_the_shipped_defaults_not_this_apps_compiled_ones() {
    let mut host = FakeService::for_word(OWN_WORD)
        .with_defaults(BUNDLE, "font.size = 20\n")
        .with_store("font.size = 30\n");
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        Profile::clear(&mut settings).expect("clears");
    }
    assert_eq!(stored_len(&host), 0, "the user's document holds nothing");
    let settings = Settings::open(&mut host, OWN_WORD);
    let (profile, _) = Profile::load(&settings);
    assert_eq!(profile.font_size_px, 20);
}

#[test]
fn a_save_never_writes_a_value_the_shipped_defaults_already_give() {
    let mut host = FakeService::for_word(OWN_WORD).with_defaults(BUNDLE, "font.size = 20\n");
    let profile = Profile {
        font_size_px: 20,
        ..Profile::default()
    };
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        profile.save(&mut settings).expect("nothing to publish");
    }
    assert_eq!(
        stored_len(&host),
        0,
        "a value the bundle already ships is not copied into the user's file"
    );
}

#[test]
fn a_key_outside_the_registry_is_left_alone_by_a_save() {
    // The store's key namespace is open; this application's is closed. A key
    // it does not read is not one it may destroy.
    let mut host = holding("scheme = contrast\nsomething.else = kept\n");
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        edited_profile().save(&mut settings).expect("publishes");
    }
    assert_eq!(
        stored(&host, "something.else"),
        Some("kept"),
        "a foreign key survives this application's save"
    );
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        Profile::clear(&mut settings).expect("clears");
    }
    assert_eq!(
        stored(&host, "something.else"),
        Some("kept"),
        "and its clear"
    );
}

#[test]
fn an_unreachable_store_loads_the_defaults_and_refuses_the_save() {
    let mut host = service();
    host.refusal().set(Some(Errno::DeviceOffline));
    let mut settings = Settings::open(&mut host, OWN_WORD);
    let (profile, refused) = Profile::load(&settings);
    assert_eq!(profile, Profile::default(), "the app still runs");
    assert!(refused.is_empty());
    assert_eq!(
        edited_profile().save(&mut settings),
        Err(Errno::DeviceOffline),
        "and is never told a save landed that did not"
    );
}

#[test]
fn every_registry_key_round_trips_through_the_store() {
    // The compiler forces a new key into `ProfileKey::ALL`; this forces it to
    // actually survive a save and a load, so a key that is written but not
    // read back cannot slip through.
    let mut host = service();
    let edited = edited_profile();
    {
        let mut settings = Settings::open(&mut host, OWN_WORD);
        edited.save(&mut settings).expect("publishes");
    }
    assert_eq!(
        stored_len(&host),
        ProfileKey::ALL.len(),
        "every key differs from its default in this fixture, so every key was written"
    );
    let settings = Settings::open(&mut host, OWN_WORD);
    let (loaded, refused) = Profile::load(&settings);
    assert!(refused.is_empty());
    for key in ProfileKey::ALL {
        assert!(
            stored(&host, key.name()).is_some(),
            "{key:?} was not written under `{}`",
            key.name()
        );
    }
    assert_eq!(loaded, edited);
}

#[test]
fn every_registry_key_has_a_distinct_dotted_name() {
    let mut seen: Vec<&str> = Vec::new();
    for key in ProfileKey::ALL {
        let name = key.name();
        assert!(!seen.contains(&name), "{name} is used twice");
        assert_eq!(
            tairix_appconf::validate_key(name),
            Ok(()),
            "{name} is outside the store's key grammar"
        );
        seen.push(name);
    }
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
