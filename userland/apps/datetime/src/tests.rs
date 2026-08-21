//! Host tests for the Date & Time engine: seeding, per-field validation,
//! the composed instant, and the status line.

use super::{
    days_in_month, is_leap_year, Editor, Fault, Field, Status, FIELD_MAX, YEAR_MAX, YEAR_MIN,
};
use tairix_abi::time::{Time64, WallClockReading, WallTimeState};

/// An editor holding exactly these six field values.
fn editor(year: &str, month: &str, day: &str, hour: &str, minute: &str, second: &str) -> Editor {
    let mut edit = Editor::new();
    edit.set_text(Field::Year, year);
    edit.set_text(Field::Month, month);
    edit.set_text(Field::Day, day);
    edit.set_text(Field::Hour, hour);
    edit.set_text(Field::Minute, minute);
    edit.set_text(Field::Second, second);
    edit
}

/// A set reading at `secs` past the epoch.
fn reading(secs: i64) -> WallClockReading {
    WallClockReading::new(instant(secs), WallTimeState::Adjusted)
}

/// A whole-second instant, which is always representable.
fn instant(secs: i64) -> Time64 {
    Time64::new(secs, 0).expect("whole seconds are representable")
}

#[test]
fn a_set_reading_seeds_every_field() {
    let mut edit = Editor::new();
    // 2024-02-29T12:34:56Z — a leap day, so the seed exercises the calendar
    // rather than a date every month has.
    edit.seed(reading(1_709_210_096));
    assert_eq!(edit.text(Field::Year), "2024");
    assert_eq!(edit.text(Field::Month), "2");
    assert_eq!(edit.text(Field::Day), "29");
    assert_eq!(edit.text(Field::Hour), "12");
    assert_eq!(edit.text(Field::Minute), "34");
    assert_eq!(edit.text(Field::Second), "56");
    assert_eq!(*edit.status(), Status::Idle);
}

#[test]
fn an_unset_clock_seeds_nothing_and_says_so() {
    let mut edit = editor("1999", "9", "9", "9", "9", "9");
    edit.seed(WallClockReading::new(instant(0), WallTimeState::Unset));
    for field in Field::ALL {
        assert!(edit.text(field).is_empty(), "{field:?} kept a reading");
    }
    assert_eq!(*edit.status(), Status::Unset);
    // And nothing can be committed from it, so an unset clock cannot be
    // "set" to the epoch by pressing the button.
    assert_eq!(edit.compose(), Err(Fault::Missing(Field::Year)));
}

#[test]
fn a_seeded_reading_composes_back_to_the_same_instant() {
    // The decomposition and the composition are inverses, so a seed followed
    // by a commit must not move the clock.
    for secs in [
        0,
        1_709_210_096,   // 2024-02-29T12:34:56Z
        -2_208_988_800,  // 1900-01-01T00:00:00Z, before the epoch
        4_102_444_800,   // 2100-01-01T00:00:00Z, past 2038
        253_402_300_799, // 9999-12-31T23:59:59Z, the widest year accepted
    ] {
        let mut edit = Editor::new();
        edit.seed(reading(secs));
        assert_eq!(
            edit.compose().map(|t| t.secs()),
            Ok(secs),
            "round trip moved the instant for {secs}"
        );
    }
}

#[test]
fn dates_before_1970_and_after_2038_are_ordinary_input() {
    // Neither is a boundary: the instant is 64-bit.
    assert_eq!(
        editor("1900", "1", "1", "0", "0", "0")
            .compose()
            .map(|t| t.secs()),
        Ok(-2_208_988_800)
    );
    // Exactly 2^32 seconds: the unsigned 32-bit wrap point, and here just a
    // number.
    assert_eq!(
        editor("2106", "2", "7", "6", "28", "16")
            .compose()
            .map(|t| t.secs()),
        Ok(4_294_967_296)
    );
    // Past the 32-bit signed second boundary (2038-01-19T03:14:07Z) by one.
    assert_eq!(
        editor("2038", "1", "19", "3", "14", "8")
            .compose()
            .map(|t| t.secs()),
        Ok(2_147_483_648)
    );
}

#[test]
fn a_year_before_the_common_era_composes_a_negative_instant() {
    let composed = editor("-1", "12", "31", "23", "59", "59")
        .compose()
        .expect("a proleptic date composes");
    assert!(composed.secs() < 0);
    assert_eq!(composed.subsec_nanos(), 0);
}

