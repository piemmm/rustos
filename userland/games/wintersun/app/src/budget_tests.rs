//! The budget adds up, and the governor turns the ladder rather than
//! dropping frames.

use super::*;
use crate::quality::Rung;

/// A frame where every pass spent exactly its allocation.
fn on_budget() -> FrameTimes {
    let mut times = FrameTimes::new();
    for pass in Pass::ALL {
        times.record(pass, pass.budget_ns());
    }
    times
}

#[test]
fn the_passes_and_the_headroom_are_the_whole_frame() {
    let drawing: u64 = Pass::ALL.iter().map(|p| p.budget_ns()).sum();
    assert_eq!(drawing + headroom_ns(), FRAME_NS);
    assert!(
        headroom_ns() > 0,
        "a frame with no headroom cannot be presented"
    );
    assert_eq!(FRAME_NS, 16_666_666, "sixty frames a second");
    assert_eq!((BASELINE_WIDTH, BASELINE_HEIGHT), (1280, 720));
}

#[test]
fn a_frame_on_budget_fits_and_names_no_overrun() {
    let times = on_budget();
    assert!(times.within_budget());
    assert_eq!(times.overruns().count(), 0);
    assert_eq!(times.total(), FRAME_NS - headroom_ns());
}

#[test]
fn an_overrunning_pass_is_named_with_what_it_cost() {
    let mut times = FrameTimes::new();
    times.record(Pass::Terrain, Pass::Terrain.budget_ns() + 1);
    times.record(Pass::Light, Pass::Light.budget_ns());
    let named: alloc::vec::Vec<_> = times.overruns().collect();
    assert_eq!(
        named,
        alloc::vec![(Pass::Terrain, Pass::Terrain.budget_ns() + 1)]
    );
}

#[test]
fn a_single_slow_frame_does_not_move_the_ladder() {
    let mut governor = Governor::new();
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 4);
    assert!(!governor.observe(&slow), "one hiccup shed quality");
    assert_eq!(governor.ladder(), Ladder::FULL);
}

#[test]
fn a_run_of_overruns_sheds_exactly_one_notch() {
    let mut governor = Governor::new();
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 4);
    let mut moves = 0;
    for _ in 0..3 {
        if governor.observe(&slow) {
            moves += 1;
        }
    }
    assert_eq!(moves, 1, "three overrunning frames shed {moves} notches");
    assert_eq!(governor.ladder().step(), 1);
    assert_eq!(governor.ladder().rung(), Rung::ParticleDensity);
}

#[test]
fn sustained_overload_walks_the_whole_ladder_in_order_and_then_stops() {
    let mut governor = Governor::new();
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 8);
    let mut seen = alloc::vec::Vec::new();
    for _ in 0..(u32::from(Ladder::MAX_STEP) + 4) * 3 {
        if governor.observe(&slow) {
            let rung = governor.ladder().rung();
            if seen.last() != Some(&rung) {
                seen.push(rung);
            }
        }
    }
    assert_eq!(
        seen,
        alloc::vec![
            Rung::ParticleDensity,
            Rung::LightResolution,
            Rung::MaterialDetail,
            Rung::ShadowSoftness,
            Rung::RenderScale,
        ]
    );
    assert_eq!(
        governor.ladder().step(),
        Ladder::MAX_STEP,
        "the ladder stops at the bottom rather than wrapping"
    );
    // Frame rate is the last thing to move, which here means: it never
    // does. A spent ladder leaves the renderer where it is, and says so.
    for _ in 0..3 {
        assert!(!governor.observe(&slow));
    }
    assert!(governor.floored());
}

#[test]
fn shedding_stops_at_the_floor_and_says_so() {
    let floor = Ladder::new(4);
    let mut governor = Governor::new();
    assert!(!governor.hold(floor), "a floor below the ladder moved it");
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 8);
    for _ in 0..u32::from(Ladder::MAX_STEP) * 3 {
        governor.observe(&slow);
        assert!(governor.ladder() <= floor, "shed past the floor");
    }
    assert_eq!(governor.ladder(), floor);
    assert!(
        governor.floored(),
        "an overrun at the floor went unreported"
    );

    // A frame that merely fits is not a machine that has caught up.
    governor.observe(&on_budget());
    assert!(governor.floored());
}

#[test]
fn a_floor_that_rises_takes_the_ladder_back_before_the_next_frame() {
    let mut governor = Governor::new();
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 8);
    for _ in 0..(u32::from(Ladder::MAX_STEP) + 1) * 3 {
        governor.observe(&slow);
    }
    assert!(governor.floored());

    let risen = Ladder::new(3);
    assert!(governor.hold(risen), "the ladder stayed past its floor");
    assert_eq!(governor.ladder(), risen);
    assert!(
        !governor.floored(),
        "a moved ladder is not the one that floored"
    );
    assert!(
        !governor.hold(risen),
        "holding the same floor moved it again"
    );
}

#[test]
fn a_floor_that_falls_lets_shedding_resume() {
    let mut governor = Governor::new();
    governor.hold(Ladder::new(2));
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 8);
    for _ in 0..12 {
        governor.observe(&slow);
    }
    assert!(governor.floored());

    assert!(
        !governor.hold(Ladder::new(6)),
        "a deeper floor moved the ladder"
    );
    assert!(!governor.floored(), "the ladder is no longer at its floor");
    for _ in 0..3 {
        governor.observe(&slow);
    }
    assert_eq!(governor.ladder(), Ladder::new(3));
}

#[test]
fn a_restored_notch_clears_the_floor() {
    let mut governor = Governor::new();
    governor.hold(Ladder::new(2));
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 8);
    for _ in 0..12 {
        governor.observe(&slow);
    }
    assert!(governor.floored());

    let mut fast = FrameTimes::new();
    fast.record(Pass::Terrain, 1_000_000);
    let mut restored = false;
    for _ in 0..256 {
        restored |= governor.observe(&fast);
    }
    assert!(restored);
    assert!(!governor.floored());
}

#[test]
fn a_comfortable_run_gives_a_notch_back() {
    let mut governor = Governor::new();
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 4);
    for _ in 0..3 {
        governor.observe(&slow);
    }
    assert_eq!(governor.ladder().step(), 1);

    let mut fast = FrameTimes::new();
    fast.record(Pass::Terrain, 1_000_000);
    let mut restored = false;
    for _ in 0..256 {
        restored |= governor.observe(&fast);
    }
    assert!(restored, "a long comfortable run never gave the notch back");
    assert_eq!(governor.ladder(), Ladder::FULL);
}

#[test]
fn a_frame_only_just_inside_the_budget_does_not_restore() {
    // The restore threshold sits well below the shed one, so a machine
    // sitting on the boundary settles instead of oscillating.
    let mut governor = Governor::new();
    let mut slow = on_budget();
    slow.record(Pass::Terrain, Pass::Terrain.budget_ns() * 4);
    for _ in 0..3 {
        governor.observe(&slow);
    }
    let shed = governor.ladder();

    let borderline = on_budget();
    for _ in 0..1_000 {
        assert!(
            !governor.observe(&borderline),
            "a frame filling its whole budget restored quality"
        );
    }
    assert_eq!(governor.ladder(), shed);
}

#[test]
fn the_governor_starts_at_full_quality() {
    assert_eq!(Governor::new().ladder(), Ladder::FULL);
    assert_eq!(Governor::default(), Governor::new());
}
