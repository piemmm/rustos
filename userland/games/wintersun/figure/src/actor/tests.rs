//! What an actor does with what the simulation tells it.

use tairix_raster::shape::Shape;
use tairix_raster::surface::SUBPIXEL;
use tairix_util::mathf;
use tairix_wintersun_net::value::{Facing, WorldPoint};

use super::{
    readable, turned, Actor, Shade, RELEASE, SHADOW_DROWN, SIT_AFTER, TURN_RATE, WORLD_SCALE,
};
use crate::error::FigureError;
use crate::humanoid::LEAST_REACH;
use crate::identity::{Identity, Setting};
use crate::motion::{Clips, Kind, Layer, Set};
use crate::reference::{self, Reference, SIDES};
use crate::rig::Placement;
use crate::shadow::PENUMBRA;
use crate::species::Species;

/// A sixtieth of a second, in nanoseconds.
const FRAME: u64 = 16_666_667;

/// A frame, in seconds.
const FRAME_SECONDS: f64 = 1.0 / 60.0;

/// The step the game's default zoom draws at, in world sub-units a pixel.
const DEFAULT_STEP: i32 = 32;

fn human() -> Identity {
    reference::identity(Species::Human).expect("the reference human")
}

fn light() -> crate::shadow::Light {
    Reference::light().expect("the sun")
}

/// How far a figure moving at `speed` figure-local units a second goes in
/// one frame, in world sub-units along the east axis.
fn step_at(speed: f64) -> (i32, i32) {
    (mathf::round_i32(speed * WORLD_SCALE * FRAME_SECONDS), 0)
}

/// Advance `actor` `frames` frames moving at `speed` along the east axis.
fn run(actor: &mut Actor<'_>, frames: u32, speed: f64) {
    for _ in 0..frames {
        actor
            .advance(FRAME, step_at(speed), Facing(0), 0)
            .expect("a real frame");
    }
}

fn place(actor: &Actor<'_>, out: &mut Placement) -> super::Drawn {
    actor
        .place(
            WorldPoint { x: 3200, y: 6400 },
            WorldPoint { x: 0, y: 0 },
            DEFAULT_STEP,
            light(),
            Shade::Hard,
            out,
        )
        .expect("it places")
}

fn clips(set: &Set) -> Clips<'_> {
    set.clips().expect("the shipped clips")
}

/// Standing still is the idle, moving at the walk's own pace the walk, and
/// at the run's the run; and a body hovering at the speed between keeps
/// whichever gait it was in rather than flickering between the two.
#[test]
fn a_figure_idles_standing_walks_at_the_walks_pace_and_runs_at_the_runs() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    run(&mut actor, 30, 0.0);
    assert_eq!(actor.moving.current, Kind::Idle);

    let [walk, run_pace] = actor.moving.paces();
    assert!(walk < run_pace, "the walk is not the slower gait");
    run(&mut actor, 120, walk);
    assert_eq!(actor.moving.current, Kind::Walk);
    run(&mut actor, 120, run_pace);
    assert_eq!(actor.moving.current, Kind::Run);
    let between = f64::midpoint(walk, run_pace);
    run(&mut actor, 240, between);
    assert_eq!(
        actor.moving.current,
        Kind::Run,
        "a run dropped to a walk at the midpoint"
    );
    run(&mut actor, 120, walk);
    assert_eq!(actor.moving.current, Kind::Walk);
    run(&mut actor, 240, between);
    assert_eq!(
        actor.moving.current,
        Kind::Walk,
        "a walk broke into a run at the midpoint"
    );
    run(&mut actor, 120, 0.0);
    assert_eq!(actor.moving.current, Kind::Idle);
    run(&mut actor, 120, run_pace * 2.0);
    assert_eq!(
        actor.moving.current,
        Kind::Run,
        "a standing start went straight to a run"
    );
}

