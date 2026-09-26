//! Unit tests for the shared `no_std` `f64` maths.
//!
//! The reference values are the IEEE-754 doubles a correctly-rounded libm
//! produces, quoted to their full precision, so a regression in a series or a
//! reduction shows up as a numeric difference rather than as artwork that
//! merely looks a bit wrong.

extern crate std;

use core::f64::consts::{FRAC_PI_2, FRAC_PI_3, FRAC_PI_4, PI, SQRT_2};

use super::{
    acos, asin, atan, atan2, ceil, clamp, cos, exp, fabs, floor, fmax, fmin, hypot, reduce, round,
    round_i32, sin, sqrt, tan, EXP_MAX_ARG, EXP_MIN_ARG, REDUCIBLE,
};

/// The accuracy every transcendental function is held to: far finer than the
/// sub-pixel grid any consumer rasterises onto, and tight enough that a wrong
/// coefficient or a mis-folded quadrant cannot pass.
const EPS: f64 = 1e-9;

#[track_caller]
fn close(actual: f64, expected: f64) {
    assert!(
        fabs(actual - expected) <= EPS,
        "expected {expected}, got {actual}"
    );
}

/// How many representable doubles lie between `a` and `b`.
fn ulps_apart(a: f64, b: f64) -> u64 {
    let ordered = |x: f64| {
        let bits = i64::from_ne_bytes(x.to_bits().to_ne_bytes());
        if bits < 0 {
            i64::MIN - bits
        } else {
            bits
        }
    };
    ordered(a).abs_diff(ordered(b))
}

/// Hold `ours` within `ulps` of the host libm's `theirs` at every point of
/// `range` stepped by `step`, reporting the worst point either way.
#[track_caller]
fn tracks_the_host(
    name: &str,
    ours: fn(f64) -> f64,
    theirs: fn(f64) -> f64,
    (from, to, step): (f64, f64, f64),
    ulps: u64,
) {
    let mut worst = (0, from);
    let mut x = from;
    while x <= to {
        let apart = ulps_apart(ours(x), theirs(x));
        if apart > worst.0 {
            worst = (apart, x);
        }
        x += step;
    }
    assert!(
        worst.0 <= ulps,
        "{name} is {} ulps from the host at {:e}",
        worst.0,
        worst.1
    );
}

// --- rounding -----------------------------------------------------------

#[test]
fn floor_and_ceil_bracket_a_fraction() {
    close(floor(2.1), 2.0);
    close(ceil(2.1), 3.0);
    close(floor(-2.1), -3.0);
    close(ceil(-2.1), -2.0);
    close(floor(4.0), 4.0);
    close(ceil(4.0), 4.0);
}

#[test]
fn round_takes_halves_upward() {
    close(round(2.5), 3.0);
    close(round(-2.5), -2.0);
    close(round(2.49), 2.0);
}

/// `floor(x + 0.5)` rounds the sum before it floors, which carries the
/// largest double below a half, and every odd integer past 2^52, up by one.
#[allow(
    clippy::float_cmp,
    reason = "a rounding result is an exact integer, so a tolerance would \
              accept the off-by-one this pins"
)]
#[test]
fn round_decides_on_the_exact_fraction() {
    let below_half = 0.499_999_999_999_999_94;
    assert!(below_half < 0.5 && below_half + 0.5 == 1.0);
    assert_eq!(round(below_half), 0.0);
    assert_eq!(round(-below_half), 0.0);
    assert_eq!(round_i32(below_half), 0);
    let odd = 4_503_599_627_370_497.0;
    assert_eq!(round(odd), odd);
    assert_eq!(round(-odd), -odd);
    assert_eq!(round(1.5), 2.0);
    assert_eq!(round(-1.5), -1.0);
    assert_eq!(round(-0.5), 0.0);
}

/// Integer rounding answers `0` for a `NaN` and keeps its exact meaning past
/// the range of any integer type.
#[allow(
    clippy::float_cmp,
    reason = "these are exact integers, which is the property under test"
)]
#[test]
fn integer_rounding_is_total_and_exact_at_any_magnitude() {
    for f in [floor, ceil, round] {
        assert_eq!(f(f64::NAN), 0.0);
        assert_eq!(f(1e300), 1e300);
        assert_eq!(f(-1e300), -1e300);
    }
    assert_eq!(round_i32(f64::NAN), 0);
    assert_eq!(floor(-0.5), -1.0);
    assert_eq!(ceil(0.5), 1.0);
    assert_eq!(floor(1.5e19), 1.5e19);
    assert_eq!(ceil(-1.5e19), -1.5e19);
}

