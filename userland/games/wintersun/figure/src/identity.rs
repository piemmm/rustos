//! A figure's identity: the validated record a character is drawn from.
//!
//! What a character looks like is a species, a build, a set of features and
//! a palette — nineteen bytes — and never geometry. Geometry is first-party
//! code; a record only chooses among it and positions within documented
//! ranges. [`Identity`] is that record once it has been checked, and a figure
//! is built from nothing else.
//!
//! # The record is untrusted input
//!
//! A character's record arrives off the wire from a client assumed hostile,
//! and a realm stores one per character. Its decoder is therefore total —
//! every byte string answers a record or a named refusal, and nothing panics
//! — and it fails closed: a byte naming nothing, a feature the species does
//! not carry, or a second spelling of a figure is refused rather than
//! repaired. A server re-validating a submitted figure runs exactly this.
//!
//! # One spelling of every figure
//!
//! A build setting is a byte spanning its species' whole documented range,
//! so no setting is out of range and none needs clamping. What is left is
//! spelled once: a bald figure stores zero for its hair colour and volume,
//! and a species without markings stores zero for its markings, so two
//! records that draw the same figure are the same bytes.

use core::fmt;

use crate::species::{Species, DYES, EYES, HAIR, LEATHER, TROUSERS, VOLUME};
use crate::tint::{Tint, Tints};

/// How many bytes a record is.
pub const RECORD_LEN: usize = 19;

/// The record format this build reads and writes.
///
/// Carried in the first byte so a record written by a different format is
/// refused rather than misread.
pub const RECORD_VERSION: u8 = 1;

/// The extension a record is shipped under as a file of its own: the presets
/// in the game's bundle, and every file the harness measures them from.
pub const RECORD_EXTENSION: &str = "figure";

/// A position within a documented interval: zero is its low end and
/// [`Self::HIGH`] its high end.
///
/// Every value is a position, so a setting has no invalid value and a
/// decoder has nothing to range-check.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Setting(pub u8);

impl Setting {
    /// The interval's low end.
    pub const LOW: Self = Self(0);

    /// The interval's high end.
    pub const HIGH: Self = Self(u8::MAX);

    /// How far along the interval it sits, in `0..=1`.
    #[must_use]
    pub fn fraction(self) -> f64 {
        f64::from(self.0) / f64::from(u8::MAX)
    }
}

/// Where a figure sits within its species' build.
///
/// Each setting spans the interval [`Species::ranges`] documents for it, so
/// a dwarf's highest height is still a dwarf's.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Build {
    /// Standing height.
    pub height: Setting,
    /// How heavy the body and limbs are.
    pub girth: Setting,
    /// Shoulders against hips.
    pub taper: Setting,
    /// Limb length against the trunk.
    pub limbs: Setting,
    /// Head size against the body.
    pub head: Setting,
}

/// The shape of a face, set by its jaw and chin.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum FaceShape {
    /// The reference.
    Oval = 0,
    /// A full jaw and a short chin.
    Round = 1,
    /// A long chin.
    Long = 2,
    /// A square, heavy jaw.
    Broad = 3,
    /// A narrow, pointed chin.
    Heart = 4,
}

/// The shape of the eyes.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum EyeShape {
    /// As tall as wide.
    Round = 0,
    /// Wider than tall.
    Almond = 1,
    /// A slit.
    Narrow = 2,
}

/// The form of the ears.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum EarForm {
    /// Close to the head.
    Round = 0,
    /// Rising to a point.
    Pointed = 1,
    /// Swept up and back to a long point.
    Long = 2,
    /// An animal's, upright on the crown.
    Upright = 3,
    /// An animal's, hanging at the side.
    Lop = 4,
    /// A swept fin.
    Finned = 5,
}

/// The form of a pair of horns.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum HornForm {
    /// Short stubs.
    Nubs = 0,
    /// Swept back over the head.
    Swept = 1,
    /// Curled round beside the ears.
    Curled = 2,
    /// Rising straight.
    Spire = 3,
}

