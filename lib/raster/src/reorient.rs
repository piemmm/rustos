//! Setting a surface down a different way up, without resampling it.
//!
//! The eight ways a rectangle can be placed on itself — the quarter turns
//! and the mirrors — are a permutation of positions, not a filter, so a
//! reorientation moves every pixel exactly once and invents none. That is
//! why it is here beside the resampler rather than expressed as an
//! [`Affine`](crate::Affine): a general transform would sample, and
//! sampling a picture that needs no sampling loses it.
//!
//! `lib/image` carries its own eight-case map for the `Orientation` tag a
//! TIFF directory or a JPEG's EXIF block states. That is deliberately not
//! this type: it is keyed to the tag's wire numbering, applies during a
//! decode as a write address, and lives in a crate that depends on neither
//! this one nor the theme and reclaim crates it would drag in. What the two
//! share is the arithmetic of a rectangle's symmetries, which is a
//! mathematical fact each proves for itself, not a project value that could
//! drift.

use crate::surface::Surface;

/// One of the eight ways a picture can be set down: a quarter-turn
/// multiple, optionally mirrored first.
///
/// The variants are ordered as TIFF and EXIF number the same eight, which
/// is the order a reader of either will expect to find them in.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Reorient {
    /// Set down unchanged.
    #[default]
    None,
    /// Mirrored left to right.
    FlipHorizontal,
    /// Turned half a turn.
    HalfTurn,
    /// Mirrored top to bottom.
    FlipVertical,
    /// Mirrored about the leading diagonal.
    Transpose,
    /// Turned a quarter turn clockwise.
    QuarterTurnRight,
    /// Mirrored about the trailing diagonal.
    Antitranspose,
    /// Turned a quarter turn anticlockwise.
    QuarterTurnLeft,
}

impl Reorient {
    /// Every reorientation, in the order the variants declare.
    pub const ALL: [Self; 8] = [
        Self::None,
        Self::FlipHorizontal,
        Self::HalfTurn,
        Self::FlipVertical,
        Self::Transpose,
        Self::QuarterTurnRight,
        Self::Antitranspose,
        Self::QuarterTurnLeft,
    ];

    /// The mirror-then-turn pair this reorientation is, as quarter turns
    /// clockwise and whether the source is mirrored left to right first.
    ///
    /// Every operation here is defined from this one decomposition, so the
    /// position map and the composition cannot disagree about what a
    /// variant means.
    const fn parts(self) -> (u32, bool) {
        match self {
            Self::None => (0, false),
            Self::FlipHorizontal => (0, true),
            Self::HalfTurn => (2, false),
            Self::FlipVertical => (2, true),
            Self::Transpose => (3, true),
            Self::QuarterTurnRight => (1, false),
            Self::Antitranspose => (1, true),
            Self::QuarterTurnLeft => (3, false),
        }
    }

    /// The reorientation `turns` quarter turns clockwise is, having
    /// mirrored the source left to right first when `mirrored`.
    const fn from_parts(turns: u32, mirrored: bool) -> Self {
        match (turns % 4, mirrored) {
            (0, false) => Self::None,
            (1, false) => Self::QuarterTurnRight,
            (2, false) => Self::HalfTurn,
            (3, false) => Self::QuarterTurnLeft,
            (0, true) => Self::FlipHorizontal,
            (1, true) => Self::Antitranspose,
            (2, true) => Self::FlipVertical,
            _ => Self::Transpose,
        }
    }

    /// Whether this reorientation swaps the picture's two axes.
    #[must_use]
    pub const fn transposes(self) -> bool {
        self.parts().0 % 2 == 1
    }

    /// The size a `width`×`height` picture occupies once set down this way.
    #[must_use]
    pub const fn applied_size(self, width: u32, height: u32) -> (u32, u32) {
        if self.transposes() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// Where the source pixel at `(x, y)` of a `width`×`height` picture
    /// lands.
    ///
    /// Saturating, so a coordinate outside the source yields an edge
    /// position rather than wrapping; a caller bounds its own walk by the
    /// geometry it has, so that never arises.
    #[must_use]
    pub const fn place(self, x: u32, y: u32, width: u32, height: u32) -> (u32, u32) {
        let (turns, mirrored) = self.parts();
        let last_x = width.saturating_sub(1);
        let last_y = height.saturating_sub(1);
        let x = if mirrored {
            last_x.saturating_sub(x)
        } else {
            x
        };
        match turns {
            1 => (last_y.saturating_sub(y), x),
            2 => (last_x.saturating_sub(x), last_y.saturating_sub(y)),
            3 => (y, last_x.saturating_sub(x)),
            _ => (x, y),
        }
    }

    /// The single reorientation that does this one and then `next`.
    ///
    /// A viewer holds one of these rather than a turn count beside a flip
    /// flag, so a picture the user has turned and mirrored is still set
    /// down in one pass.
    #[must_use]
    pub const fn then(self, next: Self) -> Self {
        let (turns, mirrored) = self.parts();
        let (next_turns, next_mirrored) = next.parts();
        // Mirroring reverses the sense of a turn taken before it, which is
        // the whole of the group's law.
        let combined = if next_mirrored {
            next_turns + 4 - turns
        } else {
            next_turns + turns
        };
        Self::from_parts(combined, mirrored != next_mirrored)
    }

    /// The reorientation that undoes this one.
    ///
    /// What a surface's own coordinates are read back through: a pointer
    /// lands on the picture as shown, and the position it means in the
    /// picture as stored is this applied to it.
    #[must_use]
    pub const fn inverse(self) -> Self {
        let (turns, mirrored) = self.parts();
        // Every mirrored placement is its own undoing; a pure turn is
        // undone by the turn that completes the circle.
        if mirrored {
            self
        } else {
            Self::from_parts(4 - turns, false)
        }
    }
}

impl Surface {
    /// A fresh surface holding this one's pixels set down `how`.
    ///
    /// Nothing is resampled: every pixel is carried across unchanged, so a
    /// quarter turn of a picture is exact and turning it back returns the
    /// picture it started as. A turn that swaps the axes cannot be done in
    /// place on an oblong, so this allocates rather than taking a
    /// destination whose geometry every caller would have to agree.
    ///
    /// Reorienting the picture a window actually shows costs a window's
    /// worth of work; reorienting a decoded master costs the master's. A
    /// viewer that scales to fit should therefore turn what it displays,
    /// not what it decoded.
    ///
    /// Returns `None` when the destination cannot be allocated, so the
    /// caller fails closed rather than panicking.
    #[must_use]
    pub fn reoriented(&self, how: Reorient) -> Option<Self> {
        let (width, height) = how.applied_size(self.width(), self.height());
        let mut out = Self::new(width, height)?;
        let source = self.pixels();
        let source_width = usize::try_from(self.width()).ok()?;
        // Reading each destination pixel back through the undoing, rather
        // than scattering each source pixel forward, keeps the writes
        // running along the destination's own rows.
        let back = how.inverse();
        for dy in 0..height {
            let (first, row) = out.row_span_mut(dy, 0, width)?;
            for (offset, slot) in row.iter_mut().enumerate() {
                let dx = first.checked_add(u32::try_from(offset).ok()?)?;
                let (sx, sy) = back.place(dx, dy, width, height);
                let at = usize::try_from(sy)
                    .ok()?
                    .checked_mul(source_width)?
                    .checked_add(usize::try_from(sx).ok()?)?;
                *slot = *source.get(at)?;
            }
        }
        Some(out)
    }
}

#[cfg(test)]
#[path = "reorient_tests.rs"]
mod tests;
