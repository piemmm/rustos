//! Host tests for the `telnet` argument grammar.

use alloc::string::ToString;

use tairix_abi::net_ipc::NetAddrFamily;

use super::{
    parse, parse_escape, parse_port, Command, Config, ParseError, Target, DEFAULT_ESCAPE,
    DEFAULT_PORT,
};

/// The session configuration and target `args` parse to.
fn session(args: &[&str]) -> (Config, Option<Target>) {
    match parse(args).expect("a valid argument vector") {
        Command::Session { config, target } => (config, target),
        Command::Help => panic!("expected a session, got the help command"),
    }
}

#[test]
fn a_bare_invocation_opens_in_command_mode() {
    let (config, target) = session(&[]);
    assert_eq!(target, None, "a bare `telnet` connects to nothing");
    assert_eq!(config, Config::default());
    assert_eq!(config.escape, Some(DEFAULT_ESCAPE));
}

#[test]
fn a_host_alone_takes_the_assigned_telnet_port() {
    let (_, target) = session(&["example.test"]);
    assert_eq!(
        target,
        Some(Target {
            host: "example.test".to_string(),
            port: DEFAULT_PORT,
        })
    );
}

#[test]
fn a_host_and_port_are_both_taken() {
    let (_, target) = session(&["10.0.0.1", "80"]);
    assert_eq!(
        target,
        Some(Target {
            host: "10.0.0.1".to_string(),
            port: 80,
        })
    );
}

#[test]
fn an_ipv6_literal_is_an_ordinary_host_operand() {
    let (_, target) = session(&["fe80::2", "23"]);
    assert_eq!(target.expect("a target").host, "fe80::2");
}

#[test]
fn the_family_switches_restrict_resolution() {
    assert_eq!(session(&["-4", "h"]).0.family, Some(NetAddrFamily::V4));
    assert_eq!(session(&["--ipv4", "h"]).0.family, Some(NetAddrFamily::V4));
    assert_eq!(session(&["-6", "h"]).0.family, Some(NetAddrFamily::V6));
    assert_eq!(session(&["--ipv6", "h"]).0.family, Some(NetAddrFamily::V6));
    assert_eq!(
        parse(&["-4", "-6", "h"]),
        Err(ParseError::FamilyConflict),
        "one identity, never two"
    );
}

#[test]
fn the_binary_switches_select_the_directions_they_name() {
    let both = session(&["-8", "h"]).0;
    assert!(both.binary_in && both.binary_out);
    let long = session(&["--binary", "h"]).0;
    assert!(long.binary_in && long.binary_out);
    let out_only = session(&["-L", "h"]).0;
    assert!(!out_only.binary_in && out_only.binary_out);
    let out_long = session(&["--eight-bit-output", "h"]).0;
    assert!(!out_long.binary_in && out_long.binary_out);
}

#[test]
fn the_escape_switch_takes_every_spelling_of_its_value() {
    for args in [
        alloc::vec!["-e^A", "h"],
        alloc::vec!["-e", "^A", "h"],
        alloc::vec!["--escape=^A", "h"],
        alloc::vec!["--escape", "^A", "h"],
    ] {
        assert_eq!(session(&args).0.escape, Some(0x01), "{args:?}");
    }
}

#[test]
fn the_no_escape_switch_leaves_no_escape_character() {
    assert_eq!(session(&["-E", "h"]).0.escape, None);
    assert_eq!(session(&["--no-escape", "h"]).0.escape, None);
}

#[test]
fn a_login_name_implies_the_automatic_login() {
    let config = session(&["-l", "ada", "h"]).0;
    assert_eq!(config.user.as_deref(), Some("ada"));
    assert!(config.auto_login);
    let long = session(&["--user=ada", "h"]).0;
    assert_eq!(long.user.as_deref(), Some("ada"));
    // `-a` alone asks for the login without naming it; the session's own `USER`
    // supplies the name.
    let bare = session(&["-a", "h"]).0;
    assert!(bare.auto_login && bare.user.is_none());
}

#[test]
fn the_bind_and_debug_switches_are_recorded() {
    let config = session(&["-b", "10.0.0.5", "-d", "h"]).0;
    assert_eq!(config.bind.as_deref(), Some("10.0.0.5"));
    assert!(config.debug);
    assert!(session(&["--debug", "h"]).0.debug);
    assert_eq!(session(&["--bind=::1", "h"]).0.bind.as_deref(), Some("::1"));
}

#[test]
fn the_help_switches_win_over_any_operand() {
    for switch in ["-?", "-h", "--help"] {
        assert_eq!(parse(&[switch, "host"]), Ok(Command::Help), "{switch}");
    }
}