#[test]
fn the_composed_instant_carries_no_invented_sub_second_part() {
    let composed = editor("2024", "1", "1", "0", "0", "0")
        .compose()
        .expect("composes");
    assert_eq!(composed.subsec_nanos(), 0);
}

#[test]
fn an_empty_field_is_missing_not_zero() {
    for field in Field::ALL {
        let mut edit = editor("2024", "6", "15", "10", "30", "0");
        edit.set_text(field, "");
        assert_eq!(
            edit.compose(),
            Err(Fault::Missing(field)),
            "{field:?} was treated as a value"
        );
    }
}

#[test]
fn each_out_of_range_field_is_refused_and_named() {
    for (field, text) in [
        (Field::Month, "13"),
        (Field::Month, "0"),
        (Field::Day, "32"),
        (Field::Day, "0"),
        (Field::Hour, "24"),
        (Field::Minute, "60"),
        (Field::Second, "60"),
    ] {
        let mut edit = editor("2024", "6", "15", "10", "30", "0");
        edit.set_text(field, text);
        assert_eq!(
            edit.compose(),
            Err(Fault::OutOfRange(field)),
            "{field:?} accepted {text}"
        );
    }
}

#[test]
fn a_year_past_the_accepted_range_is_refused_rather_than_clamped() {
    let mut edit = editor("2024", "6", "15", "10", "30", "0");
    // Typed beyond the field bound, so the value cannot even be entered —
    // the field keeps only what it accepts.
    edit.set_text(Field::Year, "123456");
    assert_eq!(edit.text(Field::Year), "12345");
    assert_eq!(edit.compose(), Err(Fault::OutOfRange(Field::Year)));
    // The refusal is the year bound's, not the field bound's: what fits in
    // the field is still wider than what the calendar accepts.
    const { assert!(YEAR_MAX < 12_345 && YEAR_MIN > -12_345) };
}

#[test]
fn a_day_that_does_not_exist_in_its_month_is_refused() {
    // 31 April, 30 February, and 29 February outside a leap year.
    for (year, month, day) in [
        ("2024", "4", "31"),
        ("2024", "2", "30"),
        ("2023", "2", "29"),
    ] {
        assert_eq!(
            editor(year, month, day, "0", "0", "0").compose(),
            Err(Fault::NoSuchDay),
            "{year}-{month}-{day} was accepted"
        );
    }
    // The leap day itself is accepted, in a leap year.
    assert!(editor("2024", "2", "29", "0", "0", "0").compose().is_ok());
    // 2000 is a leap year (divisible by 400) and 1900 is not (by 100).
    assert!(editor("2000", "2", "29", "0", "0", "0").compose().is_ok());
    assert_eq!(
        editor("1900", "2", "29", "0", "0", "0").compose(),
        Err(Fault::NoSuchDay)
    );
}

#[test]
fn the_first_fault_in_field_order_is_the_one_reported() {
    // Both the month and the hour are wrong; the month is asked about first,
    // so the whole edit is refused there and nothing is composed.
    let edit = editor("2024", "13", "15", "99", "30", "0");
    assert_eq!(edit.compose(), Err(Fault::OutOfRange(Field::Month)));
}

#[test]
fn a_field_keeps_only_the_characters_a_civil_value_can_have() {
    let mut edit = Editor::new();
    edit.set_text(Field::Month, "1a2!");
    assert_eq!(edit.text(Field::Month), "12");
    // The sign belongs to the year alone, and only in the leading position.
    edit.set_text(Field::Month, "-5");
    assert_eq!(edit.text(Field::Month), "5");
    edit.set_text(Field::Year, "-44");
    assert_eq!(edit.text(Field::Year), "-44");
    edit.set_text(Field::Year, "4-4");
    assert_eq!(edit.text(Field::Year), "44");
    // So a field can never hold something the commit would call not-a-number.
    edit.set_text(Field::Day, "??");
    assert_eq!(edit.compose(), Err(Fault::Missing(Field::Day)));
}

