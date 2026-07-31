//! Deterministic fuzz harness for the pinned-shortcut store.
//!
//! Invariants, for any bytes a user's `pins.conf` may carry:
//!
//! 1. [`parse`] never panics on any UTF-8 input, and an accepted document
//!    never yields more than [`MAX_PINS`] pins.
//! 2. [`render`] and [`parse`] are inverses: an accepted list re-renders to
//!    a document that parses back equal, and the rendered text is itself
//!    within [`MAX_PINS_LEN`], so a writer can never emit a store the
//!    reader would refuse as too long.
//! 3. The operations model ([`PinList::pin`], [`pin_at`], [`unpin`],
//!    [`move_pin`]) stays equivalent to a simple `Vec<PinTarget>` oracle
//!    with the same uniqueness and capacity bounds.
//!
//! The generator emits whole pin lines and mutates them at a low rate, so
//! most documents are accepted and the round-trip invariant is genuinely
//! exercised. The second test hammers the parser with arbitrary ASCII. The
//! third test drives the operations model against its oracle.
//!
//! The fixed sweep runs under plain `cargo test`; under `cargo xtask fuzz`
//! the same seeded stream keeps being drawn until the budget elapses.

use tairix_proglib::{BundlePath, EntryId};
use tairix_taskpins::{parse, render, PinKey, PinList, PinTarget, MAX_PINS, MAX_PINS_LEN};

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

const IDS: &[&str] = &[
    "editor",
    "com.example.editor",
    "chess",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
];
const BUNDLES: &[&str] = &[
    "/Apps/A.app",
    "/Apps/B.app",
    "/Apps/C.app",
    "/Apps/D.app",
    "/Apps/E.app",
    "/Apps/F.app",
    "/Apps/G.app",
    "/Apps/H.app",
];
const BAD_TOKENS: &[&str] = &["", " ", "has space", ".leading", "bogus"];

fn target(rng: &mut Lcg) -> PinTarget {
    if rng.chance(2) {
        PinTarget::Entry(EntryId::new(rng.pick(IDS)).expect("id"))
    } else {
        PinTarget::Bundle(BundlePath::new(rng.pick(BUNDLES)).expect("bundle"))
    }
}

fn line(rng: &mut Lcg, target_str: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let is_bundle = target_str.starts_with('/');
    let key = if rng.chance(64) {
        rng.pick(BAD_TOKENS)
    } else if is_bundle {
        PinKey::Bundle.as_str()
    } else {
        PinKey::Entry.as_str()
    };

    let value = if rng.chance(64) {
        rng.pick(BAD_TOKENS)
    } else {
        target_str
    };

    let _ = write!(out, "{key} {value}");
    out
}

fn document(rng: &mut Lcg) -> String {
    let mut out = String::new();
    let mut pool: Vec<&str> = IDS.iter().chain(BUNDLES.iter()).copied().collect();
    let n = rng.below(pool.len() as u64 + 1);
    for _ in 0..n {
        let index = usize::try_from(rng.below(pool.len() as u64)).expect("index fits");
        let target_str = pool.swap_remove(index);

        if rng.chance(10) {
            out.push_str("# comment\n");
        }
        out.push_str(&line(rng, target_str));
        out.push('\n');
    }
    out
}

fn check_round_trip(doc: &str) -> bool {
    let Ok(list) = parse(doc) else {
        return false;
    };
    assert!(list.len() <= MAX_PINS);

    let rendered = render(&list);
    assert!(
        rendered.len() <= MAX_PINS_LEN,
        "a rendered list exceeded the document bound"
    );
    let reparsed = parse(&rendered).expect("a rendered list re-parses");
    assert_eq!(list, reparsed, "render/parse is not a round trip");
    true
}

fn check_operations(rng: &mut Lcg) {
    let mut list = PinList::new();
    let mut oracle: Vec<PinTarget> = Vec::new();

    for _ in 0..100 {
        match rng.below(4) {
            0 => {
                // pin
                let t = target(rng);
                let is_dup = oracle.contains(&t);
                let is_full = oracle.len() >= MAX_PINS;
                let res = list.pin(t.clone());
                if is_dup || is_full {
                    assert!(res.is_err());
                } else {
                    res.expect("pin successful");
                    oracle.push(t);
                }
            }
            1 => {
                // pin_at
                let t = target(rng);
                let idx = usize::try_from(rng.below(MAX_PINS as u64 + 5)).expect("index fits");
                let is_dup = oracle.contains(&t);
                let is_full = oracle.len() >= MAX_PINS;
                let res = list.pin_at(idx, t.clone());
                if is_dup || is_full {
                    assert!(res.is_err());
                } else {
                    res.expect("pin_at successful");
                    let insert_at = std::cmp::min(idx, oracle.len());
                    oracle.insert(insert_at, t);
                }
            }
            2 => {
                // unpin
                let idx = usize::try_from(rng.below(MAX_PINS as u64 + 5)).expect("index fits");
                let res = list.unpin(idx);
                if idx < oracle.len() {
                    assert_eq!(res, Some(oracle.remove(idx)));
                } else {
                    assert_eq!(res, None);
                }
            }
            3 => {
                // move_pin
                let from = usize::try_from(rng.below(MAX_PINS as u64 + 5)).expect("index fits");
                let to = usize::try_from(rng.below(MAX_PINS as u64 + 5)).expect("index fits");
                let res = list.move_pin(from, to);
                if from < oracle.len() {
                    assert!(res);
                    let t = oracle.remove(from);
                    let insert_at = std::cmp::min(to, oracle.len());
                    oracle.insert(insert_at, t);
                } else {
                    assert!(!res);
                }
            }
            _ => unreachable!(),
        }
        assert_eq!(list.len(), oracle.len());
        let list_pins: Vec<_> = list.iter().cloned().collect();
        assert_eq!(list_pins, oracle);
    }
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
fn operations_model_matches_vec_oracle() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "operations_model_matches_vec_oracle",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS / 10 {
            check_operations(&mut rng);
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
