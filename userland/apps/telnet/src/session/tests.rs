//! Host tests for the session engine: the negotiation it opens, the receive
//! interpretation, and both keyboard relay modes.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::TerminalSize;

use super::{Relay, Session};
use crate::command::{Config, DEFAULT_ESCAPE};
use crate::linemode::{mode, slc, sub};
use crate::nvt::{
    self, NvtEvent, Parser, ABORT, AYT, DM, DO, DONT, IAC, IP, SB, SE, WILL, WONT, XEOF,
};
use crate::option::{self, Options};
use crate::subneg::cmd;

/// A session with the default operating parameters.
fn session() -> Session {
    Session::new(&Config::default(), "TAIRIX", 38_400)
}

/// Every event the parser finds in `bytes`, as owned values.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Seen {
    Data(Vec<u8>),
    Command(u8),
    Negotiate(u8, u8),
    Subnegotiation(u8, Vec<u8>),
}

fn decode(bytes: &[u8]) -> Vec<Seen> {
    let mut parser = Parser::new();
    let mut out = Vec::new();
    parser.feed(bytes, |event| match event {
        NvtEvent::Data(data) => out.push(Seen::Data(data.to_vec())),
        NvtEvent::Command(byte) => out.push(Seen::Command(byte)),
        NvtEvent::Negotiate { verb, option } => out.push(Seen::Negotiate(verb, option)),
        NvtEvent::Subnegotiation { option, params } => {
            out.push(Seen::Subnegotiation(option, params.to_vec()));
        }
        NvtEvent::SubnegotiationRefused { .. } | NvtEvent::UnknownCommand(_) => {}
    });
    out
}

/// Drive the session's own negotiation to a settled LINEMODE `EDIT` session, as
/// a cooperative server would, and return it.
fn linemode_session() -> Session {
    let mut session = session();
    session.begin(&Config::default());
    let _ = session.take_wire();
    // The server agrees to LINEMODE, then states the editing mask.
    session.on_network(&[IAC, DO, option::LINEMODE]);
    let _ = session.take_wire();
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
    assert_eq!(session.relay(), Relay::Line);
    session
}

// --- the opening negotiation ------------------------------------------------

#[test]
fn a_fresh_session_offers_the_options_an_interactive_session_needs() {
    let mut session = session();
    session.begin(&Config::default());
    let asked = decode(&session.take_wire());
    for expected in [
        Seen::Negotiate(DO, option::SUPPRESS_GO_AHEAD),
        Seen::Negotiate(WILL, option::TERMINAL_TYPE),
        Seen::Negotiate(WILL, option::NAWS),
        Seen::Negotiate(WILL, option::TERMINAL_SPEED),
        Seen::Negotiate(WILL, option::NEW_ENVIRON),
        Seen::Negotiate(WILL, option::LINEMODE),
    ] {
        assert!(asked.contains(&expected), "{expected:?} not in {asked:?}");
    }
    assert!(
        !asked.contains(&Seen::Negotiate(WILL, option::BINARY)),
        "BINARY is offered only when the operator asked for it"
    );
}

#[test]
fn the_binary_switches_add_only_the_direction_they_name() {
    let mut config = Config {
        binary_out: true,
        ..Config::default()
    };
    let mut session = Session::new(&config, "T", 1);
    session.begin(&config);
    let asked = decode(&session.take_wire());
    assert!(asked.contains(&Seen::Negotiate(WILL, option::BINARY)));
    assert!(!asked.contains(&Seen::Negotiate(DO, option::BINARY)));

    config.binary_in = true;
    let mut both = Session::new(&config, "T", 1);
    both.begin(&config);
    let asked = decode(&both.take_wire());
    assert!(asked.contains(&Seen::Negotiate(DO, option::BINARY)));
}

#[test]
fn a_login_name_is_defined_and_exported_and_nothing_else_is() {
    let config = Config {
        user: Some(String::from("ada")),
        ..Config::default()
    };
    let session = Session::new(&config, "T", 1);
    let vars = session.environ().vars();
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].name, "USER");
    assert!(vars[0].exported, "the operator asked for it by name");
}

#[test]
fn no_login_name_means_an_empty_environment() {
    assert!(session().environ().vars().is_empty());
}

// --- receive-side interpretation --------------------------------------------

