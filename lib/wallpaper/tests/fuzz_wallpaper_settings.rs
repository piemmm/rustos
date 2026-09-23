//! Deterministic fuzz harness for the pinboard settings registry.
//!
//! The `key = value` *grammar* is `lib/appconf`'s and is fuzzed there; what
//! this harness holds is the **registry** over it, for any bytes the desktop
//! session's published document or a pinboard-channel payload may carry:
//!
//! 1. [`merge`] — the strict reading — never panics on any input, and
//!    every document it accepts yields a total, well-formed settings value.
//! 2. [`DesktopSettings::document`] and [`merge`] are inverses: the
//!    canonical document of accepted settings re-reads equal, and its
//!    rendered text is itself within [`tairix_appconf::MAX_DOCUMENT_LEN`],
//!    so a writer can never emit a document the reader would refuse as too
//!    long.
//! 3. [`DesktopSettings::load`] — the tolerant reading — never panics and
//!    is *total*: whatever a stored document says, every field it does not
//!    accept is left at its documented default and named in the refusal
//!    list, so the two readings agree on every document the strict one
//!    accepts.
//!
//! The generator emits whole setting lines and mutates them at a low rate,
//! so most documents are accepted and the round-trip invariant is genuinely
//! exercised. The second test hammers the reader with arbitrary ASCII.
//!
//! The fixed sweep runs under plain `cargo test`; under `cargo xtask fuzz`
//! the same seeded stream keeps being drawn until the budget elapses.

use tairix_appconf::{Document, MAX_DOCUMENT_LEN};
use tairix_fuzzseed::Prng;
use tairix_wallpaper::{merge, DesktopSettings};

/// Fixed-iteration sweep run when no budget is set.
const SMOKE_ITERATIONS: u64 = 5_000;

/// Every key of the registry, so no `set_field`/`field_value` arm is left
/// unfuzzed. The count is asserted against `SettingsKey::ALL` below, so a
/// key added without a value table here fails rather than silently going
/// uncovered.
const KEYS: &[&str] = &[
    "wallpaper",
    "fit",
    "backdrop",
    "icons",
    "sort",
    "appearance",
    "contrast",
    "density",
    "motion",
    "scale",
    "cursor.set",
    "cursor.size",
];
const WALLPAPER_VALUES: &[&str] = &[
    "none",
    "/System/Graphics/Wallpapers/TAIRiX/tairix-dark.jpg",
    "/Users/ada/Documents/sunset.png",
    // A `#` no longer ends a value: the format engine quotes one, so a file
    // the user really named this way must survive the round trip.
    "/Users/ada/Documents/sunset#2.png",
];
const FIT_VALUES: &[&str] = &["fill", "fit", "stretch", "centre", "tile"];
const BACKDROP_VALUES: &[&str] = &["theme", "112233", "ffffff", "000000"];
const ICONS_VALUES: &[&str] = &["leading", "trailing"];
const SORT_VALUES: &[&str] = &["name", "kind", "size", "date"];
const APPEARANCE_VALUES: &[&str] = &["dark", "light"];
const CONTRAST_VALUES: &[&str] = &["normal", "high", "monochrome"];
const DENSITY_VALUES: &[&str] = &["compact", "normal", "comfortable"];
const MOTION_VALUES: &[&str] = &["full", "reduced"];
const SCALE_VALUES: &[&str] = &["100", "150", "300", "24", "801", "-100", "1e3"];
const CURSOR_SET_VALUES: &[&str] = &["Standard", "High Visibility", "Gone Away", ".."];
const CURSOR_SIZE_VALUES: &[&str] = &["normal", "large", "larger", "largest"];
const BAD_TOKENS: &[&str] = &["", " ", "has space", "bogus", "relative/path.png"];

