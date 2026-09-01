//! Unit tests for the terminal's screen-effect pipeline.

use tairix_raster::{Color, Pixel, Surface};

use super::{Afterglow, Effects, Pass, Phase, FULL, MAX_BLUR_RADIUS_PX, MIN_OPACITY};

fn surface(width: u32, height: u32) -> Surface {
    Surface::new(width, height).expect("surface allocation")
}

// --- Effects::default -------------------------------------------------------

#[test]
fn default_is_see_through_with_no_blur_no_passes_and_not_animated() {
    let effects = Effects::default();
    assert!(
        effects.background_alpha() < 255,
        "the default is see-through"
    );
    assert_eq!(
        effects.blur_radius_px(),
        0,
        "translucency is free; a backdrop blur costs the compositor a frost \
         per frosted window, so it is opt-in"
    );
    let (_, len) = effects.passes(100);
    assert_eq!(len, 0);
    assert!(!effects.is_animated(100));
}

// --- Effects::background_alpha ----------------------------------------------

#[test]
fn background_alpha_full_opacity_is_fully_opaque() {
    let effects = Effects {
        opacity: FULL,
        ..Effects::default()
    };
    assert_eq!(effects.background_alpha(), 255);
}

#[test]
fn background_alpha_at_the_minimum_opacity_is_proportional() {
    let effects = Effects {
        opacity: MIN_OPACITY,
        ..Effects::default()
    };
    // 300/1000 of 255, rounded to nearest, is 77.
    assert_eq!(effects.background_alpha(), 77);
}

#[test]
fn background_alpha_never_drops_below_the_minimum_opacitys_alpha() {
    let effects = Effects {
        opacity: MIN_OPACITY / 2,
        ..Effects::default()
    };
    assert_eq!(effects.background_alpha(), 77);
}

#[test]
fn background_alpha_never_exceeds_full_opacity() {
    let effects = Effects {
        opacity: FULL + 500,
        ..Effects::default()
    };
    assert_eq!(effects.background_alpha(), 255);
}

// --- Effects::blur_radius_px -------------------------------------------------

#[test]
fn blur_radius_is_zero_when_fully_opaque_however_high_the_slider() {
    let effects = Effects {
        opacity: FULL,
        blur: FULL,
        ..Effects::default()
    };
    assert_eq!(effects.blur_radius_px(), 0);
}

#[test]
fn blur_radius_is_proportional_when_translucent() {
    let effects = Effects {
        opacity: 500,
        blur: 500,
        ..Effects::default()
    };
    assert_eq!(effects.blur_radius_px(), 12);
}

#[test]
fn blur_radius_never_exceeds_the_maximum() {
    let effects = Effects {
        opacity: 1,
        blur: FULL,
        ..Effects::default()
    };
    assert!(effects.blur_radius_px() <= MAX_BLUR_RADIUS_PX);
    assert_eq!(effects.blur_radius_px(), MAX_BLUR_RADIUS_PX);
}

// --- Effects::passes ---------------------------------------------------------

#[test]
fn a_zero_strength_effect_contributes_no_pass() {
    let effects = Effects {
        wobble: 0,
        phosphor: 500,
        scanlines: 500,
        fuzz: 500,
        ..Effects::default()
    };
    let (passes, len) = effects.passes(100);
    assert!(!passes
        .iter()
        .take(len)
        .any(|pass| matches!(pass, Pass::Wobble { .. })));
}

#[test]
fn passes_are_ordered_wobble_phosphor_scanlines_fuzz() {
    let effects = Effects {
        opacity: FULL,
        blur: 0,
        wobble: 500,
        phosphor: 500,
        scanlines: 500,
        fuzz: 500,
    };
    let (passes, len) = effects.passes(100);
    assert_eq!(len, 4);
    assert!(matches!(passes[0], Pass::Wobble { .. }));
    assert!(matches!(passes[1], Pass::Phosphor { .. }));
    assert!(matches!(passes[2], Pass::ScanLines { .. }));
    assert!(matches!(passes[3], Pass::Fuzz { .. }));
}

