//! What a transition machine refuses, and how one is walked.

use tairix_util::mathf;

use super::{Animator, ClipId, Edge, StateId, Transitions, MAX_STATES};
use crate::clip::{Clip, Curve, Event, Key, Lift, Loop};
use crate::error::FigureError;
use crate::pose::Param;
use crate::socket::Side;

const SLACK: f64 = 1e-9;

const IDLE: StateId = StateId::new(0);
const WALK: StateId = StateId::new(1);
const CAST: StateId = StateId::new(2);

const KNEE: Param = Param::KneeBend(Side::Left);

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

/// Three interchangeable clips, so a test that only cares about the graph
/// does not have to author curves.
fn plain_clips() -> [Clip<'static>; 3] {
    let clip = Clip::new(1.0, Loop::Wrap, &[], &[]).expect("a well-formed clip");
    [clip, clip, clip]
}

#[test]
fn a_machine_with_no_states_is_refused() {
    let clips = plain_clips();
    assert_eq!(
        Transitions::new(&clips, &[], &[], IDLE).err(),
        Some(FigureError::NoStates)
    );
}

#[test]
fn an_initial_state_the_machine_lacks_is_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(0)];
    assert_eq!(
        Transitions::new(&clips, &states, &[], WALK).err(),
        Some(FigureError::NoSuchState)
    );
}

#[test]
fn a_state_naming_a_clip_that_is_not_there_is_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(9)];
    assert_eq!(
        Transitions::new(&clips, &states, &[], IDLE).err(),
        Some(FigureError::NoSuchClip)
    );
}

#[test]
fn an_edge_to_a_state_that_is_not_there_is_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(0)];
    let edges = [Edge::new(IDLE, WALK, 0.2)];
    assert_eq!(
        Transitions::new(&clips, &states, &edges, IDLE).err(),
        Some(FigureError::NoSuchState)
    );
}

#[test]
fn a_cross_fade_that_is_not_positive_is_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(0), ClipId::new(1)];
    for seconds in [0.0, -0.5, f64::NAN, f64::INFINITY] {
        let edges = [Edge::new(IDLE, WALK, seconds)];
        assert_eq!(
            Transitions::new(&clips, &states, &edges, IDLE).err(),
            Some(FigureError::BlendNotPositive),
            "a cross-fade of {seconds} was accepted"
        );
    }
}

#[test]
fn edges_out_of_order_are_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(0), ClipId::new(1)];
    let edges = [Edge::new(WALK, IDLE, 0.2), Edge::new(IDLE, WALK, 0.2)];
    assert_eq!(
        Transitions::new(&clips, &states, &edges, IDLE).err(),
        Some(FigureError::EdgesNotAscending)
    );
}

#[test]
fn a_repeated_edge_is_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(0), ClipId::new(1)];
    let edges = [Edge::new(IDLE, WALK, 0.2), Edge::new(IDLE, WALK, 0.4)];
    assert_eq!(
        Transitions::new(&clips, &states, &edges, IDLE).err(),
        Some(FigureError::EdgesNotAscending)
    );
}

#[test]
fn a_state_nothing_leads_to_is_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(0), ClipId::new(1), ClipId::new(2)];
    let edges = [Edge::new(IDLE, WALK, 0.2)];
    assert_eq!(
        Transitions::new(&clips, &states, &edges, IDLE).err(),
        Some(FigureError::StateUnreachable)
    );
}

#[test]
fn a_state_reached_only_through_another_is_reachable() {
    let clips = plain_clips();
    let states = [ClipId::new(0), ClipId::new(1), ClipId::new(2)];
    let edges = [Edge::new(IDLE, WALK, 0.2), Edge::new(WALK, CAST, 0.2)];
    assert!(Transitions::new(&clips, &states, &edges, IDLE).is_ok());
}

#[test]
fn a_lone_state_needs_no_edges() {
    let clips = plain_clips();
    let states = [ClipId::new(0)];
    assert!(Transitions::new(&clips, &states, &[], IDLE).is_ok());
}

#[test]
fn more_states_than_a_machine_holds_are_refused() {
    let clips = plain_clips();
    let states = [ClipId::new(0); MAX_STATES + 1];
    assert_eq!(
        Transitions::new(&clips, &states, &[], IDLE).err(),
        Some(FigureError::TooManyStates)
    );
}

