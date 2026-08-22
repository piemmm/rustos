//! Wire tests for the app-data channel: every operation round-trips, and
//! every malformed frame is refused rather than guessed at.

use super::{
    decode_value_reply, encode_value_reply, AppDataKeyRecord, AppDataRequest, APPDATA_HEADER_LEN,
    APPDATA_KEY_MAX, APPDATA_LIST_PAGE_MAX, APPDATA_MAX_LIST_REPLY, APPDATA_MAX_REPLY,
    APPDATA_MAX_REQUEST, APPDATA_MAX_VALUE_REPLY, APPDATA_VALUE_MAX,
};
use crate::le::{put_u16, put_u32};
use crate::reply::{decode_page_reply, encode_page_reply, encode_status_reply, STATUS_REPLY_LEN};
use crate::Errno;

/// A request buffer wide enough for any legal record, plus the encoded length.
struct Frame {
    bytes: [u8; APPDATA_MAX_REQUEST],
    len: usize,
}

impl Frame {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Encode `request` into a fresh full-width buffer.
fn frame(request: &AppDataRequest<'_>) -> Frame {
    let mut bytes = [0u8; APPDATA_MAX_REQUEST];
    let len = request.encode(&mut bytes).expect("a legal request encodes");
    Frame { bytes, len }
}

/// The five operations, one representative each.
fn every_operation() -> [AppDataRequest<'static>; 5] {
    [
        AppDataRequest::ConfigGet { key: "font.size" },
        AppDataRequest::ConfigSet {
            key: "font.size",
            value: "14",
        },
        AppDataRequest::ConfigUnset { key: "recent.0" },
        AppDataRequest::ConfigCommit,
        AppDataRequest::ConfigList {
            prefix: "recent.",
            cursor: 7,
        },
    ]
}

/// `recent.<index>` rendered into `buf`, for the listing-page test.
fn recent_key(index: u16, buf: &mut [u8; 16]) -> &str {
    const STEM: &[u8] = b"recent.";
    buf[..STEM.len()].copy_from_slice(STEM);
    let mut len = STEM.len();
    if index >= 10 {
        buf[len] = b'0' + u8::try_from(index / 10).expect("the page bound is under 100");
        len += 1;
    }
    buf[len] = b'0' + u8::try_from(index % 10).expect("a single digit");
    len += 1;
    core::str::from_utf8(&buf[..len]).expect("ascii")
}

#[test]
fn every_operation_round_trips() {
    for request in every_operation() {
        let encoded = frame(&request);
        assert_eq!(encoded.len, request.wire_len());
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Ok(request),
            "{request:?} must survive the wire"
        );
    }
}

#[test]
fn a_request_is_only_as_long_as_its_payload() {
    // The frame is not padded to its widest form: the settings-read path
    // must not copy a kilobyte of zeroes per call.
    let get = frame(&AppDataRequest::ConfigGet { key: "scheme" });
    assert_eq!(get.len, APPDATA_HEADER_LEN + "scheme".len());
    let commit = frame(&AppDataRequest::ConfigCommit);
    assert_eq!(commit.len, APPDATA_HEADER_LEN);
    assert!(commit.len < APPDATA_MAX_REQUEST);
}

#[test]
fn an_empty_value_is_a_value() {
    // "set to nothing" is a real setting and must survive the wire as one.
    let request = AppDataRequest::ConfigSet {
        key: "greeting",
        value: "",
    };
    assert_eq!(
        AppDataRequest::decode(frame(&request).as_slice()),
        Ok(request)
    );
}

#[test]
fn an_empty_listing_prefix_lists_everything() {
    let request = AppDataRequest::ConfigList {
        prefix: "",
        cursor: 0,
    };
    assert_eq!(
        AppDataRequest::decode(frame(&request).as_slice()),
        Ok(request)
    );
}

#[test]
fn the_widest_legal_key_and_value_round_trip() {
    let key = "a".repeat(APPDATA_KEY_MAX);
    let value = "v".repeat(APPDATA_VALUE_MAX);
    let request = AppDataRequest::ConfigSet {
        key: &key,
        value: &value,
    };
    let encoded = frame(&request);
    assert_eq!(encoded.len, APPDATA_MAX_REQUEST);
    assert_eq!(AppDataRequest::decode(encoded.as_slice()), Ok(request));
}

