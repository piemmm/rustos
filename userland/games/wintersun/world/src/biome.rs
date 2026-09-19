//! Materials, and the Whittaker classification that blends them.
//!
//! A cell does not have *a* biome. It has a **normalised weight vector**
//! over the material set, so a boundary between boreal forest and fell
//! heath is a gradient the splat renderer draws as one rather than a line
//! it has to hide. The vector is kept to the widest blend the renderer
//! can splat in one pass, and its weights are integers summing to exactly
//! [`WEIGHT_TOTAL`] — so "the weights are normalised" is a property of the
//! type, not a convention a consumer has to trust.
//!
//! # Why the memberships are quadratic and not Gaussian
//!
//! A Gaussian needs an exponential, which `no_std` does not have and a
//! platform libm would not compute identically on every target. The
//! compactly-supported quadratic kernel used here — `1 - d²` inside a
//! radius, zero outside — gives the same soft boundary, is two multiplies,
//! and has the useful property that a material's influence genuinely
//! *ends* rather than merely becoming small.

use tairix_util::mathf;

use crate::geom::{quantise_u8, Moisture, Temperature};

/// Weights in a blend, summing to [`WEIGHT_TOTAL`].
///
/// Four is what the splat pass can take in one go, and more than four
/// materials meeting at one cell is a boundary of boundaries that no
/// renderer would resolve anyway.
pub const BLEND_SLOTS: usize = 4;

/// What a blend's weights sum to, always.
pub const WEIGHT_TOTAL: u16 = 255;

/// A ground material.
///
/// The discriminants are the identifiers a stored world edit carries, so
/// they are frozen: a new material takes a new number and never reuses a
/// retired one. Held as a byte and widened at the wire, because four of
/// these sit in every cell of every cached chunk and the wire's width is
/// the wire's business.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum Material {
    /// Open water: sea, lake, or a river's bed.
    Water = 0,
    /// Permanent ice.
    Glacier = 1,
    /// Lying snow.
    Snowfield = 2,
    /// Frozen ground, low scrub.
    Tundra = 3,
    /// Exposed upland heath.
    FellHeath = 4,
    /// Dry cold grassland.
    ColdSteppe = 5,
    /// Spruce and pine.
    BorealForest = 6,
    /// Broadleaf woodland.
    TemperateForest = 7,
    /// Wet peat.
    Moor = 8,
    /// Tidal grass and mud.
    Saltmarsh = 9,
    /// Volcanic ash and clinker.
    Ashland = 10,
    /// Ground the world was torn through.
    RiftWaste = 11,
    /// Bare rock.
    Rock = 12,
    /// River gravel and scree.
    Gravel = 13,
    /// Beach and dune sand.
    Sand = 14,
}

impl Material {
    /// Every material, in discriminant order.
    pub const ALL: [Self; 15] = [
        Self::Water,
        Self::Glacier,
        Self::Snowfield,
        Self::Tundra,
        Self::FellHeath,
        Self::ColdSteppe,
        Self::BorealForest,
        Self::TemperateForest,
        Self::Moor,
        Self::Saltmarsh,
        Self::Ashland,
        Self::RiftWaste,
        Self::Rock,
        Self::Gravel,
        Self::Sand,
    ];

    /// The identifier a stored world edit carries for this material.
    #[must_use]
    pub const fn id(self) -> u16 {
        self as u16
    }
}

/// A cell's materials and their weights.
///
/// Weights sum to [`WEIGHT_TOTAL`] and are ordered heaviest first. A slot
/// with zero weight is unused, and its material is meaningless.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Blend {
    materials: [Material; BLEND_SLOTS],
    weights: [u8; BLEND_SLOTS],
}

impl Blend {
    /// A blend of one material.
    #[must_use]
    pub fn solid(material: Material) -> Self {
        let mut weights = [0; BLEND_SLOTS];
        #[allow(
            clippy::cast_possible_truncation,
            reason = "WEIGHT_TOTAL is 255, which is a u8"
        )]
        {
            weights[0] = WEIGHT_TOTAL as u8;
        }
        Self {
            materials: [material; BLEND_SLOTS],
            weights,
        }
    }

    /// The materials, heaviest first.
    #[must_use]
    pub const fn materials(&self) -> &[Material; BLEND_SLOTS] {
        &self.materials
    }

    /// Their weights, in the same order.
    #[must_use]
    pub const fn weights(&self) -> &[u8; BLEND_SLOTS] {
        &self.weights
    }

    /// The heaviest material — the label, for a consumer that wants one.
    #[must_use]
    pub const fn dominant(&self) -> Material {
        self.materials[0]
    }

    /// The weights' sum, which is always [`WEIGHT_TOTAL`].
    #[must_use]
    pub fn total(&self) -> u16 {
        self.weights.iter().map(|&w| u16::from(w)).sum()
    }
}

