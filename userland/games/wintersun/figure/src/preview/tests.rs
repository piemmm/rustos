//! That the preview draws what the harness measures, and catches up with an
//! edit by exactly what the edit costs and nothing else.

use alloc::vec::Vec;

use tairix_raster::surface::SUBPIXEL;
use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use super::{Frame, Preview, FADE};
use crate::breath::Breath;
use crate::design::{Change, Designer, Edit};
use crate::error::FigureError;
use crate::humanoid;
use crate::identity::{Identity, Setting};
use crate::motion::{self, Kind};
use crate::pose::Param;
use crate::reference::{self, Cell, Reference, Staged, BREATH, FACINGS, SIDES};
use crate::rig::{Placement, Strip};
use crate::socket::Side;
use crate::species::Species;
use crate::transition::Animator;

fn human() -> Identity {
    reference::identity(Species::Human).expect("a real record")
}

fn drawn(placement: &Placement) -> Vec<Strip<'_>> {
    placement.strips().collect()
}

/// One strip's surface and its two boundaries.
type Outline = (u16, Vec<(i32, i32)>, Vec<(i32, i32)>);

/// What a placement's strips cover, without the colours they are filled in.
fn outlines(placement: &Placement) -> Vec<Outline> {
    placement
        .strips()
        .map(|strip| (strip.surface, strip.near.to_vec(), strip.far.to_vec()))
        .collect()
}

/// The clock a preview's figure is animated by: the clips playing and how
/// far into them, the breath, and the heading.
fn clock<'a>(preview: &Preview<'a>) -> (Animator<'a>, Breath, Facing) {
    (preview.animator, preview.breath, preview.facing)
}

/// How far a placement reaches up its square, in sub-pixels above the
/// ground point.
fn tallest(placement: &Placement, side: u32, reach: f64) -> f64 {
    let (_, at) = reference::fit(reach, side).expect("a real cell");
    let ground = at.1 * f64::from(SUBPIXEL);
    placement
        .strips()
        .flat_map(|strip| strip.near.iter().chain(strip.far))
        .map(|point| ground - f64::from(point.1))
        .fold(0.0, mathf::fmax)
}

/// A preview that has not moved yet, framed as the harness frames a cell,
/// is the harness's first idle cell strip for strip and shadow for shadow,
/// at every heading and every drawn size: what a player watches is what the
/// art is measured on.
#[test]
fn a_new_preview_draws_exactly_what_the_harness_measures() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let (mut shown, mut measured) = (Placement::new(), Placement::new());
    for species in Species::ALL {
        let identity = reference::identity(species).expect("a real record");
        let grid = Reference::new(&identity).expect("it builds");
        let mut preview = Preview::new(&identity, &clips).expect("it builds");
        for facing in FACINGS {
            preview.face(facing);
            for side in SIDES {
                let (scale, at) = reference::fit(grid.rig().reach(), side).expect("a real cell");
                let cell = Cell {
                    kind: Kind::Idle,
                    step: 0,
                    facing,
                };
                grid.place(cell, scale, at, &mut measured)
                    .expect("it places");
                let shadow = preview
                    .view(Frame::Measured, side, &mut shown)
                    .expect("it places");
                assert_eq!(
                    drawn(&shown),
                    drawn(&measured),
                    "{species:?} {facing:?} {side}"
                );
                assert_eq!(
                    shadow,
                    Reference::shadow(0.0, scale, at).expect("a real shadow")
                );
            }
        }
    }
}

/// Height is a scale of the whole skeleton, so a figure filling its own
/// square is drawn the same size at every height; drawn at the scale every
/// figure shares, a taller figure is taller, and a dwarf is shorter than an
/// elf.
#[test]
fn height_reads_in_the_shared_frame_and_not_in_the_measured_one() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let side = SIDES[2];
    let reached = |identity: &Identity, frame: Frame| {
        let mut preview = Preview::new(identity, &clips).expect("it builds");
        let mut placement = Placement::new();
        preview
            .view(frame, side, &mut placement)
            .expect("it places");
        let reach = match frame {
            Frame::Shared => humanoid::MOST_REACH,
            Frame::Measured => Staged::new(identity).expect("it builds").rig().reach(),
        };
        tallest(&placement, side, reach)
    };
    let mut designer = Designer::open(human());
    designer.edit(Edit::Height(Setting::LOW)).expect("in range");
    let short = designer.live();
    designer
        .edit(Edit::Height(Setting::HIGH))
        .expect("in range");
    let tall = designer.live();

    let unit = f64::from(SUBPIXEL);
    assert!(
        mathf::fabs(reached(&short, Frame::Measured) - reached(&tall, Frame::Measured)) <= unit,
        "a measured cell showed the height it normalises away"
    );
    assert!(reached(&tall, Frame::Shared) > reached(&short, Frame::Shared) + 8.0 * unit);

    let dwarf = reference::identity(Species::Dwarf).expect("a real record");
    let elf = reference::identity(Species::Elf).expect("a real record");
    assert!(reached(&elf, Frame::Shared) > reached(&dwarf, Frame::Shared) + 8.0 * unit);
}

