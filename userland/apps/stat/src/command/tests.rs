//! Unit tests for `stat`'s option grammar and format grammar.

use super::*;
use alloc::vec;

fn describe(options: Options, paths: &[&str]) -> Command {
    Command::Describe {
        options,
        paths: paths.iter().map(|&p| String::from(p)).collect(),
    }
}

/// The options a bare `-c FORMAT` run parses to.
fn formatted(text: &str, trailer: Trailer, subject: Subject) -> Options {
    Options {
        subject,
        format: Some((
            parse_format(text, trailer, subject).expect("format parses"),
            trailer,
        )),
        ..Options::DEFAULT
    }
}

#[test]
fn no_operand_is_a_missing_operand() {
    assert_eq!(parse(&[]), Err(StatError::MissingOperand));
    assert_eq!(parse(&["-L"]), Err(StatError::MissingOperand));
}

#[test]
fn the_help_switches_are_the_reserved_pair() {
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
}

#[test]
fn switches_and_clusters_parse() {
    let options = Options {
        subject: Subject::Filesystem,
        dereference: true,
        terse: true,
        ..Options::DEFAULT
    };
    assert_eq!(parse(&["-Lft", "a"]), Ok(describe(options.clone(), &["a"])));
    assert_eq!(
        parse(&["--dereference", "--file-system", "--terse", "a"]),
        Ok(describe(options, &["a"]))
    );
}

#[test]
fn an_unknown_option_is_usage() {
    assert_eq!(parse(&["-Q", "a"]), Err(StatError::Usage));
    assert_eq!(parse(&["--nonsense", "a"]), Err(StatError::Usage));
    // A value on a switch that takes none is a mistake, not a token to drop.
    assert_eq!(parse(&["--terse=1", "a"]), Err(StatError::Usage));
}

#[test]
fn double_dash_ends_options_and_a_bare_dash_is_a_name() {
    assert_eq!(
        parse(&["--", "-L"]),
        Ok(describe(Options::DEFAULT, &["-L"]))
    );
    assert_eq!(parse(&["-"]), Ok(describe(Options::DEFAULT, &["-"])));
}

#[test]
fn the_two_format_switches_differ_only_in_trailer_and_escapes() {
    // `-c` appends a newline and leaves a backslash as typed; `--printf`
    // interprets the escape and appends nothing.
    let by_c = parse(&["-c", "%s\\n", "a"]).expect("parse -c");
    let by_printf = parse(&["--printf=%s\\n", "a"]).expect("parse --printf");
    let Command::Describe { options, .. } = by_c else {
        panic!("a format run describes")
    };
    assert_eq!(
        options.format,
        Some((
            vec![
                Piece::Field('s', Pad::default()),
                Piece::Text(String::from("\\n"))
            ],
            Trailer::Newline
        ))
    );
    let Command::Describe { options, .. } = by_printf else {
        panic!("a format run describes")
    };
    assert_eq!(
        options.format,
        Some((
            vec![
                Piece::Field('s', Pad::default()),
                Piece::Text(String::from("\n"))
            ],
            Trailer::None
        ))
    );
}

#[test]
fn a_format_value_may_be_inline_or_the_next_argument() {
    let inline = parse(&["-c%s", "a"]).expect("inline");
    let separate = parse(&["-c", "%s", "a"]).expect("separate");
    assert_eq!(inline, separate);
    assert_eq!(
        inline,
        describe(formatted("%s", Trailer::Newline, Subject::File), &["a"])
    );
    // `-c` with nothing after it has no value to take.
    assert_eq!(parse(&["-c"]), Err(StatError::Usage));
}

#[test]
fn the_last_format_switch_wins() {
    let last = parse(&["-c%s", "--printf=%i", "a"]).expect("parse");
    assert_eq!(
        last,
        describe(formatted("%i", Trailer::None, Subject::File), &["a"])
    );
}

#[test]
fn a_literal_percent_is_doubled_or_trailing() {
    let pieces = parse_format("%%x%", Trailer::None, Subject::File).expect("parses");
    assert_eq!(pieces, vec![Piece::Text(String::from("%x%"))]);
}

#[test]
fn an_unknown_specifier_is_named() {
    assert_eq!(parse(&["-c%Q", "a"]), Err(StatError::UnknownSpecifier('Q')));
    // The two vocabularies differ: `%c` is a filesystem field only, and
    // `%A` a file field only.
    assert_eq!(parse(&["-c%c", "a"]), Err(StatError::UnknownSpecifier('c')));
    assert_eq!(
        parse(&["-f", "-c%A", "a"]),
        Err(StatError::UnknownSpecifier('A'))
    );
}