#[test]
fn an_over_long_key_or_value_is_refused_at_encode() {
    let key = "a".repeat(APPDATA_KEY_MAX + 1);
    let mut out = [0u8; APPDATA_MAX_REQUEST];
    assert_eq!(
        AppDataRequest::ConfigGet { key: &key }.encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
    let value = "v".repeat(APPDATA_VALUE_MAX + 1);
    assert_eq!(
        AppDataRequest::ConfigSet {
            key: "k",
            value: &value,
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_short_buffer_refuses_the_encode_rather_than_truncating() {
    let request = AppDataRequest::ConfigGet { key: "scheme" };
    let mut out = [0u8; APPDATA_HEADER_LEN + 5];
    assert_eq!(out.len(), request.wire_len() - 1);
    assert_eq!(request.encode(&mut out), Err(Errno::BufferTooSmall));
}

#[test]
fn a_truncated_frame_is_refused() {
    for request in every_operation() {
        let encoded = frame(&request);
        assert_eq!(
            AppDataRequest::decode(&encoded.bytes[..encoded.len - 1]),
            Err(Errno::BufferTooSmall),
            "{request:?} truncated must refuse"
        );
    }
}

#[test]
fn a_trailing_byte_past_the_payload_is_refused() {
    // A request is exactly one record; a longer frame is not the one the
    // sender described.
    let encoded = frame(&AppDataRequest::ConfigGet { key: "scheme" });
    assert_eq!(
        AppDataRequest::decode(&encoded.bytes[..=encoded.len]),
        Err(Errno::BadMagic)
    );
}

#[test]
fn bad_magic_version_and_operation_are_refused() {
    let mut encoded = frame(&AppDataRequest::ConfigCommit);
    encoded.bytes[0] ^= 0xFF;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::BadMagic)
    );

    let mut encoded = frame(&AppDataRequest::ConfigCommit);
    put_u16(&mut encoded.bytes, 4, 0xBEEF);
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::AbiVersionUnsupported)
    );

    for op in [0u16, 6, 0xFFFF] {
        let mut encoded = frame(&AppDataRequest::ConfigCommit);
        put_u16(&mut encoded.bytes, super::OP_OFFSET, op);
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::OutOfRange),
            "operation {op} is outside the closed set"
        );
    }
}

#[test]
fn a_cursor_on_anything_but_a_listing_is_refused() {
    for request in [
        AppDataRequest::ConfigGet { key: "k" },
        AppDataRequest::ConfigUnset { key: "k" },
        AppDataRequest::ConfigCommit,
        AppDataRequest::ConfigSet {
            key: "k",
            value: "v",
        },
    ] {
        let mut encoded = frame(&request);
        put_u32(&mut encoded.bytes, super::CURSOR_OFFSET, 1);
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "{request:?} must not carry a cursor"
        );
    }
}

#[test]
fn a_value_on_anything_but_a_set_is_refused() {
    for op in [
        super::OP_CONFIG_GET,
        super::OP_CONFIG_UNSET,
        super::OP_CONFIG_COMMIT,
        super::OP_CONFIG_LIST,
    ] {
        let mut encoded = frame(&AppDataRequest::ConfigSet {
            key: "k",
            value: "v",
        });
        put_u16(&mut encoded.bytes, super::OP_OFFSET, op);
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "operation {op} must not carry a value"
        );
    }
}

#[test]
fn a_commit_names_nothing_at_all() {
    let mut encoded = frame(&AppDataRequest::ConfigGet { key: "k" });
    put_u16(
        &mut encoded.bytes,
        super::OP_OFFSET,
        super::OP_CONFIG_COMMIT,
    );
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::BadMagic)
    );
}

#[test]
fn an_operation_that_needs_a_key_refuses_an_empty_one() {
    for op in [
        super::OP_CONFIG_GET,
        super::OP_CONFIG_SET,
        super::OP_CONFIG_UNSET,
    ] {
        let mut encoded = frame(&AppDataRequest::ConfigCommit);
        put_u16(&mut encoded.bytes, super::OP_OFFSET, op);
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::LengthOutOfRange),
            "operation {op} requires a key"
        );
    }
}

