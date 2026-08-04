//! Deterministic fuzz harness for the pinboard settings document.
//!
//! Invariants, for any bytes a user's `pinboard.conf` may carry:
//!
//! 1. [`parse`] never panics on any input, and every accepted document
//!    still leaves the settings a total, well-formed value.
//! 2. [`render`] and [`parse`] are inverses: an accepted document
//!    re-renders to text that parses back equal, and the rendered text is
//!    itself within [`MAX_SETTINGS_LEN`], so a writer can never emit a
//!    document the reader would refuse as too long.
//!
//! The generator emits whole setting lines and mutates them at a low rate,
//! so most documents are accepted and the round-trip invariant is
//! genuinely exercised. The second test hammers the parser with arbitrary
//! ASCII.
//!
//! The fixed sweep runs under plain `cargo test`; under `cargo xtask fuzz`
//! the same seeded stream keeps being drawn until the budget elapses.

use tairix_wallpaper::{parse, render, MAX_SETTINGS_LEN};

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
    "/System/Graphics/Wallpapers/tairix-dark.jpg",
    "/Users/ada/Documents/sunset.png",
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
        let _ = writeln!(out, "{key} {value}");
    }
    out
}

fn check_round_trip(doc: &str) -> bool {
    let Ok(settings) = parse(doc) else {
        return false;
    };

    let rendered = render(&settings);
    assert!(
        rendered.len() <= MAX_SETTINGS_LEN,
        "a rendered document exceeded the document bound"
    );
    let reparsed = parse(&rendered).expect("a rendered document re-parses");
    assert_eq!(settings, reparsed, "render/parse is not a round trip");
    true
}

#[test]
fn generated_documents_round_trip_through_render() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "generated_documents_round_trip_through_render",
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
            let _ = parse(&buf);
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