#[test]
fn magnitude_and_ordering_helpers_agree_with_their_names() {
    close(fabs(-3.5), 3.5);
    close(fmax(-3.5, 2.0), 2.0);
    close(fmin(-3.5, 2.0), -3.5);
    close(clamp(9.0, 0.0, 4.0), 4.0);
    close(clamp(-9.0, 0.0, 4.0), 0.0);
    close(clamp(2.0, 0.0, 4.0), 2.0);
}

/// A coordinate a hostile document drove far out of range must clamp to the
/// extreme rather than wrap to the opposite side of the canvas.
#[test]
fn rounding_to_i32_saturates_instead_of_wrapping() {
    assert_eq!(round_i32(2.5), 3);
    assert_eq!(round_i32(-2.5), -2);
    assert_eq!(round_i32(1e300), i32::MAX);
    assert_eq!(round_i32(-1e300), i32::MIN);
}

// --- roots --------------------------------------------------------------

#[test]
fn square_roots_are_exact_to_the_last_bits() {
    close(sqrt(2.0), SQRT_2);
    close(sqrt(4.0), 2.0);
    close(sqrt(1e12), 1e6);
    close(sqrt(1e-12), 1e-6);
}

/// A negative or absent magnitude has no root to take; answering zero keeps
/// the caller's geometry finite instead of poisoning it with a `NaN`.
#[test]
fn a_non_positive_square_root_is_zero_not_a_nan() {
    close(sqrt(0.0), 0.0);
    close(sqrt(-4.0), 0.0);
    close(sqrt(f64::NAN), 0.0);
    assert_eq!(sqrt(-0.0).to_bits(), 0.0_f64.to_bits());
    assert_eq!(sqrt(f64::NEG_INFINITY).to_bits(), 0.0_f64.to_bits());
    assert_eq!(sqrt(f64::INFINITY).to_bits(), f64::INFINITY.to_bits());
}

/// The square root is correctly rounded, so it is the one IEEE 754 answer —
/// the same bits the host's own square root gives, for normal, subnormal and
/// extreme inputs alike. That agreement is what every target's instruction or
/// runtime routine is held to, and what the cross-target digests rest on.
#[test]
fn the_square_root_is_correctly_rounded() {
    // A stride through the whole positive range, co-prime to the mantissa
    // width so it lands on every exponent with a different fraction each time.
    let stride = 0x0000_1d3a_71f9_0b5d_u64;
    let mut bits = 1_u64;
    while bits < f64::INFINITY.to_bits() {
        let x = f64::from_bits(bits);
        assert_eq!(sqrt(x).to_bits(), f64::sqrt(x).to_bits(), "sqrt({x:e})");
        bits += stride;
    }
    for x in [f64::MIN_POSITIVE, f64::MAX, 2.0, 0.5, 1e-310] {
        assert_eq!(sqrt(x).to_bits(), f64::sqrt(x).to_bits(), "sqrt({x:e})");
    }
}

#[test]
fn hypotenuse_matches_the_triangle_and_handles_the_origin() {
    close(hypot(3.0, 4.0), 5.0);
    close(hypot(-3.0, -4.0), 5.0);
    close(hypot(0.0, 0.0), 0.0);
}

// --- angles -------------------------------------------------------------

#[test]
fn sine_matches_the_reference_across_every_quadrant() {
    close(sin(0.0), 0.0);
    close(sin(0.3), 0.295_520_206_661_339_55);
    close(sin(1.0), 0.841_470_984_807_896_5);
    close(sin(2.0), 0.909_297_426_825_681_7);
    close(sin(3.0), 0.141_120_008_059_867_2);
    close(sin(-2.5), -0.598_472_144_103_956_5);
    close(sin(10.0), -0.544_021_110_889_369_8);
}

#[test]
fn cosine_matches_the_reference_across_every_quadrant() {
    close(cos(0.0), 1.0);
    close(cos(0.7), 0.764_842_187_284_488_5);
    close(cos(2.4), -0.737_393_715_541_245_4);
    close(cos(-3.9), -0.725_932_304_200_140_2);
}

/// The identity holds everywhere, including far outside one turn where the
/// range reduction is doing the work.
#[test]
fn sine_and_cosine_stay_on_the_unit_circle() {
    let mut angle = -40.0;
    while angle < 40.0 {
        let unit = sin(angle) * sin(angle) + cos(angle) * cos(angle);
        close(unit, 1.0);
        angle += 0.37;
    }
}

