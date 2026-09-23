//! The committed regression corpus: records the decoder admits, built from
//! the crate's own public encoder, plus the seeding and noise the mutating
//! harness draws.
//!
//! The records come from the public encoder, so the corpus cannot disagree
//! with the source of truth: a change to the record's layout moves the
//! corpus with it. A crashing input once found is appended to
//! `regression_corpus.rs` as a byte literal with its own pinned verdict.

// Compiled into each test binary in this directory, and each uses a subset,
// so per-binary `dead_code` reports are false here.
#![allow(dead_code)]

use tairix_fuzzseed::Prng;
use tairix_wintersun_figure::identity::{Build, Identity, Setting, Spec};
use tairix_wintersun_figure::reference::{self, FIGURES};
use tairix_wintersun_figure::species::{Species, DYES, HAIR};

/// The shared generator, seeded and logged per run by `tairix_fuzzseed` so a
/// reported crash replays from the value in its log.
pub fn seeded(name: &str) -> Prng {
    Prng::new(tairix_fuzzseed::start(name, tairix_fuzzseed::FUZZ_SEED_ENV))
}

/// `len` fresh bytes.
pub fn blob(rng: &mut Prng, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    rng.fill(&mut out);
    out
}

/// Every record the corpus holds: each grid figure, and each species at both
/// ends of its build, in the last swatch of every slot, and in every form it
/// may take.
pub fn records() -> Vec<Vec<u8>> {
    let mut specs: Vec<Spec> = FIGURES.iter().map(|figure| figure.spec).collect();
    for species in Species::ALL {
        let base = reference::spec(species);
        for setting in [Setting::LOW, Setting::HIGH] {
            let mut spec = base;
            spec.build = Build {
                height: setting,
                girth: setting,
                taper: setting,
                limbs: setting,
                head: setting,
            };
            specs.push(spec);
        }
        let mut last = base;
        last.palette.skin = last_of(species.covering().len());
        last.palette.hair = last_of(HAIR.len());
        last.palette.eyes = *species.eyes().last().expect("a species has eyes");
        last.palette.markings = last_of(species.markings().len().max(1));
        last.palette.accent = last_of(DYES.len());
        specs.push(last);
        for ears in species.ears() {
            let mut spec = base;
            spec.features.ears = *ears;
            specs.push(spec);
        }
        for horns in species.horns() {
            let mut spec = base;
            spec.features.horns = *horns;
            specs.push(spec);
        }
        for tail in species.tails() {
            let mut spec = base;
            spec.features.tail = *tail;
            specs.push(spec);
        }
    }
    specs
        .into_iter()
        .map(|spec| {
            Identity::new(spec)
                .expect("a corpus record is a real one")
                .encode()
                .to_vec()
        })
        .collect()
}

/// The last index of a table `len` long.
fn last_of(len: usize) -> u8 {
    u8::try_from(len - 1).expect("a small table")
}
