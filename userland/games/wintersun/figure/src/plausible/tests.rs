//! That a plausible figure is a record, that the whole of what a species
//! admits is reachable, and that the fields a person chooses together
//! follow one another.
//!
//! Every draw here is from a fixed seed, so each statistic is one number
//! computed the same way every run: a bound on it can fail only because the
//! generator changed.

use alloc::vec::Vec;

use tairix_rng::{NonCryptoRng, RandU64};
use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use super::{figure, HAIRED};
use crate::humanoid;
use crate::identity::{EyeShape, FaceShape, HairStyle, Identity, Spec};
use crate::pose::Pose;
use crate::reference::Reference;
use crate::rig::{Placement, Stance};
use crate::species::{Species, DYES, HAIR};

/// How many figures each statistic is taken over, per species.
const DRAWS: u64 = 4096;

/// `DRAWS` figures of `species`, one per seed.
fn drawn(species: Species) -> Vec<Spec> {
    (0..DRAWS)
        .map(|seed| {
            figure(species, &mut NonCryptoRng::seed_from_u64(seed))
                .expect("every draw is a record")
                .spec()
        })
        .collect()
}

/// The sample correlation of two series.
fn correlation(pairs: impl Iterator<Item = (f64, f64)> + Clone) -> f64 {
    let count = pairs.clone().count();
    #[allow(clippy::cast_precision_loss, reason = "a sample count")]
    let n = count as f64;
    let (mean_x, mean_y) = pairs
        .clone()
        .fold((0.0, 0.0), |(x, y), (a, b)| (x + a / n, y + b / n));
    let (mut xy, mut xx, mut yy) = (0.0, 0.0, 0.0);
    for (a, b) in pairs {
        xy += (a - mean_x) * (b - mean_y);
        xx += (a - mean_x) * (a - mean_x);
        yy += (b - mean_y) * (b - mean_y);
    }
    xy / mathf::sqrt(xx * yy)
}

/// A setting of a figure, by the field it reads.
type Read = fn(&Spec) -> u8;

/// The five build settings.
const BUILD: [(&str, Read); 5] = [
    ("height", |s| s.build.height.0),
    ("girth", |s| s.build.girth.0),
    ("taper", |s| s.build.taper.0),
    ("limbs", |s| s.build.limbs.0),
    ("head", |s| s.build.head.0),
];

fn share(specs: &[Spec], holds: impl Fn(&Spec) -> bool) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "a sample count")]
    let share = specs.iter().filter(|spec| holds(spec)).count() as f64 / specs.len() as f64;
    share
}

/// Every draw is a record, and one that builds and places a figure.
#[test]
fn every_draw_is_a_record_that_builds_and_places() {
    let mut placement = Placement::new();
    let light = Reference::light().expect("a real light");
    for species in Species::ALL {
        for seed in 0..512 {
            let identity = figure(species, &mut NonCryptoRng::seed_from_u64(seed))
                .expect("every draw is a record");
            assert_eq!(identity.species(), species);
            assert_eq!(Identity::decode(&identity.encode()), Ok(identity));
            let rig = humanoid::rig(&identity).expect("every record builds");
            let stance = Stance::new(Facing(0x2000), 1.0, (40.0, 90.0), light).expect("a stance");
            humanoid::rigging(&rig)
                .expect("it binds")
                .posture(&Pose::REST)
                .expect("rest is posturable")
                .place(&stance, &[], &mut placement)
                .expect("every record places");
        }
    }
}

/// A seed draws one figure, and takes the same number of draws doing it, so
/// a caller drawing again from where it left off draws the same next figure.
#[test]
fn a_seed_draws_one_figure() {
    for species in Species::ALL {
        for seed in [0, 1, 0xDEAD_BEEF, u64::MAX] {
            let (mut one, mut other) = (
                NonCryptoRng::seed_from_u64(seed),
                NonCryptoRng::seed_from_u64(seed),
            );
            assert_eq!(figure(species, &mut one), figure(species, &mut other));
            assert_eq!(one.next_u64(), other.next_u64());
        }
    }
}