#[test]
fn the_nvt_line_endings_are_interpreted_on_display() {
    let mut session = session();
    session.on_network(b"one\r\ntwo\r\0three\r");
    assert_eq!(
        session.take_screen(),
        b"one\r\ntwo\rthree".to_vec(),
        "CR LF is one new line, CR NUL is a bare carriage return"
    );
    // The trailing CR is held; the next chunk decides what it meant.
    session.on_network(b"\nfour");
    assert_eq!(session.take_screen(), b"\r\nfour".to_vec());
}

#[test]
fn a_cr_before_an_ordinary_byte_stands_on_its_own() {
    let mut session = session();
    session.on_network(b"a\rb");
    assert_eq!(session.take_screen(), b"a\rb".to_vec());
}

#[test]
fn two_carriage_returns_in_a_row_both_survive() {
    let mut session = session();
    session.on_network(b"a\r\rb");
    assert_eq!(session.take_screen(), b"a\r\rb".to_vec());
}

#[test]
fn binary_receive_passes_every_byte_through_untouched() {
    let mut session = session();
    session.on_network(&[IAC, WILL, option::BINARY]);
    let _ = session.take_wire();
    assert!(session.options().remote(option::BINARY));
    session.on_network(b"a\rb\r\nc");
    assert_eq!(
        session.take_screen(),
        b"a\rb\r\nc".to_vec(),
        "a binary path carries no NVT line convention"
    );
}

#[test]
fn an_are_you_there_is_answered_on_the_screen_not_the_wire() {
    let mut session = session();
    session.on_network(&[IAC, AYT]);
    assert_eq!(session.take_screen(), b"\r\n[Yes]\r\n".to_vec());
    assert!(
        session.take_wire().is_empty(),
        "answering on the wire would inject bytes into the server's input"
    );
}

#[test]
fn a_data_mark_and_the_erase_commands_change_nothing() {
    let mut session = session();
    for command in [DM, nvt::EC, nvt::EL, nvt::GA, nvt::NOP, IP] {
        session.on_network(&[IAC, command]);
        assert!(session.take_wire().is_empty(), "command {command}");
        assert!(session.take_screen().is_empty(), "command {command}");
    }
}

#[test]
fn hostile_network_input_never_makes_the_session_reply_unboundedly() {
    let mut session = session();
    session.begin(&Config::default());
    let _ = session.take_wire();
    // A server that repeats the same negotiation forever gets one answer.
    let mut replies = 0usize;
    for _ in 0..500 {
        session.on_network(&[IAC, WILL, option::SUPPRESS_GO_AHEAD]);
        replies += session.take_wire().len();
        session.on_network(&[IAC, DO, option::NAWS]);
        replies += session.take_wire().len();
    }
    assert!(replies <= 16, "{replies} bytes of reply to 1000 requests");
}

#[test]
fn an_unsupported_option_is_always_refused() {
    let mut session = session();
    // 37 is AUTHENTICATION, which this client does not implement.
    session.on_network(&[IAC, WILL, 37]);
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Negotiate(DONT, 37)]
    );
    session.on_network(&[IAC, DO, 37]);
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Negotiate(WONT, 37)]
    );
}

#[test]
fn a_logout_request_is_reported_and_refused_as_an_option() {
    let mut session = session();
    let fold = session.on_network(&[IAC, DO, option::LOGOUT]);
    assert!(fold.logout, "the caller decides, not the engine");
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Negotiate(WONT, option::LOGOUT)],
        "the option itself is never accepted"
    );
}

// --- subnegotiation dispatch ------------------------------------------------

#[test]
fn a_terminal_type_request_is_answered_with_the_reported_term() {
    let mut session = Session::new(&Config::default(), "xterm", 1);
    // A server asks for the option before it subnegotiates it.
    session.on_network(&[IAC, DO, option::TERMINAL_TYPE]);
    let _ = session.take_wire();
    session.on_network(&[IAC, SB, option::TERMINAL_TYPE, cmd::SEND, IAC, SE]);
    let mut expected = alloc::vec![cmd::IS];
    expected.extend_from_slice(b"XTERM");
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Subnegotiation(option::TERMINAL_TYPE, expected)]
    );
}

