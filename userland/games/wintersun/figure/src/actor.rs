//! One character in the game's world: a figure played by the simulation it
//! belongs to.
//!
//! The simulation says where a body is, which way it faces and what it is
//! doing; an [`Actor`] turns that into a pose, frame by frame, and places it
//! on the ground the scene is drawn on.
//!
//! # A performance is locomotion under everything
//!
//! Standing, walking and running are chosen by how fast the body is moving,
//! with a margin either side of each change so a body at the edge of one
//! does not flicker. The walk and the run share one gait phase, driven by
//! the distance the body has covered rather than by a clock, at the stride
//! of whichever gait is playing: a planted foot stays planted at any speed,
//! and a change of gait fades between the two at the one phase their feet
//! share.
//!
//! Over locomotion sit two action layers, each an animator over its own
//! machine: one that takes the whole body (a dodge, a heavy blow, a stagger,
//! falling, dying, sitting, swimming, climbing) and one that takes only the
//! trunk, head and arms (a light blow, the bow, a cast, a flinch), which a
//! figure plays over whatever its legs are doing. A layer fades in as it
//! takes a clip up and out as it hands the body back; an action hands it
//! back by itself when it has played through.
//!
//! # The ground a figure stands on is the ground drawn
//!
//! The world is drawn from directly above, with height shown by shading
//! alone, so the plane a figure's feet meet on screen is level everywhere.
//! A figure is therefore planted on the level whatever the terrain's height
//! beneath it. Standing water is the exception that shows: a figure wading
//! stands on the bed, below the surface drawn over it, so it is sunk by the
//! depth and nothing of it is drawn beneath the waterline.

use tairix_inline::ArrayVec;
use tairix_raster::shape::{Placed, Shape};
use tairix_raster::Color;
use tairix_util::mathf;
use tairix_wintersun_net::value::{Facing, WorldPoint};

use crate::blend::Blend;
use crate::breath::Breath;
use crate::clip::Clip;
use crate::error::FigureError;
use crate::gait::Gait;
use crate::humanoid::{self, UPPER_BODY};
use crate::identity::Identity;
use crate::motion::{Clips, Kind, Layer};
use crate::plant::Planted;
use crate::pose::{Mask, Pose};
use crate::reference::{self, Staged, BREATH, SHADOW};
use crate::rig::Placement;
use crate::shadow::{Contact, Light, PENUMBRA};
use crate::socket::Side;
use crate::transition::{Animator, ClipId, Edge, StateId, Transitions};

/// World sub-units per figure-local unit: how big a figure is in the world.
///
/// The one conversion between the rig's own units and the world's. It puts a
/// reference-build figure a little over two cells tall, which is the scale at
/// which a character moving at the simulation's default pace runs at the run
/// clip's own cadence, and at which the smallest figure a record describes is
/// still drawn at the harness's readability floor at the game's default zoom
/// and the stop beyond it.
pub const WORLD_SCALE: f64 = 24.0;

/// How far from its ground point any figure draws, in world sub-units,
/// rounded up: the largest figure a record describes, in the motion that
/// reaches furthest from its ground point either way.
///
/// What a scene culls figures by, so one standing just off the view is still
/// drawn where it reaches into it. A contact shadow reaches far less far.
pub const REACH: i32 = {
    let mut furthest = 0.0;
    let mut index = 0;
    while index < Kind::ALL.len() {
        let (above, below) = reference::allowance(Kind::ALL[index]);
        let most = if above > below { above } else { below };
        if most > furthest {
            furthest = most;
        }
        index += 1;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a figure's reach in sub-units is a few thousand"
    )]
    let reach = (humanoid::MOST_REACH * furthest * WORLD_SCALE) as i32 + 1;
    reach
};

/// How long a figure must stand still before it sits down, in seconds.
const SIT_AFTER: f64 = 20.0;

/// How quickly a figure's shown heading swings round to its body's, in
/// [`Facing`] units per second: three whole turns, so an about-face takes a
/// sixth of a second and reads as a turn rather than a flip.
const TURN_RATE: f64 = 3.0 * 65_536.0;

/// How quickly the speed a gait is chosen at follows the body's, in seconds:
/// the time constant of a first-order lag, long enough to ride out a tick's
/// quantisation and short enough that a start or a stop reads at once.
const SPEED_SETTLE: f64 = 0.1;

