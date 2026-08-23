//! Host tests for the per-option subnegotiation codecs.

use alloc::vec::Vec;

use tairix_abi::TerminalSize;

use super::{
    cmd, env, flow, flow_control_enabled, push_flow_control, push_naws, push_status,
    push_terminal_speed, push_terminal_type, split_request, Environ, EnvironFault, Request,
    MAX_ENVIRON_NAME, MAX_ENVIRON_VALUE, MAX_ENVIRON_VARS,
};
use crate::nvt::{NvtEvent, Parser, DO, IAC, SB, SE, WILL};
use crate::option::{self, Options};

/// Decode `bytes` back through the real receive parser and return the one
/// subnegotiation it holds, so an encoder is only ever asserted against the
/// parser that has to read it.
fn only_subnegotiation(bytes: &[u8]) -> (u8, Vec<u8>) {
    let mut parser = Parser::new();
    let mut found = None;
    parser.feed(bytes, |event| {
        if let NvtEvent::Subnegotiation { option, params } = event {
            found = Some((option, params.to_vec()));
        }
    });
    found.expect("the encoder emitted exactly one complete subnegotiation")
}

#[test]
fn split_request_decodes_the_three_commands_and_refuses_the_rest() {
    assert_eq!(
        split_request(&[cmd::SEND]),
        Some((Request::Send, [].as_slice()))
    );
    assert_eq!(
        split_request(&[cmd::IS, b'x']),
        Some((Request::Is, b"x".as_slice()))
    );
    assert_eq!(
        split_request(&[cmd::INFO]),
        Some((Request::Info, [].as_slice()))
    );
    assert_eq!(split_request(&[]), None, "an empty payload fails closed");
    assert_eq!(split_request(&[9]), None, "an unknown command fails closed");
}

#[test]
fn terminal_type_is_upper_cased_and_control_bytes_are_dropped() {
    let mut out = Vec::new();
    push_terminal_type("xterm\u{1}-256color\n", &mut out);
    let (option, params) = only_subnegotiation(&out);
    assert_eq!(option, option::TERMINAL_TYPE);
    assert_eq!(params[0], cmd::IS);
    assert_eq!(
        &params[1..],
        b"XTERM-256COLOR",
        "a control byte from the local TERM never reaches the wire"
    );
}

#[test]
fn naws_reports_columns_then_rows_big_endian() {
    let mut out = Vec::new();
    push_naws(TerminalSize::new(24, 80).expect("a valid grid"), &mut out);
    let (option, params) = only_subnegotiation(&out);
    assert_eq!(option, option::NAWS);
    assert_eq!(params, alloc::vec![0, 80, 0, 24]);
}

#[test]
fn naws_survives_a_width_that_collides_with_iac() {
    // A 255-column terminal makes the width's low octet `IAC`, which must be
    // doubled on the wire and collapsed back by the parser.
    let mut out = Vec::new();
    push_naws(TerminalSize::new(24, 255).expect("a valid grid"), &mut out);
    assert!(
        out.windows(2).any(|pair| pair == [IAC, IAC]),
        "the colliding octet is escaped: {out:?}"
    );
    let (_, params) = only_subnegotiation(&out);
    assert_eq!(params, alloc::vec![0, 255, 0, 24]);
}

#[test]
fn terminal_speed_reports_the_same_rate_both_ways() {
    let mut out = Vec::new();
    push_terminal_speed(38_400, &mut out);
    let (option, params) = only_subnegotiation(&out);
    assert_eq!(option, option::TERMINAL_SPEED);
    assert_eq!(params[0], cmd::IS);
    assert_eq!(&params[1..], b"38400,38400");
}