/// Everything a classification reads about one cell.
#[derive(Copy, Clone, Debug)]
pub struct Conditions {
    /// Air temperature.
    pub temperature: Temperature,
    /// Relative moisture.
    pub moisture: Moisture,
    /// Height above sea level, in world units.
    pub elevation_units: f64,
    /// Steepest local gradient, as a rise over one cell.
    pub slope: f64,
    /// How strongly the cell sits in a mountain belt, `0.0..1.0`.
    pub belt: f64,
    /// Whether standing water covers it.
    pub submerged: bool,
    /// How close the cell is to standing water, `0.0` at it through `1.0`
    /// well away from it.
    pub dryness: f64,
}

/// One material's place in the Whittaker plane.
struct Climate {
    material: Material,
    /// Centre temperature, in degrees Celsius.
    celsius: f64,
    /// Centre moisture, as a fraction of saturation.
    moisture: f64,
    /// Temperature radius, beyond which the material does not appear.
    celsius_span: f64,
    /// Moisture radius, likewise.
    moisture_span: f64,
}

/// Where each climate-driven material sits.
///
/// Cold-biased, as the realm's name promises: seven of the nine centres
/// are at or below ten degrees.
const CLIMATES: [Climate; 9] = [
    Climate {
        material: Material::Glacier,
        celsius: -24.0,
        moisture: 0.55,
        celsius_span: 14.0,
        moisture_span: 0.85,
    },
    Climate {
        material: Material::Snowfield,
        celsius: -13.0,
        moisture: 0.5,
        celsius_span: 11.0,
        moisture_span: 0.8,
    },
    Climate {
        material: Material::Tundra,
        celsius: -6.0,
        moisture: 0.42,
        celsius_span: 10.0,
        moisture_span: 0.55,
    },
    Climate {
        material: Material::FellHeath,
        celsius: -1.0,
        moisture: 0.58,
        celsius_span: 9.0,
        moisture_span: 0.45,
    },
    Climate {
        material: Material::ColdSteppe,
        celsius: 3.0,
        moisture: 0.2,
        celsius_span: 12.0,
        moisture_span: 0.34,
    },
    Climate {
        material: Material::BorealForest,
        celsius: 4.0,
        moisture: 0.68,
        celsius_span: 10.0,
        moisture_span: 0.42,
    },
    Climate {
        material: Material::Moor,
        celsius: 7.0,
        moisture: 0.92,
        celsius_span: 12.0,
        moisture_span: 0.34,
    },
    Climate {
        material: Material::TemperateForest,
        celsius: 13.0,
        moisture: 0.62,
        celsius_span: 12.0,
        moisture_span: 0.45,
    },
    Climate {
        material: Material::Ashland,
        celsius: 21.0,
        moisture: 0.12,
        celsius_span: 14.0,
        moisture_span: 0.3,
    },
];

/// Slope, as a rise over one cell, at which bare rock wholly replaces
/// whatever would otherwise grow.
const ROCK_SLOPE: f64 = 2.6;

/// Belt strength above which torn ground shows through.
const RIFT_BELT: f64 = 0.62;

/// How close to water, on the dryness scale, sand and saltmarsh reach.
const SHORE_REACH: f64 = 0.34;

