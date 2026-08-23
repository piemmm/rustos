//! Host tests for the `telnet>` command interpreter.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{execute, render_char, render_char_opt, Action};
use crate::command::{Config, Target, DEFAULT_ESCAPE};
use crate::linemode::{mode, slc, sub};
use crate::nvt::{self, NvtEvent, Parser, DO, IAC, SB, SE, WILL};
use crate::option;
use crate::session::Session;

/// A session with a settled LINEMODE `EDIT` negotiation, as a cooperative
/// server would have left it.
fn linemode_session() -> Session {
    let mut session = Session::new(&Config::default(), "TAIRIX", 38_400);
    session.begin(&Config::default());
    session.on_network(&[IAC, DO, option::LINEMODE]);
    session.on_network(&[
        IAC,
        SB,
        option::LINEMODE,
        sub::MODE,
        mode::EDIT | mode::TRAPSIG,
        IAC,
        SE,
    ]);
    let _ = session.take_wire();
    let _ = session.take_screen();
    session
}

/// Run `line` against a connected session, returning the action, the text the
/// operator saw, and the bytes queued for the wire.
fn run(session: &mut Session, line: &str) -> (Action, String, Vec<u8>) {
    let action = execute(line, session, true);
    let screen = String::from_utf8_lossy(&session.take_screen()).into_owned();
    (action, screen, session.take_wire())
}

/// Every event the parser finds in `bytes`.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Seen {
    Command(u8),
    Negotiate(u8, u8),
    Subnegotiation(u8, Vec<u8>),
    Data(Vec<u8>),
}

fn decode(bytes: &[u8]) -> Vec<Seen> {
    let mut parser = Parser::new();
    let mut out = Vec::new();
    parser.feed(bytes, |event| match event {
        NvtEvent::Command(byte) => out.push(Seen::Command(byte)),
        NvtEvent::Negotiate { verb, option } => out.push(Seen::Negotiate(verb, option)),
        NvtEvent::Subnegotiation { option, params } => {
            out.push(Seen::Subnegotiation(option, params.to_vec()));
        }
        NvtEvent::Data(data) => out.push(Seen::Data(data.to_vec())),
        NvtEvent::SubnegotiationRefused { .. } | NvtEvent::UnknownCommand(_) => {}
    });
    out
}

#[test]
fn an_empty_line_resumes_the_relay() {
    let mut session = linemode_session();
    assert_eq!(run(&mut session, "").0, Action::Resume);
    assert_eq!(run(&mut session, "   ").0, Action::Resume);
}

#[test]
fn an_unambiguous_prefix_names_its_command() {
    let mut session = linemode_session();
    assert_eq!(run(&mut session, "q").0, Action::Quit);
    assert_eq!(run(&mut session, "c").0, Action::Close);
    assert_eq!(
        run(&mut session, "o example.test").0,
        Action::Open(Target {
            host: "example.test".to_string(),
            port: 23,
        })
    );
}

#[test]
fn an_ambiguous_prefix_is_reported_rather_than_guessed_at() {
    let mut session = linemode_session();
    // `s` opens `send`, `set`, `slc` and `status`.
    let (action, screen, wire) = run(&mut session, "s");
    assert_eq!(action, Action::Prompt);
    assert!(screen.contains("Ambiguous"), "{screen}");
    assert!(wire.is_empty());
}

#[test]
fn an_unknown_command_is_reported() {
    let mut session = linemode_session();
    let (action, screen, _) = run(&mut session, "frobnicate");
    assert_eq!(action, Action::Prompt);
    assert!(screen.contains("Invalid command"), "{screen}");
}

#[test]
fn the_help_listing_names_every_command() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "?");
    for command in [
        "close", "display", "environ", "logout", "mode", "open", "quit", "send", "set", "slc",
        "status", "toggle", "unset", "z",
    ] {
        assert!(screen.contains(command), "{command} missing from {screen}");
    }
}

#[test]
fn open_takes_a_host_and_an_optional_port() {
    let mut session = linemode_session();
    assert_eq!(
        run(&mut session, "open host 8080").0,
        Action::Open(Target {
            host: "host".to_string(),
            port: 8080,
        })
    );
    let (action, screen, _) = run(&mut session, "open");
    assert_eq!(action, Action::Prompt);
    assert!(screen.contains("usage: open"), "{screen}");
    let (action, screen, _) = run(&mut session, "open host notaport");
    assert_eq!(action, Action::Prompt);
    assert!(screen.contains("not a port number"), "{screen}");
    let (action, screen, _) = run(&mut session, "open host 23 extra");
    assert_eq!(action, Action::Prompt);
    assert!(screen.contains("usage: open"), "{screen}");
}