#[test]
fn each_unserviceable_specifier_names_itself_and_its_reason() {
    // Refused when the format is parsed — before any path is touched — so a
    // format the platform cannot serve never half-renders.
    for (args, letter) in [
        (vec!["-c%G", "a"], 'G'),
        (vec!["-c%t", "a"], 't'),
        (vec!["-c%T", "a"], 'T'),
        (vec!["-f", "-c%t", "a"], 't'),
    ] {
        match parse(&args) {
            Err(StatError::Unsupported(named, reason)) => {
                assert_eq!(named, letter, "{args:?}");
                assert!(!reason.is_empty(), "{args:?} states a reason");
            }
            other => panic!("{args:?} must be refused, got {other:?}"),
        }
    }
    // `%T` *is* served in the filesystem vocabulary, where it names the
    // type the mount records.
    assert!(parse(&["-f", "-c%T", "a"]).is_ok());
}

#[test]
fn the_subject_is_read_before_the_format_is_checked() {
    // `-f` after the format still selects the filesystem vocabulary: the
    // format is parsed once, after the whole command line.
    assert!(parse(&["-c%c", "-f", "a"]).is_ok());
}

#[test]
fn printf_resolves_the_escapes_gnu_names() {
    let pieces = parse_format("\\t\\n\\\\\\a", Trailer::None, Subject::File).expect("parses");
    assert_eq!(pieces, vec![Piece::Text(String::from("\t\n\\\u{7}"))]);
    // An unknown escape keeps its own letter rather than being guessed at.
    let kept = parse_format("\\q", Trailer::None, Subject::File).expect("parses");
    assert_eq!(kept, vec![Piece::Text(String::from("q"))]);
}

#[test]
fn help_documents_the_parser_switches() {
    extern crate std;
    use alloc::format;
    use std::fs;

    let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    for locale in tairix_help::REQUIRED_LOCALES {
        let path = format!("{help_root}/{locale}/stat.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for switch in [
            "`-L, --dereference`",
            "`-f, --file-system`",
            "`-c, --format=FORMAT`",
            "`--printf=FORMAT`",
            "`-t, --terse`",
            "`-?, --help`",
        ] {
            assert!(
                text.contains(switch),
                "{locale}/stat.md must document {switch}"
            );
        }
    }
}

#[test]
fn a_directive_carries_its_flags_width_and_precision() {
    let pieces = parse_format("%-10s|%06i|%.3n", Trailer::None, Subject::File).expect("parses");
    assert_eq!(
        pieces,
        alloc::vec![
            Piece::Field(
                's',
                Pad {
                    left_justify: true,
                    width: 10,
                    ..Pad::default()
                }
            ),
            Piece::Text(String::from("|")),
            Piece::Field(
                'i',
                Pad {
                    zero: true,
                    width: 6,
                    ..Pad::default()
                }
            ),
            Piece::Text(String::from("|")),
            Piece::Field(
                'n',
                Pad {
                    precision: Some(3),
                    ..Pad::default()
                }
            ),
        ]
    );
}

#[test]
fn padding_left_justifies_zero_fills_and_truncates() {
    let left = Pad {
        left_justify: true,
        width: 5,
        ..Pad::default()
    };
    assert_eq!(left.apply("ab"), "ab   ");
    let right = Pad {
        width: 5,
        ..Pad::default()
    };
    assert_eq!(right.apply("ab"), "   ab");
    // Zero-fill applies to a number; a name pads with spaces, because a
    // zero-padded name would be nonsense.
    let zero = Pad {
        zero: true,
        width: 4,
        ..Pad::default()
    };
    assert_eq!(zero.apply("42"), "0042");
    assert_eq!(zero.apply("ab"), "  ab");
    // A field already at or over the width is untouched.
    assert_eq!(right.apply("abcdef"), "abcdef");
    let cut = Pad {
        precision: Some(2),
        ..Pad::default()
    };
    assert_eq!(cut.apply("abcdef"), "ab");
}

#[test]
fn a_numeric_only_flag_is_refused_by_name() {
    // `+`, a leading space, and `#` only ever qualify a numeric
    // conversion, so they are refused rather than accepted and dropped.
    for flag in ['+', ' ', '#'] {
        let text = alloc::format!("%{flag}5s");
        assert_eq!(
            parse_format(&text, Trailer::None, Subject::File),
            Err(StatError::UnknownSpecifier(flag)),
            "{flag}"
        );
    }
}
