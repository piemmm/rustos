//! Host tests for the file manager's command line.
//!
//! Every decision the running program makes about its argument vector is a
//! pure function of that vector, so all of it is exercised here without a
//! kernel: which spellings open a folder, which are turned down and with what
//! reason, and which are a usage error refused outright.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::{parse, unlistable_reason, Command, Start, UsageError, USAGE};

/// The [`Start`] a command line produced, or the test's own failure text when
/// it was a usage error or the short help.
fn start_of(args: &[&str]) -> Start {
    match parse(args) {
        Ok(Command::Open(start)) => start,
        other => panic!("expected an open command, got {other:?}"),
    }
}

/// The accepted starting location of a command line, as owned components.
fn location_of(args: &[&str]) -> Option<Vec<String>> {
    start_of(args).location
}

/// The refusal a command line stated, if any.
fn refusal_of(args: &[&str]) -> Option<String> {
    start_of(args).refused
}

#[test]
fn an_absolute_directory_is_accepted_as_the_starting_location() {
    assert_eq!(
        location_of(&["/Users/ada/Documents"]),
        Some(vec![
            "Users".to_string(),
            "ada".to_string(),
            "Documents".to_string(),
        ])
    );
    assert_eq!(refusal_of(&["/Users/ada/Documents"]), None);
}

#[test]
fn redundant_separators_and_a_bare_root_still_name_a_real_place() {
    // The shared path grammar collapses leading, trailing, and repeated
    // separators, so the same directory spelled loosely opens the same place.
    assert_eq!(
        location_of(&["//Users//ada//"]),
        Some(vec!["Users".to_string(), "ada".to_string()])
    );
    // The bare root view is a location like any other: no components, and no
    // refusal — it is not confused with "no argument".
    let root = start_of(&["/"]);
    assert_eq!(root.location, Some(Vec::new()));
    assert_eq!(root.refused, None);
}

#[test]
fn no_argument_opens_the_home_directory_without_complaint() {
    let start = start_of(&[]);
    assert_eq!(start.location, None);
    assert_eq!(start.refused, None);
    assert_eq!(start, Start::default());
}

#[test]
fn a_traversal_argument_is_refused_and_falls_back_to_home() {
    let start = start_of(&["/Users/ada/../../System/Security"]);
    assert_eq!(start.location, None);
    let refused = start.refused.expect("a traversal states its reason");
    assert!(
        refused.contains("\"..\""),
        "names the offending segment: {refused}"
    );
    assert!(
        refused.contains("opening the home directory instead"),
        "states the fallback: {refused}"
    );
    // A lone `.` is refused by the same rule, so a spelling can never mean a
    // different directory than it reads as.
    assert_eq!(location_of(&["/Users/./ada"]), None);
}

#[test]
fn an_over_long_argument_is_refused_before_it_is_parsed() {
    // One byte past the kernel's own path bound. The refusal never echoes the
    // spelling back: a hostile argument is not replayed at the error stream.
    let mut spelling = String::from("/");
    while spelling.len() <= tairix_abi::fs::FS_PATH_MAX {
        spelling.push('a');
    }
    let start = start_of(&[spelling.as_str()]);
    assert_eq!(start.location, None);
    let refused = start
        .refused
        .expect("an over-long location states its reason");
    assert!(
        refused.contains("longer than"),
        "states the bound: {refused}"
    );
    assert!(
        !refused.contains(&spelling),
        "does not echo the argument back"
    );
}

#[test]
fn an_over_long_single_component_is_refused() {
    // Within the whole-path bound, but one name past the per-name bound: the
    // shared filename rule, not a second copy of it here, is what refuses it.
    let mut spelling = String::from("/");
    for _ in 0..=tairix_path::MAX_COMPONENT_LEN {
        spelling.push('a');
    }
    let start = start_of(&[spelling.as_str()]);
    assert_eq!(start.location, None);
    assert!(start.refused.is_some());
}

#[test]
fn a_relative_or_alias_rooted_argument_is_refused() {
    for spelling in ["Users/ada", "", "-", "Home:/Documents"] {
        let start = start_of(&[spelling]);
        assert_eq!(start.location, None, "{spelling:?} must not be accepted");
        assert!(
            start
                .refused
                .as_deref()
                .is_some_and(|refused| refused.contains("opening the home directory instead")),
            "{spelling:?} must state the fallback"
        );
    }
    // A relative spelling says exactly why it was turned down.
    let refused = refusal_of(&["Users/ada"]).expect("a relative path states its reason");
    assert!(refused.contains("not an absolute path"), "{refused}");
}

#[test]
fn a_control_character_in_the_argument_is_refused_and_escaped_when_stated() {
    let refused = refusal_of(&["/Users/a\u{1b}[2Jb"]).expect("a control character is refused");
    assert!(
        !refused.contains('\u{1b}'),
        "the escape must not be replayed at the error stream: {refused}"
    );
    assert!(refused.contains("\\u{1b}"), "{refused}");
}

#[test]
fn a_second_operand_is_refused_rather_than_ignored() {
    assert_eq!(
        parse(&["/Users/ada", "/System"]),
        Err(UsageError::ExtraOperand("/System".to_string()))
    );
    assert_eq!(
        UsageError::ExtraOperand("/System".to_string()).to_string(),
        "extra operand \"/System\""
    );
}

#[test]
fn an_unrecognised_option_is_refused() {
    assert_eq!(
        parse(&["--grid"]),
        Err(UsageError::UnknownOption("--grid".to_string()))
    );
    assert_eq!(
        parse(&["-x", "/Users/ada"]),
        Err(UsageError::UnknownOption("-x".to_string()))
    );
    assert_eq!(
        UsageError::UnknownOption("-x".to_string()).to_string(),
        "unrecognized option \"-x\""
    );
    assert_eq!(
        UsageError::NotUtf8.to_string(),
        "argument vector is not valid UTF-8"
    );
}

#[test]
fn the_reserved_short_help_switches_win_wherever_they_appear() {
    for args in [
        &["-h"][..],
        &["-?"][..],
        &["--help"][..],
        &["/Users/ada", "-h"][..],
        &["-h", "--not-an-option"][..],
    ] {
        assert_eq!(parse(args), Ok(Command::Help), "{args:?}");
    }
    // The banner names the operand and the switches, so a usage refusal tells
    // the caller the whole grammar.
    assert!(USAGE.contains("[directory]"));
    assert!(USAGE.contains("-h"));
}

#[test]
fn end_of_options_lets_a_dash_leading_directory_through() {
    // After `--` the next argument is an operand, even one shaped like an
    // option — the short-help switch no longer wins — and it is still
    // validated as a path, so this one is refused rather than opened.
    let after_end = start_of(&["--", "-h"]);
    assert_eq!(after_end.location, None);
    assert!(after_end.refused.is_some());
    assert_eq!(
        location_of(&["--", "/Users/-h"]),
        Some(vec!["Users".to_string(), "-h".to_string()])
    );
    // `--` does not become an operand itself, so a second one is not an extra
    // operand — but a real second operand after it still is.
    assert_eq!(
        parse(&["--", "/a", "/b"]),
        Err(UsageError::ExtraOperand("/b".to_string()))
    );
}

#[test]
fn an_unlistable_location_states_the_place_and_the_fallback() {
    let reason = unlistable_reason(&["Users".to_string(), "ada".to_string()]);
    assert_eq!(
        reason,
        "could not list /Users/ada; opening the home directory instead"
    );
    // The root view spells as `/` rather than as nothing.
    assert_eq!(
        unlistable_reason(&[]),
        "could not list /; opening the home directory instead"
    );
}
