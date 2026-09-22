//! What a clip refuses, what it samples, and when its events fire.

use tairix_util::mathf;

use super::{Clip, Curve, Easing, Event, Key, Lift, Loop, Travel};
use crate::error::FigureError;
use crate::pose::{Param, Pose};
use crate::socket::Side;

const SLACK: f64 = 1e-9;

const NOD: Param = Param::HeadNod;
const KNEE: Param = Param::KneeBend(Side::Left);

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

fn curve(param: Param, keys: &[Key]) -> Curve<'_> {
    Curve::new(param, keys).expect("a well-formed curve")
}

#[test]
fn every_easing_starts_at_nothing() {
    for easing in [
        Easing::Linear,
        Easing::EaseIn,
        Easing::EaseOut,
        Easing::EaseInOut,
        Easing::Hold,
    ] {
        assert!(
            close(easing.apply(0.0), 0.0),
            "{easing:?} did not start at 0"
        );
    }
}

#[test]
fn every_interpolating_easing_finishes_the_segment() {
    for easing in [
        Easing::Linear,
        Easing::EaseIn,
        Easing::EaseOut,
        Easing::EaseInOut,
    ] {
        assert!(close(easing.apply(1.0), 1.0), "{easing:?} did not reach 1");
    }
}

#[test]
fn a_hold_easing_never_moves_before_the_next_key() {
    for step in 0..100 {
        let t = f64::from(step) / 100.0;
        assert!(close(Easing::Hold.apply(t), 0.0));
    }
}

#[test]
fn no_easing_overshoots() {
    for easing in [
        Easing::Linear,
        Easing::EaseIn,
        Easing::EaseOut,
        Easing::EaseInOut,
        Easing::Hold,
    ] {
        for step in 0..=1000 {
            let t = f64::from(step) / 1000.0;
            let eased = easing.apply(t);
            assert!(
                (0.0..=1.0).contains(&eased),
                "{easing:?} left the unit interval at {t}: {eased}"
            );
        }
    }
}

#[test]
fn no_easing_ever_goes_backwards() {
    for easing in [
        Easing::Linear,
        Easing::EaseIn,
        Easing::EaseOut,
        Easing::EaseInOut,
        Easing::Hold,
    ] {
        let mut previous = easing.apply(0.0);
        for step in 1..=1000 {
            let eased = easing.apply(f64::from(step) / 1000.0);
            assert!(eased >= previous - SLACK, "{easing:?} went backwards");
            previous = eased;
        }
    }
}

#[test]
fn a_curve_with_no_keys_is_refused() {
    assert_eq!(Curve::new(NOD, &[]).err(), Some(FigureError::CurveEmpty));
}

#[test]
fn keys_that_do_not_ascend_are_refused() {
    let keys = [Key::new(0.5, 0.0), Key::new(0.25, 0.0)];
    assert_eq!(
        Curve::new(NOD, &keys).err(),
        Some(FigureError::KeysNotAscending)
    );
}

#[test]
fn keys_at_the_same_phase_are_refused() {
    let keys = [Key::new(0.5, 0.0), Key::new(0.5, 1.0)];
    assert_eq!(
        Curve::new(NOD, &keys).err(),
        Some(FigureError::KeysNotAscending)
    );
}

#[test]
fn a_key_outside_the_clip_is_refused() {
    assert_eq!(
        Curve::new(NOD, &[Key::new(1.5, 0.0)]).err(),
        Some(FigureError::PhaseOutsideClip)
    );
    assert_eq!(
        Curve::new(NOD, &[Key::new(-0.5, 0.0)]).err(),
        Some(FigureError::PhaseOutsideClip)
    );
    assert_eq!(
        Curve::new(NOD, &[Key::new(f64::NAN, 0.0)]).err(),
        Some(FigureError::PhaseOutsideClip)
    );
}