/// The gait is paced by distance: covering the same ground in more, shorter
/// frames at the same speed leaves the feet at the same place in the cycle,
/// and a figure held still does not walk on the spot.
#[test]
fn the_gait_follows_the_ground_covered_not_the_frames() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut once = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let mut twice = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let walk = once.moving.paces()[0];
    // Settle both at the walk's pace, where the stride is the walk's alone.
    for actor in [&mut once, &mut twice] {
        run(actor, 240, walk);
    }
    let start = (once.moving.gait.phase(), twice.moving.gait.phase());
    assert!(mathf::fabs(start.0 - start.1) < 1e-12);
    let per_frame = step_at(walk);
    for _ in 0..30 {
        once.advance(FRAME, per_frame, Facing(0), 0)
            .expect("a frame");
        for _ in 0..2 {
            twice
                .advance(FRAME / 2, (per_frame.0 / 2, 0), Facing(0), 0)
                .expect("a frame");
        }
    }
    let (a, b) = (once.moving.gait.phase(), twice.moving.gait.phase());
    assert!(mathf::fabs(a - b) < 1e-6, "{a} against {b}");

    let held = once.moving.gait.phase();
    run(&mut once, 60, 0.0);
    assert!(
        mathf::fabs(once.moving.gait.phase() - held) < 1e-12,
        "a figure held still walked on the spot"
    );
}

/// From standing to past the run's pace, the one phase the walk and the run
/// share moves on by no more than the ground covered allows: the feet change
/// gait without skipping a step.
#[test]
fn the_walk_and_the_run_keep_one_phase_across_every_speed() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let [walk, run_pace] = actor.moving.paces();
    let shortest = mathf::fmin(actor.moving.strides[0], actor.moving.strides[1]);
    let mut before = actor.moving.gait.phase();
    for frame in 0..600 {
        let speed = run_pace * 1.3 * f64::from(frame) / 600.0;
        let moved = step_at(speed);
        actor.advance(FRAME, moved, Facing(0), 0).expect("a frame");
        let after = actor.moving.gait.phase();
        let moved_by = (after - before).rem_euclid(1.0);
        let most = f64::from(moved.0) / WORLD_SCALE / shortest;
        assert!(
            moved_by <= most + 1e-9,
            "the phase jumped {moved_by} for {most} at {speed}"
        );
        before = after;
    }
    assert!(walk > 0.0);
}

/// A figure swings round to its body's heading the short way, and no faster
/// than the turn rate allows: an about-face takes a sixth of a second.
#[test]
fn a_figure_turns_the_short_way_at_a_bounded_rate() {
    let quarter = turned(Facing(0x1000), Facing(0xF000), 1.0);
    assert_eq!(quarter, Facing(0xF000), "a slow enough turn arrives");
    // From a sixteenth past north to a sixteenth before it goes through
    // north, not the long way round through south.
    let first = turned(Facing(0x1000), Facing(0xF000), 0.001);
    assert!(first.0 < 0x1000, "it turned the long way: {:#06x}", first.0);
    let limit = TURN_RATE * FRAME_SECONDS;
    let swung = turned(Facing(0), Facing(0x8000), FRAME_SECONDS);
    assert!(f64::from(swung.0.min(0u16.wrapping_sub(swung.0))) <= limit + 1.0);
    let mut facing = Facing(0);
    let mut frames = 0;
    while facing != Facing(0x8000) {
        facing = turned(facing, Facing(0x8000), FRAME_SECONDS);
        frames += 1;
        assert!(frames < 60, "the about-face never finished");
    }
    assert!(frames >= 10, "an about-face took only {frames} frames");
    assert_eq!(turned(Facing(0x4000), Facing(0x4000), 1.0), Facing(0x4000));
}

