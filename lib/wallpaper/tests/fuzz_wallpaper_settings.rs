//! Deterministic fuzz harness for the pinboard settings registry.
//!
//! The `key = value` *grammar* is `lib/appconf`'s and is fuzzed there; what
//! this harness holds is the **registry** over it, for any bytes the desktop
//! session's published document or a pinboard-channel payload may carry:
//!
//! 1. [`decode`] — the strict reading — never panics on any input, and
//!    every document it accepts yields a total, well-formed settings value.
//! 2. [`PinboardSettings::document`] and [`decode`] are inverses: the
//!    canonical document of accepted settings re-reads equal, and its
//!    rendered text is itself within [`tairix_appconf::MAX_DOCUMENT_LEN`],
//!    so a writer can never emit a document the reader would refuse as too
//!    long.
//! 3. [`PinboardSettings::load`] — the tolerant reading — never panics and
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
use tairix_wallpaper::{decode, PinboardSettings};

/// Fixed-iteration sweep run when no budget is set.
const SMOKE_ITERATIONS: u64 = 5_000;

/// Deterministic LCG, matching the sibling harnesses.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn chance(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }

    fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        choices[usize::try_from(self.below(choices.len() as u64)).expect("index fits")]
    }
}

const KEYS: &[&str] = &["wallpaper", "fit", "backdrop", "icons", "sort"];
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
const BAD_TOKENS: &[&str] = &["", " ", "has space", "bogus", "relative/path.png"];

fn value_for(rng: &mut Lcg, key: &str) -> &'static str {
    if rng.chance(32) {
        return rng.pick(BAD_TOKENS);
    }
    match key {
        "wallpaper" => rng.pick(WALLPAPER_VALUES),
        "fit" => rng.pick(FIT_VALUES),
        "backdrop" => rng.pick(BACKDROP_VALUES),
        "icons" => rng.pick(ICONS_VALUES),
        "sort" => rng.pick(SORT_VALUES),
        _ => unreachable!(),
    }
}

fn document(rng: &mut Lcg) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let mut keys: Vec<&str> = KEYS.to_vec();
    let n = rng.below(keys.len() as u64 + 1);
    for _ in 0..n {
        let index = usize::try_from(rng.below(keys.len() as u64)).expect("index fits");
        let key = keys.swap_remove(index);

        if rng.chance(10) {
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
        let (lenient, refused) = PinboardSettings::load(&parsed);
        assert!(
            refused.len() <= KEYS.len(),
            "a refusal list longer than the registry"
        );
        // A refused key left its field at the documented default.
        let defaults = PinboardSettings::default();
        for key in refused {
            assert_eq!(
                key.value_of(&lenient),
                key.value_of(&defaults),
                "a refused key did not keep its default"
            );
        }
    }

    let Ok(settings) = decode(doc) else {
        return false;
    };

    let rendered = settings.document().render();
    assert!(
        rendered.len() <= MAX_DOCUMENT_LEN,
        "a rendered document exceeded the document bound"
    );
    let reread = decode(&rendered).expect("a rendered document re-reads");
    assert_eq!(settings, reread, "render/decode is not a round trip");
    // The two readings agree on every document the strict one accepts.
    let (lenient, refused) = PinboardSettings::load(&settings.document());
    assert!(refused.is_empty(), "a canonical document refused a key");
    assert_eq!(settings, lenient, "the two readings disagree");
    true
}

#[test]
fn generated_documents_round_trip_through_the_canonical_render() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
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
    let mut rng = Lcg::new(tairix_fuzzseed::start(
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
            let _ = decode(&buf);
            if let Ok(parsed) = Document::parse(&buf) {
                let _ = PinboardSettings::load(&parsed);
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
    let mut rng = Lcg::new(tairix_fuzzseed::start(
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
