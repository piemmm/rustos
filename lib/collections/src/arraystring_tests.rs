//! Tests for [`ArrayString`]: the UTF-8 invariant, both push policies, and the
//! footprint counter.

use core::fmt::Write;
use core::mem::size_of;

use crate::{ArrayString, CapacityError};

#[test]
fn an_empty_string_reads_back_nothing() {
    let s: ArrayString<16> = ArrayString::new();
    assert!(s.is_empty());
    assert_eq!(s.len(), 0);
    assert_eq!(s.capacity(), 16);
    assert_eq!(s.remaining_capacity(), 16);
    assert_eq!(s.as_str(), "");
    assert_eq!(s.as_bytes(), b"");
}

#[test]
fn try_push_str_refuses_an_over_long_string_and_stores_nothing() {
    let mut s: ArrayString<8> = ArrayString::new();
    assert_eq!(s.try_push_str("abcd"), Ok(()));
    assert_eq!(s.try_push_str("efghi"), Err(CapacityError::new(())));
    assert_eq!(s.as_str(), "abcd", "the refusal stored no prefix");
    assert_eq!(s.try_push_str("efgh"), Ok(()));
    assert_eq!(s.as_str(), "abcdefgh");
    assert!(ArrayString::<3>::try_from("abcd").is_err());
    assert_eq!(
        ArrayString::<4>::try_from("abcd").expect("fits").as_str(),
        "abcd"
    );
}

#[test]
fn truncating_push_cuts_on_a_character_boundary() {
    // Seven ASCII bytes then a two-byte character that would straddle the
    // eighth: the character is dropped whole, never split.
    let mut s: ArrayString<8> = ArrayString::new();
    let remainder = s.push_str_truncating("aaaaaaaéz");
    assert_eq!(s.as_str(), "aaaaaaa");
    assert_eq!(s.len(), 7);
    assert_eq!(remainder, "éz", "what did not fit comes back");
    assert!(core::str::from_utf8(s.as_bytes()).is_ok());

    // A string that fits leaves no remainder.
    let mut fits: ArrayString<8> = ArrayString::new();
    assert_eq!(fits.push_str_truncating("abc"), "");
    assert_eq!(fits.as_str(), "abc");

    // A multi-byte character at the very start of a full string stores none of
    // it rather than half of it.
    let mut full: ArrayString<1> = ArrayString::new();
    assert_eq!(full.push_str_truncating("é"), "é");
    assert!(full.is_empty());

    assert_eq!(
        ArrayString::<4>::from_str_truncating("abcdef").as_str(),
        "abcd"
    );
}

#[test]
fn truncate_cuts_back_to_a_character_boundary() {
    let mut s: ArrayString<16> = ArrayString::new();
    assert_eq!(s.try_push_str("abécd"), Ok(()));
    assert_eq!(s.len(), 6);
    // Byte 3 lands inside the two-byte 'é', so the cut falls back to 2.
    s.truncate(3);
    assert_eq!(s.as_str(), "ab");
    // A truncation at or above the length changes nothing.
    s.truncate(9);
    assert_eq!(s.as_str(), "ab");
    s.clear();
    assert!(s.is_empty());
}

#[test]
fn characters_are_pushed_by_their_encoded_length() {
    let mut s: ArrayString<4> = ArrayString::new();
    assert_eq!(s.try_push('a'), Ok(()));
    assert_eq!(s.try_push('é'), Ok(()));
    assert_eq!(s.len(), 3);
    // A three-byte character needs more than the one byte left.
    assert_eq!(s.try_push('☃'), Err(CapacityError::new(())));
    assert_eq!(s.as_str(), "aé");
    assert_eq!(s.try_push('z'), Ok(()));
    assert_eq!(s.as_str(), "aéz");
}

#[test]
fn a_write_fragment_that_does_not_fit_is_refused_whole() {
    let mut s: ArrayString<8> = ArrayString::new();
    assert!(write!(&mut s, "{}-{}", 12, 34).is_ok());
    assert_eq!(s.as_str(), "12-34");
    // "678" needs three of the three bytes left; "6789" needs four.
    assert!(write!(&mut s, "{}", 6789).is_err());
    assert_eq!(s.as_str(), "12-34", "the refused fragment stored nothing");
    assert!(write!(&mut s, "{}", 678).is_ok());
    assert_eq!(s.as_str(), "12-34678");
}

#[test]
fn equality_ignores_the_residue_beyond_the_length() {
    let mut long: ArrayString<8> = ArrayString::new();
    assert_eq!(long.try_push_str("abcdefgh"), Ok(()));
    long.truncate(2);

    let mut short: ArrayString<8> = ArrayString::new();
    assert_eq!(short.try_push_str("ab"), Ok(()));

    assert_eq!(long, short, "only the live prefix is compared");
    assert_eq!(long, *"ab");
    assert!(long < ArrayString::<8>::from_str_truncating("b"));
}

#[test]
fn a_copy_is_independent_of_its_source() {
    let mut original: ArrayString<8> = ArrayString::from_str_truncating("abc");
    let taken = original;
    original.truncate(1);
    assert_eq!(original.as_str(), "a");
    assert_eq!(taken.as_str(), "abc", "a copy is unaffected");
}

/// The footprint gate: the bytes plus one length, with no heap block.
#[test]
fn the_footprint_is_the_bytes_plus_one_length() {
    assert_eq!(size_of::<ArrayString<120>>(), 120 + size_of::<usize>());
}
