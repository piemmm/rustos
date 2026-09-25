//! The five species a figure may be, and what each may look like.
//!
//! Every species stands on the one humanoid skeleton, so every clip plays on
//! every species. A species is data rather than a rig: the range each build
//! setting spans, the forms each feature may take, and the swatches each
//! palette slot draws from. A record names a species and positions within
//! it; this module is what those positions mean.
//!
//! # Why the palette is swatches and not free colours
//!
//! Every colour a figure can wear is first-party and reviewed, so a hostile
//! client cannot paint a figure the desktop cannot draw legibly — and a
//! record spends one byte per slot rather than three. The one colour every
//! figure wears whatever its palette, the trousers, is chosen to clear both
//! desktop themes on its own, so no choice of the other five can leave a
//! figure unreadable against either.

use tairix_raster::Color;
use tairix_util::mathf;

use crate::identity::{EarForm, HornForm, Setting, TailForm};

/// Which of the five peoples a figure belongs to.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Species {
    /// The reference the others are measured against.
    Human,
    /// Taller and slighter, with pointed ears.
    Elf,
    /// Short, broad, and large-headed.
    Dwarf,
    /// Furred, with an animal's ears, often its tail, and sometimes horns.
    Beastkin,
    /// Scaled, horned and tailed.
    Dragonkin,
}

/// A documented interval a setting spans: its lowest setting stands at
/// [`Self::low`], its highest at [`Self::high`], and every one between is a
/// straight step along it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Bounds {
    low: f64,
    high: f64,
}

impl Bounds {
    /// The interval from `low` to `high`.
    #[must_use]
    pub const fn new(low: f64, high: f64) -> Self {
        Self { low, high }
    }

    /// Where its lowest setting stands.
    #[must_use]
    pub const fn low(self) -> f64 {
        self.low
    }

    /// Where its highest setting stands.
    #[must_use]
    pub const fn high(self) -> f64 {
        self.high
    }

    /// Where `setting` stands in it: exactly [`Self::low`] and
    /// [`Self::high`] at its two ends, and never outside them.
    #[must_use]
    pub fn at(self, setting: Setting) -> f64 {
        // The span itself rounds, so the step across it can land a rounding
        // past the high end; the clamp absorbs only that.
        mathf::clamp(
            self.low + (self.high - self.low) * setting.fraction(),
            self.low,
            self.high,
        )
    }
}

/// The intervals a species' build spans.
///
/// Every interval is a factor on the reference human's own proportion, bar
/// `taper`, which is how much broader the shoulders are than the hips as a
/// fraction of either — so zero is neither and every species' range says
/// which way it leans.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Ranges {
    /// Standing height, against the reference's.
    pub height: Bounds,
    /// How heavy the body and limbs are, against the reference's.
    pub girth: Bounds,
    /// Shoulders against hips.
    pub taper: Bounds,
    /// Limb length against the trunk, against the reference's proportion.
    pub limbs: Bounds,
    /// Head size against the body, against the reference's proportion.
    pub head: Bounds,
}

/// What a hair style's volume setting spans, for every species: the factor on
/// the hair's own girth.
pub const VOLUME: Bounds = Bounds::new(0.90, 1.25);

/// Odds of sixteen in sixteen, the unit a species' odds of carrying a form
/// are stated in.
pub const CERTAIN: u8 = 16;

/// The forms a species may carry of a feature it can go without, and how
/// often, in sixteenths, it carries one.
///
/// The one number says both whether it may go without — under [`CERTAIN`] —
/// and whether it carries one at all — over none — so no species can state
/// odds its own forms contradict.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Forms<T: 'static> {
    forms: &'static [T],
    often: u8,
}

impl<T: Copy + PartialEq> Forms<T> {
    /// None, ever.
    const NEVER: Self = Self {
        forms: &[],
        often: 0,
    };

    /// One of `forms`, always.
    const fn always(forms: &'static [T]) -> Self {
        Self {
            forms,
            often: CERTAIN,
        }
    }

    /// One of `forms`, `often` sixteenths of the time.
    const fn sometimes(forms: &'static [T], often: u8) -> Self {
        Self { forms, often }
    }

    /// Whether the odds and the forms agree: forms to carry exactly where
    /// there is a chance of carrying one, and no chance past certain.
    const fn consistent(self) -> bool {
        self.often <= CERTAIN && (self.often == 0) == self.forms.is_empty()
    }