#[test]
fn typing_is_bounded_and_backspace_undoes_it() {
    let mut edit = Editor::new();
    for _ in 0..FIELD_MAX + 3 {
        edit.push(Field::Year, '9');
    }
    assert_eq!(edit.text(Field::Year).chars().count(), FIELD_MAX);
    edit.backspace(Field::Year);
    assert_eq!(edit.text(Field::Year).chars().count(), FIELD_MAX - 1);
    // A rejected character never lands, so the bound is not spent on it.
    let mut edit = Editor::new();
    edit.push(Field::Minute, 'x');
    edit.push(Field::Minute, '7');
    assert_eq!(edit.text(Field::Minute), "7");
    // A sign is only a leading character, and only on the year.
    let mut edit = Editor::new();
    edit.push(Field::Year, '1');
    edit.push(Field::Year, '-');
    assert_eq!(edit.text(Field::Year), "1");
    // Backspacing an empty field is a no-op, never a panic.
    let mut edit = Editor::new();
    edit.backspace(Field::Second);
    assert!(edit.text(Field::Second).is_empty());
}

#[test]
fn the_field_table_defines_the_order_and_the_tab_cycle() {
    assert_eq!(Field::ALL.len(), 6);
    for (index, field) in Field::ALL.iter().enumerate() {
        assert_eq!(field.index(), index);
        assert_eq!(Field::at(index), Some(*field));
        assert!(!field.label().is_empty());
    }
    // Past the end names no field rather than the nearest one.
    assert_eq!(Field::at(Field::ALL.len()), None);
    // Tabbing visits every field once and returns to the first.
    let mut seen = alloc::vec::Vec::new();
    let mut at = Field::Year;
    for _ in 0..Field::ALL.len() {
        seen.push(at);
        at = at.next();
    }
    assert_eq!(seen.as_slice(), Field::ALL.as_slice());
    assert_eq!(at, Field::Year);
}

#[test]
fn focus_starts_at_the_year_and_moves_where_it_is_put() {
    let mut edit = Editor::new();
    assert_eq!(edit.focus(), Field::Year);
    edit.set_focus(Field::Hour);
    assert_eq!(edit.focus(), Field::Hour);
    edit.focus_next();
    assert_eq!(edit.focus(), Field::Minute);
}

#[test]
fn every_fault_names_its_field_and_states_a_reason() {
    for field in Field::ALL {
        for fault in [
            Fault::Missing(field),
            Fault::NotANumber(field),
            Fault::OutOfRange(field),
        ] {
            assert_eq!(fault.field(), field);
            assert!(!fault.message().is_empty());
        }
    }
    assert_eq!(Fault::NoSuchDay.field(), Field::Day);
    assert!(!Fault::NoSuchDay.message().is_empty());
}

#[test]
fn every_status_but_idle_states_something_and_marks_faults() {
    assert_eq!(Status::Idle.message(), None);
    assert!(!Status::Idle.is_fault());
    assert!(!Status::Applied.is_fault());
    assert!(Status::Applied.message().is_some());
    for status in [
        Status::Unset,
        Status::Denied,
        Status::Rejected(Fault::NoSuchDay),
        Status::Failed("the clock could not be read"),
    ] {
        assert!(status.message().is_some(), "{status:?} says nothing");
        assert!(status.is_fault(), "{status:?} is not marked a fault");
    }
}

#[test]
fn a_refusal_is_stated_and_the_fields_are_left_as_typed() {
    let mut edit = editor("2024", "2", "30", "0", "0", "0");
    let fault = edit.compose().expect_err("30 February does not exist");
    edit.set_status(Status::Rejected(fault));
    assert_eq!(*edit.status(), Status::Rejected(Fault::NoSuchDay));
    // Nothing was corrected behind the user's back.
    assert_eq!(edit.text(Field::Day), "30");
}

#[test]
fn month_lengths_follow_the_leap_rule() {
    assert_eq!(days_in_month(2024, 2), 29);
    assert_eq!(days_in_month(2023, 2), 28);
    assert_eq!(days_in_month(2000, 2), 29);
    assert_eq!(days_in_month(1900, 2), 28);
    for month in [1, 3, 5, 7, 8, 10, 12] {
        assert_eq!(days_in_month(2024, month), 31);
    }
    for month in [4, 6, 9, 11] {
        assert_eq!(days_in_month(2024, month), 30);
    }
    // A month outside the range admits no day at all rather than inventing a
    // length; the commit path range-checks before ever asking.
    assert_eq!(days_in_month(2024, 0), 0);
    assert_eq!(days_in_month(2024, 13), 0);
}

#[test]
fn the_leap_rule_holds_for_centuries_and_before_the_epoch() {
    assert!(is_leap_year(2000));
    assert!(!is_leap_year(1900));
    assert!(is_leap_year(1600));
    assert!(is_leap_year(-4));
    assert!(!is_leap_year(-1));
}