#[test]
fn flow_control_commands_decode_and_a_malformed_one_fails_closed() {
    assert_eq!(flow_control_enabled(&[flow::OFF]), Some(false));
    assert_eq!(flow_control_enabled(&[flow::ON]), Some(true));
    assert_eq!(flow_control_enabled(&[flow::RESTART_ANY]), Some(true));
    assert_eq!(flow_control_enabled(&[flow::RESTART_XON]), Some(true));
    assert_eq!(flow_control_enabled(&[]), None);
    assert_eq!(flow_control_enabled(&[9]), None);
    assert_eq!(
        flow_control_enabled(&[flow::ON, flow::OFF]),
        None,
        "two commands in one payload is malformed"
    );

    let mut out = Vec::new();
    push_flow_control(flow::ON, &mut out);
    assert_eq!(
        only_subnegotiation(&out),
        (option::TOGGLE_FLOW_CONTROL, alloc::vec![flow::ON])
    );
}

#[test]
fn status_reports_exactly_the_negotiated_state() {
    let mut options = Options::new();
    options.on_will(option::ECHO);
    options.on_do(option::NAWS);
    let mut out = Vec::new();
    push_status(&options, &mut out);
    let (option, params) = only_subnegotiation(&out);
    assert_eq!(option, option::STATUS);
    assert_eq!(params[0], cmd::IS);
    assert_eq!(
        &params[1..],
        &[DO, option::ECHO, WILL, option::NAWS],
        "one DO for the peer's side, one WILL for ours, in option-code order"
    );
}

#[test]
fn status_of_a_fresh_session_claims_nothing() {
    let mut out = Vec::new();
    push_status(&Options::new(), &mut out);
    assert_eq!(only_subnegotiation(&out).1, alloc::vec![cmd::IS]);
}

#[test]
fn an_environment_starts_empty_and_discloses_nothing() {
    let environ = Environ::new();
    assert!(environ.vars().is_empty());
    let mut out = Vec::new();
    environ.push_is_reply(&[], &mut out);
    assert_eq!(
        only_subnegotiation(&out),
        (option::NEW_ENVIRON, alloc::vec![cmd::IS]),
        "a client that was told nothing tells the server nothing"
    );
}

#[test]
fn a_defined_variable_is_withheld_until_it_is_exported() {
    let mut environ = Environ::new();
    environ.define("USER", "ada").expect("defined");
    let mut out = Vec::new();
    environ.push_is_reply(&[], &mut out);
    assert_eq!(
        only_subnegotiation(&out).1,
        alloc::vec![cmd::IS],
        "defining is not disclosing"
    );

    environ.set_exported("USER", true).expect("exported");
    let mut exported = Vec::new();
    environ.push_is_reply(&[], &mut exported);
    let (_, params) = only_subnegotiation(&exported);
    let mut expected = alloc::vec![cmd::IS, env::VAR];
    expected.extend_from_slice(b"USER");
    expected.push(env::VALUE);
    expected.extend_from_slice(b"ada");
    assert_eq!(params, expected, "USER is a RFC 1572 well-known VAR");
}

#[test]
fn an_unrecognised_name_is_a_uservar() {
    let mut environ = Environ::new();
    environ.define("PROJECT", "tairix").expect("defined");
    environ.set_exported("PROJECT", true).expect("exported");
    let mut out = Vec::new();
    environ.push_is_reply(&[], &mut out);
    assert_eq!(only_subnegotiation(&out).1[1], env::USERVAR);
}

#[test]
fn a_send_naming_variables_answers_only_those_that_are_exported() {
    let mut environ = Environ::new();
    for (name, value) in [("USER", "ada"), ("PRINTER", "lp0")] {
        environ.define(name, value).expect("defined");
    }
    environ.set_exported("USER", true).expect("exported");
    let mut request = alloc::vec![env::VAR];
    request.extend_from_slice(b"USER");
    request.push(env::VAR);
    request.extend_from_slice(b"PRINTER");
    let mut out = Vec::new();
    environ.push_is_reply(&request, &mut out);
    let (_, params) = only_subnegotiation(&out);
    let text = alloc::string::String::from_utf8_lossy(&params).into_owned();
    assert!(text.contains("USER"), "{text}");
    assert!(
        !text.contains("lp0"),
        "an unexported value is never disclosed: {text}"
    );
    assert!(
        text.contains("PRINTER"),
        "the name is answered without a value, per RFC 1572: {text}"
    );
}

