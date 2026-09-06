//! The special functions the statistical battery's p-values are expressed in.
//!
//! Every test in the battery reduces its statistic to a tail probability, and
//! all of those tails are one of two functions: the regularised upper
//! incomplete gamma `Q(a, x)` and the complementary error function
//! `erfc(x)`. Only the first is implemented here — `erfc(x) = Q(1/2, x²)` —
//! so there is one convergent series and one continued fraction to be right
//! about rather than two of each.
//!
//! These are ordinary numerical algorithms (Press et al., *Numerical
//! Recipes*, §6.2), not cryptographic primitives, and they are host test
//! scaffolding that never enters a TAIRiX build.

/// Relative convergence target: near `f64` epsilon, so the iterations stop
/// when further terms cannot move the result.
const EPS: f64 = 3.0e-16;

/// Smallest magnitude the continued fraction's denominators are allowed to
/// take, so a near-zero term cannot become an infinity.
const FPMIN: f64 = 1.0e-300;

/// Iteration ceiling. Both expansions converge in well under a hundred terms
/// over the range the battery uses; the bound is what keeps a pathological
/// argument from looping without end.
const ITMAX: u32 = 1000;

/// `ln Γ(x)` for `x > 0`, by the Lanczos series.
///
/// Relative accuracy is around `1e-10`, far inside what a p-value needs.
fn ln_gamma(x: f64) -> f64 {
    const COF: [f64; 6] = [
        76.180_091_729_471_46,
        -86.505_320_329_416_77,
        24.014_098_240_830_91,
        -1.231_739_572_450_155,
        0.120_865_097_386_617_9e-2,
        -0.539_523_938_495_3e-5,
    ];
    let mut y = x;
    let mut tmp = x + 5.5;
    tmp -= (x + 0.5) * tmp.ln();
    let mut ser = 1.000_000_000_190_015;
    for c in COF {
        y += 1.0;
        ser += c / y;
    }
    -tmp + (2.506_628_274_631_000_5 * ser / x).ln()
}

/// `P(a, x)` — the regularised *lower* incomplete gamma — by its series,
/// which converges quickly for `x < a + 1`.
fn lower_by_series(a: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut ap = a;
    let mut sum = 1.0 / a;
    let mut del = sum;
    for _ in 0..ITMAX {
        ap += 1.0;
        del *= x / ap;
        sum += del;
        if del.abs() < sum.abs() * EPS {
            break;
        }
    }
    sum * (-x + a * x.ln() - ln_gamma(a)).exp()
}