/// How long an action layer takes to hand the body back, in seconds.
const RELEASE: f64 = 0.2;

/// How deep, in figure-local units, water must be before a figure standing
/// in it throws no contact shadow at all: the shadow thins over this much
/// depth rather than vanishing at the first drop.
const SHADOW_DROWN: f64 = 4.0;

/// The clips the whole-body layer plays.
const BODY: [Kind; 8] = [
    Kind::Dodge,
    Kind::MeleeHeavy,
    Kind::Stagger,
    Kind::Fall,
    Kind::Die,
    Kind::Sit,
    Kind::Swim,
    Kind::Climb,
];

/// The clips the upper-body layer plays.
const UPPER: [Kind; 6] = [
    Kind::MeleeLight,
    Kind::Draw,
    Kind::Loose,
    Kind::Cast,
    Kind::Channel,
    Kind::Hit,
];

/// How long a layer takes to fade into `kind`, in seconds: a flinch is
/// immediate, a figure settling onto the ground is not.
const fn fade_into(kind: Kind) -> f64 {
    match kind {
        Kind::Loose => 0.03,
        Kind::Hit | Kind::Stagger => 0.05,
        Kind::Dodge | Kind::MeleeLight => 0.08,
        Kind::MeleeHeavy | Kind::Cast | Kind::Die => 0.12,
        Kind::Draw | Kind::Fall => 0.15,
        Kind::Channel | Kind::Climb => 0.2,
        Kind::Idle | Kind::Walk | Kind::Run => 0.25,
        Kind::Swim => 0.3,
        Kind::Sit => 0.4,
    }
}

/// A layer's states: each plays its kind's clip, which the shared table
/// holds at the kind's own index.
const fn states<const N: usize>(kinds: [Kind; N]) -> [ClipId; N] {
    let mut states = [ClipId::new(0); N];
    let mut index = 0;
    while index < N {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the motion set is far smaller than a byte"
        )]
        let clip = kinds[index].index() as u8;
        states[index] = ClipId::new(clip);
        index += 1;
    }
    states
}

/// A layer's edges: any of its clips may give way to any other, fading as
/// long as the one being entered takes to fade into. What may follow what is
/// the simulation's to decide; the machine only has to be able to show it.
const fn edges<const N: usize, const E: usize>(kinds: [Kind; N]) -> [Edge; E] {
    assert!(E == N * (N - 1), "a layer's edges are every ordered pair");
    let mut edges = [Edge::new(StateId::new(0), StateId::new(0), 1.0); E];
    let mut at = 0;
    let mut from = 0;
    while from < N {
        let mut to = 0;
        while to < N {
            if from != to {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a layer holds a handful of states"
                )]
                let (leaving, entering) = (from as u8, to as u8);
                edges[at] = Edge::new(
                    StateId::new(leaving),
                    StateId::new(entering),
                    fade_into(kinds[to]),
                );
                at += 1;
            }
            to += 1;
        }
        from += 1;
    }
    edges
}

const BODY_STATES: [ClipId; BODY.len()] = states(BODY);
const BODY_EDGES: [Edge; 56] = edges(BODY);
const UPPER_STATES: [ClipId; UPPER.len()] = states(UPPER);
const UPPER_EDGES: [Edge; 30] = edges(UPPER);

/// Where `kind` sits in the layer that plays it.
fn state_of(kind: Kind) -> Option<StateId> {
    let kinds: &[Kind] = match kind.layer() {
        Layer::Body => &BODY,
        Layer::Upper => &UPPER,
        Layer::Locomotion => return None,
    };
    let at = kinds.iter().position(|held| *held == kind)?;
    u8::try_from(at).ok().map(StateId::new)
}

/// How a contact shadow's edge is drawn.
///
/// Never absent: the shadow is what says where a figure stands and whether
/// it has left the ground, so the cheapest it gets is one hard edge.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Shade {
    /// Feathered through [`PENUMBRA`] nested rings.
    Soft,
    /// One ellipse, as the art harness measures it, which costs one fill
    /// rather than several.
    Hard,
}