#[test]
fn a_key_outside_its_parameters_range_is_refused() {
    assert_eq!(
        Curve::new(NOD, &[Key::new(0.0, 2.0)]).err(),
        Some(FigureError::ParamOutsideRange)
    );
    assert_eq!(
        Curve::new(KNEE, &[Key::new(0.0, -0.5)]).err(),
        Some(FigureError::ParamOutsideRange)
    );
}

#[test]
fn a_clip_that_would_fold_a_knee_backwards_is_refused_where_it_is_authored() {
    assert_eq!(
        Curve::new(KNEE, &[Key::new(0.0, 0.0), Key::new(1.0, -1.0)]).err(),
        Some(FigureError::ParamOutsideRange)
    );
}

#[test]
fn a_single_key_holds_its_value_everywhere() {
    let keys = [Key::new(0.4, 0.5)];
    let curve = curve(NOD, &keys);
    for step in 0..=10 {
        let phase = f64::from(step) / 10.0;
        assert!(close(curve.sample(phase, Loop::Hold), 0.5));
    }
}

#[test]
fn a_curve_reads_its_keys_back_exactly() {
    let keys = [Key::new(0.0, -1.0), Key::new(0.5, 0.25), Key::new(1.0, 1.0)];
    let curve = curve(NOD, &keys);
    for key in keys {
        assert!(
            close(curve.sample(key.phase, Loop::Hold), key.value),
            "key at {} did not read back",
            key.phase
        );
    }
}

#[test]
fn a_linear_segment_is_halfway_at_its_middle() {
    let keys = [Key::new(0.0, 0.0), Key::new(1.0, 1.0)];
    let curve = curve(KNEE, &keys);
    assert!(close(curve.sample(0.5, Loop::Hold), 0.5));
}

#[test]
fn a_held_segment_steps_at_the_next_key() {
    let keys = [Key::new(0.0, 0.0).eased(Easing::Hold), Key::new(0.5, 1.0)];
    let curve = curve(KNEE, &keys);
    assert!(close(curve.sample(0.49, Loop::Hold), 0.0));
    assert!(close(curve.sample(0.5, Loop::Hold), 1.0));
}

#[test]
fn a_held_clip_clamps_at_both_ends() {
    let keys = [Key::new(0.25, 0.2), Key::new(0.75, 0.8)];
    let curve = curve(KNEE, &keys);
    assert!(close(curve.sample(0.0, Loop::Hold), 0.2));
    assert!(close(curve.sample(1.0, Loop::Hold), 0.8));
}

#[test]
fn a_wrapping_clip_joins_its_end_to_its_start() {
    let keys = [Key::new(0.25, 0.0), Key::new(0.75, 1.0)];
    let curve = curve(KNEE, &keys);
    // Halfway round the wrap segment, which spans 0.75 -> 1.25.
    assert!(close(curve.sample(1.0, Loop::Wrap), 0.5));
    // The same instant approached from below zero's side of the join.
    assert!(close(curve.sample(0.0, Loop::Wrap), 0.5));
}

#[test]
fn a_sampled_value_never_leaves_its_parameters_range() {
    let keys = [
        Key::new(0.0, -1.0).eased(Easing::EaseInOut),
        Key::new(0.3, 1.0).eased(Easing::EaseIn),
        Key::new(0.9, -0.5).eased(Easing::EaseOut),
    ];
    let curve = curve(NOD, &keys);
    for repeat in [Loop::Hold, Loop::Wrap, Loop::PingPong] {
        for step in 0..=1000 {
            let phase = f64::from(step) / 1000.0;
            let value = curve.sample(phase, repeat);
            assert!(
                NOD.range().holds(value),
                "{repeat:?} at {phase} produced {value}"
            );
        }
    }
}

#[test]
fn an_interpolated_value_stays_between_its_two_keys() {
    let keys = [
        Key::new(0.2, -0.4).eased(Easing::EaseInOut),
        Key::new(0.8, 0.6),
    ];
    let curve = curve(NOD, &keys);
    for step in 0..=100 {
        let phase = 0.2 + f64::from(step) / 100.0 * 0.6;
        let value = curve.sample(phase, Loop::Hold);
        assert!((-0.4..=0.6).contains(&value), "{phase} produced {value}");
    }
}

