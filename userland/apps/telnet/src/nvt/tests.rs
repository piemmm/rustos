//! Host tests for the RFC 854 codec: the receive parser's decoding and its
//! fail-closed matrix, and the transmit cooking.

use alloc::vec::Vec;

use super::{
    escape_into, push_command, push_eol, push_negotiate, push_subnegotiation, NvtEvent, Parser,
    TransmitMode, ABORT, AYT, DM, DO, DONT, EOR, GA, IAC, MAX_SUBNEG_LEN, NOP, SB, SE, SUSP, WILL,
    WONT, XEOF,
};

/// Feed `chunks` through one parser in order and collect the events as owned
/// values, so an assertion can outlive the borrowed data runs.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Owned {
    Data(Vec<u8>),
    Command(u8),
    Negotiate(u8, u8),
    Subnegotiation(u8, Vec<u8>),
    Refused(u8),
    Unknown(u8),
}

fn drive(chunks: &[&[u8]]) -> Vec<Owned> {
    let mut parser = Parser::new();
    let mut out = Vec::new();
    for chunk in chunks {
        parser.feed(chunk, |event| {
            out.push(match event {
                NvtEvent::Data(bytes) => Owned::Data(bytes.to_vec()),
                NvtEvent::Command(byte) => Owned::Command(byte),
                NvtEvent::Negotiate { verb, option } => Owned::Negotiate(verb, option),
                NvtEvent::Subnegotiation { option, params } => {
                    Owned::Subnegotiation(option, params.to_vec())
                }
                NvtEvent::SubnegotiationRefused { option } => Owned::Refused(option),
                NvtEvent::UnknownCommand(byte) => Owned::Unknown(byte),
            });
        });
    }
    out
}

#[test]
fn plain_data_passes_through_as_one_run() {
    assert_eq!(
        drive(&[b"hello"]),
        alloc::vec![Owned::Data(b"hello".to_vec())]
    );
}

#[test]
fn an_escaped_iac_collapses_to_one_data_byte() {
    assert_eq!(
        drive(&[&[b'a', IAC, IAC, b'b']]),
        alloc::vec![
            Owned::Data(alloc::vec![b'a']),
            Owned::Data(alloc::vec![IAC]),
            Owned::Data(alloc::vec![b'b']),
        ]
    );
}

#[test]
fn every_standalone_command_decodes() {
    for command in [NOP, DM, AYT, GA, EOR, ABORT, SUSP, XEOF] {
        assert_eq!(
            drive(&[&[IAC, command]]),
            alloc::vec![Owned::Command(command)],
            "command {command}"
        );
    }
}

#[test]
fn every_negotiation_verb_decodes_with_its_option() {
    for verb in [WILL, WONT, DO, DONT] {
        assert_eq!(
            drive(&[&[IAC, verb, 3]]),
            alloc::vec![Owned::Negotiate(verb, 3)],
            "verb {verb}"
        );
    }
}

#[test]
fn a_subnegotiation_decodes_with_its_parameters() {
    assert_eq!(
        drive(&[&[IAC, SB, 24, 0, b'v', b't', IAC, SE]]),
        alloc::vec![Owned::Subnegotiation(24, alloc::vec![0, b'v', b't'])]
    );
}

#[test]
fn an_escaped_iac_inside_a_subnegotiation_is_one_parameter_byte() {
    assert_eq!(
        drive(&[&[IAC, SB, 31, 0, IAC, IAC, 0, 24, IAC, SE]]),
        alloc::vec![Owned::Subnegotiation(31, alloc::vec![0, IAC, 0, 24])]
    );
}

#[test]
fn a_command_split_across_reads_still_decodes() {
    // Every boundary of `IAC SB 24 0 IAC SE` split one byte at a time.
    let message = [IAC, SB, 24, 0, IAC, SE];
    for split in 1..message.len() {
        let (head, tail) = message.split_at(split);
        assert_eq!(
            drive(&[head, tail]),
            alloc::vec![Owned::Subnegotiation(24, alloc::vec![0])],
            "split at {split}"
        );
    }
}

#[test]
fn a_trailing_iac_holds_until_its_command_arrives() {
    assert_eq!(
        drive(&[&[b'x', IAC], &[WILL, 1]]),
        alloc::vec![Owned::Data(alloc::vec![b'x']), Owned::Negotiate(WILL, 1)],
        "the held IAC completes against the next read, not as data"
    );
}

#[test]
fn an_unassigned_command_byte_is_reported_and_the_stream_stays_in_sync() {
    assert_eq!(
        drive(&[&[IAC, 200, b'o', b'k']]),
        alloc::vec![Owned::Unknown(200), Owned::Data(b"ok".to_vec())]
    );
}

#[test]
fn a_bare_se_outside_a_subnegotiation_is_reported_not_applied() {
    assert_eq!(
        drive(&[&[IAC, SE, b'z']]),
        alloc::vec![Owned::Unknown(SE), Owned::Data(alloc::vec![b'z'])]
    );
}