/// Where a placed figure lands, beyond its strips.
#[derive(Clone, Debug, PartialEq)]
pub struct Drawn {
    /// The rings of its contact shadow, outermost first: none, one, or
    /// [`PENUMBRA`] of them.
    pub shadow: ArrayVec<Placed, PENUMBRA>,
    /// The first surface row under water, where a figure wades: nothing of
    /// it is drawn there or below.
    pub waterline: Option<i32>,
    /// The surface rows figure and shadow can reach, half-open, so a scene
    /// sorts it into the pieces of a frame it can touch and no others.
    pub rows: (i32, i32),
}

/// One action layer: an animator over its own machine, and how much of the
/// body it has.
#[derive(Copy, Clone, Debug)]
struct Stage<'a> {
    animator: Animator<'a>,
    mask: Mask,
    weight: f64,
    holding: bool,
    rate: f64,
}

impl<'a> Stage<'a> {
    fn new(machine: Transitions<'a>, mask: Mask) -> Self {
        Self {
            animator: Animator::new(machine),
            mask,
            weight: 0.0,
            holding: false,
            rate: 1.0 / RELEASE,
        }
    }

    /// Take up `state`, fading in over `fade`.
    ///
    /// A layer showing nothing starts the clip from the top; one already
    /// showing a clip cross-fades from it along the machine's edge.
    fn play(&mut self, state: StateId, fade: f64) -> Result<(), FigureError> {
        if self.weight <= 0.0 {
            self.animator.restart(state)?;
        } else {
            self.animator.request(state)?;
        }
        self.holding = true;
        self.rate = 1.0 / fade;
        Ok(())
    }

    /// Hand the body back.
    fn release(&mut self) {
        self.holding = false;
        self.rate = 1.0 / RELEASE;
    }

    fn advance(&mut self, seconds: f64) -> Result<(), FigureError> {
        if self.weight <= 0.0 && !self.holding {
            return Ok(());
        }
        self.animator.advance(seconds)?;
        let playing = self
            .animator
            .machine()
            .clip(self.animator.state())
            .ok_or(FigureError::NoSuchState)?;
        // An action ends by itself; a state lasts until it is released.
        if self.holding && playing.segments().is_some() && self.animator.done()? {
            self.release();
        }
        let target = if self.holding { 1.0 } else { 0.0 };
        let step = self.rate * seconds;
        self.weight = if self.weight < target {
            mathf::fmin(self.weight + step, target)
        } else {
            mathf::fmax(self.weight - step, target)
        };
        Ok(())
    }

    /// Lay this layer over `base`, as far as it has the body.
    fn over(&self, base: &Pose) -> Result<Pose, FigureError> {
        if self.weight <= 0.0 {
            return Ok(*base);
        }
        let own = self.animator.blend()?;
        let covers = own.written().intersection(self.mask);
        let mut mixed = Blend::EMPTY;
        mixed.add_pose(base, Mask::ALL.difference(covers), 1.0)?;
        mixed.add_pose(base, covers, 1.0 - self.weight)?;
        mixed.add_pose(&own.resolve()?, covers, self.weight)?;
        mixed.resolve()
    }

    /// The height this layer holds the body at, laid over `base` as far as
    /// it has the body.
    fn lift(&self, base: f64) -> Result<f64, FigureError> {
        if self.weight <= 0.0 {
            return Ok(base);
        }
        Ok(base + (self.animator.root()? - base) * self.weight)
    }

    /// Which kind it is playing, if it has any of the body at all.
    fn playing(&self, kinds: &[Kind]) -> Option<Kind> {
        (self.weight > 0.0 || self.holding)
            .then(|| kinds.get(self.animator.state().index()).copied())
            .flatten()
    }
}

/// Standing, walking and running: one gait at a time, chosen by the body's
/// speed, and the one it gives way to while it fades out.
///
/// Not a blend weighed by speed. The walk and the run are authored with
/// different stances, so their joint angles mixed at a steady weight put a
/// foot where neither clip does: measured across the speeds between their
/// paces, such a blend sank a planted foot half a unit into the floor and
/// slid it by several. One gait at its own stride leaves the planted foot
/// still at any speed, so the mix is confined to a short fade when the gait
/// changes, with both gaits read at the one phase the feet share.
#[derive(Copy, Clone, Debug)]
struct Locomotion<'a> {
    idle: Clip<'a>,
    walk: Clip<'a>,
    run: Clip<'a>,
    /// How far one cycle of each gait carries this figure, fitted to its rig.
    strides: [f64; 2],
    /// Where the shared gait cycle is.
    gait: Gait,
    /// How long the figure has been standing, which the idle is timed by.
    resting: f64,
    /// The speed a gait is chosen at, in figure-local units a second.
    speed: f64,
    /// The gait playing.
    current: Kind,
    /// The gait fading out, and how much of its fade is left, in seconds.
    outgoing: Option<(Kind, f64)>,
}