/// An action plays on the layer it belongs to and, having played through,
/// hands the body back by itself.
#[test]
fn an_action_plays_on_its_layer_and_hands_the_body_back_when_it_ends() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    for kind in Kind::ALL
        .into_iter()
        .filter(|kind| kind.layer() != Layer::Locomotion)
    {
        let mut actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
        actor.perform(kind).expect("an action or a state");
        let playing = actor.performing();
        match kind.layer() {
            Layer::Body => assert_eq!(playing, (Some(kind), None), "{}", kind.name()),
            _ => assert_eq!(playing, (None, Some(kind)), "{}", kind.name()),
        }
        let clip = set.clip(kind).expect("its clip");
        let ends = clip.segments().is_some();
        let frames = u32::try_from(mathf::round_i32((clip.seconds() + RELEASE) * 60.0 + 30.0))
            .expect("a short clip");
        run(&mut actor, frames, 0.0);
        let after = actor.performing();
        if ends {
            assert_eq!(after, (None, None), "{} kept the body", kind.name());
        } else {
            assert!(after != (None, None), "{} let go by itself", kind.name());
        }
    }
}

/// A state lasts until it is settled, and settling hands the body back.
#[test]
fn a_state_holds_until_the_figure_settles() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    actor.perform(Kind::Channel).expect("a state");
    actor.perform(Kind::Swim).expect("a state");
    run(&mut actor, 600, 0.0);
    assert_eq!(actor.performing(), (Some(Kind::Swim), Some(Kind::Channel)));
    actor.settle();
    run(&mut actor, 30, 0.0);
    assert_eq!(actor.performing(), (None, None));
}

/// Locomotion is what the body's movement plays, and nothing asks for it.
#[test]
fn locomotion_is_never_asked_for() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    for kind in [Kind::Idle, Kind::Walk, Kind::Run] {
        assert_eq!(actor.perform(kind).err(), Some(FigureError::NoSuchState));
    }
    assert_eq!(actor.performing(), (None, None));
}

/// Left standing long enough a figure sits down, and moving brings it up.
#[test]
fn a_figure_left_standing_sits_and_rises_when_it_moves() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let before = u32::try_from(mathf::round_i32(SIT_AFTER * 60.0)).expect("frames") - 10;
    run(&mut actor, before, 0.0);
    assert_eq!(actor.performing().0, None, "it sat before it had waited");
    run(&mut actor, 20, 0.0);
    assert_eq!(actor.performing().0, Some(Kind::Sit));
    let walk = actor.moving.paces()[0];
    run(&mut actor, 1, walk);
    run(&mut actor, 30, walk);
    assert_eq!(actor.performing().0, None, "it stayed seated while walking");
}

/// A figure standing in water stands on the bed, below the surface drawn
/// over it: sunk by the depth, cut off at the waterline, and its shadow
/// thinning as the water deepens.
#[test]
fn a_wading_figure_is_sunk_below_the_waterline() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut dry = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let mut wet = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let depth = mathf::round_i32(SHADOW_DROWN * WORLD_SCALE * 0.5);
    for _ in 0..60 {
        dry.advance(FRAME, (0, 0), Facing(0), 0).expect("a frame");
        wet.advance(FRAME, (0, 0), Facing(0), depth)
            .expect("a frame");
    }

    let (mut out_dry, mut out_wet) = (Placement::new(), Placement::new());
    let drawn_dry = place(&dry, &mut out_dry);
    let drawn_wet = place(&wet, &mut out_wet);
    assert_eq!(drawn_dry.waterline, None);
    let waterline = drawn_wet
        .waterline
        .expect("a wading figure has a waterline");
    // The ground point is 6400 sub-units down at 32 a pixel.
    assert_eq!(waterline, 200);
    let (_, top_dry, _, bottom_dry) = out_dry.extent().expect("placed");
    let (_, top_wet, _, bottom_wet) = out_wet.extent().expect("placed");
    let sunk = f64::from(depth) / f64::from(DEFAULT_STEP);
    let unit = f64::from(SUBPIXEL);
    assert!(mathf::fabs(f64::from(bottom_wet - bottom_dry) / unit - sunk) < 0.25);
    assert!(mathf::fabs(f64::from(top_wet - top_dry) / unit - sunk) < 0.25);
    assert!(f64::from(bottom_wet) / unit > f64::from(waterline));

    let dry_alpha = drawn_dry.shadow[0].color.a;
    let wet_alpha = drawn_wet.shadow[0].color.a;
    assert!(
        wet_alpha < dry_alpha && wet_alpha > 0,
        "{wet_alpha} of {dry_alpha}"
    );
    let mut drowned = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let deep = mathf::round_i32(SHADOW_DROWN * WORLD_SCALE);
    drowned
        .advance(FRAME, (0, 0), Facing(0), deep)
        .expect("a frame");
    assert!(place(&drowned, &mut out_wet).shadow.is_empty());
}