#[test]
fn the_connection_commands_refuse_when_there_is_no_connection() {
    let mut session = linemode_session();
    for line in ["close", "logout", "send ayt", "mode line", "slc export"] {
        let action = execute(line, &mut session, false);
        assert_eq!(action, Action::Prompt, "{line}");
        let screen = String::from_utf8_lossy(&session.take_screen()).into_owned();
        assert!(screen.starts_with('?'), "{line}: {screen}");
        assert!(session.take_wire().is_empty(), "{line}");
    }
    // `quit` always works: it is how the operator leaves.
    assert_eq!(execute("quit", &mut session, false), Action::Quit);
}

#[test]
fn logout_asks_the_remote_host_and_returns_to_the_relay() {
    let mut session = linemode_session();
    let (action, _, wire) = run(&mut session, "logout");
    assert_eq!(action, Action::Resume);
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Negotiate(DO, option::LOGOUT)]
    );
}

#[test]
fn every_sendable_command_reaches_the_wire() {
    let cases = [
        ("abort", nvt::ABORT),
        ("ao", nvt::AO),
        ("ayt", nvt::AYT),
        ("brk", nvt::BRK),
        ("ec", nvt::EC),
        ("el", nvt::EL),
        ("eof", nvt::XEOF),
        ("eor", nvt::EOR),
        ("ga", nvt::GA),
        ("ip", nvt::IP),
        ("nop", nvt::NOP),
        ("susp", nvt::SUSP),
        ("synch", nvt::DM),
    ];
    for (name, command) in cases {
        let mut session = linemode_session();
        let (action, _, wire) = run(&mut session, &alloc::format!("send {name}"));
        assert_eq!(action, Action::Prompt, "{name}");
        assert_eq!(decode(&wire), alloc::vec![Seen::Command(command)], "{name}");
    }
}

#[test]
fn send_takes_several_arguments_in_order() {
    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "send ayt nop");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Command(nvt::AYT), Seen::Command(nvt::NOP)]
    );
}

#[test]
fn send_escape_transmits_the_escape_character_as_data() {
    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "send escape");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Data(alloc::vec![DEFAULT_ESCAPE])]
    );
    session.set_escape(None);
    let (_, screen, wire) = run(&mut session, "send escape");
    assert!(screen.contains("no escape character"), "{screen}");
    assert!(wire.is_empty());
}

#[test]
fn send_getstatus_needs_the_option_the_server_must_have_offered() {
    let mut session = linemode_session();
    let (_, screen, wire) = run(&mut session, "send getstatus");
    assert!(screen.contains("does not support STATUS"), "{screen}");
    assert!(wire.is_empty());

    session.on_network(&[IAC, WILL, option::STATUS]);
    let _ = session.take_wire();
    let (_, _, wire) = run(&mut session, "send getstatus");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Subnegotiation(
            option::STATUS,
            alloc::vec![crate::subneg::cmd::SEND]
        )]
    );
}

#[test]
fn send_do_takes_an_option_by_name_or_number() {
    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "send do 34");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Negotiate(DO, option::LINEMODE)]
    );
    let (_, _, wire) = run(&mut session, "send will binary");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Negotiate(WILL, option::BINARY)]
    );
    let (_, _, wire) = run(&mut session, "send dont suppress-go-ahead");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Negotiate(nvt::DONT, option::SUPPRESS_GO_AHEAD)]
    );
}

#[test]
fn send_do_reports_an_unknown_option_and_a_missing_argument() {
    let mut session = linemode_session();
    let (_, screen, wire) = run(&mut session, "send do nosuchoption");
    assert!(screen.contains("not an option"), "{screen}");
    assert!(wire.is_empty());
    let (_, screen, wire) = run(&mut session, "send do");
    assert!(screen.contains("usage: send do"), "{screen}");
    assert!(wire.is_empty());
}

#[test]
fn send_with_no_argument_lists_what_it_accepts() {
    let mut session = linemode_session();
    let (action, screen, wire) = run(&mut session, "send");
    assert_eq!(action, Action::Prompt);
    for name in ["abort", "ayt", "getstatus", "will", "escape"] {
        assert!(screen.contains(name), "{name} missing from {screen}");
    }
    assert!(wire.is_empty());
}

#[test]
fn an_unknown_send_argument_is_reported_and_the_rest_still_run() {
    let mut session = linemode_session();
    let (_, screen, wire) = run(&mut session, "send bogus ayt");
    assert!(screen.contains("not a valid send argument"), "{screen}");
    assert_eq!(decode(&wire), alloc::vec![Seen::Command(nvt::AYT)]);
}

