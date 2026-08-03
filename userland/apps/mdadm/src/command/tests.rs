//! Parser tests: every option, the value grammar, `--`, help/version
//! precedence, and every fail-closed refusal.

extern crate std;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::raid::RaidLevel;

use super::{parse, Command, CreateArgs, ParseError};

/// Parse a borrowed slice of `&str` arguments.
fn p(args: &[&str]) -> Result<Command, ParseError> {
    parse(args)
}

fn devices(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

#[test]
fn help_and_version_short_and_long() {
    assert_eq!(p(&["-h"]), Ok(Command::Help));
    assert_eq!(p(&["-?"]), Ok(Command::Help));
    assert_eq!(p(&["--help"]), Ok(Command::Help));
    assert_eq!(p(&["-V"]), Ok(Command::Version));
    assert_eq!(p(&["--version"]), Ok(Command::Version));
}

#[test]
fn help_wins_over_version_and_modes_regardless_of_order() {
    assert_eq!(p(&["--detail", "--help"]), Ok(Command::Help));
    assert_eq!(p(&["--help", "--version"]), Ok(Command::Help));
    assert_eq!(p(&["-V", "--create"]), Ok(Command::Version));
}

#[test]
fn detail_takes_zero_or_one_array() {
    assert_eq!(p(&["-D"]), Ok(Command::Detail { array: None }));
    assert_eq!(p(&["--detail"]), Ok(Command::Detail { array: None }));
    assert_eq!(
        p(&["--detail", "3f2a"]),
        Ok(Command::Detail {
            array: Some(String::from("3f2a"))
        })
    );
    assert_eq!(
        p(&["-D", "a", "b"]),
        Err(ParseError::UnexpectedOperand(String::from("b")))
    );
}

#[test]
fn examine_takes_no_operand() {
    assert_eq!(p(&["-E"]), Ok(Command::Examine));
    assert_eq!(p(&["--examine"]), Ok(Command::Examine));
    assert_eq!(
        p(&["-E", "node:1"]),
        Err(ParseError::UnexpectedOperand(String::from("node:1")))
    );
}

#[test]
fn create_parses_level_count_and_devices() {
    assert_eq!(
        p(&[
            "--create",
            "--level=raid5",
            "--raid-devices=3",
            "node:1",
            "node:2",
            "node:3"
        ]),
        Ok(Command::Create(CreateArgs {
            level: RaidLevel::Parity,
            raid_devices: 3,
            chunk_blocks: None,
            devices: devices(&["node:1", "node:2", "node:3"]),
        }))
    );
}

#[test]
fn create_short_forms_and_clustered_value_flags() {
    // `-Cl0` clusters the create mode with the level value `0`.
    assert_eq!(
        p(&["-Cl0", "-n2", "-c128", "node:1", "node:2"]),
        Ok(Command::Create(CreateArgs {
            level: RaidLevel::Stripe,
            raid_devices: 2,
            chunk_blocks: Some(128),
            devices: devices(&["node:1", "node:2"]),
        }))
    );
}

#[test]
fn every_level_spelling_maps() {
    let cases = [
        ("0", RaidLevel::Stripe),
        ("raid0", RaidLevel::Stripe),
        ("stripe", RaidLevel::Stripe),
        ("1", RaidLevel::Mirror),
        ("mirror", RaidLevel::Mirror),
        ("5", RaidLevel::Parity),
        ("raid5", RaidLevel::Parity),
        ("6", RaidLevel::DualParity),
        ("10", RaidLevel::Raid10),
        ("tp", RaidLevel::TripleParity),
        ("raid-tp", RaidLevel::TripleParity),
    ];
    for (spelling, level) in cases {
        let args = [
            "--create",
            "--level",
            spelling,
            "--raid-devices",
            "2",
            "node:1",
            "node:2",
        ];
        match p(&args) {
            Ok(Command::Create(created)) => assert_eq!(created.level, level, "{spelling}"),
            other => panic!("{spelling}: {other:?}"),
        }
    }
}

#[test]
fn raid4_is_named_unsupported_not_merely_unknown() {
    assert_eq!(
        p(&["--create", "--level=4", "-n", "2", "node:1", "node:2"]),
        Err(ParseError::LevelNotSupported(String::from("4")))
    );
    assert_eq!(
        p(&["--create", "--level=raid4", "-n", "2", "node:1", "node:2"]),
        Err(ParseError::LevelNotSupported(String::from("raid4")))
    );
}

#[test]
fn a_nonsense_level_is_a_bad_level() {
    assert_eq!(
        p(&["-C", "-l", "purple", "-n", "2", "node:1", "node:2"]),
        Err(ParseError::BadLevel(String::from("purple")))
    );
}

#[test]
fn chunk_is_refused_for_a_non_striped_level() {
    assert_eq!(
        p(&["-C", "-l", "1", "-n", "2", "-c", "128", "node:1", "node:2"]),
        Err(ParseError::ChunkNotAllowed)
    );
}

#[test]
fn create_requires_level_count_and_devices() {
    assert_eq!(
        p(&["-C", "-n", "2", "node:1", "node:2"]),
        Err(ParseError::MissingLevel)
    );
    assert_eq!(
        p(&["-C", "-l", "1", "node:1", "node:2"]),
        Err(ParseError::MissingRaidDevices)
    );
    assert_eq!(
        p(&["-C", "-l", "1", "-n", "2"]),
        Err(ParseError::MissingOperand("device"))
    );
}

#[test]
fn create_device_count_must_match_raid_devices() {
    assert_eq!(
        p(&["-C", "-l", "raid5", "-n", "3", "node:1", "node:2"]),
        Err(ParseError::DeviceCountMismatch {
            expected: 3,
            got: 2
        })
    );
}

#[test]
fn a_zero_device_count_is_refused() {
    assert_eq!(
        p(&["-C", "-l", "1", "-n", "0", "node:1"]),
        Err(ParseError::BadRaidDevices(String::from("0")))
    );
}

#[test]
fn add_remove_stop_operand_counts() {
    assert_eq!(
        p(&["--add", "3f2a", "node:5"]),
        Ok(Command::Add {
            array: String::from("3f2a"),
            device: String::from("node:5"),
        })
    );
    assert_eq!(
        p(&["--remove", "3f2a", "node:5"]),
        Ok(Command::Remove {
            array: String::from("3f2a"),
            device: String::from("node:5"),
        })
    );
    assert_eq!(
        p(&["-S", "3f2a"]),
        Ok(Command::Stop {
            array: String::from("3f2a")
        })
    );
    assert_eq!(
        p(&["--add", "3f2a"]),
        Err(ParseError::MissingOperand("device"))
    );
    assert_eq!(
        p(&["--add", "a", "b", "c"]),
        Err(ParseError::UnexpectedOperand(String::from("c")))
    );
    assert_eq!(p(&["-S"]), Err(ParseError::MissingOperand("array")));
    assert_eq!(
        p(&["-S", "a", "b"]),
        Err(ParseError::UnexpectedOperand(String::from("b")))
    );
}

#[test]
fn end_of_options_makes_a_dash_prefixed_operand_positional() {
    assert_eq!(
        p(&["--stop", "--", "--weird"]),
        Ok(Command::Stop {
            array: String::from("--weird")
        })
    );
    assert_eq!(
        p(&["--detail", "--", "-abc"]),
        Ok(Command::Detail {
            array: Some(String::from("-abc"))
        })
    );
}

#[test]
fn conflicting_modes_are_refused() {
    assert_eq!(p(&["-C", "-D"]), Err(ParseError::ConflictingModes));
    assert_eq!(
        p(&["--create", "--stop"]),
        Err(ParseError::ConflictingModes)
    );
    // The same mode twice is fine.
    assert_eq!(p(&["-D", "-D"]), Ok(Command::Detail { array: None }));
}

#[test]
fn no_mode_is_refused() {
    assert_eq!(p(&[]), Err(ParseError::NoMode));
    assert_eq!(p(&["node:1"]), Err(ParseError::NoMode));
}

#[test]
fn unknown_options_are_refused() {
    assert_eq!(
        p(&["--frobnicate"]),
        Err(ParseError::UnknownOption(String::from("--frobnicate")))
    );
    assert_eq!(
        p(&["-Z"]),
        Err(ParseError::UnknownOption(String::from("-Z")))
    );
    assert_eq!(
        p(&["--create=now"]),
        Err(ParseError::UnknownOption(String::from("--create=now")))
    );
}

#[test]
fn a_value_option_without_a_value_is_refused() {
    assert_eq!(
        p(&["--create", "--level"]),
        Err(ParseError::MissingValue("--level"))
    );
    assert_eq!(
        p(&["-C", "-n"]),
        Err(ParseError::MissingValue("--raid-devices"))
    );
}

#[test]
fn a_value_option_in_the_wrong_mode_is_refused() {
    assert_eq!(
        p(&["-D", "-l", "5"]),
        Err(ParseError::OptionNotAllowed {
            option: "--level",
            mode: "--detail",
        })
    );
    assert_eq!(
        p(&["--stop", "3f2a", "--chunk=8"]),
        Err(ParseError::OptionNotAllowed {
            option: "--chunk",
            mode: "--stop",
        })
    );
}

/// Every locale's `OPTIONS` section documents exactly the switches this
/// parser accepts: the flag tokens are language-neutral, so each translated
/// document must carry the same keys as the canonical one. The documents are
/// read from the bundle's own on-disk `Help/` tree — the single source the
/// image builder plants — never a copy embedded in this crate.
#[test]
fn help_documents_the_parser_switches() {
    use std::fs;

    let help_root = std::format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    for locale in tairix_help::REQUIRED_LOCALES {
        let path = std::format!("{help_root}/{locale}/mdadm.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for switch in [
            "`-C, --create`",
            "`-D, --detail`",
            "`-E, --examine`",
            "`-a, --add`",
            "`-r, --remove`",
            "`-S, --stop`",
            "`-l, --level=<level>`",
            "`-n, --raid-devices=<count>`",
            "`-c, --chunk=<blocks>`",
            "`-h, -?, --help`",
            "`-V, --version`",
        ] {
            assert!(
                text.contains(switch),
                "{locale}/mdadm.md must document {switch}"
            );
        }
    }
}