#[test]
fn a_wrapping_clip_counts_a_lap_per_play() {
    let clip = Clip::new(2.0, Loop::Wrap, &[], &[]).expect("a well-formed clip");
    assert_eq!(clip.laps_between(0.0, 1.0), 0);
    assert_eq!(clip.laps_between(0.0, 2.0), 1);
    assert_eq!(clip.laps_between(0.0, 5.0), 2);
    assert_eq!(clip.laps_between(3.0, 3.0), 0);
}

#[test]
fn a_held_clip_never_laps_however_long_it_runs() {
    let clip = Clip::new(2.0, Loop::Hold, &[], &[]).expect("a well-formed clip");
    assert_eq!(clip.laps_between(0.0, 2.0), 0);
    assert_eq!(clip.laps_between(0.0, 200.0), 0);
}

#[test]
fn a_ping_pong_laps_once_per_there_and_back() {
    let clip = Clip::new(2.0, Loop::PingPong, &[], &[]).expect("a well-formed clip");
    // One duration is halfway through the cycle, not a whole one.
    assert_eq!(clip.laps_between(0.0, 2.0), 0);
    assert_eq!(clip.laps_between(0.0, 4.0), 1);
    assert_eq!(clip.laps_between(0.0, 8.0), 2);
}

#[test]
fn a_lap_count_never_runs_backwards_or_off_the_end() {
    let clip = Clip::new(1.0, Loop::Wrap, &[], &[]).expect("a well-formed clip");
    assert_eq!(clip.laps_between(5.0, 1.0), 0);
    assert_eq!(clip.laps_between(0.0, f64::NAN), 0);
    assert_eq!(clip.laps_between(f64::NAN, 1.0), 0);
    assert_eq!(clip.laps_between(0.0, f64::MAX), u32::MAX);
}

fn clip_of<'a>(curves: &'a [Curve<'a>], events: &'a [Event]) -> Clip<'a> {
    Clip::new(2.0, Loop::Wrap, curves, events).expect("a well-formed clip")
}

#[test]
fn a_clip_of_no_duration_is_refused() {
    for seconds in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            Clip::new(seconds, Loop::Hold, &[], &[]).err(),
            Some(FigureError::DurationUnreal),
            "a duration of {seconds} was accepted"
        );
    }
}

#[test]
fn two_curves_on_one_parameter_are_refused() {
    let keys = [Key::new(0.0, 0.0)];
    let curves = [curve(NOD, &keys), curve(NOD, &keys)];
    assert_eq!(
        Clip::new(1.0, Loop::Hold, &curves, &[]).err(),
        Some(FigureError::DuplicateCurve)
    );
}

#[test]
fn an_event_outside_the_clip_is_refused() {
    for phase in [-0.1, 1.1, f64::NAN] {
        let events = [Event::new("footstep", phase)];
        assert_eq!(
            Clip::new(1.0, Loop::Hold, &[], &events).err(),
            Some(FigureError::PhaseOutsideClip),
            "an event at {phase} was accepted"
        );
    }
}

#[test]
fn events_out_of_order_are_refused() {
    let events = [Event::new("late", 0.8), Event::new("early", 0.2)];
    assert_eq!(
        Clip::new(1.0, Loop::Hold, &[], &events).err(),
        Some(FigureError::EventsNotAscending)
    );
}

#[test]
fn one_event_name_used_twice_is_refused() {
    let events = [Event::new("footstep", 0.2), Event::new("footstep", 0.7)];
    assert_eq!(
        Clip::new(1.0, Loop::Hold, &[], &events).err(),
        Some(FigureError::DuplicateEventName)
    );
}

