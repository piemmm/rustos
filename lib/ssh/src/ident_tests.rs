extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use super::{next_line, Ident, IdentError, Line, MAX_BANNER_LINE, MAX_IDENT_LINE};
use crate::Role;

#[test]
fn our_identification_is_the_rfc_form() {
    let ours = Ident::new("TAIRiX_1.0", None).expect("valid");
    assert_eq!(ours.as_bytes(), b"SSH-2.0-TAIRiX_1.0");
    assert_eq!(ours.protocol(), b"2.0");
    assert_eq!(ours.software(), b"TAIRiX_1.0");
    assert_eq!(ours.comment(), None);
    let commented = Ident::new("TAIRiX_1.0", Some("a comment")).expect("valid");
    assert_eq!(commented.as_bytes(), b"SSH-2.0-TAIRiX_1.0 a comment");
    assert_eq!(commented.comment(), Some(&b"a comment"[..]));
}

#[test]
fn our_identification_refuses_what_rfc_4253_forbids() {
    for software in ["", "has space", "has-minus", "tab\t", "caf\u{e9}"] {
        assert_eq!(
            Ident::new(software, None),
            Err(IdentError::InvalidSoftware),
            "{software:?}"
        );
    }
    for comment in ["", "line\nbreak", "\u{7f}", "caf\u{e9}"] {
        assert_eq!(
            Ident::new("x", Some(comment)),
            Err(IdentError::InvalidComment),
            "{comment:?}"
        );
    }
    let longest = "a".repeat(MAX_IDENT_LINE - 2 - b"SSH-2.0-".len());
    assert!(Ident::new(&longest, None).is_ok());
    let over = "a".repeat(longest.len() + 1);
    assert_eq!(Ident::new(&over, None), Err(IdentError::TooLong));
}

#[test]
fn a_peer_identification_is_parsed_as_sent() {
    let openssh = Ident::parse(b"SSH-2.0-OpenSSH_10.2").expect("OpenSSH's own line");
    assert_eq!(openssh.software(), b"OpenSSH_10.2");
    assert_eq!(openssh.comment(), None);
    let debian = Ident::parse(b"SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13").expect("a comment");
    assert_eq!(debian.software(), b"OpenSSH_9.6p1");
    assert_eq!(debian.comment(), Some(&b"Ubuntu-3ubuntu13"[..]));
    // A minus sign in the software field is against RFC 4253 and sent anyway.
    let cisco = Ident::parse(b"SSH-2.0-Cisco-1.25").expect("deployed peers send this");
    assert_eq!(cisco.software(), b"Cisco-1.25");
    let compatible = Ident::parse(b"SSH-1.99-OpenSSH_3.9").expect("RFC 4253 5.1");
    assert_eq!(compatible.protocol(), b"1.99");
    // The hashed form is exactly what arrived.
    let raw = b"SSH-2.0-x \x1b[2Jodd but legal";
    assert_eq!(
        Ident::parse(raw).expect("comments are opaque").as_bytes(),
        raw
    );
}

#[test]
fn an_unsupported_or_malformed_identification_is_refused() {
    for (line, err) in [
        (&b"SSH-1.5-old"[..], IdentError::UnsupportedVersion),
        (b"SSH-3.0-future", IdentError::UnsupportedVersion),
        (b"SSH-2.00-padded", IdentError::UnsupportedVersion),
        (b"SSH-2.0", IdentError::Malformed),
        (b"SSH-2.0-", IdentError::Malformed),
        (b"SSH-2.0- comment", IdentError::Malformed),
        (b"SSH-2.0-a\0b", IdentError::Malformed),
        (b"SSH-2.0-a\x7f", IdentError::Malformed),
        (b"ssh-2.0-lower", IdentError::Malformed),
    ] {
        assert_eq!(Ident::parse(line), Err(err), "{line:?}");
    }
}

#[test]
fn lines_end_at_lf_with_or_without_cr() {
    for (stream, text, consumed) in [
        (&b"SSH-2.0-a\r\nrest"[..], &b"SSH-2.0-a"[..], 11),
        (b"SSH-2.0-a\nrest", b"SSH-2.0-a", 10),
    ] {
        assert_eq!(
            next_line(stream, Role::Client, 0),
            Ok(Line::Ident { text, consumed }),
            "{stream:?}"
        );
    }
}

#[test]
fn a_stray_cr_or_nul_is_refused_at_once() {
    assert_eq!(
        next_line(b"SSH-2.0-a\rb", Role::Server, 0),
        Err(IdentError::Malformed)
    );
    assert_eq!(
        next_line(b"SSH-2.0-\0", Role::Server, 0),
        Err(IdentError::Malformed)
    );
    assert_eq!(
        next_line(b"hello\0", Role::Server, 0),
        Err(IdentError::Malformed)
    );
    // A CR at the end of what has arrived may still be followed by LF.
    assert_eq!(
        next_line(b"SSH-2.0-a\r", Role::Server, 0),
        Ok(Line::Incomplete { scanned: 9 })
    );
}

