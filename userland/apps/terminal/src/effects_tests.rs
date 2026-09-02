//! Unit tests for the terminal's screen-effect pipeline.

use tairix_raster::{Color, Pixel, Surface};

use super::{
    EffectKey, EffectState, Effects, Pass, Phase, FULL, GLOW_REACH_PX, MAX_BLUR_RADIUS_PX,
    MAX_GLOW_INTENSITY, MIN_OPACITY,
};

fn surface(width: u32, height: u32) -> Surface {
    Surface::new(width, height).expect("surface allocation")
}

// --- Effects::default -------------------------------------------------------

#[test]
fn default_is_see_through_and_frosted_with_no_passes_and_not_animated() {
    let effects = Effects::default();
    assert!(
        effects.background_alpha() < 255,
        "the default is see-through"
    );
    assert_eq!(
        effects.blur_radius_px(),
        MAX_BLUR_RADIUS_PX / 2,
        "the window reads as frosted glass, at half the strength the slider \
         reaches"
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
        glow: 500,
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
fn passes_are_ordered_wobble_phosphor_scanlines_glow_fuzz() {
    let effects = Effects {
        opacity: FULL,
        blur: 0,
        wobble: 500,
        phosphor: 500,
        scanlines: 500,
        glow: 500,
        fuzz: 500,
    };
    let (passes, len) = effects.passes(100);
    assert_eq!(len, 5);
    assert!(matches!(passes[0], Pass::Wobble { .. }));
    assert!(matches!(passes[1], Pass::Phosphor { .. }));
    assert!(matches!(passes[2], Pass::ScanLines { .. }));
    // The glow spreads the light the passes above settled on, so it follows
    // them and reaches into the scan lines' dark rows.
    assert!(matches!(passes[3], Pass::Glow { .. }));
    assert!(matches!(passes[4], Pass::Fuzz { .. }));
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
        glow: 400,
        fuzz: 300,
    };
    let mut base = surface(6, 6);
    base.fill(Color::rgb(200, 150, 50));

    let mut first = base.clone();
    let mut state_a = EffectState::new();
    effects.apply(&mut first, &mut state_a, Phase(7), 150);

    let mut second = base;
    let mut state_b = EffectState::new();
    effects.apply(&mut second, &mut state_b, Phase(7), 150);

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
    effects.apply(&mut surface, &mut EffectState::new(), Phase(0), 100);

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
    effects.apply(&mut phase0, &mut EffectState::new(), Phase(0), 100);
    assert_ne!(phase0, base, "fuzz should perturb at least one pixel");

    let mut phase1 = base;
    effects.apply(&mut phase1, &mut EffectState::new(), Phase(1), 100);
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
    effects.apply(&mut moved, &mut EffectState::new(), Phase(0), 100);

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
    let mut state = EffectState::new();

    let mut frame1 = surface(2, 2);
    frame1.set(0, 0, Color::rgb(255, 255, 255).premultiply());
    effects.apply(&mut frame1, &mut state, Phase(0), 100);

    let mut frame2 = surface(2, 2);
    frame2.set(0, 0, Color::rgb(0, 0, 0).premultiply());
    effects.apply(&mut frame2, &mut state, Phase(1), 100);

    let lit_pixel = frame2.get(0, 0).expect("in bounds");
    assert!(
        lit_pixel.r > 0,
        "the earlier frame's light should still show through"
    );
}

#[test]
fn clearing_the_state_forgets_the_trail() {
    let effects = Effects {
        phosphor: FULL,
        ..Effects::default()
    };
    let mut state = EffectState::new();

    let mut frame1 = surface(2, 2);
    frame1.set(0, 0, Color::rgb(255, 255, 255).premultiply());
    effects.apply(&mut frame1, &mut state, Phase(0), 100);

    state.clear();

    let mut frame2 = surface(2, 2);
    frame2.set(0, 0, Color::rgb(0, 0, 0).premultiply());
    effects.apply(&mut frame2, &mut state, Phase(1), 100);

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
        glow: 400,
        fuzz: 300,
    };
    let mut zero = surface(0, 0);
    effects.apply(&mut zero, &mut EffectState::new(), Phase(0), 100);
}

#[test]
fn apply_on_a_1x1_surface_does_not_panic() {
    let effects = Effects {
        opacity: 500,
        blur: 300,
        wobble: 400,
        phosphor: 300,
        scanlines: 400,
        glow: 400,
        fuzz: 300,
    };
    let mut tiny = surface(1, 1);
    effects.apply(&mut tiny, &mut EffectState::new(), Phase(0), 100);
}