/// How long one gait takes to give way to another, in seconds.
const GAIT_FADE: f64 = 0.25;

/// What fraction of the walk's pace a body must reach before it walks, and
/// what it must fall below before it stands again. Apart, so a body nudged
/// at the edge of moving does not flicker between the two.
const WALK_ONSET: (f64, f64) = (0.12, 0.04);

/// Either side of the pace midway between the walk's and the run's, how far
/// a body must go before the gait changes, as a fraction of that pace.
const RUN_BAND: f64 = 0.1;

impl<'a> Locomotion<'a> {
    fn new(clips: &Clips<'a>, staged: &Staged) -> Result<Self, FigureError> {
        let clip = |kind: Kind| clips.table().get(kind.index()).copied();
        let (idle, walk, run) = (
            clip(Kind::Idle).ok_or(FigureError::NoSuchClip)?,
            clip(Kind::Walk).ok_or(FigureError::NoSuchClip)?,
            clip(Kind::Run).ok_or(FigureError::NoSuchClip)?,
        );
        let rigging = humanoid::rigging(staged.rig())?;
        let legs = staged.legs();
        let walked = Gait::fitted(&rigging, walk, &legs, Side::Left)?;
        let ran = Gait::fitted(&rigging, run, &legs, Side::Left)?;
        Ok(Self {
            idle,
            walk,
            run,
            strides: [walked.stride(), ran.stride()],
            gait: walked,
            resting: 0.0,
            speed: 0.0,
            current: Kind::Idle,
            outgoing: None,
        })
    }

    /// Each gait's own pace: the speed it plays at its authored cadence.
    fn paces(&self) -> [f64; 2] {
        [
            self.strides[0] / self.walk.seconds(),
            self.strides[1] / self.run.seconds(),
        ]
    }

    /// The gait a body moving at the current speed should be in, given the
    /// one it is in.
    fn chosen(&self) -> Kind {
        let [walk, run] = self.paces();
        let between = f64::midpoint(walk, run);
        let (onset, halt) = (WALK_ONSET.0 * walk, WALK_ONSET.1 * walk);
        let (quicken, slow) = (between * (1.0 + RUN_BAND), between * (1.0 - RUN_BAND));
        match self.current {
            Kind::Walk | Kind::Run if self.speed < halt => Kind::Idle,
            Kind::Idle | Kind::Walk if self.speed > quicken => Kind::Run,
            Kind::Idle if self.speed > onset => Kind::Walk,
            Kind::Run if self.speed < slow => Kind::Walk,
            held => held,
        }
    }

    fn advance(&mut self, seconds: f64, travelled: f64) -> Result<(), FigureError> {
        let observed = if seconds > 0.0 {
            travelled / seconds
        } else {
            self.speed
        };
        let settle = 1.0 - mathf::exp(-seconds / SPEED_SETTLE);
        self.speed += (observed - self.speed) * settle;

        self.outgoing = self
            .outgoing
            .map(|(kind, left)| (kind, left - seconds))
            .filter(|(_, left)| *left > 0.0);
        let chosen = self.chosen();
        if chosen != self.current {
            self.outgoing = Some((self.current, GAIT_FADE));
            self.current = chosen;
        }
        if let Some(stride) = self.stride(self.current) {
            self.gait = self.gait.restrided(stride)?;
        }
        self.gait.travel(travelled)?;
        self.resting = if travelled > 0.0 {
            0.0
        } else {
            self.resting + seconds
        };
        Ok(())
    }

    /// How far a cycle of `kind` carries the figure, for a gait that moves.
    fn stride(&self, kind: Kind) -> Option<f64> {
        match kind {
            Kind::Walk => Some(self.strides[0]),
            Kind::Run => Some(self.strides[1]),
            _ => None,
        }
    }