fn value_for(rng: &mut Prng, key: &str) -> &'static str {
    if rng.below(32) == 0 {
        return rng.pick(BAD_TOKENS);
    }
    match key {
        "wallpaper" => rng.pick(WALLPAPER_VALUES),
        "fit" => rng.pick(FIT_VALUES),
        "backdrop" => rng.pick(BACKDROP_VALUES),
        "icons" => rng.pick(ICONS_VALUES),
        "sort" => rng.pick(SORT_VALUES),
        "appearance" => rng.pick(APPEARANCE_VALUES),
        "contrast" => rng.pick(CONTRAST_VALUES),
        "density" => rng.pick(DENSITY_VALUES),
        "motion" => rng.pick(MOTION_VALUES),
        "scale" => rng.pick(SCALE_VALUES),
        "cursor.set" => rng.pick(CURSOR_SET_VALUES),
        "cursor.size" => rng.pick(CURSOR_SIZE_VALUES),
        _ => unreachable!(),
    }
}

fn document(rng: &mut Prng) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let mut keys: Vec<&str> = KEYS.to_vec();
    let n = rng.below(keys.len() + 1);
    for _ in 0..n {
        let index = rng.below(keys.len());
        let key = keys.swap_remove(index);

        if rng.below(10) == 0 {
            out.push_str("# comment\n");
        }
        let value = value_for(rng, key);
        let _ = writeln!(out, "{key} = {value}");
    }
    out
}

/// Read `doc` both ways and hold every invariant the two readings owe each
/// other. Answers whether the strict reading accepted it.
fn check_round_trip(doc: &str) -> bool {
    // The tolerant reading is total for *every* document the engine can
    // parse, accepted or not, so it is exercised on both branches.
    if let Ok(parsed) = Document::parse(doc) {
        let (lenient, refused) = DesktopSettings::load(&parsed);
        assert!(
            refused.len() <= KEYS.len(),
            "a refusal list longer than the registry"
        );
        // A refused key left its field at the documented default.
        let defaults = DesktopSettings::default();
        for key in refused {
            assert_eq!(
                key.value_of(&lenient),
                key.value_of(&defaults),
                "a refused key did not keep its default"
            );
        }
    }

    let Ok(settings) = merge(&DesktopSettings::default(), doc) else {
        return false;
    };

    let rendered = settings.document().render();
    assert!(
        rendered.len() <= MAX_DOCUMENT_LEN,
        "a rendered document exceeded the document bound"
    );
    let reread =
        merge(&DesktopSettings::default(), &rendered).expect("a rendered document re-reads");
    assert_eq!(settings, reread, "render/merge is not a round trip");
    // The two readings agree on every document the strict one accepts.
    let (lenient, refused) = DesktopSettings::load(&settings.document());
    assert!(refused.is_empty(), "a canonical document refused a key");
    assert_eq!(settings, lenient, "the two readings disagree");
    true
}

/// A key added to the registry without a value table here would be
/// generated by nothing and so fuzzed by nothing, which looks exactly like
/// passing coverage.
#[test]
fn the_generator_names_every_registry_key() {
    let registry: Vec<&str> = tairix_wallpaper::SettingsKey::ALL
        .iter()
        .map(|key| key.name())
        .collect();
    assert_eq!(registry, KEYS, "the generator and the registry disagree");
}

#[test]
fn generated_documents_round_trip_through_the_canonical_render() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "generated_documents_round_trip_through_the_canonical_render",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            check_round_trip(&document(&mut rng));
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn arbitrary_ascii_never_panics() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "arbitrary_ascii_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut buf = String::new();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            buf.clear();
            let len = rng.below(256);
            for _ in 0..len {
                buf.push(char::from(u8::try_from(rng.below(128)).expect("byte fits")));
            }
            let _ = merge(&DesktopSettings::default(), &buf);
            if let Ok(parsed) = Document::parse(&buf) {
                let _ = DesktopSettings::load(&parsed);
            }
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn the_generator_produces_accepted_documents() {
    const DRAWS: u64 = 2_000;
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "the_generator_produces_accepted_documents",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let accepted = (0..DRAWS)
        .filter(|_| check_round_trip(&document(&mut rng)))
        .count();
    assert!(
        u64::try_from(accepted).expect("count fits") * 4 >= DRAWS,
        "only {accepted} of {DRAWS} generated documents parsed; the corpus is degenerate"
    );
}