#[test]
fn an_unknown_option_fails_closed() {
    assert_eq!(
        parse(&["-z"]),
        Err(ParseError::UnknownOption("-z".to_string()))
    );
    assert_eq!(
        parse(&["--bogus"]),
        Err(ParseError::UnknownOption("--bogus".to_string()))
    );
    // The switches inetutils defines for facilities TAIRiX lacks are unknown
    // rather than accepted and silently ignored.
    for absent in ["-r", "-c", "-n"] {
        assert!(
            matches!(parse(&[absent, "h"]), Err(ParseError::UnknownOption(_))),
            "{absent}"
        );
    }
}

#[test]
fn a_value_taking_option_with_no_value_fails_closed() {
    for switch in ["-e", "--escape", "-l", "--user", "-b", "--bind"] {
        assert_eq!(
            parse(&[switch]),
            Err(ParseError::MissingValue(switch.to_string())),
            "{switch}"
        );
    }
}

#[test]
fn a_bad_escape_value_is_reported_not_guessed_at() {
    assert_eq!(
        parse(&["-e", "^]x", "h"]),
        Err(ParseError::BadEscape("^]x".to_string()))
    );
}

#[test]
fn an_empty_escape_value_disables_the_escape_character() {
    assert_eq!(session(&["--escape=", "h"]).0.escape, None);
}

#[test]
fn a_service_name_is_refused_rather_than_defaulted() {
    // Silently falling back to port 23 would connect somewhere the operator
    // did not ask for.
    assert_eq!(
        parse(&["h", "telnet"]),
        Err(ParseError::BadPort("telnet".to_string()))
    );
    assert_eq!(
        parse(&["h", "0"]),
        Err(ParseError::BadPort("0".to_string()))
    );
    assert_eq!(
        parse(&["h", "65536"]),
        Err(ParseError::BadPort("65536".to_string()))
    );
    assert_eq!(parse_port("65535"), Ok(65535));
    assert_eq!(parse_port("1"), Ok(1));
}

#[test]
fn a_third_operand_and_a_lone_port_both_fail_closed() {
    assert_eq!(
        parse(&["h", "23", "extra"]),
        Err(ParseError::TooManyOperands)
    );
    assert_eq!(
        parse(&["--", "23"]),
        Ok(Command::Session {
            config: Config::default(),
            target: Some(Target {
                host: "23".to_string(),
                port: DEFAULT_PORT
            }),
        }),
        "after `--` the first operand is the host, whatever it looks like"
    );
}

#[test]
fn a_bare_dash_is_a_host_operand_not_an_option() {
    let (_, target) = session(&["-", "23"]);
    assert_eq!(
        target,
        Some(Target {
            host: "-".to_string(),
            port: 23,
        })
    );
}

#[test]
fn options_after_the_double_dash_are_operands() {
    let (config, target) = session(&["--", "-E"]);
    assert_eq!(target.expect("a target").host, "-E");
    assert_eq!(
        config.escape,
        Some(DEFAULT_ESCAPE),
        "`-E` after `--` is a host name, not a switch"
    );
}

#[test]
fn escape_spellings_decode_as_documented() {
    assert_eq!(parse_escape(""), Some(None));
    assert_eq!(parse_escape("^]"), Some(Some(0x1D)));
    assert_eq!(parse_escape("^?"), Some(Some(0x7F)));
    assert_eq!(parse_escape("^A"), Some(Some(0x01)));
    assert_eq!(parse_escape("x"), Some(Some(b'x')));
    assert_eq!(parse_escape("^"), Some(Some(b'^')));
    assert_eq!(parse_escape("ab"), None);
    assert_eq!(parse_escape("^\u{1}"), None, "the caret needs a printable");
}

#[test]
fn every_option_key_is_covered() {
    // Guards against a switch drifting out of the parser without its Help
    // OPTIONS row (and vice versa): every switch key the help document pins
    // must parse here.
    for arg in ["-4", "-6", "-8", "-E", "-L", "-a", "-d"] {
        assert!(
            matches!(parse(&[arg, "h"]), Ok(Command::Session { .. })),
            "{arg}"
        );
    }
    for arg in [
        "--ipv4",
        "--ipv6",
        "--binary",
        "--no-escape",
        "--eight-bit-output",
        "--login",
        "--debug",
    ] {
        assert!(
            matches!(parse(&[arg, "h"]), Ok(Command::Session { .. })),
            "{arg}"
        );
    }
    for (short, long) in [("-e", "--escape"), ("-l", "--user"), ("-b", "--bind")] {
        assert!(
            matches!(parse(&[short, "x", "h"]), Ok(Command::Session { .. })),
            "{short}"
        );
        assert!(
            matches!(parse(&[long, "x", "h"]), Ok(Command::Session { .. })),
            "{long}"
        );
    }
    for arg in ["-?", "-h", "--help"] {
        assert_eq!(parse(&[arg]), Ok(Command::Help), "{arg}");
    }
}