#[test]
fn a_server_may_send_banner_lines_and_a_client_may_not() {
    assert_eq!(
        next_line(b"Welcome\r\nSSH-2.0-x\r\n", Role::Server, 0),
        Ok(Line::Banner {
            text: b"Welcome",
            consumed: 9
        })
    );
    assert_eq!(
        next_line(b"\r\n", Role::Server, 0),
        Ok(Line::Banner {
            text: b"",
            consumed: 2
        })
    );
    assert_eq!(
        next_line(b"Welcome\r\n", Role::Client, 0),
        Err(IdentError::UnexpectedLine)
    );
    // Refused from the first byte that proves it, not at the line's end.
    assert_eq!(
        next_line(b"G", Role::Client, 0),
        Err(IdentError::UnexpectedLine)
    );
    assert_eq!(
        next_line(b"SSX", Role::Client, 0),
        Err(IdentError::UnexpectedLine)
    );
    assert_eq!(
        next_line(b"SS", Role::Client, 0),
        Ok(Line::Incomplete { scanned: 2 })
    );
    // A short line that only began like an identification is still a line.
    assert_eq!(
        next_line(b"SS\n", Role::Client, 0),
        Err(IdentError::UnexpectedLine)
    );
}

#[test]
fn each_kind_of_line_has_its_own_bound() {
    let mut ident = b"SSH-2.0-".to_vec();
    ident.resize(MAX_IDENT_LINE - 2, b'a');
    let mut at_bound = ident.clone();
    at_bound.extend_from_slice(b"\r\n");
    assert!(matches!(
        next_line(&at_bound, Role::Server, 0),
        Ok(Line::Ident { .. })
    ));
    ident.push(b'a');
    ident.extend_from_slice(b"\r\n");
    assert_eq!(next_line(&ident, Role::Server, 0), Err(IdentError::TooLong));
    // Refused once the bound is reached with no terminator, not later.
    assert_eq!(
        next_line(&[b'S'; 0][..], Role::Server, 0),
        Ok(Line::Incomplete { scanned: 0 })
    );
    let unterminated: Vec<u8> = b"SSH-2.0-"
        .iter()
        .copied()
        .chain([b'a'; MAX_IDENT_LINE])
        .collect();
    assert_eq!(
        next_line(&unterminated, Role::Server, 0),
        Err(IdentError::TooLong)
    );

    let mut banner = vec![b'x'; MAX_BANNER_LINE - 2];
    banner.extend_from_slice(b"\r\n");
    assert!(matches!(
        next_line(&banner, Role::Server, 0),
        Ok(Line::Banner { .. })
    ));
    let long_banner = vec![b'x'; MAX_BANNER_LINE];
    assert_eq!(
        next_line(&long_banner, Role::Server, 0),
        Err(IdentError::TooLong)
    );
}

#[test]
fn a_line_is_found_however_the_stream_is_chunked() {
    let stream = b"banner one\r\nbanner two\nSSH-2.0-OpenSSH_10.2\r\n\x00\x00\x00\x0c";
    let mut offset = 0;
    let mut resume = 0;
    let mut seen = Vec::new();
    for cut in 1..=stream.len() {
        match next_line(&stream[offset..cut], Role::Server, resume).expect("well-formed") {
            Line::Incomplete { scanned } => resume = scanned,
            Line::Banner { text, consumed } => {
                seen.push(text.to_vec());
                offset += consumed;
                resume = 0;
            }
            Line::Ident { text, consumed } => {
                seen.push(text.to_vec());
                offset += consumed;
                break;
            }
        }
    }
    assert_eq!(
        seen,
        [&b"banner one"[..], b"banner two", b"SSH-2.0-OpenSSH_10.2"]
    );
    assert_eq!(
        &stream[offset..],
        b"\x00\x00\x00\x0c",
        "packet bytes are left untouched"
    );
}

#[test]
fn a_line_arriving_a_byte_at_a_time_is_examined_once() {
    // A server trickling a long banner must cost linear work, not a rescan
    // of the whole line per byte.
    let mut line = vec![b'x'; MAX_BANNER_LINE - 2];
    line[100] = b'\r';
    line[101] = b'\n';
    let mut resume = 0;
    for len in 1..=90 {
        match next_line(&line[..len], Role::Server, resume) {
            Ok(Line::Incomplete { scanned }) => {
                assert_eq!(scanned, len, "every byte so far is examined, none twice");
                resume = scanned;
            }
            other => panic!("{other:?}"),
        }
    }
    // What was examined is not examined again: a byte behind the resume
    // point that a fresh scan would refuse is never looked at.
    assert_eq!(
        next_line(b"ab\0cdef", Role::Server, 3),
        Ok(Line::Incomplete { scanned: 7 })
    );
    assert_eq!(
        next_line(b"ab\0cdef", Role::Server, 0),
        Err(IdentError::Malformed)
    );
    // A CR at the end is examined again when the next byte arrives.
    assert_eq!(
        next_line(&line[..101], Role::Server, 90),
        Ok(Line::Incomplete { scanned: 100 })
    );
    assert_eq!(
        next_line(&line[..102], Role::Server, 100),
        Ok(Line::Banner {
            text: &line[..100],
            consumed: 102
        })
    );
    // Resuming from anywhere already examined never changes the answer.
    let rest = &line[102..];
    for cut in (0..rest.len()).step_by(97) {
        let fresh = next_line(&rest[..cut], Role::Server, 0);
        if let Ok(Line::Incomplete { scanned }) = fresh {
            assert_eq!(next_line(&rest[..cut], Role::Server, scanned / 2), fresh);
        }
    }
}