    /// The forms it carries, when it carries one.
    #[must_use]
    pub const fn forms(self) -> &'static [T] {
        self.forms
    }

    /// How often, in sixteenths, a figure of the species carries one.
    #[must_use]
    pub const fn often(self) -> u8 {
        self.often
    }

    /// Whether a figure of the species may have `form`: one of its forms, or
    /// none where it may go without.
    #[must_use]
    pub fn admits(self, form: Option<T>) -> bool {
        match form {
            None => self.often < CERTAIN,
            Some(form) => self.often > 0 && self.forms.contains(&form),
        }
    }

    /// Every choice it admits, going without first where it may.
    pub fn admitted(self) -> impl Iterator<Item = Option<T>> + Clone {
        (self.often < CERTAIN)
            .then_some(None)
            .into_iter()
            .chain(self.forms.iter().map(|form| Some(*form)))
    }
}

const _: () = {
    let mut index = 0;
    while index < Species::ALL.len() {
        let species = Species::ALL[index];
        assert!(
            species.horns().consistent() && species.tails().consistent(),
            "a species' odds of carrying a form contradict its forms"
        );
        index += 1;
    }
};

impl Species {
    /// Every species, in the order [`Self::byte`] numbers them.
    pub const ALL: [Self; 5] = [
        Self::Human,
        Self::Elf,
        Self::Dwarf,
        Self::Beastkin,
        Self::Dragonkin,
    ];

    /// Its stable name, for a ledger row or a diagnostic.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Elf => "elf",
            Self::Dwarf => "dwarf",
            Self::Beastkin => "beastkin",
            Self::Dragonkin => "dragonkin",
        }
    }

    /// The byte a record spells it as.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Human => 0,
            Self::Elf => 1,
            Self::Dwarf => 2,
            Self::Beastkin => 3,
            Self::Dragonkin => 4,
        }
    }

    /// The species `byte` spells, if it spells one.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Human),
            1 => Some(Self::Elf),
            2 => Some(Self::Dwarf),
            3 => Some(Self::Beastkin),
            4 => Some(Self::Dragonkin),
            _ => None,
        }
    }

    /// The intervals its build spans.
    #[must_use]
    pub const fn ranges(self) -> Ranges {
        match self {
            Self::Human => Ranges {
                height: Bounds::new(0.92, 1.08),
                girth: Bounds::new(0.86, 1.24),
                taper: Bounds::new(-0.10, 0.12),
                limbs: Bounds::new(0.94, 1.06),
                head: Bounds::new(0.94, 1.06),
            },
            Self::Elf => Ranges {
                height: Bounds::new(1.00, 1.12),
                girth: Bounds::new(0.82, 1.02),
                taper: Bounds::new(-0.08, 0.06),
                limbs: Bounds::new(1.00, 1.10),
                head: Bounds::new(0.92, 1.02),
            },
            Self::Dwarf => Ranges {
                height: Bounds::new(0.70, 0.80),
                girth: Bounds::new(1.10, 1.36),
                taper: Bounds::new(-0.02, 0.14),
                limbs: Bounds::new(0.82, 0.92),
                head: Bounds::new(1.04, 1.16),
            },
            Self::Beastkin => Ranges {
                height: Bounds::new(0.88, 1.06),
                girth: Bounds::new(0.84, 1.16),
                taper: Bounds::new(-0.10, 0.10),
                limbs: Bounds::new(0.96, 1.08),
                head: Bounds::new(0.94, 1.06),
            },
            Self::Dragonkin => Ranges {
                height: Bounds::new(0.98, 1.14),
                girth: Bounds::new(0.96, 1.28),
                taper: Bounds::new(0.00, 0.16),
                limbs: Bounds::new(0.96, 1.06),
                head: Bounds::new(0.92, 1.04),
            },
        }
    }

    /// The ear forms it may have.
    #[must_use]
    pub const fn ears(self) -> &'static [EarForm] {
        match self {
            Self::Human | Self::Dwarf => &[EarForm::Round],
            Self::Elf => &[EarForm::Pointed, EarForm::Long],
            Self::Beastkin => &[EarForm::Upright, EarForm::Lop],
            Self::Dragonkin => &[EarForm::Pointed, EarForm::Finned],
        }
    }

    /// The horns it may have, and how often it has them.
    #[must_use]
    pub const fn horns(self) -> Forms<HornForm> {
        match self {
            Self::Human | Self::Elf | Self::Dwarf => Forms::NEVER,
            Self::Beastkin => Forms::sometimes(&[HornForm::Nubs, HornForm::Curled], 3),
            Self::Dragonkin => Forms::always(&[
                HornForm::Nubs,
                HornForm::Swept,
                HornForm::Curled,
                HornForm::Spire,
            ]),
        }
    }

    /// The tails it may have, and how often it has one.
    #[must_use]
    pub const fn tails(self) -> Forms<TailForm> {
        match self {
            Self::Human | Self::Elf | Self::Dwarf => Forms::NEVER,
            Self::Beastkin => Forms::sometimes(&[TailForm::Brush, TailForm::Slender], 12),
            Self::Dragonkin => Forms::always(&[TailForm::Scaled]),
        }
    }

    /// The swatches its skin, fur or scale draws from.
    #[must_use]
    pub const fn covering(self) -> &'static [Color] {
        match self {
            Self::Human | Self::Elf | Self::Dwarf => &SKIN,
            Self::Beastkin => &FUR,
            Self::Dragonkin => &SCALE,
        }
    }

    /// The eye colours it may have, as indices into [`EYES`].
    #[must_use]
    pub const fn eyes(self) -> &'static [u8] {
        match self {
            Self::Human | Self::Dwarf => &[0, 1, 2, 4, 5, 6],
            Self::Elf => &[1, 2, 4, 5, 6, 7, 10],
            Self::Beastkin => &[1, 2, 3, 4, 5, 8],
            Self::Dragonkin => &[3, 4, 8, 9, 10, 11],
        }
    }

    /// The swatches its markings draw from; empty for a species with none.
    #[must_use]
    pub const fn markings(self) -> &'static [Color] {
        match self {
            Self::Human | Self::Elf | Self::Dwarf => &[],
            Self::Beastkin => &FUR_MARKINGS,
            Self::Dragonkin => &HORN,
        }
    }
}