#[test]
fn different_seeds_draw_different_figures() {
    for species in Species::ALL {
        let specs = drawn(species);
        let mut records: Vec<_> = specs
            .iter()
            .map(|spec| Identity::new(*spec).expect("a record").encode())
            .collect();
        records.sort_unstable();
        records.dedup();
        assert!(
            records.len() * 100 >= specs.len() * 99,
            "{species:?} drew only {} distinct figures in {}",
            records.len(),
            specs.len()
        );
    }
}

/// Nothing a species admits is out of reach: every form, every swatch, and
/// the outer eighth of every setting at both ends.
#[test]
fn everything_a_species_admits_is_drawn() {
    for species in Species::ALL {
        let specs = drawn(species);
        let seen = |holds: &dyn Fn(&Spec) -> bool| specs.iter().any(holds);
        for face in FaceShape::ALL {
            assert!(seen(&|s| s.features.face == *face), "{species:?} {face:?}");
        }
        for eyes in EyeShape::ALL {
            assert!(seen(&|s| s.features.eyes == *eyes), "{species:?} {eyes:?}");
        }
        for ears in species.ears() {
            assert!(seen(&|s| s.features.ears == *ears), "{species:?} {ears:?}");
        }
        for horns in species.horns() {
            assert!(
                seen(&|s| s.features.horns == *horns),
                "{species:?} {horns:?}"
            );
        }
        for tail in species.tails() {
            assert!(seen(&|s| s.features.tail == *tail), "{species:?} {tail:?}");
        }
        for hair in HairStyle::ALL
            .iter()
            .map(|style| Some(*style))
            .chain([None])
        {
            assert!(seen(&|s| s.features.hair == hair), "{species:?} {hair:?}");
        }
        for skin in 0..species.covering().len() {
            assert!(
                seen(&|s| usize::from(s.palette.skin) == skin),
                "{species:?} skin {skin}"
            );
        }
        for eyes in species.eyes() {
            assert!(
                seen(&|s| s.palette.eyes == *eyes),
                "{species:?} eyes {eyes}"
            );
        }
        for markings in 0..species.markings().len() {
            assert!(
                seen(&|s| usize::from(s.palette.markings) == markings),
                "{species:?} markings {markings}"
            );
        }
        for accent in 0..DYES.len() {
            assert!(
                seen(&|s| usize::from(s.palette.accent) == accent),
                "{species:?} accent {accent}"
            );
        }
        for hair in 0..HAIR.len() {
            assert!(
                seen(&|s| s.features.hair.is_some() && usize::from(s.palette.hair) == hair),
                "{species:?} hair colour {hair}"
            );
        }
        for (name, setting) in BUILD {
            assert!(seen(&|s| setting(s) < 32), "{species:?} {name} low");
            assert!(seen(&|s| setting(s) >= 224), "{species:?} {name} high");
        }
        let volume = |s: &Spec| s.features.hair.map(|_| s.features.volume.0);
        assert!(
            seen(&|s| volume(s).is_some_and(|v| v < 32)),
            "{species:?} thin hair"
        );
        assert!(
            seen(&|s| volume(s).is_some_and(|v| v >= 224)),
            "{species:?} full hair"
        );
    }
}

/// A setting is likeliest at the middle of its interval and seldom at an
/// end, where a uniform draw would put an eighth of every figure's settings
/// within a sixteenth of one.
#[test]
fn a_setting_is_likeliest_at_its_middle_and_rare_at_an_end() {
    for species in Species::ALL {
        let specs = drawn(species);
        for (name, setting) in BUILD {
            let middle = share(&specs, |s| (64..192).contains(&setting(s)));
            let end = share(&specs, |s| !(16..240).contains(&setting(s)));
            assert!(
                middle >= 0.75,
                "{species:?} {name}: {middle} in the middle half"
            );
            assert!(end <= 0.02, "{species:?} {name}: {end} at an end");
        }
    }
}