/// The shared frame fits the largest figure a record describes: drawn in
/// it, nothing leaves its square.
#[test]
fn the_shared_frame_holds_the_largest_figure() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let mut designer = Designer::open(reference::identity(Species::Dragonkin).expect("real"));
    for edit in [
        Edit::Height(Setting::HIGH),
        Edit::Head(Setting::HIGH),
        Edit::Hair(Some(crate::identity::HairStyle::Shaggy)),
        Edit::Volume(Setting::HIGH),
    ] {
        designer.edit(edit).expect("a dragonkin carries it");
    }
    let mut preview = Preview::new(&designer.live(), &clips).expect("it builds");
    let mut placement = Placement::new();
    let edge = i32::try_from(SIDES[2]).expect("a small side") * SUBPIXEL;
    for facing in FACINGS {
        preview.face(facing);
        preview
            .view(Frame::Shared, SIDES[2], &mut placement)
            .expect("it places");
        for strip in placement.strips() {
            for (x, y) in strip.near.iter().chain(strip.far) {
                assert!((0..=edge).contains(x) && (0..=edge).contains(y));
            }
        }
    }
}

#[test]
fn showing_the_record_already_shown_does_nothing() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let mut preview = Preview::new(&human(), &clips).expect("it builds");
    let (mut before, mut after) = (Placement::new(), Placement::new());
    preview
        .view(Frame::Shared, SIDES[2], &mut before)
        .expect("it places");
    assert_eq!(preview.show(&human()), Ok(Change::Nothing));
    preview
        .view(Frame::Shared, SIDES[2], &mut after)
        .expect("it places");
    assert_eq!(drawn(&after), drawn(&before));
}

/// A palette edit re-tints: every strip covers exactly what it covered, only
/// its colour changes, and the figure is posed exactly as it was.
#[test]
fn a_palette_edit_retints_and_moves_nothing() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let mut designer = Designer::open(human());
    let mut preview = Preview::new(&human(), &clips).expect("it builds");
    preview.select(Kind::Walk).expect("every clip gives way");
    preview.advance(0.4).expect("a real step");
    let (mut before, mut after) = (Placement::new(), Placement::new());
    preview
        .view(Frame::Shared, SIDES[2], &mut before)
        .expect("it places");
    let posed = preview.planted().expect("it poses");
    let ticking = clock(&preview);

    designer.edit(Edit::Accent(9)).expect("any dye");
    assert_eq!(preview.show(&designer.live()), Ok(Change::Tints));
    preview
        .view(Frame::Shared, SIDES[2], &mut after)
        .expect("it places");

    assert_eq!(outlines(&after), outlines(&before));
    assert_ne!(drawn(&after), drawn(&before), "the new dye was not drawn");
    assert_eq!(preview.planted().expect("it poses"), posed);
    assert_eq!(clock(&preview), ticking);
}

/// A build edit rebuilds the figure, and nothing it does reaches the clock,
/// the clips playing, the breath or the heading.
#[test]
fn a_build_edit_rebuilds_and_leaves_the_animation_where_it_was() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let mut designer = Designer::open(human());
    let mut preview = Preview::new(&human(), &clips).expect("it builds");
    preview.select(Kind::Run).expect("every clip gives way");
    preview.advance(0.55).expect("a real step");
    let (mut before, mut after) = (Placement::new(), Placement::new());
    preview
        .view(Frame::Shared, SIDES[2], &mut before)
        .expect("it places");
    let ticking = clock(&preview);

    designer
        .edit(Edit::Height(Setting::HIGH))
        .expect("in range");
    assert_eq!(preview.show(&designer.live()), Ok(Change::Rig));
    preview
        .view(Frame::Shared, SIDES[2], &mut after)
        .expect("it places");

    assert_ne!(
        outlines(&after),
        outlines(&before),
        "the figure was not rebuilt"
    );
    assert_eq!(clock(&preview), ticking);
}

