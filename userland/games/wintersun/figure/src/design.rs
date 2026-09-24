//! The designer: a figure's record, edited live and written once per
//! interaction.
//!
//! A designer is a set of sliders over a record, which makes it the surface
//! most likely to stall its own window. Two rules keep it answering within a
//! frame, and both are this module's contract rather than a surface's
//! discipline:
//!
//! - **An edit changes the record in memory and nothing else.**
//!   [`Designer::edit`] writes no store and re-derives nothing. What the next
//!   paint owes is [`Change::between`] the record it last drew and the one now
//!   live: a palette edit is a re-tint, a build or feature edit a rebuild, and
//!   any burst of edits between two paints costs one of either.
//! - **The durable write happens once, where the interaction settles.**
//!   [`Designer::settle`] answers the record to store only when the
//!   interaction changed it, so a drag of any length is at most one write and
//!   a drag that ends where it began is none.
//!
//! # A record that is always one
//!
//! The designer holds what the player chose, field by field, and the record
//! those choices come to for the species chosen. An edit the species cannot
//! carry is refused with the field it names, exactly as the record's decoder
//! would refuse it. An edit that changes what the other fields may hold — a
//! species, or going bald — re-derives them instead: a swatch index past the
//! new species' table is clamped, a form it does not carry becomes its first,
//! a form it must carry is given, an eye colour it does not admit becomes the
//! nearest it does, and a bald figure's hair colour and volume are zeroed.
//! The choices survive beneath the record, so a round trip through another
//! species, or through going bald, gives back exactly the figure it left.
//!
//! This is not a repair of a record. What it projects is the player's own
//! choices; a record arriving from anywhere else is checked by
//! [`Identity::decode`] and refused, never adjusted.

use tairix_raster::Color;

use crate::identity::{
    EarForm, EyeShape, FaceShape, HairStyle, HornForm, Identity, IdentityError, Setting, Spec,
    TailForm,
};
use crate::species::{Species, DYES, EYES, HAIR};

/// One field of a record set to a value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Edit {
    /// The species.
    Species(Species),
    /// Standing height.
    Height(Setting),
    /// How heavy the body and limbs are.
    Girth(Setting),
    /// Shoulders against hips.
    Taper(Setting),
    /// Limb length against the trunk.
    Limbs(Setting),
    /// Head size against the body.
    Head(Setting),
    /// The face.
    Face(FaceShape),
    /// The eyes' shape.
    EyeShape(EyeShape),
    /// The ears.
    Ears(EarForm),
    /// Horns, or none.
    Horns(Option<HornForm>),
    /// A tail, or none.
    Tail(Option<TailForm>),
    /// The hair, or none at all.
    Hair(Option<HairStyle>),
    /// How full the hair is.
    Volume(Setting),
    /// The skin, fur or scale swatch.
    Skin(u8),
    /// The hair swatch.
    HairColour(u8),
    /// The eye swatch.
    EyeColour(u8),
    /// The markings swatch.
    Markings(u8),
    /// The cloth swatch.
    Accent(u8),
}

impl Edit {
    /// Whether it changes what the record's other fields may hold, so they
    /// are re-derived around it rather than it being checked against them.
    const fn reshapes(self) -> bool {
        matches!(self, Self::Species(_) | Self::Hair(_))
    }

    /// `spec` with this field set.
    const fn written(self, spec: Spec) -> Spec {
        let mut spec = spec;
        match self {
            Self::Species(species) => spec.species = species,
            Self::Height(setting) => spec.build.height = setting,
            Self::Girth(setting) => spec.build.girth = setting,
            Self::Taper(setting) => spec.build.taper = setting,
            Self::Limbs(setting) => spec.build.limbs = setting,
            Self::Head(setting) => spec.build.head = setting,
            Self::Face(face) => spec.features.face = face,
            Self::EyeShape(eyes) => spec.features.eyes = eyes,
            Self::Ears(ears) => spec.features.ears = ears,
            Self::Horns(horns) => spec.features.horns = horns,
            Self::Tail(tail) => spec.features.tail = tail,
            Self::Hair(hair) => spec.features.hair = hair,
            Self::Volume(volume) => spec.features.volume = volume,
            Self::Skin(swatch) => spec.palette.skin = swatch,
            Self::HairColour(swatch) => spec.palette.hair = swatch,
            Self::EyeColour(swatch) => spec.palette.eyes = swatch,
            Self::Markings(swatch) => spec.palette.markings = swatch,
            Self::Accent(swatch) => spec.palette.accent = swatch,
        }
        spec
    }
}

/// What a change of record leaves a drawn figure owing, cheapest first.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Change {
    /// Nothing drawn depends on what changed.
    Nothing,
    /// Colours only: the rig is re-tinted and no point of it moves.
    Tints,
    /// Geometry: the rig is rebuilt, and its colours with it.
    Rig,
}