/// The form of a tail.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum TailForm {
    /// Thick and bushy, with a marked tip.
    Brush = 0,
    /// Long and thin, with a marked tip.
    Slender = 1,
    /// Heavy at the root and tapering.
    Scaled = 2,
}

/// How the hair is cut.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum HairStyle {
    /// Close to the scalp on top.
    Cropped = 0,
    /// Covering the crown and the back of the head.
    Short = 1,
    /// Full and loose.
    Shaggy = 2,
    /// Cropped, with a knot at the crown.
    Topknot = 3,
}

/// What a figure's head and body carry beyond its build.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Features {
    /// The face.
    pub face: FaceShape,
    /// The eyes' shape; their colour is the palette's.
    pub eyes: EyeShape,
    /// The ears.
    pub ears: EarForm,
    /// Horns, if any.
    pub horns: Option<HornForm>,
    /// A tail, if any.
    pub tail: Option<TailForm>,
    /// The hair, or `None` for none at all.
    pub hair: Option<HairStyle>,
    /// How full the hair is, across [`VOLUME`]; zero when there is none.
    pub volume: Setting,
}

/// Which swatch each of a figure's colours is.
///
/// Each is an index into the table its slot draws from — the species' own
/// for skin and markings, the shared ones for hair, eyes and cloth — so a
/// figure can only ever wear a colour somebody chose for it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Palette {
    /// Into [`Species::covering`].
    pub skin: u8,
    /// Into [`HAIR`]; zero for a figure with no hair.
    pub hair: u8,
    /// Into [`EYES`], among [`Species::eyes`].
    pub eyes: u8,
    /// Into [`Species::markings`]; zero for a species with none.
    pub markings: u8,
    /// Into [`DYES`].
    pub accent: u8,
}

/// A record as written, before it is checked.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Spec {
    /// The species.
    pub species: Species,
    /// Where it sits within its species' build.
    pub build: Build,
    /// What it carries.
    pub features: Features,
    /// What colours it wears.
    pub palette: Palette,
}

impl Spec {
    /// Whether the record leaves `field` nothing to choose, so a record holds
    /// zero there: a bald figure's hair colour and volume, and the markings
    /// of a species with none.
    #[must_use]
    pub fn fixed(&self, field: Field) -> bool {
        match field {
            Field::Volume | Field::HairColour => self.features.hair.is_none(),
            Field::MarkingsColour => self.species.markings().is_empty(),
            Field::Species
            | Field::Height
            | Field::Girth
            | Field::Taper
            | Field::Limbs
            | Field::Head
            | Field::Face
            | Field::Eyes
            | Field::Ears
            | Field::Horns
            | Field::Tail
            | Field::Hair
            | Field::SkinColour
            | Field::EyeColour
            | Field::AccentColour => false,
        }
    }
}

/// A checked record: a figure that can be drawn.
///
/// Its one constructor refuses anything else, so a figure built from an
/// `Identity` has no impossible case to meet.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Identity {
    spec: Spec,
}

/// One field of a record, as a refusal names it or an edit sets it.
///
/// Declared in the order a record spells its fields, which is the order
/// [`Self::ALL`] lists them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Field {
    /// The species.
    Species,
    /// Standing height.
    Height,
    /// How heavy the body and limbs are.
    Girth,
    /// Shoulders against hips.
    Taper,
    /// Limb length against the trunk.
    Limbs,
    /// Head size against the body.
    Head,
    /// The face shape.
    Face,
    /// The eye shape.
    Eyes,
    /// The ear form.
    Ears,
    /// The horns.
    Horns,
    /// The tail.
    Tail,
    /// The hair style.
    Hair,
    /// The hair volume.
    Volume,
    /// The skin, fur or scale swatch.
    SkinColour,
    /// The hair swatch.
    HairColour,
    /// The eye swatch.
    EyeColour,
    /// The markings swatch.
    MarkingsColour,
    /// The cloth swatch.
    AccentColour,
}

