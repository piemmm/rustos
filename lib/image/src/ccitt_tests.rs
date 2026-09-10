//! Facsimile codec tests, over the module's own invariants: the run-length
//! tables are hand-transcribed from ITU-T T.4, so what is worth checking
//! directly is that they are a code at all — prefix-free, complete, and
//! resolving each declared run to itself. The coded pictures those tables
//! decode are exercised through the TIFF door.

use alloc::vec::Vec;

use super::{
    fill, Bits, Codes, BLACK_CODES, EXTENDED_CODES, LENGTH_BITS, LENGTH_MASK, LOOKUP_BITS,
    WHITE_CODES,
};

/// One colour's codes, with the extended makeups both colours share.
fn table(colour: &[(u16, u8, u16)]) -> Vec<(u16, u8, u16)> {
    colour.iter().chain(EXTENDED_CODES).copied().collect()
}

#[test]
fn each_run_length_table_is_prefix_free() {
    for codes in [WHITE_CODES, BLACK_CODES] {
        let codes = table(codes);
        for (first, &(code, len, run)) in codes.iter().enumerate() {
            assert!(
                len > 0 && u32::from(len) <= LOOKUP_BITS,
                "run {run} is unreadable"
            );
            for (second, &(other, other_len, other_run)) in codes.iter().enumerate() {
                if first == second {
                    continue;
                }
                let (short, long, shift) = if len <= other_len {
                    (code, other, other_len - len)
                } else {
                    (other, code, len - other_len)
                };
                assert_ne!(
                    long >> shift,
                    short,
                    "the codes for runs {run} and {other_run} share a prefix"
                );
            }
        }
    }
}

#[test]
fn each_table_holds_exactly_the_runs_the_specification_lists() {
    let expected: Vec<u16> = (0..64)
        .chain((1..=27).map(|step| step * 64))
        .chain((28..=40).map(|step| step * 64))
        .collect();
    for codes in [WHITE_CODES, BLACK_CODES] {
        let runs: Vec<u16> = table(codes).iter().map(|(_, _, run)| *run).collect();
        assert_eq!(runs, expected);
    }
}

#[test]
fn the_lookup_resolves_each_declared_code_to_its_own_run() {
    let codes = Codes::new().expect("the tables fit in memory");
    for (black, declared) in [(false, WHITE_CODES), (true, BLACK_CODES)] {
        for &(code, len, run) in &table(declared) {
            // Every lookahead the code prefixes must resolve to it, so the
            // first and last of them are what a table error would part.
            let shift = LOOKUP_BITS - u32::from(len);
            for tail in [0u32, (1 << shift) - 1] {
                let at = (u32::from(code) << shift | tail) as usize;
                let entry = codes.table(black)[at];
                assert_eq!(u32::from(entry & LENGTH_MASK), u32::from(len), "run {run}");
                assert_eq!(entry >> LENGTH_BITS, run, "run {run}");
            }
        }
    }
}

#[test]
fn a_filled_run_sets_exactly_its_own_bits() {
    for (from, to, expected) in [
        (0u32, 0u32, [0x00u8, 0x00, 0x00]),
        (0, 1, [0x80, 0x00, 0x00]),
        (3, 5, [0x18, 0x00, 0x00]),
        (0, 8, [0xFF, 0x00, 0x00]),
        (4, 20, [0x0F, 0xFF, 0xF0]),
        (0, 24, [0xFF, 0xFF, 0xFF]),
    ] {
        let mut row = [0u8; 3];
        fill(&mut row, from, to);
        assert_eq!(row, expected, "{from}..{to}");
    }
}

#[test]
fn a_fill_past_the_row_stops_at_its_end() {
    let mut row = [0u8; 1];
    fill(&mut row, 4, 32);
    assert_eq!(row, [0x0F]);
}

#[test]
fn an_end_of_line_is_consumed_with_whatever_fill_precedes_it() {
    // Four fill bits, then the twelve-bit end-of-line code.
    let mut bits = Bits::new(&[0b0000_0000, 0b0000_0000, 0b1000_0000], false);
    assert!(bits.take_eol());
    assert_eq!(bits.remaining(), 7);
}

#[test]
fn a_short_run_of_zeros_is_not_an_end_of_line() {
    let mut bits = Bits::new(&[0b0000_0010], false);
    assert!(!bits.take_eol());
    // Nothing was consumed, so the code that follows is still readable.
    assert_eq!(bits.remaining(), 8);
}

#[test]
fn a_run_of_zeros_that_never_ends_is_not_an_end_of_line() {
    let mut bits = Bits::new(&[0u8; 4], false);
    assert!(!bits.take_eol());
    assert_eq!(bits.remaining(), 32);
}

#[test]
fn a_least_significant_first_reader_takes_a_bytes_bits_in_reverse() {
    let forward = Bits::new(&[0b1011_0000], false);
    let reversed = Bits::new(&[0b0000_1101], true);
    assert_eq!(forward.peek(4), reversed.peek(4));
    assert_eq!(forward.peek(4), 0b1011);
}