/// However long a figure stands in water, it never sits down there.
#[test]
fn a_figure_never_sits_in_water() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut wet = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    for _ in 0..u32::try_from(mathf::round_i32(SIT_AFTER * 60.0 + 60.0)).expect("frames") {
        wet.advance(FRAME, (0, 0), Facing(0), 1).expect("a frame");
    }
    assert_eq!(wet.performing().0, None, "a figure sat down in the water");
}

/// The ground point lands exactly where the view samples it, to a fraction
/// of a pixel, and the figure is drawn at the world's own scale.
#[test]
fn placing_puts_the_ground_point_where_the_view_samples_it() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let mut out = Placement::new();
    let origin = WorldPoint { x: -512, y: 1024 };
    let ground = WorldPoint {
        x: origin.x + 100 * DEFAULT_STEP + DEFAULT_STEP / 2,
        y: origin.y + 50 * DEFAULT_STEP,
    };
    let drawn = actor
        .place(ground, origin, DEFAULT_STEP, light(), Shade::Hard, &mut out)
        .expect("it places");
    let shadow = drawn.shadow[0];
    assert!(mathf::fabs(shadow.x - 100.5) < 1e-9 && mathf::fabs(shadow.y - 50.0) < 1e-9);

    // Twice the step draws the same figure at half the size.
    let mut near = Placement::new();
    actor
        .place(
            ground,
            origin,
            DEFAULT_STEP,
            light(),
            Shade::Hard,
            &mut near,
        )
        .expect("it places");
    let mut far = Placement::new();
    actor
        .place(
            ground,
            origin,
            DEFAULT_STEP * 2,
            light(),
            Shade::Hard,
            &mut far,
        )
        .expect("it places");
    let height = |placed: &Placement| {
        let (_, top, _, bottom) = placed.extent().expect("placed");
        f64::from(bottom - top) / f64::from(SUBPIXEL)
    };
    assert!(mathf::fabs(height(&near) / height(&far) - 2.0) < 0.05);

    for step in [0, -32] {
        assert_eq!(
            actor
                .place(ground, origin, step, light(), Shade::Hard, &mut out)
                .err(),
            Some(FigureError::ScaleUnreal)
        );
    }
}

/// The shade says how many rings the shadow is drawn as.
#[test]
fn the_shade_decides_how_the_shadow_is_drawn() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let actor = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let mut out = Placement::new();
    for (shade, rings) in [(Shade::Soft, PENUMBRA), (Shade::Hard, 1)] {
        let drawn = actor
            .place(
                WorldPoint { x: 0, y: 0 },
                WorldPoint { x: -3200, y: -3200 },
                DEFAULT_STEP,
                light(),
                shade,
                &mut out,
            )
            .expect("it places");
        assert_eq!(drawn.shadow.len(), rings, "{shade:?}");
    }
}