#[test]
fn tangent_matches_the_reference_and_survives_its_pole() {
    close(tan(0.4), 0.422_793_218_738_161_8);
    close(tan(-1.2), -2.572_151_622_126_318_8);
    assert!(tan(FRAC_PI_2).is_finite());
    assert!(tan(-FRAC_PI_2).is_finite());
}

#[test]
fn arctangent_matches_the_reference_on_both_sides_of_its_reduction() {
    close(atan(0.0), 0.0);
    close(atan(0.1), 0.099_668_652_491_162_04);
    close(atan(0.5), 0.463_647_609_000_806_1);
    close(atan(1.0), PI / 4.0);
    close(atan(3.0), 1.249_045_772_398_254_4);
    close(atan(-3.0), -1.249_045_772_398_254_4);
}

#[test]
fn atan2_names_the_right_quadrant() {
    close(atan2(1.0, 1.0), FRAC_PI_4);
    close(atan2(1.0, -1.0), 2.356_194_490_192_345);
    close(atan2(-1.0, -1.0), -2.356_194_490_192_345);
    close(atan2(-3.0, 4.0), -0.643_501_108_793_284_4);
    close(atan2(1.0, 0.0), FRAC_PI_2);
    close(atan2(-1.0, 0.0), -FRAC_PI_2);
}

/// A degenerate segment has no direction; answering zero keeps a stroke's
/// normal finite rather than erasing the shape.
#[test]
fn atan2_at_the_origin_is_zero_not_a_nan() {
    close(atan2(0.0, 0.0), 0.0);
}

/// The kernels are fdlibm's minimax polynomials, so over the angles and
/// arguments consumers pass they track a correctly-rounded libm to within an
/// ulp — six orders finer than the truncated series they replaced. The
/// tangent is their quotient, and carries both errors.
#[test]
fn the_transcendentals_track_a_correctly_rounded_libm() {
    tracks_the_host("sin", sin, f64::sin, (-40.0, 40.0, 0.000_97), 1);
    tracks_the_host("cos", cos, f64::cos, (-40.0, 40.0, 0.000_97), 1);
    tracks_the_host("tan", tan, f64::tan, (-1.5, 1.5, 0.000_97), 3);
    tracks_the_host("atan", atan, f64::atan, (-60.0, 60.0, 0.000_97), 1);
    tracks_the_host("exp", exp, f64::exp, (-708.39, 709.78, 0.013), 1);
}

/// Next to a multiple of `PI/2` the remainder is tiny, and only a reduction
/// that takes more of `PI/2`'s bits as the first ones cancel keeps it accurate
/// to its own last bit: with one part the answer here is off by millions of
/// ulps while still looking like zero.
#[test]
fn angles_beside_a_quarter_turn_stay_accurate_to_the_last_bit() {
    let quarter_turns = (1..=4_000_u32).chain([65_535, 262_143, 1_000_000]);
    for k in quarter_turns {
        let near = f64::from(k) * FRAC_PI_2;
        for bits in near.to_bits() - 2..=near.to_bits() + 2 {
            for x in [f64::from_bits(bits), -f64::from_bits(bits)] {
                assert!(ulps_apart(sin(x), f64::sin(x)) <= 1, "sin({x:e})");
                assert!(ulps_apart(cos(x), f64::cos(x)) <= 1, "cos({x:e})");
            }
        }
    }
}

/// Past 2^20 quarter turns the parts of `PI/2` no longer reduce exactly, and
/// an angle there once answered the value at that bound: a sway driven by
/// uptime froze after eleven days. Each reference is the correctly rounded
/// value, worked out in exact rational arithmetic; the first angle is the
/// double nearest a quarter turn of all, which leaves a remainder of 2^-61.
#[test]
fn angles_past_a_million_quarter_turns_reduce_exactly() {
    let worst = 6_381_956_970_095_103.0 * f64::from_bits((1023 + 797) << 52);
    let references = [
        (worst, 1.0, -4.687_165_924_254_628e-19),
        (
            1_647_100.0,
            0.621_640_037_921_421_2,
            0.783_303_046_880_997_4,
        ),
        (2.0e6, -0.655_714_315_563_47, 0.755_009_096_875_746_4),
        (3.0e9, 0.987_004_886_474_355_4, -0.160_690_242_627_687_05),
        (5.0e15, -0.901_711_760_523_585, -0.432_337_716_297_638),
        (1e22, -0.852_200_849_767_188_8, 0.523_214_785_395_139),
        (-7.5e30, -0.965_150_598_936_113_2, 0.261_695_092_375_195_1),
        (1e100, -0.380_637_731_005_028_7, 0.924_724_238_751_933_8),
        (
            f64::MAX,
            0.004_961_954_789_184_062,
            -0.999_987_689_426_559_9,
        ),
    ];
    for (x, sine, cosine) in references {
        assert!(ulps_apart(sin(x), sine) <= 1, "sin({x:e}) = {:e}", sin(x));
        assert!(ulps_apart(cos(x), cosine) <= 1, "cos({x:e}) = {:e}", cos(x));
    }
    let (head, tail, quadrant) = reduce(worst);
    assert_eq!(head.to_bits(), 4.687_165_924_254_628e-19_f64.to_bits());
    assert!(fabs(tail) <= f64::EPSILON * head);
    assert_eq!(quadrant, 1);
}

