//! The grammar accepts exactly what the Help documents say it does.

extern crate std;

use std::vec::Vec;

use super::{parse, CliError, Launch, HELP_SWITCHES, REFERENCE_SCENE, USAGE};

#[test]
fn no_arguments_plays_a_new_world() {
    assert_eq!(parse(&[]), Ok(Launch::Play));
}

#[test]
fn the_reference_scene_is_asked_for_by_its_one_option() {
    assert_eq!(parse(&["--reference-scene"]), Ok(Launch::ReferenceScene));
}

/// The usage banner names, as a whole word, every switch the parser takes.
#[test]
fn the_usage_banner_names_every_switch() {
    let words: Vec<&str> = USAGE
        .split(|c: char| c.is_whitespace() || "[|]".contains(c))
        .collect();
    for switch in HELP_SWITCHES.iter().chain([&REFERENCE_SCENE]) {
        assert!(words.contains(switch), "the usage banner omits {switch}");
    }
}

/// A help switch wins wherever it is reached, as it does for every command
/// app and for GNU's: after the other option as much as before it.
#[test]
fn the_help_switches_win_where_they_are_reached() {
    for switch in HELP_SWITCHES {
        assert_eq!(parse(&[switch]), Ok(Launch::Help));
        assert_eq!(parse(&[switch, "--reference-scene"]), Ok(Launch::Help));
        assert_eq!(parse(&["--reference-scene", switch]), Ok(Launch::Help));
        assert_eq!(parse(&[switch, "--frob"]), Ok(Launch::Help));
    }
}

/// A flag given twice asks for the same thing twice, and `--` ends the
/// options without taking an operand.
#[test]
fn a_repeated_flag_and_the_end_of_options_are_accepted() {
    assert_eq!(
        parse(&["--reference-scene", "--reference-scene"]),
        Ok(Launch::ReferenceScene)
    );
    assert_eq!(parse(&["--"]), Ok(Launch::Play));
    assert_eq!(
        parse(&["--reference-scene", "--"]),
        Ok(Launch::ReferenceScene)
    );
}

/// No operands and no other options: a line outside the grammar is refused
/// whole rather than half-applied, including a help switch it only reaches
/// after the error.
#[test]
fn anything_else_is_a_usage_error() {
    for line in [
        &["--frob"][..],
        &["operand"],
        &["--frob", "-h"],
        &["--reference-scene=1"],
        &["--", "-h"],
        &["--", "operand"],
        &["--reference-scene", "operand"],
    ] {
        assert_eq!(parse(line), Err(CliError::Usage), "{line:?}");
    }
}

/// Every locale's `OPTIONS` documents the switches this parser accepts, read
/// from the bundle's own `Help/` tree — the one source the image plants.
#[test]
fn every_locale_documents_the_parser_switches() {
    use std::{format, fs};

    let keys = [
        format!("`{}`", HELP_SWITCHES.join(", ")),
        format!("`{REFERENCE_SCENE}`"),
    ];
    for locale in tairix_help::REQUIRED_LOCALES {
        let path = format!("{}/Help/{locale}/wintersun.md", env!("CARGO_MANIFEST_DIR"));
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for key in &keys {
            assert!(text.contains(key.as_str()), "{locale} must document {key}");
        }
    }
}