#[test]
fn a_declared_length_beyond_its_bound_is_refused_before_the_read() {
    let mut encoded = frame(&AppDataRequest::ConfigGet { key: "k" });
    put_u16(
        &mut encoded.bytes,
        super::KEY_LEN_OFFSET,
        u16::try_from(APPDATA_KEY_MAX + 1).expect("fits a u16"),
    );
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::LengthOutOfRange)
    );

    let mut encoded = frame(&AppDataRequest::ConfigSet {
        key: "k",
        value: "v",
    });
    put_u16(
        &mut encoded.bytes,
        super::VALUE_LEN_OFFSET,
        u16::try_from(APPDATA_VALUE_MAX + 1).expect("fits a u16"),
    );
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_declared_length_beyond_the_frame_is_refused() {
    let encoded = frame(&AppDataRequest::ConfigGet { key: "k" });
    let mut bytes = encoded.bytes;
    put_u16(&mut bytes, super::KEY_LEN_OFFSET, 64);
    assert_eq!(
        AppDataRequest::decode(&bytes[..encoded.len]),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn non_utf8_text_is_refused() {
    let mut encoded = frame(&AppDataRequest::ConfigSet {
        key: "k",
        value: "v",
    });
    encoded.bytes[encoded.len - 1] = 0xFF;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::OutOfRange)
    );

    let mut encoded = frame(&AppDataRequest::ConfigGet { key: "k" });
    encoded.bytes[APPDATA_HEADER_LEN] = 0x80;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_value_reply_round_trips_including_the_empty_value() {
    let mut out = [0u8; APPDATA_MAX_VALUE_REPLY];
    for value in ["", "14", "/Users/ada/Documents/notes.txt"] {
        let len = encode_value_reply(value, &mut out).expect("encodes");
        assert_eq!(decode_value_reply(&out[..len]), Ok(value));
    }
    let widest = "v".repeat(APPDATA_VALUE_MAX);
    let len = encode_value_reply(&widest, &mut out).expect("encodes");
    assert_eq!(len, APPDATA_MAX_VALUE_REPLY);
    assert_eq!(decode_value_reply(&out[..len]), Ok(widest.as_str()));

    let over = "v".repeat(APPDATA_VALUE_MAX + 1);
    assert_eq!(
        encode_value_reply(&over, &mut out),
        Err(Errno::LengthOutOfRange)
    );
    let mut tight = [0u8; STATUS_REPLY_LEN];
    assert_eq!(
        encode_value_reply("x", &mut tight),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn an_absent_key_is_not_an_empty_value() {
    // The daemon answers a missing key with the shared status frame, so a
    // caller never has to guess which of the two it got.
    let refusal = encode_status_reply(Err(Errno::NotFound));
    assert_eq!(decode_value_reply(&refusal), Err(Errno::NotFound));

    let mut out = [0u8; APPDATA_MAX_VALUE_REPLY];
    let len = encode_value_reply("", &mut out).expect("encodes");
    assert_eq!(decode_value_reply(&out[..len]), Ok(""));
}

#[test]
fn a_malformed_value_reply_is_refused() {
    let mut out = [0u8; APPDATA_MAX_VALUE_REPLY];
    let len = encode_value_reply("dark", &mut out).expect("encodes");

    // A dirty reserved pair.
    let mut dirty = out;
    put_u16(&mut dirty, STATUS_REPLY_LEN + 2, 1);
    assert_eq!(decode_value_reply(&dirty[..len]), Err(Errno::BadMagic));

    // A trailing byte past the declared value.
    assert_eq!(decode_value_reply(&out[..=len]), Err(Errno::BadMagic));

    // A declared length beyond the bound, and one beyond the frame.
    let mut over = out;
    put_u16(
        &mut over,
        STATUS_REPLY_LEN,
        u16::try_from(APPDATA_VALUE_MAX + 1).expect("fits a u16"),
    );
    assert_eq!(
        decode_value_reply(&over[..len]),
        Err(Errno::LengthOutOfRange)
    );

    let mut short = out;
    put_u16(&mut short, STATUS_REPLY_LEN, 64);
    assert_eq!(
        decode_value_reply(&short[..len]),
        Err(Errno::BufferTooSmall)
    );

    // A header-less frame that still carries a success status.
    assert_eq!(
        decode_value_reply(&encode_status_reply(Ok(()))),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn a_key_record_round_trips_and_refuses_a_malformed_one() {
    let record = AppDataKeyRecord::new("effects.blur").expect("a legal key");
    let bytes = record.to_le_bytes();
    let decoded = AppDataKeyRecord::from_bytes(&bytes).expect("decodes");
    assert_eq!(decoded.as_str(), "effects.blur");
    assert_eq!(decoded, record);

    let mut dirty = bytes;
    dirty[2 + "effects.blur".len()] = 0xAA;
    assert_eq!(AppDataKeyRecord::from_bytes(&dirty), Err(Errno::BadMagic));

    let mut empty = bytes;
    put_u16(&mut empty, 0, 0);
    assert_eq!(
        AppDataKeyRecord::from_bytes(&empty),
        Err(Errno::LengthOutOfRange)
    );

    let mut over = bytes;
    put_u16(
        &mut over,
        0,
        u16::try_from(APPDATA_KEY_MAX + 1).expect("fits a u16"),
    );
    assert_eq!(
        AppDataKeyRecord::from_bytes(&over),
        Err(Errno::LengthOutOfRange)
    );

    assert_eq!(
        AppDataKeyRecord::from_bytes(&bytes[..bytes.len() - 1]),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(AppDataKeyRecord::new(""), Err(Errno::LengthOutOfRange));
    let long = "a".repeat(APPDATA_KEY_MAX + 1);
    assert_eq!(AppDataKeyRecord::new(&long), Err(Errno::LengthOutOfRange));
}

#[test]
fn a_full_listing_page_fits_the_endpoints_reply_bound() {
    let mut records = [[0u8; AppDataKeyRecord::WIRE_LEN]; APPDATA_LIST_PAGE_MAX as usize];
    for (index, slot) in records.iter_mut().enumerate() {
        let mut buf = [0u8; 16];
        let key = recent_key(
            u16::try_from(index).expect("under the page bound"),
            &mut buf,
        );
        *slot = AppDataKeyRecord::new(key)
            .expect("a legal key")
            .to_le_bytes();
    }
    let mut out = [0u8; APPDATA_MAX_LIST_REPLY];
    let len = encode_page_reply(&records, APPDATA_LIST_PAGE_MAX, &mut out).expect("encodes");
    assert_eq!(len, APPDATA_MAX_LIST_REPLY);
    const { assert!(APPDATA_MAX_REPLY >= APPDATA_MAX_LIST_REPLY) };
    const { assert!(APPDATA_MAX_REPLY >= APPDATA_MAX_VALUE_REPLY) };

    let (count, body) = decode_page_reply(
        &out[..len],
        AppDataKeyRecord::WIRE_LEN,
        APPDATA_LIST_PAGE_MAX,
    )
    .expect("decodes");
    assert_eq!(count, APPDATA_LIST_PAGE_MAX);
    for (index, chunk) in body.chunks(AppDataKeyRecord::WIRE_LEN).enumerate() {
        let mut buf = [0u8; 16];
        let expected = recent_key(
            u16::try_from(index).expect("under the page bound"),
            &mut buf,
        );
        let record = AppDataKeyRecord::from_bytes(chunk).expect("decodes");
        assert_eq!(record.as_str(), expected);
    }
}

#[test]
fn the_endpoint_is_reserved_but_not_seat_scoped() {
    // App data is not a property of a seat, and a headless machine serves it
    // exactly as a graphical one does — so the seat-lease bind exception must
    // not reach it.
    assert!(crate::ipc::is_reserved_endpoint(super::APPDATA_ENDPOINT));
    assert!(!crate::ipc::is_seat_scoped_endpoint(
        super::APPDATA_ENDPOINT
    ));
}