#[test]
fn set_escape_takes_a_character_or_a_caret_spelling() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "set escape ^A");
    assert_eq!(session.escape(), Some(0x01));
    assert!(screen.contains("'^A'"), "{screen}");
    run(&mut session, "set escape x");
    assert_eq!(session.escape(), Some(b'x'));
    let (_, screen, _) = run(&mut session, "set escape ^]x");
    assert!(screen.contains("not a character"), "{screen}");
    assert_eq!(session.escape(), Some(b'x'), "the refusal changed nothing");
}

#[test]
fn unset_escape_leaves_no_escape_character() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "unset escape");
    assert_eq!(session.escape(), None);
    assert!(screen.contains("no escape character"), "{screen}");
}

#[test]
fn set_rebinds_a_special_character_and_unset_disables_it() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "set erase ^H");
    assert!(screen.contains("erase character is '^H'"), "{screen}");
    assert_eq!(session.linemode().slc().char_for(slc::EC), Some(0x08));
    run(&mut session, "unset erase");
    assert_eq!(session.linemode().slc().char_for(slc::EC), None);
}

#[test]
fn set_refuses_a_character_the_server_pinned() {
    let mut session = linemode_session();
    session.on_network(&[
        IAC,
        SB,
        option::LINEMODE,
        sub::SLC,
        slc::IP,
        crate::linemode::slc_flag::CANTCHANGE,
        0x03,
        IAC,
        SE,
    ]);
    let _ = session.take_wire();
    let (_, screen, _) = run(&mut session, "set ip ^A");
    assert!(screen.contains("pinned"), "{screen}");
    assert_eq!(session.linemode().slc().char_for(slc::IP), Some(0x03));
    let (_, screen, _) = run(&mut session, "unset ip");
    assert!(screen.contains("pinned"), "{screen}");
}

#[test]
fn set_reports_an_unknown_variable_and_lists_the_settable_ones() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "set nosuchvar x");
    assert!(screen.contains("not a settable variable"), "{screen}");
    let (_, screen, _) = run(&mut session, "set ?");
    assert!(screen.contains("escape"), "{screen}");
    assert!(screen.contains("erase"), "{screen}");
    assert!(screen.contains("worderase"), "{screen}");
}

#[test]
fn set_echo_says_honestly_that_there_is_no_such_character() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "set echo ^E");
    assert!(screen.contains("no echo-toggle character"), "{screen}");
}

#[test]
fn every_local_toggle_flips_and_reports() {
    let cases = [
        ("autoflush", true),
        ("autosynch", true),
        ("crlf", false),
        ("crmod", false),
        ("localchars", true),
        ("netdata", false),
        ("options", false),
    ];
    for (name, default) in cases {
        let mut session = linemode_session();
        let (action, screen, wire) = run(&mut session, &alloc::format!("toggle {name}"));
        assert_eq!(action, Action::Prompt, "{name}");
        assert!(
            screen.contains(if default { "disabled" } else { "enabled" }),
            "{name}: {screen}"
        );
        assert!(wire.is_empty(), "a local toggle sends nothing: {name}");
    }
}

#[test]
fn the_debug_toggle_drives_the_trace_it_can_actually_produce() {
    let mut session = linemode_session();
    run(&mut session, "toggle debug");
    assert!(
        session.flags().options,
        "there is no socket-level debugging"
    );
}

#[test]
fn the_binary_toggles_negotiate_the_direction_they_name() {
    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "toggle outbinary");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Negotiate(WILL, option::BINARY)]
    );

    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "toggle inbinary");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Negotiate(DO, option::BINARY)]
    );

    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "toggle binary");
    assert_eq!(
        decode(&wire),
        alloc::vec![
            Seen::Negotiate(WILL, option::BINARY),
            Seen::Negotiate(DO, option::BINARY)
        ]
    );
}

#[test]
fn toggling_binary_off_asks_for_the_disable() {
    let mut session = linemode_session();
    session.on_network(&[IAC, DO, option::BINARY]);
    let _ = session.take_wire();
    assert!(session.options().local(option::BINARY));
    let (_, _, wire) = run(&mut session, "toggle outbinary");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Negotiate(nvt::WONT, option::BINARY)]
    );
}

#[test]
fn toggle_lists_and_refuses_honestly() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "toggle");
    assert!(screen.contains("localchars"), "{screen}");
    let (_, screen, _) = run(&mut session, "toggle nosuchtoggle");
    assert!(screen.contains("not a toggle"), "{screen}");
}