#[test]
fn a_clips_mask_is_exactly_the_parameters_it_keys() {
    let nods = [Key::new(0.0, 0.0)];
    let knees = [Key::new(0.0, 0.0)];
    let curves = [curve(NOD, &nods), curve(KNEE, &knees)];
    let clip = clip_of(&curves, &[]);
    assert_eq!(clip.mask().len(), 2);
    assert!(clip.mask().holds(NOD));
    assert!(clip.mask().holds(KNEE));
    assert!(!clip.mask().holds(Param::SpineBend));
}

#[test]
fn a_held_clip_runs_once_and_stops() {
    let clip = Clip::new(2.0, Loop::Hold, &[], &[]).expect("a well-formed clip");
    assert!(close(clip.phase_at(0.0).expect("finite"), 0.0));
    assert!(close(clip.phase_at(1.0).expect("finite"), 0.5));
    assert!(close(clip.phase_at(2.0).expect("finite"), 1.0));
    assert!(close(clip.phase_at(9.0).expect("finite"), 1.0));
    assert!(close(clip.phase_at(-1.0).expect("finite"), 0.0));
}

#[test]
fn a_wrapping_clip_cycles() {
    let clip = Clip::new(2.0, Loop::Wrap, &[], &[]).expect("a well-formed clip");
    assert!(close(clip.phase_at(0.0).expect("finite"), 0.0));
    assert!(close(clip.phase_at(0.5).expect("finite"), 0.25));
    assert!(close(clip.phase_at(2.0).expect("finite"), 0.0));
    assert!(close(clip.phase_at(2.5).expect("finite"), 0.25));
    assert!(close(clip.phase_at(-0.5).expect("finite"), 0.75));
}

#[test]
fn a_ping_pong_clip_comes_back() {
    let clip = Clip::new(2.0, Loop::PingPong, &[], &[]).expect("a well-formed clip");
    assert!(close(clip.phase_at(0.0).expect("finite"), 0.0));
    assert!(close(clip.phase_at(2.0).expect("finite"), 1.0));
    assert!(close(clip.phase_at(3.0).expect("finite"), 0.5));
    assert!(close(clip.phase_at(4.0).expect("finite"), 0.0));
}

#[test]
fn a_phase_is_never_outside_the_clip_however_long_it_has_run() {
    for repeat in [Loop::Hold, Loop::Wrap, Loop::PingPong] {
        let clip = Clip::new(0.7, repeat, &[], &[]).expect("a well-formed clip");
        for step in -50..500 {
            let elapsed = f64::from(step) / 7.0;
            let phase = clip.phase_at(elapsed).expect("finite");
            assert!(
                (0.0..=1.0).contains(&phase),
                "{repeat:?} at {elapsed} gave {phase}"
            );
        }
    }
}

#[test]
fn an_elapsed_time_that_is_not_a_number_is_refused() {
    let clip = Clip::new(1.0, Loop::Wrap, &[], &[]).expect("a well-formed clip");
    assert_eq!(
        clip.phase_at(f64::NAN).err(),
        Some(FigureError::ElapsedUnreal)
    );
    assert_eq!(
        clip.phase_at(f64::INFINITY).err(),
        Some(FigureError::ElapsedUnreal)
    );
}

#[test]
fn sampling_leaves_the_parameters_a_clip_does_not_key_alone() {
    let knees = [Key::new(0.0, 1.0)];
    let curves = [curve(KNEE, &knees)];
    let clip = clip_of(&curves, &[]);

    let mut pose = Pose::REST.with(NOD, 0.5).expect("in range");
    clip.sample_into(0.0, &mut pose).expect("a phase inside");
    assert!(close(pose.get(KNEE), 1.0));
    assert!(
        close(pose.get(NOD), 0.5),
        "an unkeyed parameter was written"
    );
}

#[test]
fn sampling_a_whole_clip_rests_everything_it_does_not_key() {
    let knees = [Key::new(0.0, 1.0)];
    let curves = [curve(KNEE, &knees)];
    let clip = clip_of(&curves, &[]);

    let pose = clip.sample(0.0).expect("a phase inside");
    assert!(close(pose.get(KNEE), 1.0));
    assert!(close(pose.get(NOD), 0.0));
}