/// Human, elven and dwarven skin, pale to dark.
pub const SKIN: [Color; 12] = [
    Color::rgb(0xF6, 0xDC, 0xC8),
    Color::rgb(0xEE, 0xCC, 0xB0),
    Color::rgb(0xE8, 0xBC, 0x98),
    Color::rgb(0xDC, 0xAE, 0x88),
    Color::rgb(0xCE, 0x9E, 0x78),
    Color::rgb(0xC0, 0x8C, 0x64),
    Color::rgb(0xA8, 0x7A, 0x58),
    Color::rgb(0x96, 0x66, 0x44),
    Color::rgb(0x80, 0x54, 0x36),
    Color::rgb(0x6A, 0x44, 0x2C),
    Color::rgb(0x54, 0x36, 0x24),
    Color::rgb(0x40, 0x2A, 0x1C),
];

/// Beastkin fur: snow, cream, sand, tawny, ginger, russet, umber, slate, ash
/// and sable.
pub const FUR: [Color; 10] = [
    Color::rgb(0xEE, 0xEA, 0xE0),
    Color::rgb(0xE4, 0xD2, 0xAE),
    Color::rgb(0xCC, 0xAC, 0x7C),
    Color::rgb(0xC0, 0x8A, 0x4E),
    Color::rgb(0xC2, 0x6E, 0x2E),
    Color::rgb(0x96, 0x4E, 0x26),
    Color::rgb(0x6E, 0x4A, 0x30),
    Color::rgb(0x7A, 0x7C, 0x80),
    Color::rgb(0x5A, 0x56, 0x54),
    Color::rgb(0x2E, 0x28, 0x26),
];

/// Dragonkin scale: ivory, pale jade, moss, bronze, copper, crimson, slate
/// blue, ash, obsidian and umber.
pub const SCALE: [Color; 10] = [
    Color::rgb(0xE6, 0xDC, 0xC4),
    Color::rgb(0xB8, 0xC8, 0xA8),
    Color::rgb(0x7C, 0x8E, 0x5C),
    Color::rgb(0xB0, 0x84, 0x50),
    Color::rgb(0xA8, 0x5C, 0x3C),
    Color::rgb(0x8C, 0x34, 0x2C),
    Color::rgb(0x5C, 0x6C, 0x84),
    Color::rgb(0x74, 0x70, 0x6C),
    Color::rgb(0x34, 0x30, 0x38),
    Color::rgb(0x5E, 0x44, 0x34),
];

/// Hair, for every species: platinum, flaxen, golden, strawberry, copper,
/// auburn, light brown, brown, dark brown, black, blue-black, silver, white,
/// grey, frost and plum.
pub const HAIR: [Color; 16] = [
    Color::rgb(0xEC, 0xE4, 0xCC),
    Color::rgb(0xE0, 0xC8, 0x8C),
    Color::rgb(0xD0, 0xA4, 0x50),
    Color::rgb(0xC8, 0x7C, 0x48),
    Color::rgb(0xB0, 0x58, 0x2C),
    Color::rgb(0x80, 0x3C, 0x24),
    Color::rgb(0x9A, 0x70, 0x48),
    Color::rgb(0x6C, 0x4A, 0x30),
    Color::rgb(0x46, 0x30, 0x22),
    Color::rgb(0x22, 0x1E, 0x1C),
    Color::rgb(0x1E, 0x24, 0x34),
    Color::rgb(0xB8, 0xBC, 0xC4),
    Color::rgb(0xF2, 0xF0, 0xEC),
    Color::rgb(0x80, 0x80, 0x84),
    Color::rgb(0x9C, 0xB8, 0xD4),
    Color::rgb(0x5C, 0x34, 0x50),
];