/// The rows a placed figure reports hold every point it draws and every
/// ring of its shadow, so a scene sorting it into pieces of a frame never
/// leaves any of it out.
#[test]
fn the_rows_hold_everything_a_figure_draws() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut actor = Actor::new(&human(), &clips, Facing(0x2000)).expect("a figure");
    actor.perform(Kind::Fall).expect("a state");
    run(&mut actor, 40, 30.0);
    let mut out = Placement::new();
    let drawn = actor
        .place(
            WorldPoint { x: 800, y: 800 },
            WorldPoint { x: 0, y: 0 },
            8,
            light(),
            Shade::Soft,
            &mut out,
        )
        .expect("it places");
    let (top, bottom) = drawn.rows;
    for strip in out.strips() {
        for &(_, y) in strip.near.iter().chain(strip.far) {
            let row = y.div_euclid(SUBPIXEL);
            assert!(
                row >= top && row < bottom,
                "row {row} outside {top}..{bottom}"
            );
        }
    }
    for ring in &drawn.shadow {
        let Shape::Superellipse { rx, ry, .. } = ring.shape else {
            panic!("a shadow is an ellipse");
        };
        let spread = mathf::fmax(rx, ry);
        assert!(ring.y - spread >= f64::from(top) && ring.y + spread < f64::from(bottom));
    }
}

/// A figure's footprint is half its width across the arms, and a broader
/// build stands on a wider one.
#[test]
fn the_footprint_is_half_the_width_and_grows_with_girth() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let human = Actor::new(&human(), &clips, Facing(0)).expect("a figure");
    let reach = Reference::new(&self::human())
        .expect("builds")
        .rig()
        .reach();
    let footprint = f64::from(human.footprint());
    assert!(
        footprint > 0.15 * reach * WORLD_SCALE && footprint < 0.3 * reach * WORLD_SCALE,
        "a footprint of {footprint}"
    );
    let mut spec = self::human().spec();
    spec.build.girth = Setting::LOW;
    let slight = Identity::new(spec).expect("a real record");
    spec.build.girth = Setting::HIGH;
    let broad = Identity::new(spec).expect("a real record");
    let slight = Actor::new(&slight, &clips, Facing(0)).expect("a figure");
    let broad = Actor::new(&broad, &clips, Facing(0)).expect("a figure");
    assert!(broad.footprint() > slight.footprint());
}

/// A view is readable exactly while the smallest figure a record describes
/// still fills a cell at least as large as the harness's floor.
#[test]
fn readability_is_judged_at_the_smallest_figure_and_the_harness_floor() {
    assert!(
        readable(DEFAULT_STEP),
        "the default zoom draws below the floor"
    );
    assert!(!readable(DEFAULT_STEP * 4), "a map view counts as readable");
    assert!(!readable(0) && !readable(-8));
    let coarsest = (1..=256)
        .rev()
        .find(|step| readable(*step))
        .expect("some step reads");
    let side = |step: i32| reference::side_at(LEAST_REACH, WORLD_SCALE / f64::from(step));
    assert!(side(coarsest) >= f64::from(SIDES[0]));
    assert!(side(coarsest + 1) < f64::from(SIDES[0]));
    assert!((1..=coarsest).all(readable));
}

/// Every clip, on every species, fades in, plays and places a real figure
/// whose every parameter sits inside its range.
#[test]
fn every_clip_on_every_species_places_a_real_figure() {
    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let mut out = Placement::new();
    for species in Species::ALL {
        let identity = reference::identity(species).expect("a record");
        for kind in Kind::ALL
            .into_iter()
            .filter(|kind| kind.layer() != Layer::Locomotion)
        {
            let mut actor = Actor::new(&identity, &clips, Facing(0x6000)).expect("a figure");
            actor.perform(kind).expect("it plays");
            for frame in 0..90 {
                let speed = f64::from(frame) * 2.0;
                actor
                    .advance(FRAME, step_at(speed), Facing(0x2000), 0)
                    .expect("a frame");
                let drawn = place(&actor, &mut out);
                assert!(drawn.rows.0 < drawn.rows.1, "{species:?} {}", kind.name());
                let planted = actor.planted().expect("a pose");
                for param in crate::pose::Param::ALL {
                    assert!(param.range().holds(planted.pose().get(param)));
                }
            }
        }
    }
}

