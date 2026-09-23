//! Sockets: the named places equipment hangs, so gear is parts rather than
//! paint.
//!
//! A helm is a panel and a wedge mounted on the head socket, not a redrawn
//! head — which is what makes gear visible, mixable across every rig that
//! offers the same socket, and free of any new art path.

use crate::frame::{Body, Rotation};
use crate::joint::JointId;

/// Which of a pair.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Side {
    /// The figure's left.
    Left,
    /// The figure's right.
    Right,
}

impl Side {
    /// Both, left first.
    pub const BOTH: [Self; 2] = [Self::Left, Self::Right];

    /// Which way an offset across the figure points for this side.
    ///
    /// The frame's `side` axis is the figure's left, so a right-hand part is
    /// the left-hand one mirrored by this rather than authored twice.
    #[must_use]
    pub const fn across(self) -> f64 {
        match self {
            Self::Left => 1.0,
            Self::Right => -1.0,
        }
    }
}

/// Where a piece of equipment attaches.
///
/// A closed set: each member is a place a rig genuinely offers and gear
/// genuinely asks for, and a socket nobody mounts or fits is surface with no
/// present-day caller.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Socket {
    /// The weapon hand.
    MainHand,
    /// The shield or second-weapon hand.
    OffHand,
    /// A helm or circlet.
    Head,
    /// A cloak, quiver or scabbard.
    Back,
    /// A pauldron.
    Shoulder(Side),
    /// A belt fitting or holstered weapon.
    Hip(Side),
    /// A boot or greave.
    Foot(Side),
}

impl Socket {
    /// Every socket, in the order [`Self::index`] numbers them.
    pub const ALL: [Self; Self::COUNT] = [
        Self::MainHand,
        Self::OffHand,
        Self::Head,
        Self::Back,
        Self::Shoulder(Side::Left),
        Self::Shoulder(Side::Right),
        Self::Hip(Side::Left),
        Self::Hip(Side::Right),
        Self::Foot(Side::Left),
        Self::Foot(Side::Right),
    ];

    /// How many sockets there are.
    ///
    /// The set is closed, so a rig holds its mounts in an array this long and
    /// a second mount for one socket is caught by the slot already being
    /// taken rather than by a search.
    pub const COUNT: usize = 10;

    /// Its slot in a rig's mount table.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::MainHand => 0,
            Self::OffHand => 1,
            Self::Head => 2,
            Self::Back => 3,
            Self::Shoulder(side) => 4 + side as usize,
            Self::Hip(side) => 6 + side as usize,
            Self::Foot(side) => 8 + side as usize,
        }
    }
}

/// Where a rig puts one of its sockets.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Mount {
    /// The joint that carries it.
    pub joint: JointId,
    /// Where it sits in that joint's frame.
    pub at: Body,
    /// How gear rests there.
    ///
    /// On the mount rather than on the gear, so a scabbard angled across the
    /// back and a grip pointed along the hand are the *rig's* statement and
    /// one blade fits every rig that offers the socket.
    pub orientation: Rotation,
    /// How large the body is here, against the figure gear is authored for.
    ///
    /// Also the rig's statement, for the same reason: a helm authored once
    /// sits on a dwarf's broad head and an elf's narrow one without either
    /// knowing which it is on.
    pub scale: f64,
}

impl Mount {
    /// A mount on `joint` at `at`, resting square, at the authored size.
    #[must_use]
    pub const fn new(joint: JointId, at: Body) -> Self {
        Self {
            joint,
            at,
            orientation: Rotation::REST,
            scale: 1.0,
        }
    }

    /// The same mount resting at `orientation`.
    #[must_use]
    pub const fn oriented(mut self, orientation: Rotation) -> Self {
        self.orientation = orientation;
        self
    }

    /// The same mount on a body `scale` times the authored size.
    #[must_use]
    pub const fn scaled(mut self, scale: f64) -> Self {
        self.scale = scale;
        self
    }
}

#[cfg(test)]
#[path = "socket/tests.rs"]
mod tests;