// --- Glow ---------------------------------------------------------------------

/// The glow tests' field: a 32x32 screen with a lit 8x8 square spanning
/// `12..20` on both axes, so [`OUTSIDE`] is the first unlit column and the
/// reach at reference density ends five columns past the square.
const FIELD: u32 = 32;

/// The side of that lit square.
const BLOCK: u32 = 8;

/// A row, and column, through the middle of the lit square.
const MIDDLE: u32 = FIELD / 2;

/// The first unlit column to the right of the lit square.
const OUTSIDE: u32 = MIDDLE + BLOCK / 2;

/// A `side`x`side` field of `background` with a centred `block`x`block`
/// square of `lit`: a stand-in for a run of text on a screen.
fn lit_block(side: u32, block: u32, background: Pixel, lit: Pixel) -> Surface {
    let mut surface = surface(side, side);
    for y in 0..side {
        for x in 0..side {
            surface.set(x, y, background);
        }
    }
    let start = (side - block) / 2;
    for y in start..start + block {
        for x in start..start + block {
            surface.set(x, y, lit);
        }
    }
    surface
}

/// Every effect off but the glow, spelled in full so a new effect has to be
/// considered here rather than defaulted into these tests.
fn glow_only(strength: u16) -> Effects {
    Effects {
        opacity: FULL,
        blur: 0,
        scanlines: 0,
        glow: strength,
        fuzz: 0,
        phosphor: 0,
        wobble: 0,
    }
}

/// The glow pass `strength` resolves to at `scale_percent`.
fn glow_pass(strength: u16, scale_percent: u32) -> Option<Pass> {
    let (passes, len) = glow_only(strength).passes(scale_percent);
    passes
        .iter()
        .take(len)
        .find(|pass| matches!(pass, Pass::Glow { .. }))
        .copied()
}

#[test]
fn glow_spreads_light_off_a_lit_square_and_falls_off_with_distance() {
    let dark = Color::rgb(0, 0, 0).premultiply();
    let white = Color::rgb(255, 255, 255).premultiply();
    let before = lit_block(FIELD, BLOCK, dark, white);
    let mut after = before.clone();
    glow_only(FULL).apply(&mut after, &mut EffectState::new(), Phase(0), 100);

    let red_at = |frame: &Surface, x: u32| frame.get(x, MIDDLE).expect("in bounds").r;
    let near = red_at(&after, OUTSIDE);
    let far = red_at(&after, OUTSIDE + 3);
    assert!(near > 0, "the pixel beside the square should be lit");
    assert!(far > 0, "and one three further out still reached");
    assert!(near > far, "nearer the square is brighter: {near} vs {far}");

    let beyond = OUTSIDE + GLOW_REACH_PX + 1;
    assert_eq!(
        red_at(&after, beyond),
        red_at(&before, beyond),
        "past its reach the glow leaves the frame alone"
    );
}

#[test]
fn a_field_below_the_knee_does_not_glow_at_all() {
    // Nothing here drives a channel past the knee, so the frame comes back
    // untouched: the effect reads as light off the text, not a flat wash.
    let dim = Pixel {
        r: 100,
        g: 100,
        b: 100,
        a: 255,
    };
    let before = lit_block(FIELD, BLOCK, dim, dim);
    let mut after = before.clone();
    glow_only(FULL).apply(&mut after, &mut EffectState::new(), Phase(0), 100);
    assert_eq!(after, before);
}

#[test]
fn a_saturated_colour_glows_as_hard_as_any_other() {
    // The drive is the peak channel, not a luma weighting: weighted, a
    // saturated red would not clear the knee at all where green sailed over
    // it, and red text would carry no halo.
    let dark = Color::rgb(0, 0, 0).premultiply();
    let mut reddish = lit_block(FIELD, BLOCK, dark, Color::rgb(255, 0, 0).premultiply());
    let mut greenish = lit_block(FIELD, BLOCK, dark, Color::rgb(0, 255, 0).premultiply());
    glow_only(FULL).apply(&mut reddish, &mut EffectState::new(), Phase(0), 100);
    glow_only(FULL).apply(&mut greenish, &mut EffectState::new(), Phase(0), 100);

    let red_halo = reddish.get(OUTSIDE, MIDDLE).expect("in bounds");
    let green_halo = greenish.get(OUTSIDE, MIDDLE).expect("in bounds");
    assert!(red_halo.r > 0, "a saturated red square must glow");
    assert_eq!(
        red_halo.r, green_halo.g,
        "and by as much as a saturated green one"
    );
}

