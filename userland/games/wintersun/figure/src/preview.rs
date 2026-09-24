//! The designer's live preview: the record being designed, playing, at any
//! size.
//!
//! A build judged in a static pose is a build whose walk nobody checked, so
//! the preview plays the shipped motions, cross-fading between them through
//! the engine's own transition machine, and breathes. It stands on the art
//! grid's stage — the same ground, light and breath the harness measures.
//!
//! A view is framed one of two ways, and a designer wants both at once.
//! Filling the square with the figure is how the harness measures a cell, so
//! that view at the smallest drawn side is the readability floor, visible
//! while a player chooses rather than discovered by a failing test. But
//! height is a scale of the whole skeleton, so a figure filling its own
//! square is the same size at every height: the view a player shapes the
//! figure in draws every figure at one scale instead.
//!
//! It catches up with a record by what the change costs:
//! [`Change::between`] the record it last showed and the new one — nothing,
//! a re-tint, or a rebuild. None of the three touches the clock, the clip
//! playing or the heading, so an edit never restarts the animation it is
//! being judged by.

use tairix_raster::shape::Placed;
use tairix_wintersun_net::value::Facing;

use crate::breath::Breath;
use crate::design::Change;
use crate::error::FigureError;
use crate::humanoid;
use crate::identity::Identity;
use crate::motion::{Clips, Kind};
use crate::plant::Planted;
use crate::reference::{self, Reference, Staged, BREATH};
use crate::rig::Placement;
use crate::transition::{Advanced, Animator, ClipId, Edge, StateId, Transitions};

/// How long choosing another clip takes to cross-fade into it, in seconds.
pub const FADE: f64 = 0.25;

/// The heading a preview opens at: an eighth of a turn from facing the
/// camera, so the face and the profile both show.
pub const OPENING: Facing = Facing(0x2000);

const _: () = assert!(Kind::ALL.len() <= u8::MAX as usize);

/// Where `kind` sits in the preview's machine: its state, and the clip that
/// state plays, which [`Clips`] holds at the same place.
const fn slot(kind: Kind) -> u8 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the kinds are asserted above to number within a byte"
    )]
    let slot = kind.index() as u8;
    slot
}

const fn state(kind: Kind) -> StateId {
    StateId::new(slot(kind))
}

/// Each kind's state plays its own clip.
const STATES: [ClipId; Kind::ALL.len()] = [
    ClipId::new(slot(Kind::Idle)),
    ClipId::new(slot(Kind::Walk)),
    ClipId::new(slot(Kind::Run)),
];

/// Every clip may give way to every other, ascending as a machine holds its
/// edges.
const EDGES: [Edge; 6] = [
    Edge::new(state(Kind::Idle), state(Kind::Walk), FADE),
    Edge::new(state(Kind::Idle), state(Kind::Run), FADE),
    Edge::new(state(Kind::Walk), state(Kind::Idle), FADE),
    Edge::new(state(Kind::Walk), state(Kind::Run), FADE),
    Edge::new(state(Kind::Run), state(Kind::Idle), FADE),
    Edge::new(state(Kind::Run), state(Kind::Walk), FADE),
];

/// How a view frames the figure in its square.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    /// Every figure at the one scale its side gives the largest figure a
    /// record describes, so a change of height or species reads as one.
    Shared,
    /// The figure filling its square as the art harness frames a cell, so
    /// the view is the readability it is measured at.
    Measured,
}

/// The record being designed, playing.
#[derive(Clone, Debug)]
pub struct Preview<'a> {
    shown: Identity,
    staged: Staged,
    animator: Animator<'a>,
    breath: Breath,
    facing: Facing,
    /// The pose the clock stands at, once a view has asked for it.
    planted: Option<Planted>,
}

impl<'a> Preview<'a> {
    /// A preview of `identity` idling at [`OPENING`], playing `clips`.
    ///
    /// # Errors
    ///
    /// Whatever building the figure refuses, which for a checked identity is
    /// nothing.
    pub fn new(identity: &Identity, clips: &'a Clips<'a>) -> Result<Self, FigureError> {
        let machine = Transitions::new(clips.table(), &STATES, &EDGES, state(Kind::Idle))?;
        Ok(Self {
            shown: *identity,
            staged: Staged::new(identity)?,
            animator: Animator::new(machine),
            breath: Breath::new(BREATH.0, BREATH.1)?,
            facing: OPENING,
            planted: None,
        })
    }

    /// Catch up with `identity`, doing only what its change from the record
    /// last shown costs, and answer what that was.
    ///
    /// # Errors
    ///
    /// Whatever building the figure refuses, which for a checked identity is
    /// nothing; the preview goes on showing the record it had.
    pub fn show(&mut self, identity: &Identity) -> Result<Change, FigureError> {
        let change = Change::between(&self.shown, identity);
        match change {
            Change::Nothing => {}
            Change::Tints => self.staged.retint(identity.tints()),
            Change::Rig => {
                self.staged = Staged::new(identity)?;
                self.planted = None;
            }
        }
        self.shown = *identity;
        Ok(change)
    }

    /// Play `kind`, cross-fading into it over [`FADE`].
    ///
    /// # Errors
    ///
    /// None: every shipped clip may give way to every other.
    pub fn select(&mut self, kind: Kind) -> Result<(), FigureError> {
        self.animator.request(state(kind))?;
        self.planted = None;
        Ok(())
    }

    /// Turn the figure to `facing`.
    pub fn face(&mut self, facing: Facing) {
        self.facing = facing;
    }

    /// Run the clock on by `seconds`, answering how far each live clip moved
    /// — the events it crossed included — as [`Animator::advance`] does.
    ///
    /// # Errors
    ///
    /// [`FigureError::ElapsedUnreal`] for a step that is not finite and
    /// non-negative, which changes nothing.
    pub fn advance(&mut self, seconds: f64) -> Result<Advanced, FigureError> {
        let advanced = self.animator.advance(seconds)?;
        self.breath.advance(seconds)?;
        self.planted = None;
        Ok(advanced)
    }

    /// Place the figure in a square of `side` pixels framed by `frame`, and
    /// answer the contact shadow to paint under it.
    ///
    /// Every view of one moment draws one pose: a large view and the small
    /// one beside it cost one pose between them.
    ///
    /// # Errors
    ///
    /// [`FigureError::ScaleUnreal`] for a side no figure can be framed in,
    /// and otherwise whatever the pose, the planting solve or the placement
    /// refuse, which for a checked identity is nothing.
    pub fn view(
        &mut self,
        frame: Frame,
        side: u32,
        out: &mut Placement,
    ) -> Result<Placed, FigureError> {
        let planted = self.planted()?;
        let reach = match frame {
            Frame::Shared => humanoid::MOST_REACH,
            Frame::Measured => self.staged.rig().reach(),
        };
        let (scale, at) = reference::fit(reach, side)?;
        self.staged.place(&planted, self.facing, scale, at, out)?;
        Reference::shadow(0.0, scale, at)
    }

    /// The pose the clock stands at, planted on the stage.
    pub(crate) fn planted(&mut self) -> Result<Planted, FigureError> {
        if let Some(planted) = self.planted {
            return Ok(planted);
        }
        let pose = self.animator.blend()?.resolve()?;
        let planted = self
            .staged
            .plant(&pose, self.breath, self.animator.root()?)?;
        self.planted = Some(planted);
        Ok(planted)
    }
}

#[cfg(test)]
#[path = "preview/tests.rs"]
mod tests;