/// The three-state graph the walking tests below use.
fn states() -> [ClipId; 3] {
    [ClipId::new(0), ClipId::new(1), ClipId::new(2)]
}

fn edges() -> [Edge; 4] {
    [
        Edge::new(IDLE, WALK, 0.4),
        Edge::new(IDLE, CAST, 0.2),
        Edge::new(WALK, IDLE, 0.4),
        Edge::new(CAST, IDLE, 0.2),
    ]
}

#[test]
fn a_machine_reports_what_it_was_built_from() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    assert_eq!(machine.initial(), IDLE);
    assert_eq!(machine.states(), 3);
    assert!(machine.clip(IDLE).is_some());
    assert!(machine.clip(StateId::new(9)).is_none());
}

#[test]
fn an_edge_is_found_only_where_one_was_declared() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");

    assert!(close(
        machine.edge(IDLE, CAST).expect("declared").seconds,
        0.2
    ));
    assert!(close(
        machine.edge(IDLE, WALK).expect("declared").seconds,
        0.4
    ));
    assert!(machine.edge(WALK, CAST).is_none());
}

#[test]
fn a_states_edges_are_the_contiguous_run_that_leaves_it() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");

    let leaving = machine.edges_from(IDLE);
    assert_eq!(leaving.len(), 2);
    assert!(leaving.iter().all(|edge| edge.from == IDLE));
    assert_eq!(machine.edges_from(WALK).len(), 1);
    assert_eq!(machine.edges_from(StateId::new(9)).len(), 0);
}

#[test]
fn an_animator_starts_at_the_initial_state() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let animator = Animator::new(machine);
    assert_eq!(animator.state(), IDLE);
    assert!(!animator.fading());
}

#[test]
fn asking_for_the_state_already_playing_changes_nothing() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);
    animator.request(IDLE).expect("already there");
    assert_eq!(animator.state(), IDLE);
    assert!(!animator.fading());
}

#[test]
fn asking_for_a_state_with_no_edge_to_it_is_refused() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);
    animator.request(WALK).expect("declared");
    assert_eq!(animator.request(CAST).err(), Some(FigureError::NoSuchEdge));
    assert_eq!(animator.state(), WALK);
}

#[test]
fn a_request_starts_a_fade_that_runs_out() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    animator.request(WALK).expect("declared");
    assert_eq!(animator.state(), WALK);
    assert!(animator.fading());

    let moved = animator.advance(0.2).expect("a real step");
    assert!(animator.fading(), "the fade ended early");
    assert_eq!(moved.current.state, WALK);
    assert_eq!(moved.outgoing.expect("still fading").state, IDLE);

    let moved = animator.advance(0.2).expect("a real step");
    assert!(!animator.fading(), "the fade did not end");
    assert!(
        moved.outgoing.is_some(),
        "the last step of a fade went unreported"
    );

    let moved = animator.advance(0.2).expect("a real step");
    assert!(moved.outgoing.is_none());
}

#[test]
fn a_step_that_is_not_a_real_duration_is_refused() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);
    for seconds in [-0.1, f64::NAN, f64::INFINITY] {
        assert_eq!(
            animator.advance(seconds).err(),
            Some(FigureError::ElapsedUnreal),
            "a step of {seconds} was accepted"
        );
    }
}

#[test]
fn an_advance_reports_the_phase_it_crossed() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    let moved = animator.advance(0.25).expect("a real step");
    assert!(close(moved.current.from, 0.0));
    assert!(close(moved.current.to, 0.25));
    assert_eq!(moved.current.laps, 0);
}

#[test]
fn a_step_longer_than_the_clip_says_how_many_cycles_went_by() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    let moved = animator.advance(2.5).expect("a real step");
    assert_eq!(moved.current.laps, 2);
}

