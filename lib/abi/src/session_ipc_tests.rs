//! Wire tests for the `session-v1` graphical-login protocol: round trips,
//! and a fail-closed refusal for every malformation the login screen or a
//! hostile caller could put on the wire.

use super::{
    decode_account_page, encode_account_page, session_wake_endpoint, AccountPage, SessionAccount,
    SessionRequest, SessionVerdict, SessionWake, SESSION_ACCOUNTS_HEADER_LEN,
    SESSION_ACCOUNTS_MAGIC, SESSION_ACCOUNTS_PER_PAGE, SESSION_ACCOUNT_RECORD_LEN,
    SESSION_DISPLAY_NAME_MAX, SESSION_ENDPOINT, SESSION_LOGIN_NAME_MAX, SESSION_MAX_REPLY,
    SESSION_MAX_REQUEST, SESSION_REQUEST_MAGIC, SESSION_SECRET_MAX, SESSION_VERDICT_LEN,
    SESSION_VERDICT_MAGIC, SESSION_VERSION, SESSION_WAKE_LEN,
};
use crate::ipc::{is_reserved_endpoint, is_seat_scoped_endpoint};
use crate::time::Duration64;
use crate::Errno;

/// One account record, for the page tests.
fn account(display: &str, login: &str, live: bool) -> SessionAccount {
    SessionAccount::new(display, login, live).expect("well-formed fixture")
}

/// `len` ASCII `b'x'` characters, without an allocator (`lib/abi` has none).
fn filler(buf: &mut [u8], len: usize) -> &str {
    buf[..len].fill(b'x');
    core::str::from_utf8(&buf[..len]).expect("ascii")
}

/// Encode `page` into a fresh buffer and hand back the encoded bytes.
fn encoded_page(
    total: u32,
    offset: u32,
    accounts: &[SessionAccount],
) -> ([u8; SESSION_MAX_REPLY], usize) {
    let mut buf = [0u8; SESSION_MAX_REPLY];
    let len = encode_account_page(&mut buf, total, offset, accounts).expect("encodes");
    (buf, len)
}

#[test]
fn the_endpoint_is_reserved_but_not_seat_scoped() {
    // Only a privileged-bind holder may serve it: the authority owns no
    // seat, and holding the seat must not let a process adjudicate
    // passwords.
    assert!(is_reserved_endpoint(SESSION_ENDPOINT));
    assert!(!is_seat_scoped_endpoint(SESSION_ENDPOINT));
}

#[test]
fn accounts_request_round_trips() {
    let mut buf = [0u8; SESSION_MAX_REQUEST];
    for offset in [0u32, 1, 16, u32::MAX] {
        let request = SessionRequest::Accounts { offset };
        let len = request.encode(&mut buf).expect("encodes");
        assert_eq!(len, 12);
        assert_eq!(SessionRequest::decode(&buf[..len]), Ok(request));
    }
}