    /// `kind`'s clip and the phase it stands at now.
    fn sampled(&self, kind: Kind) -> Result<(Clip<'a>, f64), FigureError> {
        Ok(match kind {
            Kind::Walk => (self.walk, self.gait.phase()),
            Kind::Run => (self.run, self.gait.phase()),
            _ => (self.idle, self.idle.phase_at(self.resting)?),
        })
    }

    /// The pose the gait playing, and the one fading out, amount to, and the
    /// height they hold the body at.
    fn pose(&self) -> Result<(Pose, f64), FigureError> {
        let share = self
            .outgoing
            .map_or(1.0, |(_, left)| 1.0 - left / GAIT_FADE);
        let (clip, phase) = self.sampled(self.current)?;
        let mut blend = Blend::EMPTY;
        blend.add_clip(clip, phase, share)?;
        let mut lift = share * clip.root_at(phase);
        if let Some((kind, _)) = self.outgoing {
            let (fading, at) = self.sampled(kind)?;
            blend.add_clip(fading, at, 1.0 - share)?;
            lift += (1.0 - share) * fading.root_at(at);
        }
        Ok((blend.resolve()?, mathf::clamp(lift, -1.0, 1.0)))
    }
}

/// A figure in the world, played by the simulation it belongs to.
#[derive(Clone, Debug)]
pub struct Actor<'a> {
    staged: Staged,
    footprint: u16,
    moving: Locomotion<'a>,
    body: Stage<'a>,
    upper: Stage<'a>,
    breath: Breath,
    heading: Facing,
    wading: f64,
}

impl<'a> Actor<'a> {
    /// `identity`'s figure standing still, facing `heading`, playing `clips`.
    ///
    /// # Errors
    ///
    /// Whatever building the figure or fitting its gaits refuses, which for a
    /// checked identity and the shipped clips is nothing.
    pub fn new(
        identity: &Identity,
        clips: &'a Clips<'a>,
        heading: Facing,
    ) -> Result<Self, FigureError> {
        let staged = Staged::new(identity)?;
        let footprint = footprint(&staged)?;
        Ok(Self {
            moving: Locomotion::new(clips, &staged)?,
            body: Stage::new(
                Transitions::new(clips.table(), &BODY_STATES, &BODY_EDGES, StateId::new(0))?,
                Mask::ALL,
            ),
            upper: Stage::new(
                Transitions::new(clips.table(), &UPPER_STATES, &UPPER_EDGES, StateId::new(0))?,
                UPPER_BODY,
            ),
            breath: Breath::new(BREATH.0, BREATH.1)?,
            heading,
            wading: 0.0,
            footprint,
            staged,
        })
    }

    /// The radius of the circle the figure's body occupies on the ground, in
    /// world sub-units: half its width across the arms at rest.
    ///
    /// Its widest, so two bodies the simulation lets stand touching are drawn
    /// touching and never drawn through one another.
    #[must_use]
    pub const fn footprint(&self) -> u16 {
        self.footprint
    }

    /// The way the figure is shown facing, which turns toward its body's at
    /// a bounded rate rather than flipping with it.
    #[must_use]
    pub const fn heading(&self) -> Facing {
        self.heading
    }

    /// Which clip each action layer is playing, whole body then upper body,
    /// while it has any of the body.
    #[must_use]
    pub fn performing(&self) -> (Option<Kind>, Option<Kind>) {
        (self.body.playing(&BODY), self.upper.playing(&UPPER))
    }

    /// Play `kind` on the layer it belongs to.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchState`] for locomotion, which the body's own
    /// movement plays and nothing asks for.
    pub fn perform(&mut self, kind: Kind) -> Result<(), FigureError> {
        let state = state_of(kind).ok_or(FigureError::NoSuchState)?;
        let layer = match kind.layer() {
            Layer::Upper => &mut self.upper,
            Layer::Body | Layer::Locomotion => &mut self.body,
        };
        layer.play(state, fade_into(kind))?;
        Ok(())
    }

    /// Hand the body back from whatever state the whole-body layer is in —
    /// a figure standing up, surfacing or letting go — and the arms back from
    /// whatever the upper layer holds.
    pub fn settle(&mut self) {
        self.body.release();
        self.upper.release();
    }