impl Field {
    /// Every field, in the order a record spells them.
    pub const ALL: [Self; 18] = [
        Self::Species,
        Self::Height,
        Self::Girth,
        Self::Taper,
        Self::Limbs,
        Self::Head,
        Self::Face,
        Self::Eyes,
        Self::Ears,
        Self::Horns,
        Self::Tail,
        Self::Hair,
        Self::Volume,
        Self::SkinColour,
        Self::HairColour,
        Self::EyeColour,
        Self::MarkingsColour,
        Self::AccentColour,
    ];

    /// Its position in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Why a record was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IdentityError {
    /// Fewer bytes than a record holds.
    Truncated,
    /// More bytes than a record holds.
    TrailingBytes,
    /// A record format this build does not read.
    UnknownVersion,
    /// A byte naming no value of its field: a species past the last, a
    /// swatch past its table.
    Unknown(Field),
    /// A value its species does not carry: a tail on a human, a dragon's
    /// eyes on an elf.
    NotOfSpecies(Field),
    /// A field the record must leave at zero, set: a bald figure's hair
    /// colour, a human's markings.
    NonCanonical(Field),
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Species => "species",
            Self::Height => "height",
            Self::Girth => "girth",
            Self::Taper => "taper",
            Self::Limbs => "limb length",
            Self::Head => "head size",
            Self::Face => "face shape",
            Self::Eyes => "eye shape",
            Self::Ears => "ear form",
            Self::Horns => "horns",
            Self::Tail => "tail",
            Self::Hair => "hair style",
            Self::Volume => "hair volume",
            Self::SkinColour => "skin colour",
            Self::HairColour => "hair colour",
            Self::EyeColour => "eye colour",
            Self::MarkingsColour => "markings colour",
            Self::AccentColour => "cloth colour",
        })
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("figure record shorter than a record"),
            Self::TrailingBytes => f.write_str("figure record longer than a record"),
            Self::UnknownVersion => {
                f.write_str("figure record in a format this build does not read")
            }
            Self::Unknown(field) => write!(f, "figure record names no such {field}"),
            Self::NotOfSpecies(field) => write!(f, "figure record's {field} is not of its species"),
            Self::NonCanonical(field) => write!(f, "figure record's {field} must be zero"),
        }
    }
}

/// The physical proportions a record describes.
///
/// Each is where its setting stands in the species' documented interval —
/// a factor on the reference human's own proportion, bar `taper`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Proportions {
    /// Standing height, against the reference's.
    pub height: f64,
    /// Girth, against the reference's.
    pub girth: f64,
    /// How much broader the shoulders are than the hips.
    pub taper: f64,
    /// Limb length against the trunk.
    pub limbs: f64,
    /// Head size against the body.
    pub head: f64,
    /// The hair's girth.
    pub volume: f64,
}

