//! Wire tests for the app-data channel: every operation round-trips, and
//! every malformed frame is refused rather than guessed at.

use super::{
    decode_document_reply, encode_document_reply, AppDataRequest, ConfigDocument,
    APPDATA_DOCUMENT_HEADER_LEN, APPDATA_DOCUMENT_MAX, APPDATA_HEADER_LEN, APPDATA_KEY_MAX,
    APPDATA_MAX_REPLY, APPDATA_MAX_REQUEST, APPDATA_VALUE_MAX,
};
use crate::le::{put_u16, put_u32};
use crate::reply::{encode_status_reply, STATUS_REPLY_LEN};
use crate::Errno;

extern crate alloc;
use alloc::string::String;
use alloc::vec;

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

/// The four operations, one representative each.
fn every_operation() -> [AppDataRequest<'static>; 4] {
    [
        AppDataRequest::ConfigRead { capacity: 4096 },
        AppDataRequest::ConfigSet {
            key: "font.size",
            value: "14",
        },
        AppDataRequest::ConfigUnset { key: "recent.0" },
        AppDataRequest::ConfigCommit,
    ]
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
    // The frame is not padded to its widest form: the settings path must not
    // copy a kilobyte of zeroes per call.
    let unset = frame(&AppDataRequest::ConfigUnset { key: "scheme" });
    assert_eq!(unset.len, APPDATA_HEADER_LEN + "scheme".len());
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
fn a_read_may_ask_for_the_length_alone_or_for_the_widest_document() {
    let widest = u32::try_from(APPDATA_DOCUMENT_MAX).expect("the document bound fits a u32");
    for capacity in [0, 1, 4096, widest] {
        let request = AppDataRequest::ConfigRead { capacity };
        assert_eq!(
            AppDataRequest::decode(frame(&request).as_slice()),
            Ok(request)
        );
    }
}

#[test]
fn a_capacity_beyond_the_document_bound_is_refused() {
    // No document can be larger, so a larger buffer claim is a malformed
    // request rather than a generous one.
    let mut out = [0u8; APPDATA_MAX_REQUEST];
    let over = u32::try_from(APPDATA_DOCUMENT_MAX + 1).expect("fits a u32");
    assert_eq!(
        AppDataRequest::ConfigRead { capacity: over }.encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );

    let mut encoded = frame(&AppDataRequest::ConfigRead { capacity: 0 });
    put_u32(&mut encoded.bytes, super::CAPACITY_OFFSET, over);
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::LengthOutOfRange)
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
        AppDataRequest::ConfigUnset { key: &key }.encode(&mut out),
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
    let request = AppDataRequest::ConfigUnset { key: "scheme" };
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
    let encoded = frame(&AppDataRequest::ConfigUnset { key: "scheme" });
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

    for op in [0u16, 5, 0xFFFF] {
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
fn a_capacity_on_anything_but_a_read_is_refused() {
    for request in [
        AppDataRequest::ConfigUnset { key: "k" },
        AppDataRequest::ConfigCommit,
        AppDataRequest::ConfigSet {
            key: "k",
            value: "v",
        },
    ] {
        let mut encoded = frame(&request);
        put_u32(&mut encoded.bytes, super::CAPACITY_OFFSET, 1);
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "{request:?} must not carry a capacity"
        );
    }
}

#[test]
fn a_value_on_anything_but_a_set_is_refused() {
    for op in [
        super::OP_CONFIG_READ,
        super::OP_CONFIG_UNSET,
        super::OP_CONFIG_COMMIT,
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
fn a_read_and_a_commit_name_nothing_at_all() {
    for op in [super::OP_CONFIG_READ, super::OP_CONFIG_COMMIT] {
        let mut encoded = frame(&AppDataRequest::ConfigUnset { key: "k" });
        put_u16(&mut encoded.bytes, super::OP_OFFSET, op);
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "operation {op} must name no key"
        );
    }
}

#[test]
fn an_operation_that_needs_a_key_refuses_an_empty_one() {
    for op in [super::OP_CONFIG_SET, super::OP_CONFIG_UNSET] {
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
    let mut encoded = frame(&AppDataRequest::ConfigUnset { key: "k" });
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
    let encoded = frame(&AppDataRequest::ConfigUnset { key: "k" });
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

    let mut encoded = frame(&AppDataRequest::ConfigUnset { key: "k" });
    encoded.bytes[APPDATA_HEADER_LEN] = 0x80;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_document_reply_round_trips_including_the_empty_document() {
    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    for document in ["", "scheme = dark\n", "a = 1\nb = 2\n"] {
        let capacity = u32::try_from(document.len()).expect("fits a u32");
        let len = encode_document_reply(document, capacity, &mut out).expect("encodes");
        assert_eq!(
            decode_document_reply(&out[..len]),
            Ok(ConfigDocument::Whole(document))
        );
    }
}

#[test]
fn the_widest_document_fits_the_endpoints_reply_bound() {
    let widest: String = core::iter::repeat_n('x', APPDATA_DOCUMENT_MAX).collect();
    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    let len = encode_document_reply(
        &widest,
        u32::try_from(APPDATA_DOCUMENT_MAX).expect("fits a u32"),
        &mut out,
    )
    .expect("encodes");
    assert_eq!(len, APPDATA_MAX_REPLY);
    assert_eq!(
        decode_document_reply(&out[..len]),
        Ok(ConfigDocument::Whole(widest.as_str()))
    );

    let over: String = core::iter::repeat_n('x', APPDATA_DOCUMENT_MAX + 1).collect();
    assert_eq!(
        encode_document_reply(&over, u32::MAX, &mut out),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_document_past_the_declared_capacity_comes_back_as_its_length() {
    // A caller never parses a prefix: an oversize document is answered with
    // the byte count to ask again with, and no body at all.
    let document = "scheme = dark\nfont.size = 14\n";
    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    let capacity = u32::try_from(document.len() - 1).expect("fits a u32");
    let len = encode_document_reply(document, capacity, &mut out).expect("encodes");
    assert_eq!(len, STATUS_REPLY_LEN + APPDATA_DOCUMENT_HEADER_LEN);
    assert_eq!(
        decode_document_reply(&out[..len]),
        Ok(ConfigDocument::NeedsCapacity(document.len()))
    );

    // Exactly enough is enough.
    let len = encode_document_reply(
        document,
        u32::try_from(document.len()).expect("fits a u32"),
        &mut out,
    )
    .expect("encodes");
    assert_eq!(
        decode_document_reply(&out[..len]),
        Ok(ConfigDocument::Whole(document))
    );
}

#[test]
fn a_zero_capacity_read_asks_only_for_the_length() {
    let document = "scheme = dark\n";
    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    let len = encode_document_reply(document, 0, &mut out).expect("encodes");
    assert_eq!(
        decode_document_reply(&out[..len]),
        Ok(ConfigDocument::NeedsCapacity(document.len()))
    );
    // An empty document needs no capacity at all, so it is whole either way.
    let len = encode_document_reply("", 0, &mut out).expect("encodes");
    assert_eq!(
        decode_document_reply(&out[..len]),
        Ok(ConfigDocument::Whole(""))
    );
}

#[test]
fn a_malformed_document_reply_is_refused() {
    let document = "scheme = dark\n";
    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    let len = encode_document_reply(
        document,
        u32::try_from(document.len()).expect("fits a u32"),
        &mut out,
    )
    .expect("encodes");

    // A body shorter or longer than the declared document.
    assert_eq!(
        decode_document_reply(&out[..len - 1]),
        Err(Errno::BadMagic),
        "a short body is not the document the header declared"
    );
    assert_eq!(
        decode_document_reply(&out[..=len]),
        Err(Errno::BadMagic),
        "a trailing byte is not the document the header declared"
    );

    // A declared length beyond the bound.
    let mut over = out.clone();
    put_u32(
        &mut over,
        STATUS_REPLY_LEN,
        u32::try_from(APPDATA_DOCUMENT_MAX + 1).expect("fits a u32"),
    );
    assert_eq!(
        decode_document_reply(&over[..len]),
        Err(Errno::LengthOutOfRange)
    );

    // A header-less frame that still carries a success status.
    assert_eq!(
        decode_document_reply(&encode_status_reply(Ok(()))),
        Err(Errno::BufferTooSmall)
    );

    // Non-UTF-8 document bytes.
    let mut dirty = out.clone();
    dirty[STATUS_REPLY_LEN + APPDATA_DOCUMENT_HEADER_LEN] = 0xFF;
    assert_eq!(decode_document_reply(&dirty[..len]), Err(Errno::OutOfRange));

    // The daemon's own refusal reaches the caller as itself.
    assert_eq!(
        decode_document_reply(&encode_status_reply(Err(Errno::PermissionDenied))),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn a_short_reply_buffer_refuses_the_encode() {
    let mut tight = [0u8; STATUS_REPLY_LEN];
    assert_eq!(
        encode_document_reply("scheme = dark\n", 64, &mut tight),
        Err(Errno::BufferTooSmall)
    );
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