#[test]
fn sampling_outside_the_clip_is_refused() {
    let clip = clip_of(&[], &[]);
    let mut pose = Pose::REST;
    for phase in [-0.1, 1.1, f64::NAN] {
        assert_eq!(
            clip.sample_into(phase, &mut pose).err(),
            Some(FigureError::PhaseOutsideClip),
            "a phase of {phase} was accepted"
        );
    }
}

/// The events `from..to` fires, in order, must be exactly `expected`.
fn assert_fires(clip: Clip<'_>, from: f64, to: f64, expected: &[&str]) {
    let mut seen = 0;
    for event in clip.events_between(from, to).expect("phases inside") {
        assert!(
            seen < expected.len(),
            "{from}..{to} fired an unexpected {}",
            event.name
        );
        assert_eq!(
            event.name, expected[seen],
            "{from}..{to} fired out of order"
        );
        seen += 1;
    }
    assert_eq!(seen, expected.len(), "{from}..{to} missed an event");
}

fn count_fires(clip: Clip<'_>, from: f64, to: f64) -> usize {
    clip.events_between(from, to)
        .expect("phases inside")
        .count()
}

#[test]
fn an_event_fires_when_the_phase_passes_it() {
    let events = [Event::new("left", 0.25), Event::new("right", 0.75)];
    let clip = clip_of(&[], &events);
    assert_fires(clip, 0.0, 0.5, &["left"]);
    assert_fires(clip, 0.5, 1.0, &["right"]);
}

#[test]
fn an_event_exactly_at_the_end_of_a_step_fires_and_at_the_start_does_not() {
    let events = [Event::new("left", 0.25)];
    let clip = clip_of(&[], &events);
    assert_fires(clip, 0.0, 0.25, &["left"]);
    assert_fires(clip, 0.25, 0.5, &[]);
}

#[test]
fn an_event_fires_exactly_once_as_the_phase_walks_past_it() {
    let events = [Event::new("left", 0.25), Event::new("right", 0.75)];
    let clip = clip_of(&[], &events);

    let mut fired = 0;
    let mut previous = 0.0;
    for step in 1..=100 {
        let phase = f64::from(step) / 100.0;
        fired += count_fires(clip, previous, phase);
        previous = phase;
    }
    assert_eq!(fired, 2);
}

#[test]
fn every_event_fires_exactly_once_over_a_lap() {
    let events = [Event::new("left", 0.25), Event::new("right", 0.75)];
    let clip = clip_of(&[], &events);
    assert_eq!(count_fires(clip, 0.4, 0.4), 0);
    assert_eq!(count_fires(clip, 0.9, 0.9 - SLACK), 2);
}

#[test]
fn a_lap_reports_the_tail_before_the_head() {
    let events = [Event::new("left", 0.25), Event::new("right", 0.75)];
    let clip = clip_of(&[], &events);
    assert_fires(clip, 0.5, 0.3, &["right", "left"]);
}

#[test]
fn a_step_that_crosses_nothing_reports_nothing() {
    let events = [Event::new("left", 0.25)];
    let clip = clip_of(&[], &events);
    assert_fires(clip, 0.3, 0.7, &[]);
}

#[test]
fn asking_for_events_outside_the_clip_is_refused() {
    let clip = clip_of(&[], &[]);
    assert!(clip.events_between(-0.1, 0.5).is_err());
    assert!(clip.events_between(0.5, 1.1).is_err());
    assert!(clip.events_between(f64::NAN, 0.5).is_err());
}

