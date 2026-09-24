//! Figures drawn at random that look as if somebody chose them.
//!
//! Drawing every field of a record uniformly produces monsters: a build with
//! settings pulling in unrelated directions, a tunic the colour of the skin
//! beside it, markings that fight the fur they sit on. A plausible figure
//! draws each field from a distribution of its own and lets the fields a
//! person would choose together follow one another:
//!
//! - a build setting is a bell about the middle of its species' interval, so
//!   either end is reachable and rare;
//! - a heavy build leans broad-shouldered, and a tall one long-limbed and
//!   small-headed for its height;
//! - hair and markings lean toward the lightness of the skin, fur or scale
//!   they grow from, so a pale figure has pale markings;
//! - the tunic leans away from the lightness of the skin it adjoins, so the
//!   two read as two;
//! - a form a species may go without is carried as often as the species is
//!   described as carrying it.
//!
//! Every value is drawn from what the species admits, so every draw is a
//! record; and nothing here touches a float, so a seed draws the same figure
//! on every target.
//!
//! # Which generator
//!
//! The generator is injected as any [`RandU64`], and the predictable tier,
//! [`tairix_rng::NonCryptoRng`], is the one to inject. A plausible figure is
//! one a player could have built by hand, and the server checks whatever
//! record it is sent, so predicting the next draw gains an observer nothing —
//! while drawing a figure again from its seed is exactly what the art grid
//! and the tests need. An unpredictable generator would cost ten times as
//! much per draw to protect nothing.

use tairix_raster::Color;
use tairix_rng::RandU64;

use crate::identity::{
    Build, EyeShape, FaceShape, Features, Field, HairStyle, Identity, IdentityError, Palette,
    Setting, Spec,
};
use crate::species::{Species, DYES, HAIR};

/// Draw a plausible figure of `species` from `rng`.
///
/// # Errors
///
/// None for the shipped species: every value is drawn from what the species
/// admits, and the crate's tests hold every draw to a record. A species
/// table edited empty is refused by the field it would have supplied.
pub fn figure<R: RandU64 + ?Sized>(
    species: Species,
    rng: &mut R,
) -> Result<Identity, IdentityError> {
    let build = build(rng);
    let features = features(species, rng)?;
    let palette = palette(species, features, rng)?;
    Identity::new(Spec {
        species,
        build,
        features,
        palette,
    })
}

/// The span of a bell draw: three uniform bytes summed.
const SPAN: i32 = 3 * 255;

/// The middle of a bell draw, rounded down.
const MIDDLE: i32 = SPAN / 2;

/// How much of its own spread a setting that follows another keeps, in
/// eighths.
const OWN: i32 = 7;

/// How far taper leans with girth, in eighths of girth's offset from the
/// middle: a heavy build is a broad-shouldered one.
const BROAD: i32 = 4;

/// How far limb length leans with height: a tall figure is long in the leg.
const LONG: i32 = 3;

/// How far head size leans against height: a tall figure's head is smaller
/// for its stature.
const SMALL: i32 = -3;

fn build<R: RandU64 + ?Sized>(rng: &mut R) -> Build {
    let height = bell(rng);
    let girth = bell(rng);
    let taper = follow(rng, girth, BROAD);
    let limbs = follow(rng, height, LONG);
    let head = follow(rng, height, SMALL);
    Build {
        height: setting(height),
        girth: setting(girth),
        taper: setting(taper),
        limbs: setting(limbs),
        head: setting(head),
    }
}

/// A draw across `0..=SPAN`, likeliest at its middle.
///
/// Three uniform bytes summed: the middle half of an interval takes about
/// six draws in seven, and either end about one in several hundred.
fn bell<R: RandU64 + ?Sized>(rng: &mut R) -> i32 {
    byte(rng) + byte(rng) + byte(rng)
}

/// A bell draw leaning `eighths` of the way with `lead`'s offset from the
/// middle.
fn follow<R: RandU64 + ?Sized>(rng: &mut R, lead: i32, eighths: i32) -> i32 {
    MIDDLE + (lead - MIDDLE) * eighths / 8 + (bell(rng) - MIDDLE) * OWN / 8
}

/// The setting a bell draw stands for.
fn setting(drawn: i32) -> Setting {
    Setting(u8::try_from(drawn.clamp(0, SPAN) / 3).unwrap_or(u8::MAX))
}

/// How often, in sixteenths, a figure has hair: no species is described as
/// going bald, so all but a few do.
const HAIRED: u8 = 14;

/// How often, in sixteenths, a species carries horns and a tail.
///
/// Beastkin are described as often tailed and never as horned. Every other
/// species always or never carries each, which its admitted forms say too.
const fn carried(species: Species) -> (u8, u8) {
    match species {
        Species::Human | Species::Elf | Species::Dwarf => (0, 0),
        Species::Beastkin => (3, 12),
        Species::Dragonkin => (16, 16),
    }
}