#[test]
fn a_value_holding_a_type_code_byte_is_refused_rather_than_disclosed() {
    // `\u{3}` is RFC 1572's `USERVAR` code. The table refuses it up front — a
    // value the operator could not have typed at the prompt anyway — so a
    // colliding byte never reaches the encoder from here.
    let mut environ = Environ::new();
    assert_eq!(
        environ.define("K", "a\u{3}b"),
        Err(EnvironFault::NotPrintable)
    );
}

#[test]
fn the_encoder_escapes_a_colliding_byte_whatever_it_is_handed() {
    // The encoder is total independently of the table's validation: handed a
    // byte that reads as a type code, it escapes it rather than emitting an
    // ambiguous stream.
    let mut params = Vec::new();
    super::push_var(false, "K", Some("a\u{3}b"), &mut params);
    assert_eq!(
        params,
        alloc::vec![
            env::USERVAR,
            b'K',
            env::VALUE,
            b'a',
            env::ESC,
            env::USERVAR,
            b'b'
        ]
    );
}

#[test]
fn define_enforces_its_bounds_and_leaves_the_table_untouched() {
    let mut environ = Environ::new();
    assert_eq!(environ.define("", "v"), Err(EnvironFault::NameLength));
    assert_eq!(
        environ.define(&"n".repeat(MAX_ENVIRON_NAME + 1), "v"),
        Err(EnvironFault::NameLength)
    );
    assert_eq!(
        environ.define("n", &"v".repeat(MAX_ENVIRON_VALUE + 1)),
        Err(EnvironFault::ValueLength)
    );
    assert_eq!(
        environ.define("n\u{1}", "v"),
        Err(EnvironFault::NotPrintable)
    );
    assert_eq!(
        environ.define("n", "v\u{1}"),
        Err(EnvironFault::NotPrintable)
    );
    assert!(environ.vars().is_empty(), "no refusal stored anything");
}

#[test]
fn the_table_fills_closed_and_a_redefinition_is_not_a_new_entry() {
    let mut environ = Environ::new();
    for index in 0..MAX_ENVIRON_VARS {
        environ
            .define(&alloc::format!("V{index}"), "x")
            .expect("within the bound");
    }
    assert_eq!(environ.define("EXTRA", "x"), Err(EnvironFault::TableFull));
    // A redefinition of an existing name still succeeds at capacity.
    assert_eq!(environ.define("V0", "y"), Ok(()));
    assert_eq!(environ.vars().len(), MAX_ENVIRON_VARS);
    assert_eq!(environ.vars()[0].value, "y");
}

#[test]
fn undefine_and_export_refuse_an_unknown_name() {
    let mut environ = Environ::new();
    assert_eq!(environ.undefine("NOPE"), Err(EnvironFault::Unknown));
    assert_eq!(
        environ.set_exported("NOPE", true),
        Err(EnvironFault::Unknown)
    );
    environ.define("K", "v").expect("defined");
    assert_eq!(environ.undefine("K"), Ok(()));
    assert!(environ.vars().is_empty());
}

#[test]
fn a_reply_never_exceeds_what_the_parser_will_read_back() {
    // The table's own bounds must keep the encoded reply inside the region a
    // peer is allowed to send, so an export can always be read back.
    let mut environ = Environ::new();
    for index in 0..MAX_ENVIRON_VARS {
        environ
            .define(&alloc::format!("{index:0>3}"), &"v".repeat(8))
            .expect("within the bound");
        environ
            .set_exported(&alloc::format!("{index:0>3}"), true)
            .expect("exported");
    }
    let mut out = Vec::new();
    environ.push_is_reply(&[], &mut out);
    let (_, params) = only_subnegotiation(&out);
    assert!(
        params.len() <= crate::nvt::MAX_SUBNEG_LEN,
        "a full table encodes to {} bytes",
        params.len()
    );
    assert_eq!(out[0], IAC);
    assert_eq!(out[1], SB);
    assert_eq!(&out[out.len() - 2..], &[IAC, SE]);
}