#[test]
fn wobble_amplitude_is_at_least_one_pixel_and_grows_with_scale() {
    let effects = Effects {
        wobble: 1,
        ..Effects::default()
    };
    let (passes, len) = effects.passes(100);
    assert_eq!(len, 1);
    let Pass::Wobble {
        amplitude_px: small_amplitude,
    } = passes[0]
    else {
        panic!("expected a wobble pass");
    };
    assert!(small_amplitude >= 1);

    let effects = Effects {
        wobble: 500,
        ..Effects::default()
    };
    let (passes_100, _) = effects.passes(100);
    let (passes_200, _) = effects.passes(200);
    let Pass::Wobble {
        amplitude_px: at_100,
    } = passes_100[0]
    else {
        panic!("expected a wobble pass");
    };
    let Pass::Wobble {
        amplitude_px: at_200,
    } = passes_200[0]
    else {
        panic!("expected a wobble pass");
    };
    assert!(at_200 > at_100);
}

// --- Effects::is_animated -----------------------------------------------------

#[test]
fn is_animated_true_with_wobble() {
    let effects = Effects {
        wobble: 500,
        ..Effects::default()
    };
    assert!(effects.is_animated(100));
}

#[test]
fn is_animated_true_with_fuzz() {
    let effects = Effects {
        fuzz: 500,
        ..Effects::default()
    };
    assert!(effects.is_animated(100));
}

#[test]
fn is_animated_true_with_phosphor() {
    let effects = Effects {
        phosphor: 500,
        ..Effects::default()
    };
    assert!(effects.is_animated(100));
}

#[test]
fn is_animated_false_with_only_scan_lines() {
    let effects = Effects {
        scanlines: 500,
        ..Effects::default()
    };
    assert!(!effects.is_animated(100));
}

#[test]
fn is_animated_false_with_nothing_on() {
    assert!(!Effects::default().is_animated(100));
}

// --- Effects::apply ------------------------------------------------------------

#[test]
fn apply_is_deterministic_for_the_same_surface_and_phase() {
    let effects = Effects {
        opacity: 500,
        blur: 300,
        wobble: 400,
        phosphor: 300,
        scanlines: 400,
        fuzz: 300,
    };
    let mut base = surface(6, 6);
    base.fill(Color::rgb(200, 150, 50));

    let mut first = base.clone();
    let mut afterglow_a = Afterglow::new();
    effects.apply(&mut first, &mut afterglow_a, Phase(7), 150);

    let mut second = base;
    let mut afterglow_b = Afterglow::new();
    effects.apply(&mut second, &mut afterglow_b, Phase(7), 150);

    assert_eq!(first, second);
}

#[test]
fn scan_lines_darken_odd_rows_and_leave_even_rows_alone() {
    let mut surface = surface(2, 4);
    surface.fill(Color::rgb(255, 255, 255));
    let effects = Effects {
        scanlines: FULL,
        ..Effects::default()
    };
    effects.apply(&mut surface, &mut Afterglow::new(), Phase(0), 100);

    let untouched = Pixel {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };
    assert_eq!(surface.get(0, 0), Some(untouched));
    assert_eq!(surface.get(1, 0), Some(untouched));
    let darkened = surface.get(0, 1).expect("in bounds");
    assert!(darkened.r < untouched.r);
    // Row 2 is even again and stays untouched.
    assert_eq!(surface.get(0, 2), Some(untouched));
}

#[test]
fn fuzz_changes_pixels_and_a_different_phase_gives_a_different_result() {
    let mut base = surface(4, 4);
    base.fill(Color::rgb(128, 128, 128));
    let effects = Effects {
        fuzz: FULL,
        ..Effects::default()
    };

    let mut phase0 = base.clone();
    effects.apply(&mut phase0, &mut Afterglow::new(), Phase(0), 100);
    assert_ne!(phase0, base, "fuzz should perturb at least one pixel");

    let mut phase1 = base;
    effects.apply(&mut phase1, &mut Afterglow::new(), Phase(1), 100);
    assert_ne!(
        phase0, phase1,
        "a different phase should give a different jitter"
    );
}