/// A move's curve must begin at none of it and end at all of it, or playing
/// the clip out would deliver something other than what was authorised.
#[test]
fn a_root_motion_curve_that_does_not_span_the_move_is_refused() {
    for keys in [
        [Key::new(0.0, 0.2), Key::new(1.0, 1.0)],
        [Key::new(0.0, 0.0), Key::new(1.0, 0.8)],
        [Key::new(0.1, 0.0), Key::new(1.0, 1.0)],
        [Key::new(0.0, 0.0), Key::new(0.9, 1.0)],
    ] {
        assert_eq!(
            Travel::new(&keys).map(|_| ()),
            Err(FigureError::TravelNotSpanning),
            "{keys:?} must be refused"
        );
    }
    assert_eq!(Travel::new(&[]).map(|_| ()), Err(FigureError::CurveEmpty));
}

/// The fraction is a fraction: a clip cannot ask for more of the move than
/// the simulation authorised, nor for a negative amount of it.
#[test]
fn a_root_motion_value_outside_the_move_is_refused() {
    for value in [-0.01, 1.01, f64::NAN] {
        let keys = [Key::new(0.0, 0.0), Key::new(0.5, value), Key::new(1.0, 1.0)];
        assert_eq!(
            Travel::new(&keys).map(|_| ()),
            Err(FigureError::TravelOutsideRange),
            "a value of {value} must be refused"
        );
    }
}

#[test]
fn a_root_motion_curve_out_of_order_is_refused() {
    let keys = [
        Key::new(0.0, 0.0),
        Key::new(0.7, 0.5),
        Key::new(0.7, 0.6),
        Key::new(1.0, 1.0),
    ];
    assert_eq!(
        Travel::new(&keys).map(|_| ()),
        Err(FigureError::KeysNotAscending)
    );
    let outside = [Key::new(0.0, 0.0), Key::new(1.5, 0.5), Key::new(1.0, 1.0)];
    assert_eq!(
        Travel::new(&outside).map(|_| ()),
        Err(FigureError::PhaseOutsideClip)
    );
}

/// A clip owns the *pacing* of a move and never its distance, so the figure
/// ends up exactly where the simulation authorised however the curve got
/// there.
#[test]
fn a_move_delivers_exactly_what_was_authorised() {
    // An anticipation: the figure draws back before it springs forward.
    let keys = [
        Key::new(0.0, 0.0),
        Key::new(0.2, 0.0),
        Key::new(0.45, 0.08).eased(Easing::EaseOut),
        Key::new(0.8, 0.92),
        Key::new(1.0, 1.0),
    ];
    let travel = Travel::new(&keys).expect("a real move");
    assert!(close(travel.at(0.0), 0.0));
    assert!(close(travel.at(1.0), 1.0));
    assert!(close(
        travel.spent(1.0, 30.0).expect("a real distance"),
        30.0
    ));
    assert!(close(
        travel.spent(0.0, 30.0).expect("a real distance"),
        0.0
    ));

    let mut phase = 0.0;
    while phase <= 1.0 {
        let spent = travel.spent(phase, 30.0).expect("a real distance");
        assert!(
            (0.0..=30.0).contains(&spent),
            "at {phase} the move had spent {spent} of 30"
        );
        phase += 1.0 / 256.0;
    }
    assert_eq!(
        travel.spent(0.5, f64::NAN).map(|_| ()),
        Err(FigureError::GeometryUnreal)
    );
}

/// A move is spent once: past the end there is no more of it, and before the
/// start none has been spent, whatever the clip's loop mode says.
#[test]
fn a_move_does_not_run_backward_across_the_join() {
    let keys = [Key::new(0.0, 0.0), Key::new(1.0, 1.0)];
    let travel = Travel::new(&keys).expect("a real move");
    assert!(close(travel.at(-0.5), 0.0));
    assert!(close(travel.at(1.5), 1.0));
}

/// Most clips move nothing; only one whose animation paces the move carries
/// a curve, and the clip reads it back unchanged.
#[test]
fn a_clip_carries_its_move_or_none_at_all() {
    let keys = [Key::new(0.0, 0.0), Key::new(1.0, 1.0)];
    let travel = Travel::new(&keys).expect("a real move");
    let plain = Clip::new(1.0, Loop::Hold, &[], &[]).expect("a real clip");
    assert!(plain.travel().is_none());
    let moving = plain.travelling(travel);
    assert_eq!(moving.travel(), Some(travel));
    assert!(close(moving.travel().expect("a move").at(0.5), 0.5));
}