/// Choosing a clip fades into it over [`FADE`] and then plays it alone, as
/// the stage would pose it at the phase the clock has reached.
#[test]
fn choosing_a_clip_fades_into_it_and_then_plays_it_alone() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let staged = Staged::new(&human()).expect("it builds");
    let mut preview = Preview::new(&human(), &clips).expect("it builds");
    let knee = |preview: &mut Preview<'_>| {
        preview
            .planted()
            .expect("it poses")
            .pose()
            .get(Param::KneeBend(Side::Left))
    };

    preview.advance(0.3).expect("a real step");
    let idle = knee(&mut preview);
    preview.select(Kind::Run).expect("every clip gives way");
    assert!(preview.animator.fading());
    assert_eq!(
        knee(&mut preview).to_bits(),
        idle.to_bits(),
        "the fade began with a jump"
    );

    preview.advance(FADE / 2.0).expect("a real step");
    assert!(preview.animator.fading());
    let halfway = knee(&mut preview);

    preview.advance(FADE).expect("a real step");
    assert!(!preview.animator.fading(), "the fade outlasted itself");
    let run = motions.clip(Kind::Run).expect("the shipped run");
    let phase = run.phase_at(FADE / 2.0 + FADE).expect("a real phase");
    let mut breath = Breath::new(BREATH.0, BREATH.1).expect("the stage's breath");
    for seconds in [0.3, FADE / 2.0, FADE] {
        breath.advance(seconds).expect("a real step");
    }
    let alone = staged
        .plant(
            &run.sample(phase).expect("in the clip"),
            breath,
            run.root_at(phase),
        )
        .expect("it plants");
    assert_eq!(preview.planted().expect("it poses"), alone);
    assert_ne!(
        halfway.to_bits(),
        idle.to_bits(),
        "the fade did not move the figure"
    );
}

/// Every view of one moment is one figure at its own size: the same strips
/// in the same order and colours, with every point where the larger view's
/// scale puts it, to within the rounding onto each view's grid.
#[test]
fn every_view_of_one_moment_is_one_figure_at_its_size() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let mut preview = Preview::new(&human(), &clips).expect("it builds");
    preview.select(Kind::Walk).expect("every clip gives way");
    preview.advance(0.9).expect("a real step");
    let (small, large) = (SIDES[0], SIDES[2]);
    let (mut little, mut big) = (Placement::new(), Placement::new());
    preview
        .view(Frame::Measured, small, &mut little)
        .expect("it places");
    preview
        .view(Frame::Measured, large, &mut big)
        .expect("it places");

    let reach = Staged::new(&human()).expect("it builds").rig().reach();
    let origin = |side: u32| {
        let (_, at) = reference::fit(reach, side).expect("a real cell");
        (at.0 * f64::from(SUBPIXEL), at.1 * f64::from(SUBPIXEL))
    };
    let (from, to) = (origin(small), origin(large));
    let ratio = f64::from(large) / f64::from(small);
    let (little, big) = (drawn(&little), drawn(&big));
    assert_eq!(little.len(), big.len());
    for (one, other) in little.iter().zip(&big) {
        assert_eq!((one.surface, one.color), (other.surface, other.color));
        for (a, b) in one
            .near
            .iter()
            .chain(one.far)
            .zip(other.near.iter().chain(other.far))
        {
            let expected = (
                to.0 + ratio * (f64::from(a.0) - from.0),
                to.1 + ratio * (f64::from(a.1) - from.1),
            );
            // Each view rounds onto its own grid once, and the smaller
            // view's rounding is magnified by the ratio.
            let tolerance = 0.5 + ratio * 0.5 + 1e-9;
            let off = mathf::fmax(
                mathf::fabs(expected.0 - f64::from(b.0)),
                mathf::fabs(expected.1 - f64::from(b.1)),
            );
            assert!(off <= tolerance, "{a:?} at {small} is not {b:?} at {large}");
        }
    }
}

/// A step that is not a real duration is refused and moves nothing.
#[test]
fn a_step_that_is_not_real_moves_nothing() {
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");
    let mut preview = Preview::new(&human(), &clips).expect("it builds");
    preview.advance(0.2).expect("a real step");
    let ticking = clock(&preview);
    for seconds in [-0.1, f64::NAN, f64::INFINITY] {
        assert_eq!(
            preview.advance(seconds).err(),
            Some(FigureError::ElapsedUnreal)
        );
        assert_eq!(clock(&preview), ticking);
    }
}