/// At every steady speed from a stroll to a sprint, a planted foot is on
/// the floor and stays where it was put: the gait at its own stride leaves
/// nothing for a foot to slide by, whatever speed the body moves at.
#[test]
fn a_moving_figure_plants_its_feet_at_every_speed() {
    use crate::humanoid;
    use crate::plant::Legs;
    use crate::quality::{MAX_GROUNDING, MAX_SKATE};
    use crate::rig::{Frames, Resolved};

    let set = Set::new().expect("the shipped set");
    let clips = clips(&set);
    let identity = human();
    let rig = humanoid::rig(&identity).expect("it builds");
    let rigging = humanoid::rigging(&rig).expect("rigged");
    let legs = Legs::new(&rigging, humanoid::legs()).expect("two legs");
    let walk = Actor::new(&identity, &clips, Facing(0))
        .expect("a figure")
        .moving
        .paces()[0];
    // A quarter of a frame at a time, so the ground covered between samples
    // is well inside a foot's contact; and a foot counts as down within a
    // tenth of a unit of the floor, inside the fiftieth of its lift the gait
    // fits its stride over, so no part of a strike or a toe-off is read as
    // the foot sliding.
    let (tick, seconds) = (4_166_667, 1.0 / 240.0);
    let band = 0.1;
    for fraction in [0.3, 0.6, 1.0, 1.4, 1.8, 2.2, 2.6, 3.0, 3.4] {
        let speed = walk * fraction;
        let mut actor = Actor::new(&identity, &clips, Facing(0)).expect("a figure");
        let per_tick = speed * WORLD_SCALE * seconds;
        let mut carried = 0.0;
        let mut frames = Frames::new();
        let mut ground = 0.0;
        let mut lowest = f64::MAX;
        let mut held: [Option<(f64, f64)>; 2] = [None, None];
        let mut worst: f64 = 0.0;
        for tick_index in 0..1920 {
            carried += per_tick;
            let whole = mathf::floor(carried);
            carried -= whole;
            let moved = mathf::round_i32(whole);
            actor
                .advance(tick, (moved, 0), Facing(0), 0)
                .expect("a frame");
            ground += f64::from(moved) / WORLD_SCALE;
            // Measure the steady gait, after the start and its fade.
            if tick_index < 960 {
                continue;
            }
            let planted = actor.planted().expect("a pose");
            rigging
                .posture(&planted.pose())
                .expect("posturable")
                .resolve(Resolved::REST, &mut frames);
            let root = planted.root().at.up / legs.straight();
            let feet = legs.standing(&frames, root).expect("two feet");
            for (index, foot) in feet.iter().enumerate() {
                lowest = mathf::fmin(lowest, foot.up);
                let over = ground + foot.forward;
                if foot.up - legs.sole() < band {
                    let (least, most) = held[index].unwrap_or((over, over));
                    let span = (mathf::fmin(least, over), mathf::fmax(most, over));
                    worst = mathf::fmax(worst, span.1 - span.0);
                    held[index] = Some(span);
                } else {
                    held[index] = None;
                }
            }
        }
        let stride = if actor.moving.current == Kind::Walk {
            actor.moving.strides[0]
        } else {
            actor.moving.strides[1]
        };
        let sunk = mathf::fabs(lowest - legs.sole());
        assert!(
            sunk <= MAX_GROUNDING,
            "at {fraction} of the walk's pace a foot is {sunk} off the floor"
        );
        assert!(
            worst / stride <= MAX_SKATE,
            "at {fraction} of the walk's pace a planted foot slides {worst} of a {stride} stride"
        );
    }
}