impl Identity {
    /// Check `spec`.
    ///
    /// # Errors
    ///
    /// [`IdentityError::NotOfSpecies`] for a form or eye colour the species
    /// does not carry, [`IdentityError::Unknown`] for a swatch past the
    /// table its slot draws from, and [`IdentityError::NonCanonical`] for a
    /// field the record must leave at zero. The first failing field is
    /// named, so a refusal says what to correct.
    pub fn new(spec: Spec) -> Result<Self, IdentityError> {
        let species = spec.species;
        let features = spec.features;
        let palette = spec.palette;

        if !species.ears().contains(&features.ears) {
            return Err(IdentityError::NotOfSpecies(Field::Ears));
        }
        if !species.horns().admits(features.horns) {
            return Err(IdentityError::NotOfSpecies(Field::Horns));
        }
        if !species.tails().admits(features.tail) {
            return Err(IdentityError::NotOfSpecies(Field::Tail));
        }

        if usize::from(palette.skin) >= species.covering().len() {
            return Err(IdentityError::Unknown(Field::SkinColour));
        }
        if usize::from(palette.hair) >= HAIR.len() {
            return Err(IdentityError::Unknown(Field::HairColour));
        }
        if usize::from(palette.eyes) >= EYES.len() {
            return Err(IdentityError::Unknown(Field::EyeColour));
        }
        if !species.eyes().contains(&palette.eyes) {
            return Err(IdentityError::NotOfSpecies(Field::EyeColour));
        }
        if usize::from(palette.accent) >= DYES.len() {
            return Err(IdentityError::Unknown(Field::AccentColour));
        }
        if spec.fixed(Field::MarkingsColour) {
            if palette.markings != 0 {
                return Err(IdentityError::NonCanonical(Field::MarkingsColour));
            }
        } else if usize::from(palette.markings) >= species.markings().len() {
            return Err(IdentityError::Unknown(Field::MarkingsColour));
        }
        if spec.fixed(Field::Volume) && features.volume != Setting::LOW {
            return Err(IdentityError::NonCanonical(Field::Volume));
        }
        if spec.fixed(Field::HairColour) && palette.hair != 0 {
            return Err(IdentityError::NonCanonical(Field::HairColour));
        }

        Ok(Self { spec })
    }

    /// Read a record from exactly [`RECORD_LEN`] bytes.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Truncated`] or [`IdentityError::TrailingBytes`] for
    /// any other length, [`IdentityError::UnknownVersion`] for another
    /// format, [`IdentityError::Unknown`] for a byte naming nothing, and
    /// whatever [`Self::new`] refuses of the record it spells.
    pub fn decode(bytes: &[u8]) -> Result<Self, IdentityError> {
        let Ok(record) = <[u8; RECORD_LEN]>::try_from(bytes) else {
            return Err(if bytes.len() < RECORD_LEN {
                IdentityError::Truncated
            } else {
                IdentityError::TrailingBytes
            });
        };
        let [version, species, height, girth, taper, limbs, head, face, eyes, ears, horns, tail, hair, volume, skin, hair_colour, eye_colour, markings, accent] =
            record;
        if version != RECORD_VERSION {
            return Err(IdentityError::UnknownVersion);
        }
        let spec = Spec {
            species: Species::from_byte(species).ok_or(IdentityError::Unknown(Field::Species))?,
            build: Build {
                height: Setting(height),
                girth: Setting(girth),
                taper: Setting(taper),
                limbs: Setting(limbs),
                head: Setting(head),
            },
            features: Features {
                face: FaceShape::from_byte(face).ok_or(IdentityError::Unknown(Field::Face))?,
                eyes: EyeShape::from_byte(eyes).ok_or(IdentityError::Unknown(Field::Eyes))?,
                ears: EarForm::from_byte(ears).ok_or(IdentityError::Unknown(Field::Ears))?,
                horns: optional(horns, HornForm::from_byte, Field::Horns)?,
                tail: optional(tail, TailForm::from_byte, Field::Tail)?,
                hair: optional(hair, HairStyle::from_byte, Field::Hair)?,
                volume: Setting(volume),
            },
            palette: Palette {
                skin,
                hair: hair_colour,
                eyes: eye_colour,
                markings,
                accent,
            },
        };
        Self::new(spec)
    }