#[test]
fn a_terminal_speed_request_is_answered() {
    let mut session = Session::new(&Config::default(), "T", 9600);
    session.on_network(&[IAC, DO, option::TERMINAL_SPEED]);
    let _ = session.take_wire();
    session.on_network(&[IAC, SB, option::TERMINAL_SPEED, cmd::SEND, IAC, SE]);
    let mut expected = alloc::vec![cmd::IS];
    expected.extend_from_slice(b"9600,9600");
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Subnegotiation(option::TERMINAL_SPEED, expected)]
    );
}

#[test]
fn a_status_request_is_answered_from_the_real_negotiated_state() {
    let mut session = session();
    session.on_network(&[IAC, WILL, option::ECHO]);
    session.on_network(&[IAC, DO, option::STATUS]);
    let _ = session.take_wire();
    session.on_network(&[IAC, SB, option::STATUS, cmd::SEND, IAC, SE]);
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Subnegotiation(
            option::STATUS,
            alloc::vec![cmd::IS, DO, option::ECHO, WILL, option::STATUS]
        )]
    );
}

#[test]
fn an_is_subnegotiation_asks_for_nothing_and_is_answered_with_nothing() {
    let mut session = session();
    session.on_network(&[IAC, DO, option::TERMINAL_TYPE]);
    let _ = session.take_wire();
    session.on_network(&[IAC, SB, option::TERMINAL_TYPE, cmd::IS, b'X', IAC, SE]);
    assert!(session.take_wire().is_empty());
}

#[test]
fn naws_is_reported_once_the_option_is_agreed_and_again_only_on_a_change() {
    let mut session = session();
    session.set_terminal_size(Some(TerminalSize::new(24, 80).expect("a grid")));
    assert!(
        session.take_wire().is_empty(),
        "nothing is reported before the option is agreed"
    );
    session.on_network(&[IAC, DO, option::NAWS]);
    let wire = decode(&session.take_wire());
    assert!(wire.contains(&Seen::Negotiate(WILL, option::NAWS)));
    assert!(wire.contains(&Seen::Subnegotiation(
        option::NAWS,
        alloc::vec![0, 80, 0, 24]
    )));

    // The same grid is not re-reported; a changed one is.
    session.set_terminal_size(Some(TerminalSize::new(24, 80).expect("a grid")));
    assert!(session.take_wire().is_empty());
    session.set_terminal_size(Some(TerminalSize::new(50, 132).expect("a grid")));
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Subnegotiation(
            option::NAWS,
            alloc::vec![0, 132, 0, 50]
        )]
    );
}

#[test]
fn a_console_with_no_attestable_size_reports_no_window() {
    let mut session = session();
    session.on_network(&[IAC, DO, option::NAWS]);
    let _ = session.take_wire();
    session.set_terminal_size(None);
    assert!(
        session.take_wire().is_empty(),
        "a size the kernel cannot attest is never fabricated"
    );
}