#[test]
fn wobble_displaces_content_horizontally() {
    let width = 8;
    let mut original = surface(width, 2);
    for x in 0..width {
        let value = u8::try_from(x * 30).unwrap_or(u8::MAX);
        original.set(x, 0, Color::rgb(value, 0, 0).premultiply());
        original.set(x, 1, Color::rgb(value, 0, 0).premultiply());
    }
    let effects = Effects {
        wobble: FULL,
        ..Effects::default()
    };
    let mut moved = original.clone();
    effects.apply(&mut moved, &mut Afterglow::new(), Phase(0), 100);

    // Row 0's phase angle is exactly zero, so its content is untouched.
    for x in 0..width {
        assert_eq!(moved.get(x, 0), original.get(x, 0));
    }
    // Row 1 is displaced: content moves right, so the leftmost columns that
    // have nothing to its left to draw from turn transparent, and every
    // other column reads from three columns to its left (amplitude 6 at the
    // full strength, angle 6 sixty-fourths of a turn, at 100% scale).
    assert_eq!(moved.get(0, 1), Some(Pixel::TRANSPARENT));
    for x in 3..width {
        assert_eq!(moved.get(x, 1), original.get(x - 3, 1));
    }
    assert_ne!(moved.get(0, 1), original.get(0, 1));
}

#[test]
fn phosphor_leaves_a_trail_that_survives_into_a_darker_frame() {
    let effects = Effects {
        phosphor: FULL,
        ..Effects::default()
    };
    let mut afterglow = Afterglow::new();

    let mut frame1 = surface(2, 2);
    frame1.set(0, 0, Color::rgb(255, 255, 255).premultiply());
    effects.apply(&mut frame1, &mut afterglow, Phase(0), 100);

    let mut frame2 = surface(2, 2);
    frame2.set(0, 0, Color::rgb(0, 0, 0).premultiply());
    effects.apply(&mut frame2, &mut afterglow, Phase(1), 100);

    let lit_pixel = frame2.get(0, 0).expect("in bounds");
    assert!(
        lit_pixel.r > 0,
        "the earlier frame's light should still show through"
    );
}

#[test]
fn afterglow_clear_forgets_the_trail() {
    let effects = Effects {
        phosphor: FULL,
        ..Effects::default()
    };
    let mut afterglow = Afterglow::new();

    let mut frame1 = surface(2, 2);
    frame1.set(0, 0, Color::rgb(255, 255, 255).premultiply());
    effects.apply(&mut frame1, &mut afterglow, Phase(0), 100);

    afterglow.clear();

    let mut frame2 = surface(2, 2);
    frame2.set(0, 0, Color::rgb(0, 0, 0).premultiply());
    effects.apply(&mut frame2, &mut afterglow, Phase(1), 100);

    assert_eq!(frame2.get(0, 0), Some(Color::rgb(0, 0, 0).premultiply()));
}

#[test]
fn apply_on_a_zero_sized_surface_does_not_panic() {
    let effects = Effects {
        opacity: 500,
        blur: 300,
        wobble: 400,
        phosphor: 300,
        scanlines: 400,
        fuzz: 300,
    };
    let mut zero = surface(0, 0);
    effects.apply(&mut zero, &mut Afterglow::new(), Phase(0), 100);
}

#[test]
fn apply_on_a_1x1_surface_does_not_panic() {
    let effects = Effects {
        opacity: 500,
        blur: 300,
        wobble: 400,
        phosphor: 300,
        scanlines: 400,
        fuzz: 300,
    };
    let mut tiny = surface(1, 1);
    effects.apply(&mut tiny, &mut Afterglow::new(), Phase(0), 100);
}

// --- Phase::advance -----------------------------------------------------------

#[test]
fn phase_advance_wraps_at_the_maximum_without_panicking() {
    let phase = Phase(u32::MAX);
    assert_eq!(phase.advance(), Phase(0));
}