/// Every exponent a double can carry past the moderate reduction, several
/// mantissas each, held to the host's libm: a wrong word of `2/PI` would
/// throw every angle whose reduction reads it off by a whole remainder.
#[test]
fn angles_past_a_million_quarter_turns_track_a_correctly_rounded_libm() {
    let stride = 0x0000_28f5_c28f_5c29_u64;
    let mut bits = REDUCIBLE.to_bits() + 1;
    while bits < f64::INFINITY.to_bits() {
        for x in [f64::from_bits(bits), -f64::from_bits(bits)] {
            assert!(ulps_apart(sin(x), f64::sin(x)) <= 1, "sin({x:e})");
            assert!(ulps_apart(cos(x), f64::cos(x)) <= 1, "cos({x:e})");
            assert!(ulps_apart(tan(x), f64::tan(x)) <= 3, "tan({x:e})");
        }
        bits += stride;
    }
}

/// Sine is odd and cosine even however large the angle, to the last bit.
#[test]
fn a_large_angle_and_its_negation_share_a_reduction() {
    for x in [REDUCIBLE.next_up(), 2.0e6, 7.3e19, 1e200, f64::MAX] {
        assert_eq!(sin(-x).to_bits(), (-sin(x)).to_bits(), "sin(-{x:e})");
        assert_eq!(cos(-x).to_bits(), cos(x).to_bits(), "cos(-{x:e})");
    }
}

/// The oddness the kernels would lose on their own: the sign of a zero angle
/// is kept, so a rotation by `-0` reflects nothing.
#[test]
fn a_negative_zero_angle_keeps_its_sign() {
    assert_eq!(sin(-0.0).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(atan(-0.0).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(cos(-0.0).to_bits(), 1.0_f64.to_bits());
}

/// A `NaN`, an infinity or an angle past any meaningful turn still answers a
/// finite value: a degenerate transform draws, rather than erasing a shape.
/// An infinite or `NaN` angle has no direction, so it turns nothing.
#[allow(
    clippy::float_cmp,
    reason = "a direction-less angle answers exactly the zero angle's values"
)]
#[test]
fn angles_are_total() {
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!((sin(x), cos(x), tan(x)), (0.0, 1.0, 0.0), "angle {x}");
    }
    for x in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1e300,
        -1e300,
        1e17,
    ] {
        for (name, f) in [("sin", sin as fn(f64) -> f64), ("cos", cos), ("tan", tan)] {
            assert!(f(x).is_finite(), "{name}({x}) = {}", f(x));
        }
        assert!(atan(x).is_finite(), "atan({x})");
        assert!(
            atan2(x, 1.0).is_finite() && atan2(1.0, x).is_finite(),
            "atan2 over {x}"
        );
    }
    close(atan(f64::NAN), 0.0);
    close(atan(f64::INFINITY), FRAC_PI_2);
    close(atan(f64::NEG_INFINITY), -FRAC_PI_2);
    close(sin(f64::NAN), 0.0);
    close(cos(f64::NAN), 1.0);
}

#[test]
fn arccosine_matches_the_reference_and_clamps_its_domain() {
    close(acos(1.0), 0.0);
    close(acos(0.5), FRAC_PI_3);
    close(acos(-0.25), 1.823_476_581_936_975_4);
    close(acos(-1.0), PI);
    // An arc conversion can overshoot the domain by a rounding step; the
    // endpoint is the honest answer there, never a `NaN`.
    close(acos(1.000_000_1), 0.0);
    close(acos(-1.000_000_1), PI);
}