impl Change {
    /// What a figure drawn from `was` owes to be drawn as `now`.
    ///
    /// A rig's geometry is its species, build and features, and its colours
    /// its species and palette, so a palette edit never moves a point.
    #[must_use]
    pub fn between(was: &Identity, now: &Identity) -> Self {
        let (was, now) = (was.spec(), now.spec());
        if was.species != now.species || was.build != now.build || was.features != now.features {
            Self::Rig
        } else if was.palette != now.palette {
            Self::Tints
        } else {
            Self::Nothing
        }
    }
}

/// A record being designed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Designer {
    /// What the player chose, field by field, whether or not the species
    /// chosen carries it.
    chosen: Spec,
    /// The record those choices come to, which is what a preview draws.
    live: Identity,
    /// The record the last settled interaction left.
    settled: Identity,
}

impl Designer {
    /// Open `record` for editing, as it is stored.
    ///
    /// Opening again on what a store holds is how a refused write is undone:
    /// the choices that produced the refused record go with it.
    #[must_use]
    pub const fn open(record: Identity) -> Self {
        Self {
            chosen: record.spec(),
            live: record,
            settled: record,
        }
    }

    /// The record as it stands.
    #[must_use]
    pub const fn live(&self) -> Identity {
        self.live
    }

    /// Make `edit` to the live record.
    ///
    /// # Errors
    ///
    /// What [`Identity::new`] refuses of the live record with this field so
    /// set: a form or eye colour the species does not carry, a swatch past
    /// its table, or a value a bald or unmarked figure must leave at zero. A
    /// refused edit changes nothing.
    pub fn edit(&mut self, edit: Edit) -> Result<(), IdentityError> {
        if !edit.reshapes() {
            Identity::new(edit.written(self.live.spec()))?;
        }
        let chosen = edit.written(self.chosen);
        self.live = Identity::new(canonical(chosen))?;
        self.chosen = chosen;
        Ok(())
    }

    /// Replace the whole record with `record`: a preset picked, or a
    /// plausible figure drawn.
    pub fn apply(&mut self, record: Identity) {
        self.chosen = record.spec();
        self.live = record;
    }

    /// The interaction making the edits has finished: the record to write,
    /// if it changed since the last interaction settled.
    ///
    /// A drag settles once however many samples it took; a key step or a
    /// pick, being a whole interaction, settles at once.
    pub fn settle(&mut self) -> Option<Identity> {
        if self.live == self.settled {
            return None;
        }
        self.settled = self.live;
        Some(self.live)
    }
}

/// The record `chosen` comes to for its species: every choice the species
/// carries kept, and every other field the nearest thing it does carry.
fn canonical(chosen: Spec) -> Spec {
    let species = chosen.species;
    let mut spec = chosen;
    spec.features.ears = carried(chosen.features.ears, species.ears());
    spec.features.horns = carried(chosen.features.horns, species.horns());
    spec.features.tail = carried(chosen.features.tail, species.tails());
    spec.palette.skin = within(chosen.palette.skin, species.covering().len());
    spec.palette.hair = within(chosen.palette.hair, HAIR.len());
    spec.palette.eyes = nearest_eye(chosen.palette.eyes, species.eyes());
    spec.palette.markings = within(chosen.palette.markings, species.markings().len());
    spec.palette.accent = within(chosen.palette.accent, DYES.len());
    if chosen.features.hair.is_none() {
        spec.features.volume = Setting::LOW;
        spec.palette.hair = 0;
    }
    spec
}

/// `form` where `admitted` holds it, and the first it holds otherwise.
fn carried<T: Copy + PartialEq>(form: T, admitted: &[T]) -> T {
    if admitted.contains(&form) {
        form
    } else {
        admitted.first().copied().unwrap_or(form)
    }
}

/// `index` inside a table of `len` swatches: itself where it is one, the
/// last otherwise, and zero for a table with none.
fn within(index: u8, len: usize) -> u8 {
    index.min(u8::try_from(len.saturating_sub(1)).unwrap_or(u8::MAX))
}

/// `chosen` where `admitted` holds it, and otherwise the admitted eye colour
/// nearest to it.
fn nearest_eye(chosen: u8, admitted: &[u8]) -> u8 {
    if admitted.contains(&chosen) {
        return chosen;
    }
    let wanted = EYES.get(usize::from(chosen));
    admitted
        .iter()
        .copied()
        .min_by_key(|eye| match (wanted, EYES.get(usize::from(*eye))) {
            (Some(wanted), Some(eye)) => apart(*wanted, *eye),
            _ => u32::MAX,
        })
        .unwrap_or(chosen)
}

/// How far apart two colours are: the squared distance between their
/// channels.
fn apart(one: Color, other: Color) -> u32 {
    [(one.r, other.r), (one.g, other.g), (one.b, other.b)]
        .into_iter()
        .map(|(a, b)| u32::from(a.abs_diff(b)).pow(2))
        .sum()
}

#[cfg(test)]
#[path = "design/tests.rs"]
mod tests;
