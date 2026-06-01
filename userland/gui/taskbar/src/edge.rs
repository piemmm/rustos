//! The screen edge the taskbar is pinned to, and the axis that follows.
//!
//! A taskbar is pinned to one screen [`Edge`]. That choice fixes its
//! [`Orientation`]: a top or bottom bar runs horizontally (its long, main
//! axis is `x`), a left or right bar runs vertically (main axis is `y`). The
//! rest of the crate lays regions out along the main axis and is otherwise
//! orientation-agnostic.

/// Which screen edge the taskbar is pinned to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Edge {
    /// Pinned to the top of the screen (horizontal).
    Top,
    /// Pinned to the bottom of the screen (horizontal).
    Bottom,
    /// Pinned to the left of the screen (vertical).
    Left,
    /// Pinned to the right of the screen (vertical).
    Right,
}

impl Edge {
    /// The axis the bar's regions are laid out along.
    #[must_use]
    pub const fn orientation(self) -> Orientation {
        match self {
            Self::Top | Self::Bottom => Orientation::Horizontal,
            Self::Left | Self::Right => Orientation::Vertical,
        }
    }

    /// `true` when the bar hugs the far (high-coordinate) cross edge — the
    /// bottom of the screen for a horizontal bar, the right for a vertical
    /// one — so it is offset by `screen − thickness` on the cross axis.
    #[must_use]
    pub const fn at_trailing_cross_edge(self) -> bool {
        matches!(self, Self::Bottom | Self::Right)
    }
}

/// The axis a taskbar's regions are laid out along.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Orientation {
    /// Regions are laid out left-to-right along `x`; the bar's thickness is
    /// its height.
    Horizontal,
    /// Regions are laid out top-to-bottom along `y`; the bar's thickness is
    /// its width.
    Vertical,
}