/// `Q(a, x)` — the regularised *upper* incomplete gamma — by its continued
/// fraction (modified Lentz), which converges quickly for `x >= a + 1`.
fn upper_by_continued_fraction(shape: f64, point: f64) -> f64 {
    // `b`, `c`, `d`, `h` are the modified-Lentz recurrence's own names; a
    // reader checking this against the reference needs them to match it.
    let mut b = point + 1.0 - shape;
    let mut c = 1.0 / FPMIN;
    let mut d = 1.0 / b;
    let mut h = d;
    for i in 1..ITMAX {
        let term = f64::from(i);
        let an = -term * (term - shape);
        b += 2.0;
        d = an * d + b;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = b + an / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h * (-point + shape * point.ln() - ln_gamma(shape)).exp()
}

/// `Q(a, x)`, the regularised upper incomplete gamma: the probability that a
/// chi-square variate with `2a` degrees of freedom exceeds `2x`.
///
/// Returns `1.0` for a non-positive `x` (nothing has been observed, so
/// nothing is surprising) and `0.0` for a non-positive `a`, which no caller
/// in the battery passes.
#[must_use]
pub fn gamma_q(a: f64, x: f64) -> f64 {
    if a <= 0.0 {
        return 0.0;
    }
    if x <= 0.0 {
        return 1.0;
    }
    if x < a + 1.0 {
        (1.0 - lower_by_series(a, x)).clamp(0.0, 1.0)
    } else {
        upper_by_continued_fraction(a, x).clamp(0.0, 1.0)
    }
}

/// `erfc(x)`, the complementary error function.
///
/// Expressed through [`gamma_q`] (`erfc(x) = Q(1/2, x²)` for `x >= 0`) and
/// reflected for negative arguments, so the battery carries one tail
/// implementation rather than two.
#[must_use]
pub fn erfc(x: f64) -> f64 {
    if x < 0.0 {
        2.0 - gamma_q(0.5, x * x)
    } else {
        gamma_q(0.5, x * x)
    }
}

/// The standard normal cumulative distribution `Φ(z)`.
#[must_use]
pub fn normal_cdf(z: f64) -> f64 {
    0.5 * erfc(-z / core::f64::consts::SQRT_2)
}

/// The upper-tail probability of a chi-square statistic with `df` degrees of
/// freedom — the form every goodness-of-fit test in the battery reports.
#[must_use]
pub fn chi_square_q(chi_square: f64, df: f64) -> f64 {
    gamma_q(df / 2.0, chi_square / 2.0)
}

#[cfg(test)]
mod tests {
    use super::{chi_square_q, erfc, gamma_q, ln_gamma, normal_cdf};

    fn close(got: f64, want: f64, tol: f64) {
        assert!(
            (got - want).abs() <= tol,
            "got {got}, want {want} (tolerance {tol})"
        );
    }

    #[test]
    fn ln_gamma_matches_known_values() {
        // Γ(1) = Γ(2) = 1, Γ(1/2) = √π, Γ(6) = 120.
        close(ln_gamma(1.0), 0.0, 1e-12);
        close(ln_gamma(2.0), 0.0, 1e-12);
        close(ln_gamma(0.5), core::f64::consts::PI.sqrt().ln(), 1e-12);
        close(ln_gamma(6.0), 120.0_f64.ln(), 1e-11);
        // A large argument, as approximate entropy's 2^(m-1) supplies.
        close(ln_gamma(512.0), 2_679.822_147_001_309, 1e-6);
    }

    #[test]
    fn erfc_matches_known_values() {
        close(erfc(0.0), 1.0, 1e-14);
        close(erfc(1.0), 0.157_299_207_050_285_13, 1e-12);
        close(erfc(-1.0), 1.842_700_792_949_715, 1e-12);
        close(erfc(2.0), 0.004_677_734_981_047_266, 1e-14);
        close(erfc(3.0), 2.209_049_699_858_544_3e-5, 1e-16);
        close(erfc(5.0), 1.537_459_794_428_035_7e-12, 1e-22);
    }

    /// Both branches of `gamma_q` must agree with each other where they meet,
    /// or a p-value would jump at `x = a + 1`.
    #[test]
    fn the_two_gamma_expansions_agree_at_their_boundary() {
        for a in [0.5, 1.0, 4.5, 24.5, 512.0] {
            let boundary = a + 1.0;
            let below = gamma_q(a, boundary - 1e-9);
            let above = gamma_q(a, boundary + 1e-9);
            close(below, above, 1e-9);
        }
    }

    #[test]
    fn chi_square_tails_match_published_values() {
        // A chi-square of 3.841 with 1 df is the 5% point; 16.919 with 9 df
        // is the 5% point; 0.0 is never surprising.
        close(chi_square_q(3.841_458_820_694_124, 1.0), 0.05, 1e-9);
        close(chi_square_q(16.918_977_604_620_448, 9.0), 0.05, 1e-9);
        close(chi_square_q(0.0, 9.0), 1.0, 1e-15);
        // The far tail, where a rejected generator lands.
        assert!(chi_square_q(500.0, 9.0) < 1e-100);
    }

    #[test]
    fn a_tail_probability_is_always_a_probability() {
        for a in [0.5, 1.0, 4.5, 512.0] {
            for x in [0.0, 1e-12, 0.1, 1.0, 10.0, 1e3, 1e6] {
                let q = gamma_q(a, x);
                assert!((0.0..=1.0).contains(&q), "Q({a}, {x}) = {q}");
            }
        }
    }

    #[test]
    fn normal_cdf_matches_known_values() {
        close(normal_cdf(0.0), 0.5, 1e-14);
        close(normal_cdf(1.959_963_984_540_054), 0.975, 1e-12);
        close(normal_cdf(-1.959_963_984_540_054), 0.025, 1e-12);
        // Monotone and bounded across a wide span.
        let mut previous = 0.0;
        for step in -80..=80 {
            let value = normal_cdf(f64::from(step) / 10.0);
            assert!(value >= previous, "Φ is not monotone at {step}");
            assert!((0.0..=1.0).contains(&value));
            previous = value;
        }
    }
}