/// How long one frame of the simulated surface lasts.
const FRAME: f64 = 1.0 / 60.0;

/// A designer surface as the charter requires one to run: input drains
/// between frames, each frame paints once from the state the input left,
/// and the durable write waits for the interaction to settle.
struct Surface<'a> {
    designer: Designer,
    preview: Preview<'a>,
    /// The same figure receiving no edits, so any effect an edit had on the
    /// animation shows as a difference from it.
    untouched: Preview<'a>,
    large: Placement,
    small: Placement,
    owed: Vec<Change>,
    writes: Vec<Identity>,
}

impl<'a> Surface<'a> {
    fn new(identity: Identity, clips: &'a motion::Clips<'a>) -> Self {
        let mut surface = Self {
            designer: Designer::open(identity),
            preview: Preview::new(&identity, clips).expect("it builds"),
            untouched: Preview::new(&identity, clips).expect("it builds"),
            large: Placement::new(),
            small: Placement::new(),
            owed: Vec::new(),
            writes: Vec::new(),
        };
        for preview in [&mut surface.preview, &mut surface.untouched] {
            preview.select(Kind::Walk).expect("every clip gives way");
        }
        surface
    }

    /// Paint once: catch the preview up with the record, run the clock, and
    /// draw both views.
    fn frame(&mut self) {
        let owed = self.preview.show(&self.designer.live()).expect("it shows");
        self.owed.push(owed);
        self.preview.advance(FRAME).expect("a real step");
        self.untouched.advance(FRAME).expect("a real step");
        self.preview
            .view(Frame::Shared, SIDES[2], &mut self.large)
            .expect("it places");
        self.preview
            .view(Frame::Measured, SIDES[0], &mut self.small)
            .expect("it places");
        assert_eq!(
            clock(&self.preview),
            clock(&self.untouched),
            "an edit reached the animation"
        );
    }

    /// A drag of `samples`, arriving `per_frame` at a time, and its settle.
    fn drag(&mut self, samples: impl Iterator<Item = Edit>, per_frame: usize) {
        let mut pending = 0;
        for edit in samples {
            self.designer.edit(edit).expect("an admitted value");
            pending += 1;
            if pending == per_frame {
                self.frame();
                pending = 0;
            }
        }
        if pending > 0 {
            self.frame();
        }
        self.writes.extend(self.designer.settle());
        self.frame();
    }
}

/// The designer check `plans/FIGURE.md` states, run as a surface runs: a
/// drag's samples arrive a burst per frame; each frame catches up once,
/// whatever its burst held; the drag writes once, when it settles; and no
/// edit reaches the animation it is judged by.
#[test]
fn a_simulated_drag_writes_once_and_repaints_once_per_burst() {
    const BURSTS: usize = 8;
    const PER_FRAME: usize = 6;
    let motions = motion::Set::new().expect("the shipped set");
    let clips = motions.clips().expect("the shipped clips");

    let mut built = Surface::new(human(), &clips);
    built.drag(
        (0..BURSTS * PER_FRAME)
            .map(|sample| Edit::Height(Setting(u8::try_from(sample * 5).expect("in a byte")))),
        PER_FRAME,
    );
    assert_eq!(built.writes, [built.designer.live()], "one drag, one write");
    assert_eq!(
        built.owed.len(),
        BURSTS + 1,
        "one paint per burst, and one after"
    );
    assert!(built.owed[..BURSTS].iter().all(|owed| *owed == Change::Rig));
    assert_eq!(built.owed[BURSTS], Change::Nothing, "the write repainted");

    let mut dyed = Surface::new(human(), &clips);
    let skins = u8::try_from(Species::Human.covering().len()).expect("a small table");
    dyed.drag(
        (0..BURSTS * PER_FRAME)
            .map(|sample| Edit::Skin(u8::try_from(sample).expect("in a byte") % skins)),
        PER_FRAME,
    );
    assert_eq!(dyed.writes, [dyed.designer.live()], "one drag, one write");
    assert!(dyed.owed[..BURSTS]
        .iter()
        .all(|owed| *owed == Change::Tints));
    assert_eq!(dyed.owed[BURSTS], Change::Nothing, "the write repainted");
}
