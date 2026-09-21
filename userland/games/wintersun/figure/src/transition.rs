//! Which clip plays, and how one gives way to another.
//!
//! The machine is data: states naming clips, and edges carrying the seconds
//! one state takes to become another. It is checked once when it is
//! assembled — every clip present, every blend positive, every state
//! reachable — so a machine that could strand a figure in a state nothing
//! leads to is refused at load rather than discovered when it happens.
//!
//! What decides *which* state to be in is the simulation's, not the engine's:
//! a consumer maps its own notion of grounded, speed, action and stagger onto
//! a state and asks for it. The engine knows the graph and the timing, and
//! nothing about what a stagger is.

use crate::blend::Blend;
use crate::clip::Clip;
use crate::error::FigureError;

/// How many states one machine holds.
///
/// A bound on authored content rather than a capacity, like a rig's joint
/// count: a machine is code, not input.
pub const MAX_STATES: usize = 64;

const _: () = assert!(MAX_STATES <= u8::MAX as usize);

/// Which clip, by position in its machine's own table.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ClipId(u8);

impl ClipId {
    /// The clip at `index`.
    #[must_use]
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// Its position in the machine's clip table.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Which state, by position in its machine's own table.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct StateId(u8);

impl StateId {
    /// The state at `index`.
    #[must_use]
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// Its position in the machine's state table.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// One state becoming another, and how long it takes.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Edge {
    /// The state being left.
    pub from: StateId,
    /// The state being entered.
    pub to: StateId,
    /// How long the cross-fade lasts, in seconds.
    pub seconds: f64,
}

impl Edge {
    /// An edge from `from` to `to` taking `seconds`.
    #[must_use]
    pub const fn new(from: StateId, to: StateId, seconds: f64) -> Self {
        Self { from, to, seconds }
    }

    /// Its position in the order edges are held in.
    const fn order(self) -> (usize, usize) {
        (self.from.index(), self.to.index())
    }
}

/// A validated graph of states, the clips they play, and the edges between.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Transitions<'a> {
    clips: &'a [Clip<'a>],
    states: &'a [ClipId],
    edges: &'a [Edge],
    initial: StateId,
}

impl<'a> Transitions<'a> {
    /// Assemble and check a machine.
    ///
    /// `edges` are held ascending by `(from, to)`, which is what makes a
    /// state's outgoing edges a contiguous run — so a lookup is a search
    /// rather than a scan, and a repeated edge cannot be spelled.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoStates`] for an empty machine,
    /// [`FigureError::TooManyStates`] beyond what a machine holds,
    /// [`FigureError::NoSuchClip`] for a state naming a clip that is not
    /// there, [`FigureError::NoSuchState`] for an initial state or an edge
    /// end that is not there, [`FigureError::BlendNotPositive`] for a blend
    /// that is not finite and positive,
    /// [`FigureError::EdgesNotAscending`] for edges out of order or repeated,
    /// and [`FigureError::StateUnreachable`] for a state nothing leads to.
    pub fn new(
        clips: &'a [Clip<'a>],
        states: &'a [ClipId],
        edges: &'a [Edge],
        initial: StateId,
    ) -> Result<Self, FigureError> {
        if states.is_empty() {
            return Err(FigureError::NoStates);
        }
        if states.len() > MAX_STATES {
            return Err(FigureError::TooManyStates);
        }
        if initial.index() >= states.len() {
            return Err(FigureError::NoSuchState);
        }
        for clip in states {
            if clip.index() >= clips.len() {
                return Err(FigureError::NoSuchClip);
            }
        }
        for (index, edge) in edges.iter().enumerate() {
            if edge.from.index() >= states.len() || edge.to.index() >= states.len() {
                return Err(FigureError::NoSuchState);
            }
            if !edge.seconds.is_finite() || edge.seconds <= 0.0 {
                return Err(FigureError::BlendNotPositive);
            }
            if index > 0 && edge.order() <= edges[index - 1].order() {
                return Err(FigureError::EdgesNotAscending);
            }
        }

        let machine = Self {
            clips,
            states,
            edges,
            initial,
        };
        machine.check_reachable()?;
        Ok(machine)
    }

    /// The state a figure starts in.
    #[must_use]
    pub const fn initial(&self) -> StateId {
        self.initial
    }

    /// How many states it holds.
    #[must_use]
    pub const fn states(&self) -> usize {
        self.states.len()
    }

    /// The clip `state` plays, or `None` if there is no such state.
    #[must_use]
    pub fn clip(&self, state: StateId) -> Option<Clip<'a>> {
        let id = *self.states.get(state.index())?;
        self.clips.get(id.index()).copied()
    }

    /// The edge from `from` to `to`, or `None` if the machine has none.
    #[must_use]
    pub fn edge(&self, from: StateId, to: StateId) -> Option<Edge> {
        let key = (from.index(), to.index());
        let at = self.edges.partition_point(|edge| edge.order() < key);
        self.edges
            .get(at)
            .copied()
            .filter(|edge| edge.order() == key)
    }

    /// The edges leaving `from`, ascending by destination.
    #[must_use]
    pub fn edges_from(&self, from: StateId) -> &'a [Edge] {
        let index = from.index();
        let start = self.edges.partition_point(|edge| edge.from.index() < index);
        let end = self
            .edges
            .partition_point(|edge| edge.from.index() <= index);
        &self.edges[start..end]
    }

    /// Every state is reachable from the initial one.
    ///
    /// Relaxed to a fixed point rather than walked with a stack: the graph is
    /// authored content of at most [`MAX_STATES`] nodes and this runs once,
    /// so the simpler form that cannot index past its array is the right
    /// trade.
    fn check_reachable(&self) -> Result<(), FigureError> {
        let mut seen = [false; MAX_STATES];
        seen[self.initial.index()] = true;

        let mut spreading = true;
        while spreading {
            spreading = false;
            for edge in self.edges {
                if seen[edge.from.index()] && !seen[edge.to.index()] {
                    seen[edge.to.index()] = true;
                    spreading = true;
                }
            }
        }

        if seen[..self.states.len()].iter().all(|reached| *reached) {
            Ok(())
        } else {
            Err(FigureError::StateUnreachable)
        }
    }
}