#[test]
fn glow_lifts_the_alpha_of_what_it_lights() {
    // Light the tube is emitting is not see-through, however translucent the
    // background behind it is.
    let translucent = Pixel {
        r: 0,
        g: 0,
        b: 0,
        a: 128,
    };
    let mut after = lit_block(
        FIELD,
        BLOCK,
        translucent,
        Color::rgb(255, 255, 255).premultiply(),
    );
    glow_only(FULL).apply(&mut after, &mut EffectState::new(), Phase(0), 100);

    let halo = after.get(OUTSIDE, MIDDLE).expect("in bounds");
    assert!(
        halo.a > translucent.a,
        "alpha {} should have risen above {}",
        halo.a,
        translucent.a
    );
    assert!(halo.r <= halo.a, "and the pixel must stay premultiplied");
}

#[test]
fn a_stronger_glow_adds_more_light() {
    let dark = Color::rgb(0, 0, 0).premultiply();
    let white = Color::rgb(255, 255, 255).premultiply();
    let halo = |strength: u16| {
        let mut frame = lit_block(FIELD, BLOCK, dark, white);
        glow_only(strength).apply(&mut frame, &mut EffectState::new(), Phase(0), 100);
        frame.get(OUTSIDE, MIDDLE).expect("in bounds").r
    };
    assert_eq!(halo(0), 0, "no glow leaves the unlit pixel unlit");
    assert!(halo(FULL / 2) > halo(0));
    assert!(halo(FULL) > halo(FULL / 2));
}

#[test]
fn glow_reach_is_density_scaled_and_never_nothing() {
    let reach = |scale: u32| match glow_pass(FULL, scale) {
        Some(Pass::Glow { reach_px, .. }) => reach_px,
        _ => panic!("a glow strength should contribute a glow pass"),
    };
    assert_eq!(reach(100), GLOW_REACH_PX);
    assert!(reach(200) > reach(100), "a denser screen spreads further");
    assert!(reach(1) >= 1, "and a halo is never nothing");
}

#[test]
fn glow_intensity_is_capped_so_text_stays_legible() {
    // A light scheme's background clears the knee on its own, so at full
    // strength the whole field glows; the cap is what keeps dark glyphs
    // standing off it.
    match glow_pass(FULL, 100) {
        Some(Pass::Glow { intensity, .. }) => assert_eq!(intensity, MAX_GLOW_INTENSITY),
        _ => panic!("a glow strength should contribute a glow pass"),
    }
}

#[test]
fn a_zero_glow_contributes_no_pass() {
    assert_eq!(glow_pass(0, 100), None);
}

#[test]
fn is_animated_false_with_only_glow() {
    // The halo is a function of the frame, so it needs no timed repaint of
    // its own.
    assert!(!glow_only(FULL).is_animated(100));
}

// --- EffectKey ----------------------------------------------------------------

#[test]
fn every_effect_key_reads_and_writes_only_its_own_field() {
    // The settings sheet builds a slider per key and reads the row back
    // through the same list, so a key aliasing another's field would move the
    // wrong effect.
    const MARK: u16 = 111;
    for key in EffectKey::ALL {
        let mut effects = Effects::default();
        key.set(&mut effects, MARK);
        assert_eq!(key.of(effects), MARK, "{} does not read back", key.label());
        for other in EffectKey::ALL {
            if other == key {
                continue;
            }
            assert_eq!(
                other.of(effects),
                other.of(Effects::default()),
                "{} moved when {} was set",
                other.label(),
                key.label()
            );
        }
    }
}

#[test]
fn effect_keys_and_their_labels_are_all_distinct() {
    assert_eq!(EffectKey::ALL.len(), EffectKey::COUNT);
    for (index, key) in EffectKey::ALL.iter().enumerate() {
        for other in EffectKey::ALL.iter().skip(index + 1) {
            assert_ne!(key, other, "duplicated key");
            assert_ne!(key.label(), other.label(), "duplicated label");
        }
    }
}

#[test]
fn only_opacity_has_a_raised_floor() {
    for key in EffectKey::ALL {
        let (min, max) = key.bounds();
        assert_eq!(max, FULL, "{} spans to full strength", key.label());
        let floor = if key == EffectKey::Opacity {
            MIN_OPACITY
        } else {
            0
        };
        assert_eq!(min, floor, "{} floor", key.label());
    }
}

// --- Phase::advance -----------------------------------------------------------

#[test]
fn phase_advance_wraps_at_the_maximum_without_panicking() {
    let phase = Phase(u32::MAX);
    assert_eq!(phase.advance(), Phase(0));
}