/// The root height has to cover the cycle: a curve stopping short would
/// leave the height at some phase carried from an end key rather than
/// authored, which is the guesswork the curve exists to remove.
#[test]
fn a_root_height_curve_that_does_not_span_the_cycle_is_refused() {
    for keys in [
        [Key::new(0.1, -0.1), Key::new(1.0, -0.1)],
        [Key::new(0.0, -0.1), Key::new(0.9, -0.1)],
    ] {
        assert_eq!(
            Lift::new(&keys).map(|_| ()),
            Err(FigureError::LiftNotSpanning),
            "{keys:?} must be refused"
        );
    }
    assert_eq!(Lift::new(&[]).map(|_| ()), Err(FigureError::CurveEmpty));
}

/// A displacement past a whole leg either way is a move the simulation
/// authorised, not a cycle's own rise and fall.
#[test]
fn a_root_height_beyond_a_leg_either_way_is_refused() {
    for value in [-1.01, 1.01, f64::NAN, f64::INFINITY] {
        let keys = [Key::new(0.0, 0.0), Key::new(0.5, value), Key::new(1.0, 0.0)];
        assert_eq!(
            Lift::new(&keys).map(|_| ()),
            Err(FigureError::LiftOutsideRange),
            "a value of {value} must be refused"
        );
    }
}

#[test]
fn a_root_height_curve_out_of_order_is_refused() {
    let keys = [
        Key::new(0.0, -0.1),
        Key::new(0.7, 0.0),
        Key::new(0.7, 0.1),
        Key::new(1.0, -0.1),
    ];
    assert_eq!(
        Lift::new(&keys).map(|_| ()),
        Err(FigureError::KeysNotAscending)
    );
}

/// A cycling clip whose height ends somewhere other than it began hitches
/// vertically on every lap. The loop mode is the clip's, so the clip is
/// where that refusal belongs.
#[test]
fn a_looping_clip_whose_root_height_does_not_close_is_refused() {
    const OPEN: [Key; 2] = [Key::new(0.0, -0.1), Key::new(1.0, 0.2)];
    let lift = Lift::new(&OPEN).expect("the curve itself is real");
    assert_eq!(
        clip_of(&[], &[]).lifting(lift).map(|_| ()),
        Err(FigureError::LiftNotClosing)
    );
    // A clip that plays once and holds has no join to hitch at, so the same
    // curve is admitted there.
    let once = Clip::new(1.0, Loop::Hold, &[], &[]).expect("a real clip");
    assert!(once.lifting(lift).is_ok());
}

/// A clip that authors no height stands on straight legs, which is what
/// makes the curve optional rather than a thing every clip must carry.
#[test]
fn a_clip_with_no_root_height_reads_as_standing_straight() {
    let clip = clip_of(&[], &[]);
    for phase in [0.0, 0.25, 0.5, 0.75, 1.0] {
        assert!(mathf::fabs(clip.root_at(phase)) < 1e-12);
    }
}

/// The height wraps with the clip, so a cycling figure's body is at the same
/// place either side of the join.
#[test]
fn a_cycling_root_height_wraps_with_its_clip() {
    const ARC: [Key; 3] = [Key::new(0.0, -0.2), Key::new(0.5, 0.1), Key::new(1.0, -0.2)];
    let lift = Lift::new(&ARC).expect("a real lift");
    let clip = clip_of(&[], &[]).lifting(lift).expect("it closes");
    assert!(mathf::fabs(clip.root_at(0.0) - clip.root_at(1.0)) < 1e-12);
    assert!(mathf::fabs(clip.root_at(0.5) - 0.1) < 1e-12);
    // Mid-segment, between its two keys and nowhere outside them.
    let quarter = clip.root_at(0.25);
    assert!(quarter > -0.2 && quarter < 0.1, "{quarter} left its keys");
}
