//! Unit tests for SVG's number, length, and coordinate-list grammar.

use crate::error::SvgError;

use super::{opacity_to_alpha, parse_length, parse_number, parse_opacity, Numbers};

/// How close a parsed number must be to its value. Exact for everything an
/// asset spells; the tolerance only absorbs the unit conversions' division.
const EPS: f64 = 1e-9;

#[track_caller]
fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= EPS,
        "expected {expected}, got {actual}"
    );
}

// --- the number run ------------------------------------------------------

#[test]
fn a_run_may_be_separated_by_spaces_commas_or_nothing_at_all() {
    let mut numbers = Numbers::new("1 2,3  ,4-5+6");
    for expected in [1.0, 2.0, 3.0, 4.0, -5.0, 6.0] {
        close(numbers.required().expect("a number"), expected);
    }
    assert!(numbers.is_exhausted());
    assert_eq!(numbers.take(), Ok(None));
}

#[test]
fn a_run_reads_decimals_exponents_and_bare_points() {
    let mut numbers = Numbers::new(".5 -.25 1.5e2 3E-2 7.");
    for expected in [0.5, -0.25, 150.0, 0.03, 7.0] {
        close(numbers.required().expect("a number"), expected);
    }
}

/// Path data runs a coordinate straight into the next when the decimal point
/// makes the boundary unambiguous, so `.5.5` is two numbers, not one.
#[test]
fn a_second_decimal_point_starts_a_new_number() {
    let mut numbers = Numbers::new(".5.5");
    close(numbers.required().expect("first"), 0.5);
    close(numbers.required().expect("second"), 0.5);
    assert!(numbers.is_exhausted());
}

#[test]
fn a_malformed_run_is_refused_rather_than_half_read() {
    let mut numbers = Numbers::new("1 2 wat");
    close(numbers.required().expect("first"), 1.0);
    close(numbers.required().expect("second"), 2.0);
    assert_eq!(numbers.take(), Err(SvgError::InvalidNumber));
}

#[test]
fn an_exhausted_run_has_no_required_number() {
    let mut numbers = Numbers::new("   ");
    assert_eq!(numbers.required(), Err(SvgError::InvalidNumber));
}

/// An arc's flags are single digits that may be run into the value after
/// them, so they cannot be scanned as numbers.
#[test]
fn arc_flags_are_single_digits_that_may_touch_their_neighbour() {
    let mut numbers = Numbers::new("011 5");
    assert_eq!(numbers.required_flag(), Ok(false));
    assert_eq!(numbers.required_flag(), Ok(true));
    close(numbers.required().expect("the x coordinate"), 1.0);
    close(numbers.required().expect("the y coordinate"), 5.0);
}

#[test]
fn a_flag_that_is_not_zero_or_one_is_refused() {
    assert_eq!(
        Numbers::new("2").required_flag(),
        Err(SvgError::InvalidNumber)
    );
    assert_eq!(
        Numbers::new("").required_flag(),
        Err(SvgError::InvalidNumber)
    );
}

// --- single values -------------------------------------------------------

#[test]
fn a_lone_number_may_not_carry_trailing_text() {
    close(parse_number(" 42 ").expect("a number"), 42.0);
    assert_eq!(parse_number("42px"), Err(SvgError::InvalidNumber));
    assert_eq!(parse_number(""), Err(SvgError::InvalidNumber));
}

/// The shared scanner is C's, which also reads hexadecimal floats and the
/// `inf` / `nan` words; none of those is an SVG number.
#[test]
fn c_number_forms_that_are_not_svg_numbers_are_refused() {
    assert_eq!(parse_number("0x10"), Err(SvgError::InvalidNumber));
    assert_eq!(parse_number("inf"), Err(SvgError::InvalidNumber));
    assert_eq!(parse_number("nan"), Err(SvgError::InvalidNumber));
    assert_eq!(parse_number("-inf"), Err(SvgError::InvalidNumber));
}

/// An exponent large enough to overflow a double is not a coordinate any
/// asset means; it fails closed rather than becoming an infinity the
/// geometry would carry.
#[test]
fn an_overflowing_number_is_refused() {
    assert_eq!(parse_number("1e400"), Err(SvgError::InvalidNumber));
}

#[test]
fn lengths_convert_every_absolute_css_unit() {
    close(parse_length("10", 0.0).expect("user units"), 10.0);
    close(parse_length("10px", 0.0).expect("pixels"), 10.0);
    close(parse_length("1in", 0.0).expect("inches"), 96.0);
    close(parse_length("72pt", 0.0).expect("points"), 96.0);
    close(parse_length("1pc", 0.0).expect("picas"), 16.0);
    close(parse_length("25.4mm", 0.0).expect("millimetres"), 96.0);
    close(parse_length("2.54cm", 0.0).expect("centimetres"), 96.0);
}

#[test]
fn a_percentage_length_resolves_against_its_basis() {
    close(parse_length("50%", 200.0).expect("a percentage"), 100.0);
    close(parse_length("0%", 200.0).expect("a percentage"), 0.0);
}

/// A font-relative unit has no meaning without text, which this decoder does
/// not render, so it is refused rather than guessed at.
#[test]
fn an_unsupported_unit_is_refused() {
    assert_eq!(parse_length("2em", 0.0), Err(SvgError::InvalidNumber));
    assert_eq!(parse_length("2furlong", 0.0), Err(SvgError::InvalidNumber));
}

/// SVG clamps an out-of-range opacity rather than rejecting it, so only a
/// value that is not a number at all fails.
#[test]
fn opacity_clamps_instead_of_rejecting() {
    close(parse_opacity("0.5").expect("a half"), 0.5);
    close(parse_opacity("50%").expect("a half"), 0.5);
    close(parse_opacity("1.5").expect("over one"), 1.0);
    close(parse_opacity("-1").expect("under zero"), 0.0);
    assert_eq!(parse_opacity("opaque"), Err(SvgError::InvalidNumber));
}

#[test]
fn opacity_maps_onto_the_whole_alpha_range() {
    assert_eq!(opacity_to_alpha(0.0), 0);
    assert_eq!(opacity_to_alpha(1.0), 255);
    assert_eq!(opacity_to_alpha(0.5), 128);
    assert_eq!(opacity_to_alpha(2.0), 255);
    assert_eq!(opacity_to_alpha(-2.0), 0);
}
