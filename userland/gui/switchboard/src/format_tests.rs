//! Unit tests for the crate's one set of display formatters.

use super::{
    byte_parts, format_bytes, format_duration, format_latency, format_pixels, format_rate, percent,
    pixel_parts, whole_percent,
};
use tairix_abi::Duration64;

#[test]
fn bytes_below_a_kibibyte_are_whole_bytes() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1023), "1023 B");
}

#[test]
fn bytes_scale_to_the_largest_unit_with_one_decimal() {
    assert_eq!(format_bytes(1024), "1.0 KiB");
    assert_eq!(format_bytes(655_360), "640.0 KiB");
    assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
}

#[test]
fn the_largest_unit_is_the_last_one_rather_than_a_wrap() {
    let pib = 1024u64 * 1024 * 1024 * 1024 * 1024;
    assert_eq!(format_bytes(pib), "1.0 PiB");
    assert!(
        format_bytes(u64::MAX).ends_with(" PiB"),
        "a count past the last unit stays in it rather than wrapping"
    );
}

#[test]
fn pixels_below_a_thousand_are_whole_pixels() {
    assert_eq!(format_pixels(0), "0 px");
    assert_eq!(format_pixels(512), "512 px");
    assert_eq!(format_pixels(999), "999 px");
}

#[test]
fn pixels_scale_by_thousands_with_one_decimal() {
    assert_eq!(format_pixels(1_000), "1.0k px");
    assert_eq!(format_pixels(3_200), "3.2k px");
    assert_eq!(format_pixels(1920 * 1080), "2.0M px");
}

#[test]
fn a_pixel_count_past_the_last_unit_stays_in_it() {
    assert_eq!(format_pixels(4_000_000_000), "4.0G px");
    assert!(
        format_pixels(u64::MAX).ends_with("G px"),
        "a count past the last unit stays in it rather than wrapping"
    );
}

#[test]
fn a_rate_is_a_byte_count_per_second() {
    assert_eq!(format_rate(0), "0 B/s");
    assert_eq!(format_rate(1024), "1.0 KiB/s");
}

#[test]
fn a_duration_drops_the_units_that_are_nought() {
    assert_eq!(format_duration(Duration64::from_secs(45)), "45s");
    assert_eq!(format_duration(Duration64::from_secs(600)), "10m");
    assert_eq!(format_duration(Duration64::from_secs(7_260)), "2h 1m");
    assert_eq!(format_duration(Duration64::from_secs(90_120)), "1d 1h 2m");
}

#[test]
fn a_negative_duration_reads_as_no_elapsed_time() {
    assert_eq!(
        format_duration(Duration64::from_secs(-5)),
        "0s",
        "a clock that moved backwards must not read as an enormous uptime"
    );
}

#[test]
fn a_latency_is_scaled_to_the_unit_that_keeps_it_readable() {
    assert_eq!(format_latency(0), "0 ns");
    assert_eq!(format_latency(999), "999 ns");
    assert_eq!(format_latency(125_000), "125.0 us");
    assert_eq!(format_latency(5_000_000_000), "5.0 s");
    // A figure beyond the last unit saturates in that unit rather than
    // wrapping to a smaller, misleading number.
    assert_eq!(format_latency(u64::MAX), "18446744073.7 s");
}

/// A hero's figure carries no unit of its own, because the hero draws the
/// unit beside it: spelled with a `%` the CPU pane would read `18% % busy`.
#[test]
fn a_whole_percent_carries_no_unit_and_the_spelled_form_adds_one() {
    assert_eq!(whole_percent(185), "18");
    assert_eq!(percent(185), "18%");
    assert_eq!(percent(0), "0%");
    // Over a hundred percent is legitimate on more than one core, and is
    // shown as measured rather than clamped.
    assert_eq!(whole_percent(2_400), "240");
}

/// The magnitude prefix belongs to the unit, so a hero's figure is the
/// mantissa alone and the joined spelling is unchanged by the split.
#[test]
fn pixel_parts_split_the_magnitude_into_the_unit() {
    assert_eq!(pixel_parts(512), ("512".into(), "px".into()));
    assert_eq!(pixel_parts(3_200), ("3.2".into(), "k px".into()));
    assert_eq!(pixel_parts(4_200_000), ("4.2".into(), "M px".into()));
    assert_eq!(pixel_parts(4_000_000_000), ("4.0".into(), "G px".into()));
    // Saturates in the last unit rather than wrapping to a smaller figure.
    assert_eq!(pixel_parts(u64::MAX).1, "G px");
}

/// A hero reads as one quantity, so the figure and the whole it is a share of
/// are scaled to the *whole's* unit. Scaling each independently spells a ratio
/// out of two numbers that are not comparable.
#[test]
fn byte_parts_scales_the_figure_to_the_whole_it_is_a_share_of() {
    let gib = 1024u64 * 1024 * 1024;
    assert_eq!(
        byte_parts(8 * gib + gib / 2, 16 * gib),
        (
            alloc::string::String::from("8.5"),
            alloc::string::String::from("/ 16.0 GiB")
        )
    );
    // Half a gibibyte of sixteen is not "512": the whole is in GiB, so the
    // figure is too.
    assert_eq!(
        byte_parts(gib / 2, 16 * gib),
        (
            alloc::string::String::from("0.5"),
            alloc::string::String::from("/ 16.0 GiB")
        )
    );
    // And the figure never carries a unit of its own — that is what put
    // "8.6 GiB" in the hero's large face beside a second unit.
    let (figure, _) = byte_parts(8 * gib, 16 * gib);
    assert!(
        !figure.contains("iB") && !figure.contains(' '),
        "the figure carried a unit: {figure}"
    );
}