#[test]
fn a_cross_fade_hands_over_from_one_clip_to_the_other() {
    let low = [Key::new(0.0, 0.0)];
    let high = [Key::new(0.0, 1.0)];
    let low_curves = [Curve::new(KNEE, &low).expect("a well-formed curve")];
    let high_curves = [Curve::new(KNEE, &high).expect("a well-formed curve")];
    let clips = [
        Clip::new(1.0, Loop::Wrap, &low_curves, &[]).expect("a well-formed clip"),
        Clip::new(1.0, Loop::Wrap, &high_curves, &[]).expect("a well-formed clip"),
    ];
    let states = [ClipId::new(0), ClipId::new(1)];
    let edges = [Edge::new(IDLE, WALK, 1.0)];
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    let knee = |animator: &Animator<'_>| {
        animator
            .blend()
            .expect("live states have clips")
            .resolve()
            .expect("in range")
            .get(KNEE)
    };

    assert!(close(knee(&animator), 0.0), "the fade started early");

    animator.request(WALK).expect("declared");
    assert!(
        close(knee(&animator), 0.0),
        "the new clip took over at once"
    );

    animator.advance(0.5).expect("a real step");
    assert!(close(knee(&animator), 0.5), "the fade was not halfway");

    animator.advance(0.5).expect("a real step");
    assert!(close(knee(&animator), 1.0), "the fade did not finish");
}

/// Two one-second clips holding the body at the heights `first` and
/// `second` key.
fn heights<'a>(first: &'a [Key], second: &'a [Key]) -> [Clip<'a>; 2] {
    let held = |keys: &'a [Key]| {
        Clip::new(1.0, Loop::Wrap, &[], &[])
            .and_then(|clip| clip.lifting(Lift::new(keys)?))
            .expect("a well-formed clip")
    };
    [held(first), held(second)]
}

/// A clip playing on its own holds the root where it says, at the phase it
/// has reached.
#[test]
fn a_settled_animator_stands_at_its_own_clips_height() {
    let rising = [Key::new(0.0, -0.2), Key::new(0.5, 0.1), Key::new(1.0, -0.2)];
    let level = [Key::new(0.0, 0.0), Key::new(1.0, 0.0)];
    let clips = heights(&rising, &level);
    let states = [ClipId::new(0), ClipId::new(1)];
    let edges = [Edge::new(IDLE, WALK, 1.0)];
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    for _ in 0..8 {
        let phase = animator.advance(0.125).expect("a real step").current.to;
        assert!(close(
            animator.root().expect("live states have clips"),
            clips[0].root_at(phase)
        ));
    }
}

/// The regression the root height was missing: a fade between clips standing
/// at different heights hands the body over from one to the other, starting
/// exactly where the outgoing clip held it and ending exactly where the
/// incoming one does, and never jumping on the way.
#[test]
fn a_cross_fade_carries_the_body_from_one_height_to_the_other() {
    let low = [Key::new(0.0, -0.3), Key::new(1.0, -0.3)];
    let high = [Key::new(0.0, 0.2), Key::new(1.0, 0.2)];
    let clips = heights(&low, &high);
    let states = [ClipId::new(0), ClipId::new(1)];
    let edges = [Edge::new(IDLE, WALK, 1.0)];
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);
    let root = |animator: &Animator<'_>| animator.root().expect("live states have clips");

    assert!(close(root(&animator), -0.3));
    animator.request(WALK).expect("declared");
    assert!(
        close(root(&animator), -0.3),
        "the body jumped the moment the fade began"
    );

    // An eighth of the fade per step moves the body an eighth of the way.
    let mut was = root(&animator);
    for _ in 0..8 {
        animator.advance(0.125).expect("a real step");
        let now = root(&animator);
        assert!(now >= was, "the body sank on its way up");
        assert!(now - was <= 0.0625 + SLACK, "the body jumped {}", now - was);
        was = now;
    }
    assert!(!animator.fading());
    assert!(close(root(&animator), 0.2), "the fade did not land");
}

#[test]
fn a_settled_animator_plays_only_its_own_clip() {
    let high = [Key::new(0.0, 1.0)];
    let high_curves = [Curve::new(KNEE, &high).expect("a well-formed curve")];
    let clips = [Clip::new(1.0, Loop::Wrap, &high_curves, &[]).expect("a well-formed clip")];
    let states = [ClipId::new(0)];
    let machine = Transitions::new(&clips, &states, &[], IDLE).expect("a sound machine");
    let animator = Animator::new(machine);

    let blend = animator.blend().expect("live states have clips");
    assert_eq!(blend.written().len(), 1);
    assert!(close(blend.resolve().expect("in range").get(KNEE), 1.0));
}