    /// Run the figure on by `nanos` of real time, over which its body moved
    /// `moved` world sub-units and now faces `toward`, standing in water
    /// `submerged` world sub-units deep.
    ///
    /// Every argument is the simulation's, read between its ticks: the figure
    /// never moves itself.
    ///
    /// # Errors
    ///
    /// [`FigureError::ElapsedUnreal`] never, for a real time; otherwise
    /// whatever a layer refuses, which a machine this module assembled
    /// cannot.
    pub fn advance(
        &mut self,
        nanos: u64,
        moved: (i32, i32),
        toward: Facing,
        submerged: i32,
    ) -> Result<(), FigureError> {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a frame's nanoseconds are far below the mantissa's own range"
        )]
        let seconds = nanos as f64 / 1e9;
        let travelled = mathf::hypot(f64::from(moved.0), f64::from(moved.1)) / WORLD_SCALE;
        self.moving.advance(seconds, travelled)?;
        self.body.advance(seconds)?;
        self.upper.advance(seconds)?;
        self.breath.advance(seconds)?;
        self.heading = turned(self.heading, toward, seconds);
        self.wading = f64::from(submerged.max(0)) / WORLD_SCALE;

        // Standing still long enough on dry ground, a figure sits down; any
        // movement brings it back up.
        let seated = self.body.playing(&BODY) == Some(Kind::Sit);
        if travelled > 0.0 && seated {
            self.body.release();
        } else if !seated
            && self.body.playing(&BODY).is_none()
            && self.wading <= 0.0
            && self.moving.resting >= SIT_AFTER
        {
            self.perform(Kind::Sit)?;
        }
        Ok(())
    }

    /// Place the figure for its current moment, standing at `ground` in a
    /// view whose top-left pixel samples `origin` at `step` world sub-units a
    /// pixel, under `light`, with its contact shadow drawn as `shade` says.
    ///
    /// # Errors
    ///
    /// [`FigureError::ScaleUnreal`] for a step that is not a positive whole
    /// number of sub-units; otherwise whatever the pose, the planting solve
    /// or the placement refuse, which for a checked identity is nothing.
    pub fn place(
        &self,
        ground: WorldPoint,
        origin: WorldPoint,
        step: i32,
        light: Light,
        shade: Shade,
        out: &mut Placement,
    ) -> Result<Drawn, FigureError> {
        if step <= 0 {
            return Err(FigureError::ScaleUnreal);
        }
        let planted = self.planted()?;
        let pixel = f64::from(step);
        let scale = WORLD_SCALE / pixel;
        let surface = (
            (f64::from(ground.x) - f64::from(origin.x)) / pixel,
            (f64::from(ground.y) - f64::from(origin.y)) / pixel,
        );
        // A wading figure stands on the bed, below the surface drawn over it.
        let sunk = self.wading * scale;
        let feet = (surface.0, surface.1 + sunk);
        self.staged
            .place(&planted, self.heading, scale, feet, out)?;

        let mut shadow = ArrayVec::new();
        let drowned = mathf::clamp(self.wading / SHADOW_DROWN, 0.0, 1.0);
        if drowned < 1.0 {
            let (radius, tone) = SHADOW;
            let faded = Color::rgba(tone.r, tone.g, tone.b, fade(tone.a, 1.0 - drowned));
            let contact = Contact::new(radius, faded)?;
            match shade {
                Shade::Soft => {
                    for ring in contact.penumbra(light, 0.0, scale, surface)? {
                        let _ = shadow.try_push(ring);
                    }
                }
                Shade::Hard => {
                    let _ = shadow.try_push(contact.cast(light, 0.0, scale, surface)?);
                }
            }
        }

        let waterline = (self.wading > 0.0).then(|| row(surface.1));
        Ok(Drawn {
            rows: reach(out, &shadow),
            shadow,
            waterline,
        })
    }

    /// The pose the current moment stands in, planted on the level.
    fn planted(&self) -> Result<Planted, FigureError> {
        let (locomotion, lift) = self.moving.pose()?;
        let body = self.body.over(&locomotion)?;
        let lift = self.body.lift(lift)?;
        let pose = self.upper.over(&body)?;
        self.staged.plant(&pose, self.breath, lift)
    }
}

