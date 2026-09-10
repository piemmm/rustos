//! Canonical prefix-code assignment tests.

use super::{Canonical, MAX_CODE_BITS};

/// Decode `bits`, most significant first, as one canonical code, answering
/// the symbol index and the bits it consumed.
fn decode(table: &Canonical, bits: &[u8]) -> Option<(u32, usize)> {
    let mut walk = Canonical::walk();
    for bit in bits {
        if let Some(index) = walk.push(table, u32::from(*bit)) {
            return Some((index, walk.len()));
        }
    }
    None
}

#[test]
fn the_assignment_is_the_worked_example_from_the_jpeg_annex() {
    // One code of length two and two of length three: "00", "010", "011".
    let table = Canonical::build(&[0, 1, 2]).expect("a valid assignment");
    assert_eq!(decode(&table, &[0, 0]), Some((0, 2)));
    assert_eq!(decode(&table, &[0, 1, 0]), Some((1, 3)));
    assert_eq!(decode(&table, &[0, 1, 1]), Some((2, 3)));
    // "1..." is unassigned, so the code is incomplete.
    assert!(!table.complete());
    assert_eq!(decode(&table, &[1, 1, 1]), None);
}

#[test]
fn a_complete_code_reports_itself_complete() {
    let table = Canonical::build(&[0, 3, 2]).expect("a valid assignment");
    assert!(table.complete());
    assert_eq!(decode(&table, &[0, 0]), Some((0, 2)));
    assert_eq!(decode(&table, &[0, 1]), Some((1, 2)));
    assert_eq!(decode(&table, &[1, 0]), Some((2, 2)));
    assert_eq!(decode(&table, &[1, 1, 0]), Some((3, 3)));
    assert_eq!(decode(&table, &[1, 1, 1]), Some((4, 3)));
}

#[test]
fn a_single_one_bit_code_is_complete_only_with_both_codes_used() {
    assert!(!Canonical::build(&[1])
        .expect("one one-bit code is a valid assignment")
        .complete());
    assert!(Canonical::build(&[2])
        .expect("two one-bit codes are a valid assignment")
        .complete());
}

#[test]
fn an_oversubscribed_assignment_is_refused() {
    assert!(Canonical::build(&[3]).is_none());
    assert!(Canonical::build(&[1, 3]).is_none());
    assert!(Canonical::build(&[0, 0, 0, 0, 0, 0, 0, 257]).is_none());
    assert!(Canonical::build(&[0, 0, 0, 0, 0, 0, 0, 0, 257]).is_some());
}

#[test]
fn an_empty_assignment_holds_no_code_of_any_length() {
    let table = Canonical::build(&[]).expect("no codes at all is a valid assignment");
    assert!(!table.complete());
    for len in 0..=MAX_CODE_BITS {
        assert_eq!(table.run(len), None);
    }
}

#[test]
fn counts_longer_than_the_longest_code_are_refused() {
    assert!(Canonical::build(&[0u32; MAX_CODE_BITS]).is_some());
    assert!(Canonical::build(&[0u32; MAX_CODE_BITS + 1]).is_none());
}

#[test]
fn a_run_past_the_longest_code_is_absent() {
    let table = Canonical::build(&[2]).expect("a valid assignment");
    assert_eq!(table.run(MAX_CODE_BITS + 1), None);
}

#[test]
fn the_longest_permitted_code_assigns_and_walks() {
    let mut counts = [0u32; MAX_CODE_BITS];
    counts[MAX_CODE_BITS - 1] = 2;
    for count in counts.iter_mut().take(MAX_CODE_BITS - 1) {
        *count = 1;
    }
    let table = Canonical::build(&counts).expect("a valid assignment");
    assert!(table.complete());
    assert_eq!(
        table.symbols(),
        u32::try_from(MAX_CODE_BITS).unwrap_or(0) + 1
    );
    let run = table
        .run(MAX_CODE_BITS)
        .expect("codes at the longest length");
    assert_eq!(run.count, 2);
    // Every shorter length holds one code, all of them "1"-prefixed, so the
    // longest length's two codes are the all-ones patterns.
    let all_ones = [1u8; MAX_CODE_BITS];
    assert_eq!(
        decode(&table, &all_ones),
        Some((run.first_symbol + 1, MAX_CODE_BITS))
    );
}

#[test]
fn a_lone_symbol_is_reported_and_never_complete() {
    let table = Canonical::build(&[1]).expect("a valid assignment");
    assert!(table.lone_symbol());
    assert!(!table.complete());
    assert_eq!(table.symbols(), 1);
    assert!(!Canonical::build(&[2])
        .expect("a valid assignment")
        .lone_symbol());
}

#[test]
fn a_count_wider_than_the_table_holds_is_refused() {
    assert!(Canonical::build(&[0u32; 17]).is_none());
    let mut counts = [0u32; MAX_CODE_BITS];
    counts[MAX_CODE_BITS - 1] = u32::from(u16::MAX) + 1;
    assert!(Canonical::build(&counts).is_none());
}

#[test]
fn a_run_reports_where_its_symbols_start() {
    let table = Canonical::build(&[1, 1, 2]).expect("a valid assignment");
    assert_eq!(table.run(1).map(|run| run.first_symbol), Some(0));
    assert_eq!(table.run(2).map(|run| run.first_symbol), Some(1));
    assert_eq!(table.run(3).map(|run| run.first_symbol), Some(2));
    assert_eq!(table.run(3).map(|run| run.count), Some(2));
    assert_eq!(table.run(4), None);
}