/// One clip playing: which state, and how far into it.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Play {
    state: StateId,
    elapsed: f64,
}

/// A clip on its way out, and how much of its cross-fade is left.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Fade {
    play: Play,
    remaining: f64,
    seconds: f64,
}

/// How far one clip's phase moved over an advance.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Advance {
    /// The state whose clip moved.
    pub state: StateId,
    /// The phase it was at.
    pub from: f64,
    /// The phase it reached.
    pub to: f64,
    /// Whole cycles crossed on the way, for a frame longer than the clip.
    ///
    /// Events are reported for the partial interval only, so this is how a
    /// consumer knows a cycle's worth went by unreported rather than
    /// silently losing it.
    pub laps: u32,
}

/// Everything that moved over one advance: at most the current clip and the
/// one fading out from under it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Advanced {
    /// The clip being played into.
    pub current: Advance,
    /// The clip fading out, while one is.
    pub outgoing: Option<Advance>,
}

/// A machine being walked: the state playing, the one fading out, and the
/// pose the two amount to.
///
/// At most two clips are live at once. Asking for a state while a fade is
/// still running replaces the outgoing clip with the one being left, rather
/// than queueing a chain of fades that would take longer to settle than the
/// input that caused them.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Animator<'a> {
    machine: Transitions<'a>,
    current: Play,
    outgoing: Option<Fade>,
}

impl<'a> Animator<'a> {
    /// A figure at the start of `machine`'s initial state.
    #[must_use]
    pub const fn new(machine: Transitions<'a>) -> Self {
        Self {
            current: Play {
                state: machine.initial,
                elapsed: 0.0,
            },
            machine,
            outgoing: None,
        }
    }

    /// The machine it walks.
    #[must_use]
    pub const fn machine(&self) -> Transitions<'a> {
        self.machine
    }

    /// The state playing.
    #[must_use]
    pub const fn state(&self) -> StateId {
        self.current.state
    }

    /// Whether a cross-fade is still running.
    #[must_use]
    pub const fn fading(&self) -> bool {
        self.outgoing.is_some()
    }

    /// Begin the transition to `to`.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchEdge`] if the machine has no edge from the
    /// current state to `to`, which is how a state the simulation should not
    /// be able to jump to stays unreachable from where it is.
    pub fn request(&mut self, to: StateId) -> Result<(), FigureError> {
        if to == self.current.state {
            return Ok(());
        }
        let Some(edge) = self.machine.edge(self.current.state, to) else {
            return Err(FigureError::NoSuchEdge);
        };
        self.outgoing = Some(Fade {
            play: self.current,
            remaining: edge.seconds,
            seconds: edge.seconds,
        });
        self.current = Play {
            state: to,
            elapsed: 0.0,
        };
        Ok(())
    }

    /// Advance every live clip by `seconds`.
    ///
    /// # Errors
    ///
    /// [`FigureError::ElapsedUnreal`] for a step that is not finite and
    /// non-negative, and [`FigureError::NoSuchState`] if a live state has no
    /// clip — which a machine this crate assembled cannot have.
    pub fn advance(&mut self, seconds: f64) -> Result<Advanced, FigureError> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(FigureError::ElapsedUnreal);
        }

        let current = self.step(self.current, seconds)?;
        self.current.elapsed += seconds;

        let outgoing = match self.outgoing {
            None => None,
            Some(mut fade) => {
                let moved = self.step(fade.play, seconds)?;
                fade.play.elapsed += seconds;
                fade.remaining -= seconds;
                self.outgoing = if fade.remaining > 0.0 {
                    Some(fade)
                } else {
                    None
                };
                Some(moved)
            }
        };

        Ok(Advanced { current, outgoing })
    }

    /// How far `play` moves over `seconds`, without committing it.
    fn step(&self, play: Play, seconds: f64) -> Result<Advance, FigureError> {
        let Some(clip) = self.machine.clip(play.state) else {
            return Err(FigureError::NoSuchState);
        };
        let after = play.elapsed + seconds;
        let from = clip.phase_at(play.elapsed)?;
        let to = clip.phase_at(after)?;
        let laps = clip.laps_between(play.elapsed, after);
        Ok(Advance {
            state: play.state,
            from,
            to,
            laps,
        })
    }

    /// The pose the live clips amount to.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchState`] if a live state has no clip, which a
    /// machine this crate assembled cannot have; otherwise as
    /// [`Blend::add_clip`].
    pub fn blend(&self) -> Result<Blend, FigureError> {
        let mut blend = Blend::EMPTY;
        let share = match self.outgoing {
            None => 1.0,
            Some(fade) => 1.0 - fade.remaining / fade.seconds,
        };
        self.mix(&mut blend, self.current, share)?;
        if let Some(fade) = self.outgoing {
            self.mix(&mut blend, fade.play, 1.0 - share)?;
        }
        Ok(blend)
    }

    fn mix(&self, blend: &mut Blend, play: Play, weight: f64) -> Result<(), FigureError> {
        let Some(clip) = self.machine.clip(play.state) else {
            return Err(FigureError::NoSuchState);
        };
        blend.add_clip(clip, clip.phase_at(play.elapsed)?, weight)
    }
}

#[cfg(test)]
#[path = "transition/tests.rs"]
mod tests;