#[test]
fn arcsine_matches_the_reference_and_clamps_its_domain() {
    close(asin(0.0), 0.0);
    close(asin(1.0), FRAC_PI_2);
    close(asin(-1.0), -FRAC_PI_2);
    close(asin(0.5), PI / 6.0);
    close(asin(-0.25), -0.252_680_255_142_078_64);
    close(asin(1.000_000_1), FRAC_PI_2);
    close(asin(-1.000_000_1), -FRAC_PI_2);
}

/// The two inverses answer the same triangle, so their sum is the right angle
/// between the sides they each name.
#[test]
fn arcsine_and_arccosine_are_complementary() {
    let mut x = -1.0;
    while x <= 1.0 {
        close(asin(x) + acos(x), FRAC_PI_2);
        x += 0.05;
    }
}

/// Round-tripping an angle through its own tangent is the sharpest check that
/// the two reductions agree with one another.
#[test]
fn arctangent_inverts_the_tangent() {
    let mut angle = -1.5;
    while angle < 1.5 {
        close(atan(tan(angle)), angle);
        angle += 0.13;
    }
}

#[test]
fn exponential_matches_the_reference_across_its_reduction() {
    close(exp(0.0), 1.0);
    close(exp(1.0), core::f64::consts::E);
    close(exp(-1.0), 0.367_879_441_171_442_33);
    close(exp(0.25), 1.284_025_416_687_741_4);
    close(exp(-0.25), 0.778_800_783_071_404_9);
    // Either side of the half-`ln(2)` fold, where `k` steps.
    close(exp(0.34), 1.404_947_591_288_49);
    close(exp(0.35), 1.419_067_548_593_257);
    close(exp(10.0), 22_026.465_794_806_718);
    close(exp(-10.0), 4.539_992_976_248_485_e-5);
}

/// A gain curve is the consumer, so the relative error is what matters over
/// the whole range rather than the absolute one a fixed epsilon measures.
#[test]
fn exponential_is_accurate_relative_to_its_own_magnitude() {
    let mut x = -300.0;
    while x < 300.0 {
        let got = exp(x);
        let halved = exp(x / 2.0);
        // `exp(x)` and `exp(x/2)^2` are computed through different reductions,
        // so agreement between them is a check on both.
        assert!(
            fabs(got - halved * halved) <= 1e-12 * got,
            "exp({x}) = {got} disagrees with exp({})^2 = {}",
            x / 2.0,
            halved * halved
        );
        x += 7.3;
    }
}

/// The evaluated range is every argument whose exponential is a normal
/// double: it once stopped short at 709 and -708, answering `f64::MAX` for
/// `exp(709.5)` and zero for `exp(-708)`. The references are the correctly
/// rounded values, worked out in exact rational arithmetic.
#[allow(
    clippy::float_cmp,
    reason = "the saturation endpoints are exact values, which is what is pinned"
)]
#[test]
fn exponential_reaches_both_ends_of_the_double_range() {
    let references = [
        (709.0, 8.218_407_461_554_972e307),
        (709.5, 1.354_986_319_314_632_8e308),
        (709.7, 1.654_984_027_680_264_4e308),
        (EXP_MAX_ARG, 1.797_693_134_862_273_2e308),
        (-708.0, 3.307_553_003_638_408e-308),
        (-708.2, 2.707_995_361_514_091_3e-308),
        (EXP_MIN_ARG, 2.225_073_858_507_262_6e-308),
    ];
    for (x, expected) in references {
        assert!(ulps_apart(exp(x), expected) <= 1, "exp({x}) = {:e}", exp(x));
    }
    assert!(exp(EXP_MAX_ARG) < f64::MAX);
    assert_eq!(exp(EXP_MAX_ARG.next_up()), f64::MAX);
    assert_eq!(exp(EXP_MIN_ARG.next_down()), 0.0);
}

/// Total like the rest of the module: the answer saturates rather than
/// becoming an infinity or a `NaN` a caller would have to guard against.
#[allow(
    clippy::float_cmp,
    reason = "the saturation endpoints are exact values, so a tolerance here \
              would accept the infinity the function exists to avoid"
)]
#[test]
fn exponential_saturates_instead_of_overflowing() {
    assert_eq!(exp(1e9), f64::MAX);
    assert_eq!(exp(f64::INFINITY), f64::MAX);
    assert_eq!(exp(-1e9), 0.0);
    assert_eq!(exp(f64::NEG_INFINITY), 0.0);
    assert_eq!(exp(f64::NAN), 0.0);
    // Everything it does answer is finite and non-negative.
    let mut x = -750.0;
    while x < 750.0 {
        let got = exp(x);
        assert!(got.is_finite() && got >= 0.0, "exp({x}) = {got}");
        x += 11.0;
    }
}