/// Eye colours, which each species admits a subset of: dark brown, brown,
/// hazel, amber, green, blue, grey, violet, gold, red, ice and black.
pub const EYES: [Color; 12] = [
    Color::rgb(0x3C, 0x26, 0x18),
    Color::rgb(0x66, 0x40, 0x24),
    Color::rgb(0x80, 0x6A, 0x34),
    Color::rgb(0xC0, 0x84, 0x2C),
    Color::rgb(0x4C, 0x80, 0x48),
    Color::rgb(0x44, 0x6C, 0xB0),
    Color::rgb(0x7C, 0x88, 0x94),
    Color::rgb(0x7A, 0x52, 0xB0),
    Color::rgb(0xD8, 0xB0, 0x38),
    Color::rgb(0xB8, 0x30, 0x28),
    Color::rgb(0xA8, 0xD4, 0xE8),
    Color::rgb(0x16, 0x14, 0x14),
];

/// Beastkin markings — an inner ear, a tail's tip, a horn: white, cream,
/// pink, tan, ginger, brown, black and grey.
pub const FUR_MARKINGS: [Color; 8] = [
    Color::rgb(0xF2, 0xF0, 0xEA),
    Color::rgb(0xE8, 0xD8, 0xB8),
    Color::rgb(0xE0, 0xA8, 0xA0),
    Color::rgb(0xC8, 0x9C, 0x68),
    Color::rgb(0xC0, 0x68, 0x2C),
    Color::rgb(0x6A, 0x46, 0x2E),
    Color::rgb(0x24, 0x20, 0x1E),
    Color::rgb(0x88, 0x88, 0x88),
];

/// Dragonkin horn: ivory, bone, amber, bronze, slate, ebony, crimson and
/// jade.
pub const HORN: [Color; 8] = [
    Color::rgb(0xEA, 0xE0, 0xC8),
    Color::rgb(0xD2, 0xC4, 0xA0),
    Color::rgb(0xB8, 0x8C, 0x4C),
    Color::rgb(0x9C, 0x70, 0x40),
    Color::rgb(0x6C, 0x70, 0x78),
    Color::rgb(0x2C, 0x28, 0x28),
    Color::rgb(0x84, 0x2C, 0x28),
    Color::rgb(0x5C, 0x84, 0x6C),
];

/// The dyes a tunic is coloured with, for every species: slate, crimson,
/// forest, royal, ochre, plum, teal, rust, charcoal, bone, sky, moss, wine,
/// sand, snow and amber.
pub const DYES: [Color; 16] = [
    Color::rgb(0x4C, 0x5A, 0x6E),
    Color::rgb(0x8C, 0x24, 0x24),
    Color::rgb(0x2E, 0x5E, 0x38),
    Color::rgb(0x2C, 0x3E, 0x8C),
    Color::rgb(0xB0, 0x84, 0x2C),
    Color::rgb(0x5E, 0x2E, 0x5A),
    Color::rgb(0x1E, 0x6A, 0x6C),
    Color::rgb(0x9C, 0x48, 0x22),
    Color::rgb(0x30, 0x30, 0x34),
    Color::rgb(0xD8, 0xCC, 0xB0),
    Color::rgb(0x6C, 0x9C, 0xC8),
    Color::rgb(0x6A, 0x74, 0x3C),
    Color::rgb(0x5C, 0x1C, 0x2C),
    Color::rgb(0xC4, 0xAC, 0x80),
    Color::rgb(0xE8, 0xEC, 0xEE),
    Color::rgb(0xC8, 0x8A, 0x2C),
];

/// The undyed cloth of the trousers every figure wears.
///
/// A slate whose relative luminance is about seven hundredths — the middle
/// of the narrow band that clears two-to-one contrast against both the dark
/// desktop and the mid-grey light one. It is what keeps any palette readable:
/// the art harness holds this tone to both desktops on its own, and the legs
/// are always a substantial share of the figure.
pub const TROUSERS: Color = Color::rgb(0x44, 0x4C, 0x58);

/// Boot leather.
pub const LEATHER: Color = Color::rgb(0x4A, 0x36, 0x24);

#[cfg(test)]
#[path = "species/tests.rs"]
mod tests;