#[test]
fn mode_edits_the_linemode_mask_when_the_option_is_in_force() {
    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "mode softtab");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Subnegotiation(
            option::LINEMODE,
            alloc::vec![sub::MODE, mode::EDIT | mode::TRAPSIG | mode::SOFT_TAB]
        )]
    );

    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "mode -isig");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Subnegotiation(
            option::LINEMODE,
            alloc::vec![sub::MODE, mode::EDIT]
        )]
    );

    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "mode character");
    assert_eq!(
        decode(&wire),
        alloc::vec![Seen::Subnegotiation(
            option::LINEMODE,
            alloc::vec![sub::MODE, mode::TRAPSIG]
        )],
        "character mode clears EDIT"
    );
}

#[test]
fn mode_without_linemode_negotiates_the_historical_way() {
    let mut session = Session::new(&Config::default(), "T", 1);
    let (_, _, wire) = run(&mut session, "mode character");
    assert_eq!(
        decode(&wire),
        alloc::vec![
            Seen::Negotiate(DO, option::SUPPRESS_GO_AHEAD),
            Seen::Negotiate(DO, option::ECHO)
        ],
        "a character-at-a-time server suppresses Go Ahead and echoes"
    );
}

#[test]
fn a_mode_bit_that_needs_linemode_says_so_rather_than_pretending() {
    let mut session = Session::new(&Config::default(), "T", 1);
    let (_, screen, wire) = run(&mut session, "mode softtab");
    assert!(screen.contains("needs the LINEMODE option"), "{screen}");
    assert!(wire.is_empty());
}

#[test]
fn mode_lists_and_refuses_honestly() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "mode");
    assert!(screen.contains("character"), "{screen}");
    assert!(screen.contains("-litecho"), "{screen}");
    let (_, screen, _) = run(&mut session, "mode nosuchmode");
    assert!(screen.contains("not a mode"), "{screen}");
}

#[test]
fn environ_defines_exports_and_lists() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "environ define PROJECT tairix");
    assert!(screen.contains("not yet exported"), "{screen}");
    let (_, screen, _) = run(&mut session, "environ list");
    assert!(screen.contains("PROJECT=tairix"), "{screen}");
    assert!(!screen.contains("export"), "unexported: {screen}");
    run(&mut session, "environ export PROJECT");
    let (_, screen, _) = run(&mut session, "environ list");
    assert!(screen.contains("export"), "{screen}");
    run(&mut session, "environ unexport PROJECT");
    assert!(!session.environ().vars()[0].exported);
    run(&mut session, "environ undefine PROJECT");
    let (_, screen, _) = run(&mut session, "environ list");
    assert!(screen.contains("No environment variables"), "{screen}");
}

#[test]
fn environ_reports_its_refusals() {
    let mut session = linemode_session();
    let (_, screen, _) = run(&mut session, "environ export NOPE");
    assert!(screen.contains("No such variable"), "{screen}");
    let (_, screen, _) = run(&mut session, "environ define");
    assert!(screen.contains("usage: environ define"), "{screen}");
    let (_, screen, _) = run(&mut session, "environ nosuchsub");
    assert!(screen.contains("not an environ command"), "{screen}");
    let (_, screen, _) = run(&mut session, "environ");
    assert!(screen.contains("usage: environ"), "{screen}");
}

#[test]
fn slc_export_states_the_table_and_import_asks_for_the_servers() {
    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "slc export");
    let events = decode(&wire);
    assert_eq!(events.len(), 1);
    let Seen::Subnegotiation(option, params) = &events[0] else {
        panic!("expected a subnegotiation: {events:?}");
    };
    assert_eq!(*option, option::LINEMODE);
    assert_eq!(params[0], sub::SLC);
    let default = session
        .linemode()
        .slc()
        .char_for(slc::EC)
        .expect("a default erase character");
    assert!(params.contains(&default), "our own values are stated");

    let mut session = linemode_session();
    let (_, _, wire) = run(&mut session, "slc import");
    let events = decode(&wire);
    let Seen::Subnegotiation(_, params) = &events[0] else {
        panic!("expected a subnegotiation: {events:?}");
    };
    assert!(
        params[1..]
            .as_chunks::<3>()
            .0
            .iter()
            .all(|triplet| triplet[1] == crate::linemode::slc_flag::DEFAULT),
        "every function is asked for at the DEFAULT level"
    );
}

#[test]
fn slc_needs_the_linemode_option_and_lists_its_subcommands() {
    let mut session = Session::new(&Config::default(), "T", 1);
    let (_, screen, wire) = run(&mut session, "slc export");
    assert!(
        screen.contains("LINEMODE option is not in force"),
        "{screen}"
    );
    assert!(wire.is_empty());

    let (_, screen, _) = run(&mut session, "slc ?");
    assert!(screen.contains("export"), "{screen}");
    assert!(screen.contains("import"), "{screen}");
    let (_, screen, _) = run(&mut session, "slc");
    assert!(screen.contains("usage: slc"), "{screen}");
    let (_, screen, _) = run(&mut session, "slc nosuch");
    assert!(screen.contains("not an slc command"), "{screen}");
}