/// Whether every figure a record describes is still drawn at a size the art
/// harness holds readable, at `step` world sub-units a pixel.
///
/// Taken at the smallest figure, since every figure in one scene shares one
/// scale, and at the smallest of the harness's own sides — the readability
/// floor its checks are proven at. A view drawn coarser than this is one the
/// harness has said nothing about.
#[must_use]
pub fn readable(step: i32) -> bool {
    if step <= 0 {
        return false;
    }
    let scale = WORLD_SCALE / f64::from(step);
    reference::side_at(humanoid::LEAST_REACH, scale) >= f64::from(reference::SIDES[0])
}

/// `shown` swung toward `toward` by at most [`TURN_RATE`] over `seconds`,
/// the short way round.
fn turned(shown: Facing, toward: Facing, seconds: f64) -> Facing {
    // The wrapping difference read as signed is the short way round.
    #[allow(
        clippy::cast_possible_wrap,
        reason = "a heading's difference is a turn either way, which is what \
                  a signed half-turn range holds"
    )]
    let apart = toward.0.wrapping_sub(shown.0) as i16;
    let limit = mathf::fmin(TURN_RATE * mathf::fmax(seconds, 0.0), 32_768.0);
    let swing = mathf::clamp(f64::from(apart), -limit, limit);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the swing is clamped inside a half turn, which an i16 holds"
    )]
    let swing = mathf::round_i32(swing) as i16;
    #[allow(
        clippy::cast_sign_loss,
        reason = "a signed turn added with wrapping is the heading it reaches"
    )]
    Facing(shown.0.wrapping_add(swing as u16))
}

/// `alpha` thinned to `left` of itself.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "`left` is clamped into 0..=1 and the alpha is a byte, so the \
              rounded product is inside 0..=255 before the cast"
)]
fn fade(alpha: u8, left: f64) -> u8 {
    (f64::from(alpha) * mathf::clamp(left, 0.0, 1.0) + 0.5) as u8
}

/// The surface row a real row coordinate falls in.
fn row(y: f64) -> i32 {
    let floored = mathf::floor(y);
    if floored <= f64::from(i32::MIN) {
        i32::MIN
    } else if floored >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        mathf::round_i32(floored)
    }
}

/// The surface rows a placed figure and its shadow can reach, half-open.
fn reach(placement: &Placement, shadow: &[Placed]) -> (i32, i32) {
    let unit = tairix_raster::surface::SUBPIXEL;
    let (mut top, mut bottom) = match placement.extent() {
        Some((_, top, _, bottom)) => (top.div_euclid(unit), bottom.div_euclid(unit) + 1),
        None => (i32::MAX, i32::MIN),
    };
    for ring in shadow {
        let Shape::Superellipse { rx, ry, .. } = ring.shape else {
            continue;
        };
        let spread = mathf::fmax(rx, ry);
        top = top.min(row(ring.y - spread));
        bottom = bottom.max(row(ring.y + spread).saturating_add(1));
    }
    if top > bottom {
        (0, 0)
    } else {
        (top, bottom)
    }
}

/// Half the figure's width across the arms at rest, in world sub-units,
/// rounded up.
fn footprint(staged: &Staged) -> Result<u16, FigureError> {
    /// Placed large, so the sub-pixel grid the extent is read on is far
    /// finer than a world sub-unit.
    const SCALE: f64 = 64.0;
    let mut placement = Placement::new();
    let rest = staged.plant(&Pose::REST, Breath::new(BREATH.0, BREATH.1)?, 0.0)?;
    // Facing the viewer, the width across the body is the screen's.
    staged.place(&rest, Facing(0x4000), SCALE, (0.0, 0.0), &mut placement)?;
    let (left, _, right, _) = placement.extent().ok_or(FigureError::NoSuchJoint)?;
    let width = f64::from(right - left) / f64::from(tairix_raster::surface::SUBPIXEL) / SCALE;
    let radius = mathf::ceil(width * 0.5 * WORLD_SCALE);
    u16::try_from(mathf::round_i32(radius)).map_err(|_| FigureError::GeometryUnreal)
}

#[cfg(test)]
#[path = "actor/tests.rs"]
mod tests;
