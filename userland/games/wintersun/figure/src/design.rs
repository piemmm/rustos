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
//! # A write lands on what the player is doing now
//!
//! One write is outstanding at a time, and its answer arrives while the
//! player may already be dragging something else. [`Designer::landed`] and
//! [`Designer::refused`] therefore touch only the fields the player has not
//! edited since that write was handed out: the store's record where it took
//! the write, and the one it kept where it refused it. The drag in hand is
//! the player's either way. An interaction that settles while a write is out
//! is owed, and both answers hand it out — one write for however many
//! settles it covers.
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
//! The choices survive beneath the record — including through an edit to a
//! field the record holds at zero, which can only ask for the zero it holds —
//! so a round trip through another species, or through going bald, gives
//! back exactly the figure it left.
//!
//! This is not a repair of a record. What it projects is the player's own
//! choices; a record arriving from anywhere else is checked by
//! [`Identity::decode`] and refused, never adjusted.

use tairix_raster::Color;

use crate::identity::{
    EarForm, EyeShape, FaceShape, Field, HairStyle, HornForm, Identity, IdentityError, Setting,
    Spec, TailForm,
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

    /// The field it sets.
    #[must_use]
    pub const fn field(self) -> Field {
        match self {
            Self::Species(_) => Field::Species,
            Self::Height(_) => Field::Height,
            Self::Girth(_) => Field::Girth,
            Self::Taper(_) => Field::Taper,
            Self::Limbs(_) => Field::Limbs,
            Self::Head(_) => Field::Head,
            Self::Face(_) => Field::Face,
            Self::EyeShape(_) => Field::Eyes,
            Self::Ears(_) => Field::Ears,
            Self::Horns(_) => Field::Horns,
            Self::Tail(_) => Field::Tail,
            Self::Hair(_) => Field::Hair,
            Self::Volume(_) => Field::Volume,
            Self::Skin(_) => Field::SkinColour,
            Self::HairColour(_) => Field::HairColour,
            Self::EyeColour(_) => Field::EyeColour,
            Self::Markings(_) => Field::MarkingsColour,
            Self::Accent(_) => Field::AccentColour,
        }
    }

    /// The edit that sets `field` to what `spec` holds there.
    #[must_use]
    pub const fn of(field: Field, spec: Spec) -> Self {
        match field {
            Field::Species => Self::Species(spec.species),
            Field::Height => Self::Height(spec.build.height),
            Field::Girth => Self::Girth(spec.build.girth),
            Field::Taper => Self::Taper(spec.build.taper),
            Field::Limbs => Self::Limbs(spec.build.limbs),
            Field::Head => Self::Head(spec.build.head),
            Field::Face => Self::Face(spec.features.face),
            Field::Eyes => Self::EyeShape(spec.features.eyes),
            Field::Ears => Self::Ears(spec.features.ears),
            Field::Horns => Self::Horns(spec.features.horns),
            Field::Tail => Self::Tail(spec.features.tail),
            Field::Hair => Self::Hair(spec.features.hair),
            Field::Volume => Self::Volume(spec.features.volume),
            Field::SkinColour => Self::Skin(spec.palette.skin),
            Field::HairColour => Self::HairColour(spec.palette.hair),
            Field::EyeColour => Self::EyeColour(spec.palette.eyes),
            Field::MarkingsColour => Self::Markings(spec.palette.markings),
            Field::AccentColour => Self::Accent(spec.palette.accent),
        }
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

/// A set of a record's fields.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Fields(u32);

const _: () = assert!(Field::ALL.len() <= u32::BITS as usize);

impl Fields {
    const NONE: Self = Self(0);
    const ALL: Self = Self(u32::MAX >> (u32::BITS as usize - Field::ALL.len()));

    const fn with(self, field: Field) -> Self {
        Self(self.0 | 1 << field.index())
    }

    const fn holds(self, field: Field) -> bool {
        self.0 & 1 << field.index() != 0
    }
}

/// Choices, and the record they come to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Chosen {
    spec: Spec,
    record: Identity,
}

/// A record being designed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Designer {
    /// What the player chose, field by field, whether or not the species
    /// chosen carries it, and the record that comes to — which is what a
    /// preview draws.
    live: Chosen,
    /// The choices behind the record the store holds.
    stored: Chosen,
    /// The choices behind the write handed out and not yet answered.
    written: Option<Chosen>,
    /// The fields edited since that write was handed out.
    touched: Fields,
    /// Whether an interaction settled while a write was out.
    owed: bool,
}