fn features<R: RandU64 + ?Sized>(species: Species, rng: &mut R) -> Result<Features, IdentityError> {
    let (horned, tailed) = carried(species);
    let face = uniform(rng, FaceShape::ALL).ok_or(IdentityError::Unknown(Field::Face))?;
    let eyes = uniform(rng, EyeShape::ALL).ok_or(IdentityError::Unknown(Field::Eyes))?;
    let ears = uniform(rng, species.ears()).ok_or(IdentityError::NotOfSpecies(Field::Ears))?;
    let horns = optional(rng, species.horns(), horned, Field::Horns)?;
    let tail = optional(rng, species.tails(), tailed, Field::Tail)?;
    let hair = if chance(rng, HAIRED) {
        Some(uniform(rng, HairStyle::ALL).ok_or(IdentityError::Unknown(Field::Hair))?)
    } else {
        None
    };
    let volume = match hair {
        Some(_) => setting(bell(rng)),
        None => Setting::LOW,
    };
    Ok(Features {
        face,
        eyes,
        ears,
        horns,
        tail,
        hair,
        volume,
    })
}

fn palette<R: RandU64 + ?Sized>(
    species: Species,
    features: Features,
    rng: &mut R,
) -> Result<Palette, IdentityError> {
    let covering = species.covering();
    let skin = below(rng, covering.len());
    let tone = covering
        .get(skin)
        .ok_or(IdentityError::Unknown(Field::SkinColour))?
        .luma();
    let hair = match features.hair {
        Some(_) => near(rng, &HAIR, tone),
        None => 0,
    };
    let eyes = uniform(rng, species.eyes()).ok_or(IdentityError::NotOfSpecies(Field::EyeColour))?;
    let markings = near(rng, species.markings(), tone);
    let accent = away(rng, &DYES, tone);
    Ok(Palette {
        skin: u8::try_from(skin).map_err(|_| IdentityError::Unknown(Field::SkinColour))?,
        hair,
        eyes,
        markings,
        accent,
    })
}

/// How far in lightness a swatch may sit from the tone it follows and still
/// be favoured.
const REACH: u32 = 128;

/// The weight every swatch keeps however it sits against the tone, so no
/// swatch is out of a draw's reach.
const FLOOR: u32 = 8;

/// A swatch of `table` leaning toward `tone`'s lightness; zero for an empty
/// table.
fn near<R: RandU64 + ?Sized>(rng: &mut R, table: &[Color], tone: u8) -> u8 {
    weighted(rng, table, |luma| {
        REACH.saturating_sub(u32::from(luma.abs_diff(tone))) + FLOOR
    })
}

/// A swatch of `table` leaning away from `tone`'s lightness.
fn away<R: RandU64 + ?Sized>(rng: &mut R, table: &[Color], tone: u8) -> u8 {
    weighted(rng, table, |luma| u32::from(luma.abs_diff(tone)) + FLOOR)
}

/// A swatch of `table`, each as likely as `weigh` makes its lightness; zero
/// for an empty table, without a draw.
fn weighted<R: RandU64 + ?Sized>(rng: &mut R, table: &[Color], weigh: impl Fn(u8) -> u32) -> u8 {
    let total: u32 = table.iter().map(|swatch| weigh(swatch.luma())).sum();
    if total == 0 {
        return 0;
    }
    let mut left = rng.next_below(u64::from(total));
    for (index, swatch) in table.iter().enumerate() {
        let weight = u64::from(weigh(swatch.luma()));
        if left < weight {
            return u8::try_from(index).unwrap_or(0);
        }
        left -= weight;
    }
    0
}

/// One of `admitted`: a form with `odds` sixteenths' likelihood where it
/// holds both a form and none, and otherwise whichever it holds.
///
/// An empty list admits nothing, and is refused as `field`.
fn optional<R: RandU64 + ?Sized, T: Copy>(
    rng: &mut R,
    admitted: &[Option<T>],
    odds: u8,
    field: Field,
) -> Result<Option<T>, IdentityError> {
    let refused = IdentityError::NotOfSpecies(field);
    let forms = admitted.iter().flatten().count();
    let bare = admitted.iter().any(Option::is_none);
    if forms == 0 {
        return if bare { Ok(None) } else { Err(refused) };
    }
    if bare && !chance(rng, odds) {
        return Ok(None);
    }
    admitted
        .iter()
        .flatten()
        .nth(below(rng, forms))
        .copied()
        .map(Some)
        .ok_or(refused)
}

/// Whether a draw lands inside `sixteenths` of sixteen.
fn chance<R: RandU64 + ?Sized>(rng: &mut R, sixteenths: u8) -> bool {
    rng.next_below(16) < u64::from(sixteenths)
}

/// One of `items`, each as likely as the next; `None` for none.
fn uniform<R: RandU64 + ?Sized, T: Copy>(rng: &mut R, items: &[T]) -> Option<T> {
    items.get(below(rng, items.len())).copied()
}

/// A uniform draw below `bound`, which every caller keeps to a table's
/// length; zero for an empty one.
fn below<R: RandU64 + ?Sized>(rng: &mut R, bound: usize) -> usize {
    let drawn = rng.next_below(u64::try_from(bound).unwrap_or(u64::MAX));
    usize::try_from(drawn).unwrap_or(0)
}

/// A uniform byte.
fn byte<R: RandU64 + ?Sized>(rng: &mut R) -> i32 {
    i32::try_from(rng.next_below(256)).unwrap_or(0)
}

#[cfg(test)]
#[path = "plausible/tests.rs"]
mod tests;