#[test]
fn an_over_long_subnegotiation_is_discarded_whole_and_resynchronises() {
    let mut message = alloc::vec![IAC, SB, 39];
    message.extend(core::iter::repeat_n(b'A', MAX_SUBNEG_LEN + 64));
    message.extend_from_slice(&[IAC, SE]);
    message.extend_from_slice(b"after");
    assert_eq!(
        drive(&[&message]),
        alloc::vec![Owned::Refused(39), Owned::Data(b"after".to_vec())],
        "nothing partial is surfaced and parsing resumes after IAC SE"
    );
}

#[test]
fn a_command_other_than_se_inside_a_subnegotiation_discards_it() {
    // RFC 855 permits only `IAC SE` and an escaped `IAC` inside a
    // subnegotiation; `IAC NOP` is malformed, so the whole region is dropped.
    assert_eq!(
        drive(&[&[IAC, SB, 34, 1, IAC, NOP, b'q']]),
        alloc::vec![Owned::Refused(34), Owned::Data(alloc::vec![b'q'])]
    );
}

#[test]
fn an_unterminated_subnegotiation_surfaces_nothing() {
    let mut message = alloc::vec![IAC, SB, 34];
    message.extend(core::iter::repeat_n(1u8, 32));
    assert!(
        drive(&[&message]).is_empty(),
        "an incomplete subnegotiation is never reported as complete"
    );
}

#[test]
fn arbitrary_bytes_never_desynchronise_the_parser() {
    // Every single byte value, fed after an IAC, either decodes or is
    // reported — and the parser is always back in the data state afterwards,
    // so one hostile byte can never swallow the rest of the stream.
    for byte in 0u16..=255 {
        let byte = u8::try_from(byte).expect("0..=255 fits u8");
        let events = drive(&[&[IAC, byte], b"tail"]);
        let tail_seen = events.iter().any(|e| match e {
            Owned::Data(bytes) => bytes.as_slice() == b"tail",
            _ => false,
        });
        // `SB` legitimately consumes the tail as its option and parameters,
        // and a negotiation verb consumes exactly one following byte.
        let consumes_tail = byte == SB || matches!(byte, WILL | WONT | DO | DONT);
        assert_eq!(tail_seen, !consumes_tail, "byte {byte}: {events:?}");
    }
}

#[test]
fn reset_returns_a_mid_subnegotiation_parser_to_the_data_state() {
    let mut parser = Parser::new();
    parser.feed(&[IAC, SB, 24, 1], |_| {});
    parser.reset();
    let mut seen = Vec::new();
    parser.feed(b"x", |event| {
        seen.push(matches!(event, NvtEvent::Data(bytes) if bytes == b"x"));
    });
    assert_eq!(seen, alloc::vec![true]);
}

#[test]
fn transmit_doubles_iac_in_every_mode() {
    for binary in [false, true] {
        let mut out = Vec::new();
        escape_into(
            &[b'a', IAC, b'b'],
            TransmitMode {
                binary,
                crlf: false,
                crmod: false,
            },
            &mut out,
        );
        assert_eq!(out, alloc::vec![b'a', IAC, IAC, b'b'], "binary={binary}");
    }
}

#[test]
fn transmit_maps_cr_to_the_configured_line_terminator() {
    let cases = [
        (
            TransmitMode {
                binary: false,
                crlf: false,
                crmod: false,
            },
            b"a\r\0b".to_vec(),
        ),
        (
            TransmitMode {
                binary: false,
                crlf: true,
                crmod: false,
            },
            b"a\r\nb".to_vec(),
        ),
        (
            TransmitMode {
                binary: true,
                crlf: false,
                crmod: false,
            },
            b"a\rb".to_vec(),
        ),
    ];
    for (mode, expected) in cases {
        let mut out = Vec::new();
        escape_into(b"a\rb", mode, &mut out);
        assert_eq!(out, expected, "{mode:?}");
    }
}

#[test]
fn crmod_maps_a_local_line_feed_to_the_line_terminator() {
    let mut out = Vec::new();
    escape_into(
        b"a\nb",
        TransmitMode {
            binary: false,
            crlf: true,
            crmod: true,
        },
        &mut out,
    );
    assert_eq!(out, b"a\r\nb".to_vec());

    // Without `crmod` a bare line feed is legal NVT and passes through.
    let mut plain = Vec::new();
    escape_into(
        b"a\nb",
        TransmitMode {
            binary: false,
            crlf: true,
            crmod: false,
        },
        &mut plain,
    );
    assert_eq!(plain, b"a\nb".to_vec());
}

#[test]
fn push_eol_is_the_one_line_terminator_decision() {
    let mut out = Vec::new();
    push_eol(TransmitMode::default(), &mut out);
    assert_eq!(out, b"\r\0".to_vec());
}

#[test]
fn emitted_frames_reparse_to_what_was_asked_for() {
    let mut out = Vec::new();
    push_command(AYT, &mut out);
    push_negotiate(DO, 34, &mut out);
    push_subnegotiation(34, &[1, IAC, 2], &mut out);
    escape_into(b"hi", TransmitMode::default(), &mut out);
    assert_eq!(
        drive(&[&out]),
        alloc::vec![
            Owned::Command(AYT),
            Owned::Negotiate(DO, 34),
            Owned::Subnegotiation(34, alloc::vec![1, IAC, 2]),
            Owned::Data(b"hi".to_vec()),
        ],
        "an IAC inside subnegotiation parameters survives the round trip"
    );
}