impl Designer {
    /// Open `record` for editing, as the store holds it.
    #[must_use]
    pub const fn open(record: Identity) -> Self {
        let chosen = Chosen {
            spec: record.spec(),
            record,
        };
        Self {
            live: chosen,
            stored: chosen,
            written: None,
            touched: Fields::NONE,
            owed: false,
        }
    }

    /// The record as it stands.
    #[must_use]
    pub const fn live(&self) -> Identity {
        self.live.record
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
        let field = edit.field();
        if !edit.reshapes() {
            let set = edit.written(self.live.record.spec());
            Identity::new(set)?;
            if set.fixed(field) {
                return Ok(());
            }
        }
        let spec = edit.written(self.live.spec);
        self.live = Chosen {
            spec,
            record: Identity::new(canonical(spec))?,
        };
        self.touched = self.touched.with(field);
        Ok(())
    }

    /// Replace the whole record with `record`: a preset picked, or a
    /// plausible figure drawn.
    pub fn apply(&mut self, record: Identity) {
        self.live = Chosen {
            spec: record.spec(),
            record,
        };
        self.touched = Fields::ALL;
    }

    /// The interaction making the edits has finished: the record to write,
    /// if the store does not hold it already.
    ///
    /// A drag settles once however many samples it took; a key step or a
    /// pick, being a whole interaction, settles at once. While an earlier
    /// write is unanswered this answers nothing and owes the write instead.
    #[must_use = "a record handed out is written, or the interaction is lost"]
    pub fn settle(&mut self) -> Option<Identity> {
        if self.written.is_some() {
            self.owed = true;
            return None;
        }
        if self.live.record == self.stored.record {
            return None;
        }
        self.written = Some(self.live);
        self.touched = Fields::NONE;
        Some(self.live.record)
    }

    /// The write handed out landed, and the store now holds `stored`.
    ///
    /// Every field the player has not edited since takes the store's record;
    /// a store that took the write exactly keeps the choices behind it too.
    /// Answers the write owed, if an interaction settled meanwhile. With no
    /// write out, an answer changes nothing.
    #[must_use = "an owed record is written, or the interaction is lost"]
    pub fn landed(&mut self, stored: Identity) -> Option<Identity> {
        let written = self.written.take()?;
        self.stored = if stored == written.record {
            written
        } else {
            Chosen {
                spec: stored.spec(),
                record: stored,
            }
        };
        self.answered()
    }

    /// The write handed out was refused, and the store holds what it held.
    ///
    /// Every field the player has not edited since goes back to it. Answers
    /// the write owed, if an interaction settled meanwhile. With no write
    /// out, a refusal changes nothing.
    #[must_use = "an owed record is written, or the interaction is lost"]
    pub fn refused(&mut self) -> Option<Identity> {
        self.written.take()?;
        self.answered()
    }

    /// Take what the store holds wherever the player is not editing, and hand
    /// out the write the answer was holding up.
    fn answered(&mut self) -> Option<Identity> {
        let mut spec = self.live.spec;
        for field in Field::ALL {
            if !self.touched.holds(field) {
                spec = Edit::of(field, self.stored.spec).written(spec);
            }
        }
        // The projection of any choices is a record, so this never falls
        // back; were it to, the store's own record is the one to show.
        self.live =
            Identity::new(canonical(spec)).map_or(self.stored, |record| Chosen { spec, record });
        if core::mem::take(&mut self.owed) {
            self.settle()
        } else {
            None
        }
    }
}

/// The record `chosen` comes to for its species: every choice the species
/// carries kept, and every other field the nearest thing it does carry.
fn canonical(chosen: Spec) -> Spec {
    let species = chosen.species;
    let mut spec = chosen;
    spec.features.ears = carried(chosen.features.ears, species.ears().iter().copied());
    spec.features.horns = carried(chosen.features.horns, species.horns().admitted());
    spec.features.tail = carried(chosen.features.tail, species.tails().admitted());
    spec.palette.skin = within(chosen.palette.skin, species.covering().len());
    spec.palette.hair = within(chosen.palette.hair, HAIR.len());
    spec.palette.eyes = nearest_eye(chosen.palette.eyes, species.eyes());
    spec.palette.markings = within(chosen.palette.markings, species.markings().len());
    spec.palette.accent = within(chosen.palette.accent, DYES.len());
    if spec.fixed(Field::Volume) {
        spec.features.volume = Setting::LOW;
    }
    if spec.fixed(Field::HairColour) {
        spec.palette.hair = 0;
    }
    spec
}

/// `form` where `admitted` holds it, and the first it holds otherwise.
fn carried<T: Copy + PartialEq>(form: T, mut admitted: impl Iterator<Item = T> + Clone) -> T {
    if admitted.clone().any(|one| one == form) {
        form
    } else {
        admitted.next().unwrap_or(form)
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