#[test]
fn status_describes_the_negotiated_session() {
    let mut session = linemode_session();
    let (action, screen, wire) = run(&mut session, "status");
    assert_eq!(action, Action::Prompt);
    assert!(screen.contains("LINEMODE option"), "{screen}");
    assert!(screen.contains("Local line editing"), "{screen}");
    assert!(screen.contains("Local signal handling"), "{screen}");
    assert!(screen.contains("Escape character is '^]'"), "{screen}");
    assert!(wire.is_empty());
}

#[test]
fn status_of_a_disconnected_session_says_so() {
    let mut session = Session::new(&Config::default(), "T", 1);
    let action = execute("status", &mut session, false);
    assert_eq!(action, Action::Prompt);
    let screen = String::from_utf8_lossy(&session.take_screen()).into_owned();
    assert!(screen.contains("No connection"), "{screen}");
    assert!(screen.contains("line-by-line"), "{screen}");
}

#[test]
fn status_reports_the_character_mode_and_binary_directions() {
    let mut session = Session::new(&Config::default(), "T", 1);
    session.on_network(&[IAC, WILL, option::ECHO]);
    session.on_network(&[IAC, DO, option::BINARY]);
    session.on_network(&[IAC, WILL, option::BINARY]);
    let _ = session.take_wire();
    let _ = session.take_screen();
    let (_, screen, _) = run(&mut session, "status");
    assert!(screen.contains("character-at-a-time"), "{screen}");
    assert!(screen.contains("binary mode on transmit"), "{screen}");
    assert!(screen.contains("binary mode on receive"), "{screen}");
}

#[test]
fn display_lists_the_operating_parameters_and_the_special_characters() {
    let mut session = linemode_session();
    let (action, screen, wire) = run(&mut session, "display");
    assert_eq!(action, Action::Prompt);
    assert!(screen.contains("will flush output"), "{screen}");
    assert!(screen.contains("won't map carriage return"), "{screen}");
    assert!(screen.contains("escape    '^]'"), "{screen}");
    assert!(screen.contains("erase"), "{screen}");
    assert!(wire.is_empty());
}

#[test]
fn display_names_a_disabled_character_honestly() {
    let mut session = linemode_session();
    run(&mut session, "unset erase");
    let (_, screen, _) = run(&mut session, "display");
    assert!(screen.contains("(disabled)"), "{screen}");
}

#[test]
fn z_asks_to_suspend_and_quit_asks_to_leave() {
    let mut session = linemode_session();
    assert_eq!(run(&mut session, "z").0, Action::Suspend);
    assert_eq!(run(&mut session, "quit").0, Action::Quit);
}

#[test]
fn a_character_renders_in_the_familiar_spellings() {
    assert_eq!(render_char(0x1D), "'^]'");
    assert_eq!(render_char(0x00), "'^@'");
    assert_eq!(render_char(0x7F), "'^?'");
    assert_eq!(render_char(b'x'), "'x'");
    assert_eq!(render_char(b' '), "' '");
    assert_eq!(render_char(0x80), "'\\200'");
    assert_eq!(render_char_opt(None), "(none)");
    assert_eq!(render_char_opt(Some(b'x')), "'x'");
}

#[test]
fn no_command_ever_leaves_a_half_applied_change() {
    // Every refusal path is expected to change nothing observable, so an
    // operator's mistake can never silently reconfigure the session.
    let before = {
        let session = linemode_session();
        (
            session.escape(),
            *session.flags(),
            session.linemode().mask(),
            session.environ().vars().len(),
        )
    };
    let mut session = linemode_session();
    for line in [
        "frobnicate",
        "s",
        "open",
        "open h notaport",
        "send bogus",
        "send do nope",
        "set nosuchvar x",
        "set escape ^]x",
        "unset nosuchvar",
        "toggle nosuchtoggle",
        "mode nosuchmode",
        "environ nosuchsub",
        "environ export NOPE",
        "slc nosuch",
    ] {
        assert_eq!(execute(line, &mut session, true), Action::Prompt, "{line}");
        let _ = session.take_screen();
        let _ = session.take_wire();
    }
    assert_eq!(
        (
            session.escape(),
            *session.flags(),
            session.linemode().mask(),
            session.environ().vars().len()
        ),
        before
    );
}