#[test]
fn a_new_environ_request_discloses_only_what_was_exported() {
    let config = Config {
        user: Some(String::from("ada")),
        ..Config::default()
    };
    let mut session = Session::new(&config, "T", 1);
    session
        .environ_mut()
        .define("SECRET", "x")
        .expect("defined");
    session.on_network(&[IAC, DO, option::NEW_ENVIRON]);
    let _ = session.take_wire();
    session.on_network(&[IAC, SB, option::NEW_ENVIRON, cmd::SEND, IAC, SE]);
    let wire = session.take_wire();
    let text = String::from_utf8_lossy(&wire).into_owned();
    assert!(text.contains("USER"), "{text}");
    assert!(text.contains("ada"), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
}

#[test]
fn a_flow_control_command_is_applied_and_a_malformed_one_is_not() {
    let mut session = session();
    // Flow control is the peer's option to offer, so it says WILL and the
    // client answers DO before any command is meaningful.
    session.on_network(&[IAC, WILL, option::TOGGLE_FLOW_CONTROL]);
    let _ = session.take_wire();
    session.on_network(&[
        IAC,
        SB,
        option::TOGGLE_FLOW_CONTROL,
        crate::subneg::flow::OFF,
        IAC,
        SE,
    ]);
    assert!(!session.flags().flow_control);
    session.on_network(&[IAC, SB, option::TOGGLE_FLOW_CONTROL, 99, IAC, SE]);
    assert!(
        !session.flags().flow_control,
        "a malformed command changes nothing"
    );
}

#[test]
fn linemode_agreement_states_the_mode_and_exports_the_slc_table() {
    let mut session = session();
    session.on_network(&[IAC, DO, option::LINEMODE]);
    let wire = decode(&session.take_wire());
    assert!(wire.contains(&Seen::Negotiate(WILL, option::LINEMODE)));
    assert!(wire.contains(&Seen::Subnegotiation(
        option::LINEMODE,
        alloc::vec![sub::MODE, mode::EDIT | mode::TRAPSIG | mode::SOFT_TAB]
    )));
    let exported = wire.iter().any(|seen| {
        matches!(seen, Seen::Subnegotiation(opt, params)
            if *opt == option::LINEMODE && params.first() == Some(&sub::SLC))
    });
    assert!(exported, "the SLC table is stated too: {wire:?}");
}

#[test]
fn a_request_for_an_option_the_server_never_asked_for_discloses_nothing() {
    // RFC 855 allows a subnegotiation only for an enabled option. Without the
    // gate a server that never asked could make the client disclose the
    // operator's exported environment, its terminal and its window purely on
    // request.
    let config = Config {
        user: Some(String::from("ada")),
        ..Config::default()
    };
    // NAWS is in the list for a second reason: it travels client to server
    // only, so an inbound one is meaningless however it was negotiated.
    for option in [
        option::NEW_ENVIRON,
        option::TERMINAL_TYPE,
        option::TERMINAL_SPEED,
        option::NAWS,
        option::STATUS,
    ] {
        let mut session = Session::new(&config, "xterm", 9600);
        session.set_terminal_size(Some(TerminalSize::new(24, 80).expect("a grid")));
        let _ = session.take_wire();
        session.on_network(&[IAC, SB, option, cmd::SEND, IAC, SE]);
        assert!(
            session.take_wire().is_empty(),
            "option {option} was answered without being negotiated"
        );
    }
}

#[test]
fn a_flow_control_command_the_peer_never_offered_is_ignored() {
    let mut session = session();
    session.on_network(&[
        IAC,
        SB,
        option::TOGGLE_FLOW_CONTROL,
        crate::subneg::flow::OFF,
        IAC,
        SE,
    ]);
    assert!(
        session.flags().flow_control,
        "an un-negotiated command changes nothing"
    );
}

#[test]
fn a_subnegotiation_for_an_unnegotiated_option_is_ignored() {
    let mut session = session();
    session.on_network(&[IAC, SB, 99, 1, 2, IAC, SE]);
    assert!(session.take_wire().is_empty());
    assert!(session.take_screen().is_empty());
}

// --- relay mode -------------------------------------------------------------

#[test]
fn a_server_that_echoes_puts_the_session_in_character_mode() {
    let mut session = session();
    assert_eq!(session.relay(), Relay::Line, "nothing negotiated yet");
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    assert_eq!(session.relay(), Relay::Character);
    assert!(!session.local_echo(), "the server is echoing for us");
}

#[test]
fn linemode_decides_the_relay_once_it_is_in_force() {
    let mut session = linemode_session();
    assert_eq!(session.relay(), Relay::Line);
    // The server clears EDIT: the client stops editing even though it never
    // negotiated ECHO.
    session.on_network(&[IAC, SB, option::LINEMODE, sub::MODE, 0, IAC, SE]);
    let _ = session.take_wire();
    assert_eq!(session.relay(), Relay::Character);
}

#[test]
fn line_mode_forwards_a_whole_line_with_the_configured_terminator() {
    let mut session = linemode_session();
    session.on_keyboard(b"hello\r");
    assert_eq!(
        session.take_wire(),
        b"hello\r\0".to_vec(),
        "CR NUL is the default line terminator"
    );
    session.flags_mut().crlf = true;
    session.on_keyboard(b"again\r");
    assert_eq!(session.take_wire(), b"again\r\n".to_vec());
}

#[test]
fn line_mode_echoes_locally_and_sends_nothing_until_the_line_ends() {
    let mut session = linemode_session();
    session.on_keyboard(b"par");
    assert!(session.take_wire().is_empty());
    assert_eq!(session.take_screen(), b"par".to_vec());
}

#[test]
fn character_mode_sends_each_keystroke_and_lets_the_server_echo() {
    let mut session = session();
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    let _ = session.take_screen();
    session.on_keyboard(b"ab");
    assert_eq!(session.take_wire(), b"ab".to_vec());
    assert!(
        session.take_screen().is_empty(),
        "echoing here would double every character"
    );
}

#[test]
fn an_iac_in_typed_data_is_doubled_on_the_wire() {
    let mut session = session();
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    session.on_keyboard(&[IAC]);
    assert_eq!(session.take_wire(), alloc::vec![IAC, IAC]);
}

#[test]
fn the_escape_character_stops_the_fold_where_it_was_typed() {
    let mut session = linemode_session();
    let fold = session.on_keyboard(&[b'a', b'b', DEFAULT_ESCAPE, b'q']);
    assert_eq!(fold.escape_at, Some(2));
    assert_eq!(
        session.take_screen(),
        b"ab".to_vec(),
        "only the bytes before the escape were consumed"
    );
}

#[test]
fn a_session_with_no_escape_character_relays_that_byte_as_data() {
    let config = Config {
        escape: None,
        ..Config::default()
    };
    let mut session = Session::new(&config, "T", 1);
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    let fold = session.on_keyboard(&[DEFAULT_ESCAPE]);
    assert_eq!(fold.escape_at, None);
    assert_eq!(session.take_wire(), alloc::vec![DEFAULT_ESCAPE]);
}

#[test]
fn a_trapped_signal_character_becomes_its_telnet_command() {
    let mut session = linemode_session();
    let interrupt = session
        .linemode()
        .slc()
        .char_for(slc::IP)
        .expect("a default interrupt character");
    session.on_keyboard(b"half-typed");
    let _ = session.take_screen();
    session.on_keyboard(&[interrupt]);
    assert_eq!(
        decode(&session.take_wire()),
        alloc::vec![Seen::Command(IP), Seen::Command(DM)],
        "RFC 854 pairs an interrupt with a Synch"
    );
    session.on_keyboard(b"\r");
    assert_eq!(
        session.take_wire(),
        b"\r\0".to_vec(),
        "the abandoned line is never replayed"
    );
}

#[test]
fn autosynch_off_sends_the_interrupt_alone() {
    let mut session = linemode_session();
    session.flags_mut().autosynch = false;
    let interrupt = session
        .linemode()
        .slc()
        .char_for(slc::IP)
        .expect("a default");
    session.on_keyboard(&[interrupt]);
    assert_eq!(decode(&session.take_wire()), alloc::vec![Seen::Command(IP)]);
}

#[test]
fn every_signal_function_maps_to_its_own_command() {
    let cases = [
        (slc::ABORT, ABORT),
        (slc::EOF, XEOF),
        (slc::SUSP, nvt::SUSP),
        (slc::AO, nvt::AO),
        (slc::AYT, AYT),
    ];
    for (function, command) in cases {
        let mut session = linemode_session();
        session.flags_mut().autosynch = false;
        let byte = session
            .linemode()
            .slc()
            .char_for(function)
            .expect("a default");
        session.on_keyboard(&[byte]);
        assert_eq!(
            decode(&session.take_wire()),
            alloc::vec![Seen::Command(command)],
            "function {function}"
        );
    }
}

#[test]
fn localchars_off_sends_a_signal_character_as_data_in_character_mode() {
    let mut session = session();
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    let interrupt = session
        .linemode()
        .slc()
        .char_for(slc::IP)
        .expect("a default");
    session.flags_mut().localchars = false;
    session.on_keyboard(&[interrupt]);
    assert_eq!(session.take_wire(), alloc::vec![interrupt]);
}

#[test]
fn a_changed_editing_mode_drops_the_part_typed_line() {
    let mut session = linemode_session();
    session.on_keyboard(b"half");
    let _ = session.take_screen();
    // The server changes the editing rules; the line cannot be carried across.
    session.on_network(&[
        IAC,
        SB,
        option::LINEMODE,
        sub::MODE,
        mode::EDIT | mode::LIT_ECHO,
        IAC,
        SE,
    ]);
    let _ = session.take_wire();
    session.on_keyboard(b"\r");
    assert_eq!(
        session.take_wire(),
        b"\r\0".to_vec(),
        "the line typed under the old rules is not forwarded under the new ones"
    );
}

#[test]
fn reset_connection_clears_the_negotiation_but_keeps_the_settings() {
    let mut session = session();
    session.flags_mut().crlf = true;
    session.set_escape(Some(b'x'));
    session.environ_mut().define("K", "v").expect("defined");
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    assert!(session.options().remote(option::ECHO));

    session.reset_connection();
    assert!(!session.options().remote(option::ECHO));
    assert_eq!(session.linemode().mask(), 0);
    assert!(session.flags().crlf, "an operator setting survives");
    assert_eq!(session.escape(), Some(b'x'));
    assert_eq!(session.environ().vars().len(), 1);
}

#[test]
fn tracing_is_silent_unless_the_operator_asked_for_it() {
    let mut session = session();
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    assert!(session.take_trace().is_empty());

    session.flags_mut().options = true;
    session.on_network(&[IAC, WILL, option::SUPPRESS_GO_AHEAD]);
    let _ = session.take_wire();
    let trace = session.take_trace();
    assert!(
        trace.iter().any(|line| line.contains("SUPPRESS GO AHEAD")),
        "{trace:?}"
    );
}

#[test]
fn debug_mode_traces_without_a_toggle() {
    let config = Config {
        debug: true,
        ..Config::default()
    };
    let mut session = Session::new(&config, "T", 1);
    session.on_network(&[IAC, WILL, option::ECHO]);
    let _ = session.take_wire();
    assert!(!session.take_trace().is_empty());
}

#[test]
fn an_unnamed_option_traces_by_number_rather_than_an_invented_label() {
    let config = Config {
        debug: true,
        ..Config::default()
    };
    let mut session = Session::new(&config, "T", 1);
    session.on_network(&[IAC, WILL, 200]);
    let _ = session.take_wire();
    let trace = session.take_trace();
    assert!(
        trace.iter().any(|line| line.contains("option 200")),
        "{trace:?}"
    );
}

#[test]
fn arbitrary_network_bytes_never_panic() {
    let mut session = session();
    session.begin(&Config::default());
    let _ = session.take_wire();
    // Every byte value as a command argument, as a subnegotiation option, and
    // as data, through one live session: a hostile server must not be able to
    // crash it or make it answer unboundedly.
    for byte in 0u16..=255 {
        let byte = u8::try_from(byte).expect("0..=255 fits u8");
        session.on_network(&[IAC, byte, byte, IAC, SB, byte, byte, IAC, SE, byte]);
        assert!(
            session.take_wire().len() <= 64,
            "byte {byte} drew an outsized reply"
        );
        let _ = session.take_screen();
        let _ = session.take_trace();
    }
    // A server that closes whatever subnegotiation it left open — the last
    // round leaves one mid-flight, since only `IAC SE` can end one — gets a
    // fully working session back.
    session.on_network(&[IAC, SE]);
    session.on_network(&[IAC, DO, option::TERMINAL_TYPE]);
    let _ = session.take_wire();
    session.on_network(&[IAC, SB, option::TERMINAL_TYPE, cmd::SEND, IAC, SE]);
    assert!(!session.take_wire().is_empty());
}

#[test]
fn an_unterminated_subnegotiation_cannot_grow_state_without_bound() {
    let mut session = session();
    let mut flood = alloc::vec![IAC, SB, option::NEW_ENVIRON];
    flood.extend(core::iter::repeat_n(b'A', 4096));
    session.on_network(&flood);
    assert!(
        session.take_wire().is_empty(),
        "nothing partial is acted on"
    );
    // The parser resynchronises, so the next real message still works.
    session.on_network(&[IAC, SE, IAC, DO, option::TERMINAL_TYPE]);
    let _ = session.take_wire();
    session.on_network(&[IAC, SB, option::TERMINAL_TYPE, cmd::SEND, IAC, SE]);
    assert!(!session.take_wire().is_empty());
}

#[test]
fn the_default_options_table_never_offers_echo() {
    // A client that echoed on the server's behalf would double every
    // character, so the offer is withheld however supported the option is.
    let mut options = Options::new();
    assert_eq!(
        options.on_do(option::ECHO).reply,
        Some((WONT, option::ECHO))
    );
}