    /// The record's bytes, which [`Self::decode`] reads back as this
    /// identity and no other.
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let Spec {
            species,
            build,
            features,
            palette,
        } = self.spec;
        [
            RECORD_VERSION,
            species.byte(),
            build.height.0,
            build.girth.0,
            build.taper.0,
            build.limbs.0,
            build.head.0,
            features.face.byte(),
            features.eyes.byte(),
            features.ears.byte(),
            spelled(features.horns, HornForm::byte),
            spelled(features.tail, TailForm::byte),
            spelled(features.hair, HairStyle::byte),
            features.volume.0,
            palette.skin,
            palette.hair,
            palette.eyes,
            palette.markings,
            palette.accent,
        ]
    }

    /// The record it checked.
    #[must_use]
    pub const fn spec(&self) -> Spec {
        self.spec
    }

    /// Its species.
    #[must_use]
    pub const fn species(&self) -> Species {
        self.spec.species
    }

    /// The physical proportions its settings stand for.
    #[must_use]
    pub fn proportions(&self) -> Proportions {
        let ranges = self.spec.species.ranges();
        let build = self.spec.build;
        Proportions {
            height: ranges.height.at(build.height),
            girth: ranges.girth.at(build.girth),
            taper: ranges.taper.at(build.taper),
            limbs: ranges.limbs.at(build.limbs),
            head: ranges.head.at(build.head),
            volume: VOLUME.at(self.spec.features.volume),
        }
    }

    /// The colours it is drawn in.
    ///
    /// A species with no markings draws nothing in them, so that slot carries
    /// its skin rather than a colour nobody chose. Every index was checked
    /// when the record was, so each lookup is inside its table.
    #[must_use]
    pub fn tints(&self) -> Tints {
        let species = self.spec.species;
        let palette = self.spec.palette;
        let skin = species.covering()[usize::from(palette.skin)];
        let markings = species
            .markings()
            .get(usize::from(palette.markings))
            .copied()
            .unwrap_or(skin);
        let mut colours = [skin; Tint::COUNT];
        colours[Tint::Hair.index()] = HAIR[usize::from(palette.hair)];
        colours[Tint::Eyes.index()] = EYES[usize::from(palette.eyes)];
        colours[Tint::Markings.index()] = markings;
        colours[Tint::Accent.index()] = DYES[usize::from(palette.accent)];
        colours[Tint::Trousers.index()] = TROUSERS;
        colours[Tint::Leather.index()] = LEATHER;
        Tints::new(colours)
    }
}

/// An optional form as its record byte: zero for none, and one past the
/// form's own byte otherwise.
fn spelled<T>(form: Option<T>, byte: fn(T) -> u8) -> u8 {
    form.map_or(0, |form| byte(form) + 1)
}

/// The optional form `byte` spells for `field`: zero is none, and anything
/// else must be one past a form's own byte.
fn optional<T>(
    byte: u8,
    from: fn(u8) -> Option<T>,
    field: Field,
) -> Result<Option<T>, IdentityError> {
    match byte {
        0 => Ok(None),
        present => from(present - 1)
            .map(Some)
            .ok_or(IdentityError::Unknown(field)),
    }
}

/// A closed set of forms, each spelled as its discriminant.
macro_rules! forms {
    ($name:ident: $($variant:ident),+ $(,)?) => {
        impl $name {
            /// Every form, in the order [`Self::byte`] numbers them.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The byte a record spells it as.
            #[must_use]
            pub const fn byte(self) -> u8 {
                self as u8
            }

            /// The form `byte` spells, if it spells one.
            #[must_use]
            pub const fn from_byte(byte: u8) -> Option<Self> {
                let mut index = 0;
                while index < Self::ALL.len() {
                    if Self::ALL[index] as u8 == byte {
                        return Some(Self::ALL[index]);
                    }
                    index += 1;
                }
                None
            }
        }
    };
}

forms!(FaceShape: Oval, Round, Long, Broad, Heart);
forms!(EyeShape: Round, Almond, Narrow);
forms!(EarForm: Round, Pointed, Long, Upright, Lop, Finned);
forms!(HornForm: Nubs, Swept, Curled, Spire);
forms!(TailForm: Brush, Slender, Scaled);
forms!(HairStyle: Cropped, Short, Shaggy, Topknot);

#[cfg(test)]
#[path = "identity/tests.rs"]
mod tests;
