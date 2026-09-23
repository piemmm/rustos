//! Which of a figure's colours a surface is drawn in.
//!
//! A surface names a role rather than a colour, so a figure is re-tinted by
//! swapping one small table while none of its geometry is rebuilt: a
//! palette edit re-tints the rig a designer already holds rather than
//! re-rigging it.

use tairix_raster::Color;

/// The role a surface's colour plays.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Tint {
    /// Whatever covers the species: skin, fur or scale.
    Skin,
    /// Hair.
    Hair,
    /// The eyes.
    Eyes,
    /// A species' markings: an inner ear, a tail's tip, a horn.
    Markings,
    /// The dyed cloth of the tunic.
    Accent,
    /// The undyed cloth of the trousers.
    Trousers,
    /// Boot leather.
    Leather,
}

impl Tint {
    /// How many roles there are.
    pub const COUNT: usize = 7;

    /// Every role, in the order [`Self::index`] numbers them.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Skin,
        Self::Hair,
        Self::Eyes,
        Self::Markings,
        Self::Accent,
        Self::Trousers,
        Self::Leather,
    ];

    /// Its slot in a [`Tints`] table.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Skin => 0,
            Self::Hair => 1,
            Self::Eyes => 2,
            Self::Markings => 3,
            Self::Accent => 4,
            Self::Trousers => 5,
            Self::Leather => 6,
        }
    }
}

/// The colour each role resolves to, for one figure.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Tints([Color; Tint::COUNT]);

impl Tints {
    /// A table holding `colours`, indexed by [`Tint::index`].
    #[must_use]
    pub const fn new(colours: [Color; Tint::COUNT]) -> Self {
        Self(colours)
    }

    /// What `tint` is drawn in.
    #[must_use]
    pub const fn get(&self, tint: Tint) -> Color {
        self.0[tint.index()]
    }
}

#[cfg(test)]
#[path = "tint/tests.rs"]
mod tests;