/// A heavy build leans broad-shouldered, and a tall one long-limbed and
/// small-headed for its height; settings no one chooses together do not
/// move together.
#[test]
fn a_build_leans_the_way_a_person_would_choose_it() {
    for species in Species::ALL {
        let specs = drawn(species);
        let pair = |one: Read, other: Read| {
            correlation(
                specs
                    .iter()
                    .map(move |s| (f64::from(one(s)), f64::from(other(s)))),
            )
        };
        let [(_, height), (_, girth), (_, taper), (_, limbs), (_, head)] = BUILD;
        let broad = pair(girth, taper);
        let long = pair(height, limbs);
        let small = pair(height, head);
        let apart = pair(height, girth);
        assert!(broad >= 0.35, "{species:?} girth and taper: {broad}");
        assert!(long >= 0.25, "{species:?} height and limbs: {long}");
        assert!(small <= -0.25, "{species:?} height and head: {small}");
        assert!(
            mathf::fabs(apart) <= 0.1,
            "{species:?} height and girth: {apart}"
        );
    }
}

/// Hair and markings lean toward the lightness of what they grow from, so
/// a pale figure has pale markings.
#[test]
fn a_pale_figure_leans_to_pale_hair_and_markings() {
    for species in Species::ALL {
        let specs = drawn(species);
        let tone = |s: &Spec| f64::from(species.covering()[usize::from(s.palette.skin)].luma());
        let haired: Vec<_> = specs.iter().filter(|s| s.features.hair.is_some()).collect();
        let hair = correlation(
            haired
                .iter()
                .map(|s| (tone(s), f64::from(HAIR[usize::from(s.palette.hair)].luma()))),
        );
        assert!(hair >= 0.2, "{species:?} hair against covering: {hair}");
        if !species.markings().is_empty() {
            let markings = correlation(specs.iter().map(|s| {
                (
                    tone(s),
                    f64::from(species.markings()[usize::from(s.palette.markings)].luma()),
                )
            }));
            assert!(
                markings >= 0.3,
                "{species:?} markings against covering: {markings}"
            );
        }
    }
}

/// The tunic leans away from the lightness of the skin it adjoins: on
/// average it sits a quarter as far again from it as a dye chosen at random.
#[test]
fn the_tunic_leans_away_from_the_skin_beside_it() {
    for species in Species::ALL {
        let specs = drawn(species);
        let (mut drawn_gap, mut uniform_gap) = (0.0, 0.0);
        for spec in &specs {
            let tone = species.covering()[usize::from(spec.palette.skin)].luma();
            drawn_gap += f64::from(DYES[usize::from(spec.palette.accent)].luma().abs_diff(tone));
            #[allow(clippy::cast_precision_loss, reason = "a table length")]
            let dyes = DYES.len() as f64;
            uniform_gap += DYES
                .iter()
                .map(|dye| f64::from(dye.luma().abs_diff(tone)))
                .sum::<f64>()
                / dyes;
        }
        assert!(
            drawn_gap >= 1.25 * uniform_gap,
            "{species:?}: the tunic sat {drawn_gap} from the skin against {uniform_gap}"
        );
    }
}

/// A form a species may go without is carried as often as the species is
/// described as carrying it: beastkin often tailed and seldom horned, and
/// every species usually haired.
#[test]
fn a_species_carries_what_it_is_described_as_carrying() {
    let near = |measured: f64, sixteenths: u8| {
        mathf::fabs(measured - f64::from(sixteenths) / 16.0) <= 0.03
    };
    for species in Species::ALL {
        let specs = drawn(species);
        let haired = share(&specs, |s| s.features.hair.is_some());
        assert!(near(haired, HAIRED), "{species:?} haired {haired}");
    }
    let beastkin = drawn(Species::Beastkin);
    let tailed = share(&beastkin, |s| s.features.tail.is_some());
    let horned = share(&beastkin, |s| s.features.horns.is_some());
    assert!(near(tailed, 12), "beastkin tailed {tailed}");
    assert!(near(horned, 3), "beastkin horned {horned}");
    let dragonkin = drawn(Species::Dragonkin);
    assert!(dragonkin
        .iter()
        .all(|s| s.features.horns.is_some() && s.features.tail.is_some()));
}
