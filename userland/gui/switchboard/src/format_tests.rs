//! Unit tests for the crate's one set of display formatters.

use super::{format_bytes, format_duration, format_rate};
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