#[test]
fn a_second_request_mid_fade_leaves_only_two_clips_live() {
    let clips = plain_clips();
    let states = states();
    let edges = edges();
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    animator.request(WALK).expect("declared");
    animator.advance(0.1).expect("a real step");
    animator.request(IDLE).expect("declared");

    assert_eq!(animator.state(), IDLE);
    let moved = animator.advance(0.1).expect("a real step");
    assert_eq!(moved.current.state, IDLE);
    assert_eq!(moved.outgoing.expect("still fading").state, WALK);
}

/// How many times each clip's own event fired over `steps` of `seconds`.
///
/// Pulls from both live clips, which is what a consumer does: an advance
/// hands back the phase interval each moved through, and the events are read
/// off the clip that moved.
fn walk(animator: &mut Animator<'_>, steps: u32, seconds: f64) -> (usize, usize) {
    let (mut idle, mut walking) = (0, 0);
    for _ in 0..steps {
        let moved = animator.advance(seconds).expect("a real step");
        for advance in core::iter::once(moved.current).chain(moved.outgoing) {
            let clip = animator
                .machine()
                .clip(advance.state)
                .expect("a live state has a clip");
            let fired = clip
                .events_between(advance.from, advance.to)
                .expect("phases inside")
                .count();
            if advance.state == IDLE {
                idle += fired;
            } else {
                walking += fired;
            }
        }
    }
    (idle, walking)
}

#[test]
fn events_survive_a_cross_fade_without_duplicating_or_being_dropped() {
    let idle_events = [Event::new("idle_mid", 0.5)];
    let walk_events = [Event::new("footstep", 0.5)];
    let clips = [
        Clip::new(1.0, Loop::Wrap, &[], &idle_events).expect("a well-formed clip"),
        Clip::new(1.0, Loop::Wrap, &[], &walk_events).expect("a well-formed clip"),
    ];
    let states = [ClipId::new(0), ClipId::new(1)];
    // A cross-fade long enough that the outgoing clip crosses its own event
    // while it is still fading out.
    let edges = [Edge::new(IDLE, WALK, 0.8)];
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    let (idle, walking) = walk(&mut animator, 10, 0.1);
    assert_eq!(idle, 1, "the idle clip's event did not fire exactly once");
    assert_eq!(walking, 0, "a clip that was not playing fired");

    animator.request(WALK).expect("declared");
    let (idle, walking) = walk(&mut animator, 10, 0.1);
    assert_eq!(
        idle, 1,
        "the outgoing clip's event was dropped or duplicated"
    );
    assert_eq!(walking, 1, "the incoming clip's event did not fire once");

    assert!(!animator.fading(), "the fade outlived its seconds");
}

#[test]
fn an_outgoing_clip_stops_firing_once_its_fade_is_over() {
    let idle_events = [Event::new("idle_mid", 0.5)];
    let clips = [
        Clip::new(1.0, Loop::Wrap, &[], &idle_events).expect("a well-formed clip"),
        Clip::new(1.0, Loop::Wrap, &[], &[]).expect("a well-formed clip"),
    ];
    let states = [ClipId::new(0), ClipId::new(1)];
    let edges = [Edge::new(IDLE, WALK, 0.2)];
    let machine = Transitions::new(&clips, &states, &edges, IDLE).expect("a sound machine");
    let mut animator = Animator::new(machine);

    animator.request(WALK).expect("declared");
    let (settled, _) = walk(&mut animator, 3, 0.1);
    assert!(!animator.fading());

    // The idle clip's event sits at 0.5, past where its 0.2s fade ended.
    let (after, _) = walk(&mut animator, 10, 0.1);
    assert_eq!(
        settled + after,
        0,
        "a clip kept firing after its fade ended"
    );
}

#[test]
fn every_identifier_reads_back_the_index_it_was_made_from() {
    assert_eq!(ClipId::new(3).index(), 3);
    assert_eq!(StateId::new(7).index(), 7);
}