/// Classify one cell.
#[must_use]
pub fn classify(site: Conditions) -> Blend {
    if site.submerged {
        return Blend::solid(Material::Water);
    }

    let mut raw = [0.0_f64; Material::ALL.len()];
    let celsius = site.temperature.celsius();
    let damp = site.moisture.fraction();

    for climate in &CLIMATES {
        let dt = (celsius - climate.celsius) / climate.celsius_span;
        let dm = (damp - climate.moisture) / climate.moisture_span;
        let membership = 1.0 - (dt * dt + dm * dm);
        if membership > 0.0 {
            raw[climate.material as usize] += membership * membership;
        }
    }

    // Slope wins over climate: nothing grows on a face, and a steep
    // hillside is scree before it is soil.
    let steep = mathf::clamp(site.slope / ROCK_SLOPE, 0.0, 1.0);
    raw[Material::Rock as usize] += steep * steep * 2.4;
    raw[Material::Gravel as usize] += steep * (1.0 - steep) * 1.1;

    // A shore is either sand or marsh, depending on how cold and how flat
    // it is; a river's bed is gravel.
    let shore = mathf::clamp(1.0 - site.dryness / SHORE_REACH, 0.0, 1.0);
    if shore > 0.0 {
        let frozen = mathf::clamp((4.0 - celsius) / 12.0, 0.0, 1.0);
        let flat = 1.0 - mathf::clamp(site.slope, 0.0, 1.0);
        raw[Material::Saltmarsh as usize] += shore * flat * frozen * 1.3;
        raw[Material::Sand as usize] += shore * (1.0 - frozen) * 1.2;
        raw[Material::Gravel as usize] += shore * (1.0 - flat) * 0.7;
    }

    // Ground the plates tore open, which the realm's story turns on.
    if site.belt > RIFT_BELT && site.elevation_units < 90.0 {
        raw[Material::RiftWaste as usize] += (site.belt - RIFT_BELT) * 4.0;
    }

    // Permanent ice above the snow line, whatever the Whittaker plane
    // says: altitude has already cooled the temperature, but a high
    // plateau should read as ice rather than as merely cold heath.
    if celsius < -8.0 {
        raw[Material::Glacier as usize] += (-8.0 - celsius) / 10.0;
    }

    blend(&raw)
}

/// Take the heaviest [`BLEND_SLOTS`] materials and normalise them to
/// [`WEIGHT_TOTAL`].
fn blend(raw: &[f64; Material::ALL.len()]) -> Blend {
    let mut chosen = [(0.0_f64, Material::Rock); BLEND_SLOTS];
    let mut taken = [false; Material::ALL.len()];

    for slot in &mut chosen {
        let mut best = (0.0_f64, usize::MAX);
        for (index, &weight) in raw.iter().enumerate() {
            // Strictly greater, walking in discriminant order, so a tie
            // resolves to the lower discriminant everywhere.
            if !taken[index] && weight > best.0 {
                best = (weight, index);
            }
        }
        if best.1 == usize::MAX {
            break;
        }
        taken[best.1] = true;
        *slot = (best.0, Material::ALL[best.1]);
    }

    let total: f64 = chosen.iter().map(|&(weight, _)| weight).sum();
    if total <= 0.0 {
        // Nothing claimed the cell. Bare rock is the honest answer, and it
        // keeps the sum exact rather than leaving an unnormalised blend.
        return Blend::solid(Material::Rock);
    }

    // Largest remainder, so the integer weights sum to exactly the total
    // rather than to whatever rounding each slot happened to give.
    let mut weights = [0_u16; BLEND_SLOTS];
    let mut remainders = [(0_i64, 0_usize); BLEND_SLOTS];
    let mut assigned = 0_u16;
    for (slot, &(weight, _)) in chosen.iter().enumerate() {
        let exact = weight / total * f64::from(WEIGHT_TOTAL);
        let floor = mathf::floor(exact);
        weights[slot] = u16::from(quantise_u8(floor));
        assigned += weights[slot];
        let remainder = i64::from(mathf::round_i32((exact - floor) * 1_048_576.0));
        remainders[slot] = (-remainder, slot);
    }
    remainders.sort_unstable();
    let mut cursor = 0;
    while assigned < WEIGHT_TOTAL {
        let (_, slot) = remainders[cursor % BLEND_SLOTS];
        weights[slot] += 1;
        assigned += 1;
        cursor += 1;
    }

    // The remainder pass can hand a +1 to a slot whose floor tied with the
    // one above it, so the heaviest-first order is restored here rather
    // than assumed. The original slot is the tiebreak, which keeps the
    // classifier's own ordering where the integer weights are equal.
    let mut ordered = [(0_u16, 0_usize); BLEND_SLOTS];
    for (slot, weight) in weights.iter().copied().enumerate() {
        ordered[slot] = (u16::MAX - weight, slot);
    }
    ordered.sort_unstable();

    let mut materials = [Material::Rock; BLEND_SLOTS];
    let mut out = [0_u8; BLEND_SLOTS];
    for (rank, &(_, slot)) in ordered.iter().enumerate() {
        materials[rank] = chosen[slot].1;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the weights sum to WEIGHT_TOTAL, so none exceeds 255"
        )]
        {
            out[rank] = weights[slot] as u8;
        }
    }
    Blend {
        materials,
        weights: out,
    }
}

#[cfg(test)]
mod tests;
