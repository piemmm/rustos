//! The ladder sheds in the stated order, one knob at a time, and every
//! step is reachable and reversible.

use super::*;

/// Every knob the ladder turns, as one comparable tuple.
fn knobs(ladder: Ladder) -> (Option<u32>, u32, u32, Shadow, (u32, u32)) {
    let scale = ladder.render_scale();
    (
        ladder.particle_shift(),
        ladder.light_shift(),
        ladder.material_quality().octaves(),
        ladder.shadow(),
        (scale.numerator(), scale.denominator()),
    )
}

#[test]
fn a_full_ladder_turns_nothing() {
    let full = Ladder::FULL;
    assert_eq!(full.step(), 0);
    assert_eq!(full.rung(), Rung::Full);
    assert_eq!(full.particle_shift(), Some(0));
    assert_eq!(full.material_quality().octaves(), MAX_OCTAVES);
    assert_eq!(full.shadow(), Shadow::Soft);
    assert!(full.render_scale().is_native());
    assert_eq!(full.restore(), None, "there is nothing above full");
    assert_eq!(Ladder::default(), full);
}

#[test]
fn shedding_walks_every_step_once_and_stops_at_the_bottom() {
    let mut ladder = Ladder::FULL;
    let mut steps = 0u8;
    while let Some(next) = ladder.shed() {
        assert_eq!(next.step(), ladder.step() + 1);
        ladder = next;
        steps += 1;
        assert!(steps <= Ladder::MAX_STEP, "the ladder did not terminate");
    }
    assert_eq!(steps, Ladder::MAX_STEP);
    assert_eq!(
        ladder.shed(),
        None,
        "the bottom rung reports it has nothing left"
    );
}

#[test]
fn each_step_turns_exactly_one_knob() {
    let mut ladder = Ladder::FULL;
    while let Some(next) = ladder.shed() {
        let (a, b) = (knobs(ladder), knobs(next));
        let differing = usize::from(a.0 != b.0)
            + usize::from(a.1 != b.1)
            + usize::from(a.2 != b.2)
            + usize::from(a.3 != b.3)
            + usize::from(a.4 != b.4);
        assert_eq!(
            differing,
            1,
            "step {} -> {} turned {differing} knobs: {a:?} then {b:?}",
            ladder.step(),
            next.step()
        );
        ladder = next;
    }
}

#[test]
fn the_rungs_give_way_in_the_stated_order() {
    let mut seen = alloc::vec::Vec::new();
    let mut ladder = Ladder::FULL;
    while let Some(next) = ladder.shed() {
        ladder = next;
        let rung = ladder.rung();
        if seen.last() != Some(&rung) {
            seen.push(rung);
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
        ],
        "particle density, light buffer, material detail, shadow softness, render scale"
    );
}

#[test]
fn a_rung_is_fully_shed_before_the_next_is_touched() {
    // The first render-scale notch may only appear once shadows are off,
    // materials are flat and particles are gone. Anything else means a
    // later rung was reached early.
    let mut ladder = Ladder::FULL;
    while ladder.render_scale().is_native() {
        match ladder.shed() {
            Some(next) => ladder = next,
            None => unreachable!("the render-scale rung is reachable"),
        }
    }
    assert_eq!(ladder.particle_shift(), None);
    assert_eq!(ladder.material_quality().octaves(), 0);
    assert_eq!(ladder.shadow(), Shadow::Off);
    assert!(ladder.light_shift() > 1);
}

#[test]
fn shedding_and_restoring_are_inverses() {
    let mut ladder = Ladder::FULL;
    while let Some(next) = ladder.shed() {
        assert_eq!(next.restore(), Some(ladder), "restore did not undo shed");
        assert_eq!(knobs(next.restore().expect("just shed")), knobs(ladder));
        ladder = next;
    }
}

#[test]
fn every_knob_is_monotone_down_the_ladder() {
    let mut ladder = Ladder::FULL;
    while let Some(next) = ladder.shed() {
        assert!(
            next.light_shift() >= ladder.light_shift(),
            "the light buffer got finer while shedding"
        );
        assert!(
            next.material_quality().octaves() <= ladder.material_quality().octaves(),
            "materials gained detail while shedding"
        );
        let coarser = |s: RenderScale| u64::from(s.numerator()) * 1000 / u64::from(s.denominator());
        assert!(
            coarser(next.render_scale()) <= coarser(ladder.render_scale()),
            "the render target grew while shedding"
        );
        ladder = next;
    }
}

#[test]
fn a_step_beyond_the_bottom_clamps_rather_than_wrapping() {
    assert_eq!(Ladder::new(u8::MAX).step(), Ladder::MAX_STEP);
    assert_eq!(Ladder::new(Ladder::MAX_STEP), Ladder::new(u8::MAX));
}

#[test]
fn render_scale_never_scales_a_length_to_nothing() {
    for step in 0..=Ladder::MAX_STEP {
        let scale = Ladder::new(step).render_scale();
        assert!(
            scale.apply(1) >= 1,
            "a one-pixel window lost its only pixel"
        );
        assert!(scale.apply(0) >= 1, "a zero length must still floor at one");
        assert!(scale.apply(1280) <= 1280);
        assert!(
            scale.apply(u32::MAX) > 0,
            "the widest window still has pixels"
        );
    }
    assert_eq!(RenderScale::ONE.apply(1280), 1280);
}

#[test]
fn shadow_hardens_before_it_disappears() {
    let mut seen = alloc::vec::Vec::new();
    for step in 0..=Ladder::MAX_STEP {
        let shadow = Ladder::new(step).shadow();
        if seen.last() != Some(&shadow) {
            seen.push(shadow);
        }
    }
    assert_eq!(seen, alloc::vec![Shadow::Soft, Shadow::Hard, Shadow::Off]);
}
