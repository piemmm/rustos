//! The ladder sheds in the stated order, one knob at a time, and every
//! step is reachable and reversible.

use super::*;

/// Every knob the ladder turns: particles, light, materials, shadows, and
/// the render scale.
type Knobs = (Option<u32>, u32, u32, (Shade, Relief), (u32, u32));

/// Every knob the ladder turns, as one comparable tuple.
///
/// A figure's contact shadow and the ground's relief penumbra are one knob:
/// the shadow-softness rung's first notch hardens every shadow edge in the
/// frame at once, and its second flattens the relief.
fn knobs(ladder: Ladder) -> Knobs {
    let scale = ladder.render_scale();
    (
        ladder.particle_shift(),
        ladder.light_shift(),
        ladder.material_quality().octaves(),
        (ladder.shadow(), ladder.relief()),
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
    assert_eq!(full.shadow(), Shade::Soft);
    assert_eq!(full.relief(), Relief::Wide);
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
    // The first render-scale notch may only appear once shadows are hard,
    // the relief is flat, materials are flat and particles are gone.
    // Anything else means a later rung was reached early.
    let mut ladder = Ladder::FULL;
    while ladder.render_scale().is_native() {
        match ladder.shed() {
            Some(next) => ladder = next,
            None => unreachable!("the render-scale rung is reachable"),
        }
    }
    assert_eq!(ladder.particle_shift(), None);
    assert_eq!(ladder.material_quality().octaves(), 0);
    assert_eq!(ladder.shadow(), Shade::Hard);
    assert_eq!(ladder.relief(), Relief::Flat);
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
fn a_contact_shadow_hardens_and_never_goes() {
    let mut seen = alloc::vec::Vec::new();
    for step in 0..=Ladder::MAX_STEP {
        let shadow = Ladder::new(step).shadow();
        if seen.last() != Some(&shadow) {
            seen.push(shadow);
        }
    }
    assert_eq!(seen, alloc::vec![Shade::Soft, Shade::Hard]);
}

#[test]
fn the_relief_narrows_then_flattens() {
    let mut seen = alloc::vec::Vec::new();
    for step in 0..=Ladder::MAX_STEP {
        let relief = Ladder::new(step).relief();
        if seen.last() != Some(&relief) {
            seen.push(relief);
        }
    }
    assert_eq!(
        seen,
        alloc::vec![Relief::Wide, Relief::Narrow, Relief::Flat]
    );
}

#[test]
fn every_render_scale_keeps_every_zoom_whole() {
    let mut zooms = alloc::vec![Zoom::NEAREST];
    while let Some(next) = zooms.last().and_then(|zoom| zoom.further()) {
        zooms.push(next);
    }
    for cap in CAPS {
        for step in 0..=Ladder::MAX_STEP {
            let scale = cap.of(Ladder::new(step).render_scale());
            for zoom in &zooms {
                let base = zoom.sub_units_per_pixel();
                let whole = scale.step(base).expect("a whole step");
                assert_eq!(
                    i64::from(whole) * i64::from(scale.numerator()),
                    i64::from(base) * i64::from(scale.denominator()),
                    "{scale:?} at {zoom:?} is not the zoom's own step widened"
                );
            }
        }
    }
}

#[test]
fn a_fraction_that_splits_a_sub_unit_is_refused() {
    let three_quarters = RenderScale {
        numerator: 3,
        denominator: 4,
    };
    assert_eq!(
        three_quarters.step(8),
        None,
        "eight sub-units widened by a third split one"
    );
    assert_eq!(three_quarters.step(12), Some(16));
    assert_eq!(RenderScale::ONE.step(8), Some(8));
}

/// The floor falls where the smallest figure a record describes is drawn at
/// the art harness's own floor, and a view the player's zoom has already
/// drawn smaller than that leaves the render scale alone.
#[test]
fn the_floor_holds_the_smallest_figure_at_the_readable_size() {
    let notches = u8::try_from(RENDER_SCALES.len()).expect("a handful of fractions");
    let native_floor = Ladder::new(Ladder::MAX_STEP - notches);
    for zoom in [Zoom::NEAREST, Zoom::DEFAULT, Zoom::FURTHEST] {
        let floor = Ladder::floor(1280, 720, zoom);
        let view = Viewport::new(1280, 720, floor.render_scale()).expect("a real window");
        if floor.render_scale().is_native() {
            assert_eq!(floor, native_floor, "{zoom:?} shed past the native scale");
        } else {
            assert!(
                readable(view.step(zoom)),
                "{zoom:?}'s floor draws figures unreadably"
            );
        }
        if let Some(deeper) = floor.shed() {
            let past = Viewport::new(1280, 720, deeper.render_scale()).expect("a real window");
            assert!(
                !readable(past.step(zoom)),
                "{zoom:?} stopped short of a readable notch"
            );
        }
    }
    assert_eq!(
        Ladder::floor(1280, 720, Zoom::DEFAULT),
        Ladder::new(Ladder::MAX_STEP),
        "the view the game is authored at stays readable down the whole ladder"
    );
    assert!(
        Ladder::floor(1280, 720, Zoom::FURTHEST) < Ladder::floor(1280, 720, Zoom::DEFAULT),
        "a further view did not stop the render scale sooner"
    );
}