#[test]
fn a_background_request_round_trips_and_carries_no_body() {
    let mut buf = [0u8; SESSION_MAX_REQUEST];
    let len = SessionRequest::Background
        .encode(&mut buf)
        .expect("encodes");
    assert_eq!(len, 8);
    assert_eq!(
        SessionRequest::decode(&buf[..len]),
        Ok(SessionRequest::Background)
    );
    // Trailing bytes are a different frame, not a background request.
    assert_eq!(
        SessionRequest::decode(&buf[..=len]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn authenticate_request_round_trips() {
    let request = SessionRequest::Authenticate {
        username: "ada",
        password: "byron",
    };
    let mut buf = [0u8; SESSION_MAX_REQUEST];
    let len = request.encode(&mut buf).expect("encodes");
    assert_eq!(SessionRequest::decode(&buf[..len]), Ok(request));
}

#[test]
fn the_longest_legal_authenticate_fits_the_declared_request_bound() {
    let mut user_bytes = [0u8; SESSION_LOGIN_NAME_MAX];
    let mut secret_bytes = [0u8; SESSION_SECRET_MAX];
    let request = SessionRequest::Authenticate {
        username: filler(&mut user_bytes, SESSION_LOGIN_NAME_MAX),
        password: filler(&mut secret_bytes, SESSION_SECRET_MAX),
    };
    let mut buf = [0u8; SESSION_MAX_REQUEST];
    let len = request.encode(&mut buf).expect("encodes");
    assert_eq!(len, SESSION_MAX_REQUEST);
    assert_eq!(SessionRequest::decode(&buf[..len]), Ok(request));
}

#[test]
fn authenticate_refuses_empty_and_over_long_fields_both_ways() {
    let mut buf = [0u8; SESSION_MAX_REQUEST * 2];
    let mut user_bytes = [0u8; SESSION_LOGIN_NAME_MAX + 1];
    let mut secret_bytes = [0u8; SESSION_SECRET_MAX + 1];
    let long_user = filler(&mut user_bytes, SESSION_LOGIN_NAME_MAX + 1);
    let long_secret = filler(&mut secret_bytes, SESSION_SECRET_MAX + 1);
    for request in [
        SessionRequest::Authenticate {
            username: "",
            password: "p",
        },
        SessionRequest::Authenticate {
            username: "u",
            password: "",
        },
        SessionRequest::Authenticate {
            username: long_user,
            password: "p",
        },
        SessionRequest::Authenticate {
            username: "u",
            password: long_secret,
        },
    ] {
        assert_eq!(request.encode(&mut buf), Err(Errno::LengthOutOfRange));
    }

    // A hand-built frame with an empty username is refused at decode too:
    // the wire is never trusted to mirror the encoder.
    let mut bytes = [0u8; 14];
    bytes[..4].copy_from_slice(&SESSION_REQUEST_MAGIC.to_le_bytes());
    bytes[4..6].copy_from_slice(&SESSION_VERSION.to_le_bytes());
    bytes[6] = 1;
    bytes[8..10].copy_from_slice(&0u16.to_le_bytes());
    bytes[10..12].copy_from_slice(&1u16.to_le_bytes());
    bytes[12] = b'p';
    assert_eq!(
        SessionRequest::decode(&bytes[..13]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn request_decode_fails_closed_on_every_malformation() {
    let request = SessionRequest::Authenticate {
        username: "ada",
        password: "byron",
    };
    let mut buf = [0u8; SESSION_MAX_REQUEST];
    let len = request.encode(&mut buf).expect("encodes");

    let mut wrong_magic = buf;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(
        SessionRequest::decode(&wrong_magic[..len]),
        Err(Errno::BadMagic)
    );

    let mut dirty_reserved = buf;
    dirty_reserved[7] = 1;
    assert_eq!(
        SessionRequest::decode(&dirty_reserved[..len]),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = buf;
    wrong_version[4] = 9;
    assert_eq!(
        SessionRequest::decode(&wrong_version[..len]),
        Err(Errno::OutOfRange)
    );

    let mut unknown_opcode = buf;
    unknown_opcode[6] = 7;
    assert_eq!(
        SessionRequest::decode(&unknown_opcode[..len]),
        Err(Errno::OutOfRange)
    );

    let mut not_utf8 = buf;
    not_utf8[10] = 0xFF;
    assert_eq!(
        SessionRequest::decode(&not_utf8[..len]),
        Err(Errno::OutOfRange)
    );

    // Truncated, trailing bytes, a runt, and an over-long buffer.
    assert_eq!(
        SessionRequest::decode(&buf[..len - 1]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        SessionRequest::decode(&buf[..=len]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        SessionRequest::decode(&buf[..7]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        SessionRequest::decode(&[0u8; SESSION_MAX_REQUEST + 1]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_short_output_buffer_is_refused_rather_than_partly_written() {
    let request = SessionRequest::Authenticate {
        username: "ada",
        password: "byron",
    };
    let mut small = [0u8; 8];
    assert_eq!(request.encode(&mut small), Err(Errno::BufferTooSmall));
    assert_eq!(small, [0u8; 8]);
}

#[test]
fn an_account_carries_only_what_a_tile_draws() {
    let acct = account("Ada Lovelace", "ada", true);
    assert_eq!(acct.display_name(), "Ada Lovelace");
    assert_eq!(acct.login_name(), "ada");
    assert!(acct.has_live_session());
    assert!(!account("Bob", "bob", false).has_live_session());
}

#[test]
fn an_account_refuses_an_empty_over_long_or_control_bearing_name() {
    assert_eq!(
        SessionAccount::new("", "ada", false),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        SessionAccount::new("Ada", "", false),
        Err(Errno::LengthOutOfRange)
    );
    let mut display_bytes = [0u8; SESSION_DISPLAY_NAME_MAX + 1];
    let mut login_bytes = [0u8; SESSION_LOGIN_NAME_MAX + 1];
    assert_eq!(
        SessionAccount::new(
            filler(&mut display_bytes, SESSION_DISPLAY_NAME_MAX + 1),
            "ada",
            false
        ),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        SessionAccount::new(
            "Ada",
            filler(&mut login_bytes, SESSION_LOGIN_NAME_MAX + 1),
            false
        ),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        SessionAccount::new("Ada\nLovelace", "ada", false),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn an_account_page_round_trips() {
    let accounts = [
        account("Ada Lovelace", "ada", true),
        account("Grace Hopper", "grace", false),
    ];
    let (buf, len) = encoded_page(2, 0, &accounts);
    let page = decode_account_page(&buf[..len]).expect("decodes");
    assert_eq!(page.total(), 2);
    assert_eq!(page.offset(), 0);
    assert_eq!(page.accounts(), &accounts[..]);
    assert!(page.is_last());
}

#[test]
fn an_empty_page_is_legal_and_final_when_the_machine_has_no_accounts() {
    let (buf, len) = encoded_page(0, 0, &[]);
    assert_eq!(len, SESSION_ACCOUNTS_HEADER_LEN);
    let page = decode_account_page(&buf[..len]).expect("decodes");
    assert!(page.accounts().is_empty());
    assert_eq!(page.total(), 0);
    assert!(page.is_last());
}

#[test]
fn a_full_page_reports_more_to_come_and_fits_the_declared_reply_bound() {
    let accounts: [SessionAccount; SESSION_ACCOUNTS_PER_PAGE] =
        core::array::from_fn(|i| account("Ada Lovelace", "ada", i % 2 == 0));
    let total = u32::try_from(SESSION_ACCOUNTS_PER_PAGE * 3).expect("small");
    let (buf, len) = encoded_page(total, 0, &accounts);
    assert_eq!(len, SESSION_MAX_REPLY);
    let page = decode_account_page(&buf[..len]).expect("decodes");
    assert_eq!(page.accounts().len(), SESSION_ACCOUNTS_PER_PAGE);
    assert!(!page.is_last());

    // The last page of the same list is final.
    let offset = u32::try_from(SESSION_ACCOUNTS_PER_PAGE * 2).expect("small");
    let (buf, len) = encoded_page(total, offset, &accounts);
    let page = decode_account_page(&buf[..len]).expect("decodes");
    assert_eq!(page.offset(), offset);
    assert!(page.is_last());
}

#[test]
fn a_page_that_overruns_its_total_is_refused_both_ways() {
    let accounts = [account("Ada", "ada", false)];
    let mut buf = [0u8; SESSION_MAX_REPLY];
    // Encoding a record the total does not account for is a bug in the
    // authority, refused rather than emitted.
    assert_eq!(
        encode_account_page(&mut buf, 0, 0, &accounts),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        encode_account_page(&mut buf, 1, 1, &accounts),
        Err(Errno::LengthOutOfRange)
    );

    // And a hand-built frame claiming the same is refused at decode.
    let (mut bytes, len) = encoded_page(1, 0, &accounts);
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_account_page(&bytes[..len]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_page_longer_than_one_page_is_refused() {
    let accounts: [SessionAccount; SESSION_ACCOUNTS_PER_PAGE + 1] =
        core::array::from_fn(|_| account("Ada Lovelace", "ada", false));
    let mut buf = [0u8; SESSION_MAX_REPLY + SESSION_ACCOUNT_RECORD_LEN];
    let total = u32::try_from(accounts.len()).expect("small");
    assert_eq!(
        encode_account_page(&mut buf, total, 0, &accounts),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn page_decode_fails_closed_on_every_malformation() {
    let accounts = [account("Ada", "ada", true)];
    let (buf, len) = encoded_page(1, 0, &accounts);

    let mut wrong_magic = buf;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(
        decode_account_page(&wrong_magic[..len]),
        Err(Errno::BadMagic)
    );

    let mut dirty_reserved = buf;
    dirty_reserved[7] = 3;
    assert_eq!(
        decode_account_page(&dirty_reserved[..len]),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = buf;
    wrong_version[4] = 2;
    assert_eq!(
        decode_account_page(&wrong_version[..len]),
        Err(Errno::OutOfRange)
    );

    // A count past the page bound, and a count the length does not match.
    let mut over_count = buf;
    over_count[6] = u8::try_from(SESSION_ACCOUNTS_PER_PAGE + 1).expect("small");
    assert_eq!(
        decode_account_page(&over_count[..len]),
        Err(Errno::LengthOutOfRange)
    );
    let mut lying_count = buf;
    lying_count[6] = 2;
    assert_eq!(
        decode_account_page(&lying_count[..len]),
        Err(Errno::LengthOutOfRange)
    );

    // A record with an undefined flag bit, a dirty reserved byte, an empty
    // name, or a dirty tail behind a name.
    let flags_at = SESSION_ACCOUNTS_HEADER_LEN + SESSION_ACCOUNT_RECORD_LEN - 2;
    let mut bad_flag = buf;
    bad_flag[flags_at] = 0x80;
    assert_eq!(decode_account_page(&bad_flag[..len]), Err(Errno::BadMagic));
    let mut dirty_record = buf;
    dirty_record[flags_at + 1] = 1;
    assert_eq!(
        decode_account_page(&dirty_record[..len]),
        Err(Errno::BadMagic)
    );
    let mut empty_display = buf;
    empty_display[SESSION_ACCOUNTS_HEADER_LEN] = 0;
    assert_eq!(
        decode_account_page(&empty_display[..len]),
        Err(Errno::LengthOutOfRange)
    );
    let mut dirty_tail = buf;
    dirty_tail[SESSION_ACCOUNTS_HEADER_LEN + 1 + accounts[0].display_name().len()] = 0xAA;
    assert_eq!(
        decode_account_page(&dirty_tail[..len]),
        Err(Errno::BadMagic)
    );

    // A runt and an over-long buffer.
    assert_eq!(
        decode_account_page(&buf[..SESSION_ACCOUNTS_HEADER_LEN - 1]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        decode_account_page(&[0u8; SESSION_MAX_REPLY + 1]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_short_page_buffer_is_refused_rather_than_partly_written() {
    let accounts = [account("Ada", "ada", false)];
    let mut small = [0u8; SESSION_ACCOUNTS_HEADER_LEN];
    assert_eq!(
        encode_account_page(&mut small, 1, 0, &accounts),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(small, [0u8; SESSION_ACCOUNTS_HEADER_LEN]);
}

#[test]
fn a_verdict_round_trips_both_ways() {
    let mut buf = [0u8; SESSION_VERDICT_LEN];
    for verdict in [
        SessionVerdict::Accepted,
        SessionVerdict::Refused {
            retry_after: Duration64::ZERO,
        },
        SessionVerdict::Refused {
            retry_after: Duration64::from_secs(30),
        },
        SessionVerdict::Refused {
            retry_after: Duration64::new(4, 500_000_000).expect("canonical"),
        },
    ] {
        let len = verdict.encode(&mut buf).expect("encodes");
        assert_eq!(len, SESSION_VERDICT_LEN);
        assert_eq!(SessionVerdict::decode(&buf[..len]), Ok(verdict));
    }
}

#[test]
fn verdict_decode_fails_closed_on_every_malformation() {
    let mut buf = [0u8; SESSION_VERDICT_LEN];
    SessionVerdict::Accepted.encode(&mut buf).expect("encodes");

    assert_eq!(
        SessionVerdict::decode(&buf[..SESSION_VERDICT_LEN - 1]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        SessionVerdict::decode(&[0u8; SESSION_VERDICT_LEN + 1]),
        Err(Errno::LengthOutOfRange)
    );

    let mut wrong_magic = buf;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(SessionVerdict::decode(&wrong_magic), Err(Errno::BadMagic));

    let mut dirty_reserved = buf;
    dirty_reserved[7] = 1;
    assert_eq!(
        SessionVerdict::decode(&dirty_reserved),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = buf;
    wrong_version[4] = 0;
    assert_eq!(
        SessionVerdict::decode(&wrong_version),
        Err(Errno::OutOfRange)
    );

    let mut unknown_status = buf;
    unknown_status[6] = 2;
    assert_eq!(
        SessionVerdict::decode(&unknown_status),
        Err(Errno::OutOfRange)
    );

    // An acceptance may not carry a cooldown, and a refusal may not carry a
    // negative one: neither is a state the authority can be in.
    let mut accepted_with_cooldown = buf;
    accepted_with_cooldown[8..].copy_from_slice(&Duration64::from_secs(1).to_le_bytes());
    assert_eq!(
        SessionVerdict::decode(&accepted_with_cooldown),
        Err(Errno::OutOfRange)
    );
    let mut negative_cooldown = buf;
    negative_cooldown[6] = 1;
    negative_cooldown[8..].copy_from_slice(&Duration64::from_secs(-1).to_le_bytes());
    assert_eq!(
        SessionVerdict::decode(&negative_cooldown),
        Err(Errno::OutOfRange)
    );

    // A non-canonical nanosecond field is refused by the shared duration
    // decoder rather than normalised.
    let mut non_canonical = buf;
    non_canonical[6] = 1;
    non_canonical[16..20].copy_from_slice(&1_000_000_000u32.to_le_bytes());
    assert_eq!(
        SessionVerdict::decode(&non_canonical),
        Err(Errno::TimestampOutOfRange)
    );
}

#[test]
fn a_reply_of_the_wrong_shape_is_caught_at_the_magic() {
    // A client knows which frame it asked for, so the two reply magics must
    // never decode as each other.
    let mut verdict = [0u8; SESSION_VERDICT_LEN];
    let len = SessionVerdict::Accepted
        .encode(&mut verdict)
        .expect("encodes");
    assert_eq!(decode_account_page(&verdict[..len]), Err(Errno::BadMagic));

    let (page, page_len) = encoded_page(0, 0, &[]);
    assert_eq!(
        SessionVerdict::decode(&page[..page_len]),
        Err(Errno::LengthOutOfRange)
    );
    assert_ne!(SESSION_ACCOUNTS_MAGIC, SESSION_VERDICT_MAGIC);
    assert_ne!(SESSION_ACCOUNTS_MAGIC, SESSION_REQUEST_MAGIC);
    assert_ne!(SESSION_VERDICT_MAGIC, SESSION_REQUEST_MAGIC);
}

#[test]
fn a_wake_message_round_trips_and_fails_closed() {
    let mut buf = [0u8; SESSION_WAKE_LEN];
    for wake in [SessionWake::Foreground, SessionWake::End] {
        let len = wake.encode(&mut buf).expect("encodes");
        assert_eq!(len, SESSION_WAKE_LEN);
        assert_eq!(SessionWake::decode(&buf[..len]), Ok(wake));
    }

    assert_eq!(
        SessionWake::decode(&buf[..SESSION_WAKE_LEN - 1]),
        Err(Errno::LengthOutOfRange)
    );
    let mut wrong_magic = buf;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(SessionWake::decode(&wrong_magic), Err(Errno::BadMagic));
    let mut dirty_reserved = buf;
    dirty_reserved[7] = 1;
    assert_eq!(SessionWake::decode(&dirty_reserved), Err(Errno::BadMagic));
    let mut wrong_version = buf;
    wrong_version[4] = 7;
    assert_eq!(SessionWake::decode(&wrong_version), Err(Errno::OutOfRange));
    let mut unknown_op = buf;
    unknown_op[6] = 0;
    assert_eq!(SessionWake::decode(&unknown_op), Err(Errno::OutOfRange));

    let mut small = [0u8; SESSION_WAKE_LEN - 1];
    assert_eq!(
        SessionWake::Foreground.encode(&mut small),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn a_wake_mailbox_id_is_per_session_and_never_a_reserved_rendezvous() {
    // One distinct, collision-free id per session task, and never a
    // reserved id a squatter could be mistaken for.
    assert_ne!(session_wake_endpoint(1), session_wake_endpoint(2));
    for pid in [1u64, 2, 4096, u64::from(u32::MAX), crate::PID_MAX] {
        assert!(!is_reserved_endpoint(session_wake_endpoint(pid)));
        assert_ne!(session_wake_endpoint(pid), SESSION_ENDPOINT);
        // The tag is a pure prefix even at the widest pid the kernel can
        // draw, so the task id is recoverable and no derivation can fold two
        // sessions onto one mailbox.
        assert_eq!(session_wake_endpoint(pid) & crate::PID_MAX, pid);
    }
    // The tagged namespaces are disjoint: no pid pair can make a session's
    // wake mailbox collide with a Switchboard command mailbox.
    for pid in [1u64, crate::PID_MAX] {
        for other in [1u64, crate::PID_MAX] {
            assert_ne!(
                session_wake_endpoint(pid),
                crate::switchboard_ipc::command_endpoint_for(other)
            );
        }
    }
}

#[test]
fn page_accessors_report_the_encoded_header() {
    let accounts = [account("Ada", "ada", false)];
    let (buf, len) = encoded_page(100, 32, &accounts);
    let page: AccountPage = decode_account_page(&buf[..len]).expect("decodes");
    assert_eq!(page.total(), 100);
    assert_eq!(page.offset(), 32);
    assert_eq!(page.accounts().len(), 1);
    assert!(!page.is_last());
}
