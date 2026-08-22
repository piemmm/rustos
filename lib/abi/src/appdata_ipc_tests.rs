//! Wire tests for the app-data channel: every operation round-trips, and
//! every malformed frame is refused rather than guessed at.

use super::{
    decode_document_reply, encode_document_reply, AppDataRequest, ConfigDocument, ConfigScope,
    APPDATA_DOCUMENT_HEADER_LEN, APPDATA_DOCUMENT_MAX, APPDATA_HEADER_LEN, APPDATA_KEY_MAX,
    APPDATA_MAX_REPLY, APPDATA_MAX_REQUEST, APPDATA_VALUE_MAX,
};
use crate::appinfo::BUNDLE_ID_MAX;
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

/// The five operations, one representative each, on the private scope.
fn every_operation() -> [AppDataRequest<'static>; 5] {
    [
        AppDataRequest::ConfigRead {
            scope: ConfigScope::Private,
            capacity: 4096,
        },
        AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
            key: "font.size",
            value: "14",
        },
        AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: "recent.0",
        },
        AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        },
        AppDataRequest::PublicRead {
            bundle_id: "os.tairix.terminal",
            capacity: 4096,
        },
    ]
}

/// The four own-store operations on `scope`.
fn every_scoped_operation(scope: ConfigScope) -> [AppDataRequest<'static>; 4] {
    [
        AppDataRequest::ConfigRead {
            scope,
            capacity: 64,
        },
        AppDataRequest::ConfigSet {
            scope,
            key: "k",
            value: "v",
        },
        AppDataRequest::ConfigUnset { scope, key: "k" },
        AppDataRequest::ConfigCommit { scope },
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
fn both_scopes_round_trip_on_every_own_store_operation() {
    for scope in [ConfigScope::Private, ConfigScope::Public] {
        for request in every_scoped_operation(scope) {
            assert_eq!(
                AppDataRequest::decode(frame(&request).as_slice()),
                Ok(request),
                "{request:?} must survive the wire"
            );
        }
    }
}

#[test]
fn the_two_scopes_are_distinct_on_the_wire() {
    // A private request must never decode as a public one: the scope is the
    // whole of what separates a document nothing else may read from one every
    // app may.
    for private in every_scoped_operation(ConfigScope::Private) {
        let encoded = frame(&private);
        let decoded = AppDataRequest::decode(encoded.as_slice()).expect("decodes");
        assert_eq!(decoded, private);
        for public in every_scoped_operation(ConfigScope::Public) {
            assert_ne!(decoded, public);
        }
    }
    assert_ne!(
        ConfigScope::Private.as_wire(),
        ConfigScope::Public.as_wire()
    );
}

#[test]
fn a_scope_outside_the_closed_set_is_refused() {
    // Zero included: an all-zero frame must not decode as the private scope,
    // so a request that forgot to name one is refused rather than served.
    for wire in [0u8, 3, 0xFF] {
        assert_eq!(ConfigScope::from_wire(wire), Err(Errno::OutOfRange));
        let mut encoded = frame(&AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        });
        encoded.bytes[super::SCOPE_OFFSET] = wire;
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::OutOfRange),
            "scope {wire} is outside the closed set"
        );
    }
    for scope in [ConfigScope::Private, ConfigScope::Public] {
        assert_eq!(ConfigScope::from_wire(scope.as_wire()), Ok(scope));
    }
}

#[test]
fn a_foreign_read_cannot_name_a_scope() {
    // The one request shape that names another application is public by
    // construction: there is no scope field to set, so no frame can ask for
    // another app's private document.
    let mut encoded = frame(&AppDataRequest::PublicRead {
        bundle_id: "os.tairix.terminal",
        capacity: 32,
    });
    for wire in [
        ConfigScope::Private.as_wire(),
        ConfigScope::Public.as_wire(),
    ] {
        encoded.bytes[super::SCOPE_OFFSET] = wire;
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "a foreign read must carry no scope byte"
        );
    }
}

#[test]
fn only_a_foreign_read_may_name_an_application() {
    // An own-store operation names no store, so a bundle identifier on one is
    // a frame that does not mean what its operation says.
    for request in every_scoped_operation(ConfigScope::Private) {
        let mut bytes = [0u8; APPDATA_MAX_REQUEST];
        let len = request.encode(&mut bytes).expect("encodes");
        let id = b"os.tairix.terminal";
        // Splice the identifier in ahead of the key and value the record
        // already carries, exactly where the wire puts it.
        let mut spliced = [0u8; APPDATA_MAX_REQUEST];
        spliced[..APPDATA_HEADER_LEN].copy_from_slice(&bytes[..APPDATA_HEADER_LEN]);
        spliced[super::BUNDLE_LEN_OFFSET] = u8::try_from(id.len()).expect("fits a u8");
        spliced[APPDATA_HEADER_LEN..APPDATA_HEADER_LEN + id.len()].copy_from_slice(id);
        spliced[APPDATA_HEADER_LEN + id.len()..len + id.len()]
            .copy_from_slice(&bytes[APPDATA_HEADER_LEN..len]);
        assert_eq!(
            AppDataRequest::decode(&spliced[..len + id.len()]),
            Err(Errno::BadMagic),
            "{request:?} must name no application"
        );
    }
}

#[test]
fn a_foreign_read_applies_the_bundle_id_grammar() {
    // The identifier becomes a path component in the store tree, so nothing
    // that could traverse out of one may cross the wire at all.
    for id in [
        "..",
        ".",
        "os/tairix",
        "os..tairix",
        "OS.tairix",
        ".hidden",
        "os.tairix.",
        "os tairix",
    ] {
        let mut out = [0u8; APPDATA_MAX_REQUEST];
        let len = AppDataRequest::PublicRead {
            bundle_id: id,
            capacity: 0,
        }
        .encode(&mut out)
        .expect("the codec bounds the length, not the grammar");
        assert!(
            matches!(
                AppDataRequest::decode(&out[..len]),
                Err(Errno::OutOfRange | Errno::LengthOutOfRange)
            ),
            "`{id}` must never reach the daemon"
        );
    }

    // And an empty identifier names no application at all.
    let mut out = [0u8; APPDATA_MAX_REQUEST];
    let len = AppDataRequest::PublicRead {
        bundle_id: "",
        capacity: 0,
    }
    .encode(&mut out)
    .expect("encodes");
    assert_eq!(
        AppDataRequest::decode(&out[..len]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn the_widest_legal_bundle_id_round_trips() {
    let id = "a".repeat(BUNDLE_ID_MAX);
    let request = AppDataRequest::PublicRead {
        bundle_id: &id,
        capacity: 0,
    };
    let encoded = frame(&request);
    assert_eq!(encoded.len, APPDATA_HEADER_LEN + BUNDLE_ID_MAX);
    assert_eq!(AppDataRequest::decode(encoded.as_slice()), Ok(request));

    let over = "a".repeat(BUNDLE_ID_MAX + 1);
    let mut out = [0u8; APPDATA_MAX_REQUEST];
    assert_eq!(
        AppDataRequest::PublicRead {
            bundle_id: &over,
            capacity: 0,
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_request_is_only_as_long_as_its_payload() {
    // The frame is not padded to its widest form: the settings path must not
    // copy a kilobyte of zeroes per call.
    let unset = frame(&AppDataRequest::ConfigUnset {
        scope: ConfigScope::Private,
        key: "scheme",
    });
    assert_eq!(unset.len, APPDATA_HEADER_LEN + "scheme".len());
    let commit = frame(&AppDataRequest::ConfigCommit {
        scope: ConfigScope::Private,
    });
    assert_eq!(commit.len, APPDATA_HEADER_LEN);
    assert!(commit.len < APPDATA_MAX_REQUEST);
}

#[test]
fn the_widest_record_is_a_set_and_not_a_sum_of_every_field() {
    // No operation carries an application identifier *and* a setting, so the
    // endpoint's request bound is the wider of the two shapes rather than
    // their sum — which would over-allocate every buffer in the system.
    const {
        assert!(APPDATA_MAX_REQUEST == APPDATA_HEADER_LEN + APPDATA_KEY_MAX + APPDATA_VALUE_MAX);
        assert!(APPDATA_HEADER_LEN + BUNDLE_ID_MAX < APPDATA_MAX_REQUEST);
    }
}

#[test]
fn an_empty_value_is_a_value() {
    // "set to nothing" is a real setting and must survive the wire as one.
    let request = AppDataRequest::ConfigSet {
        scope: ConfigScope::Private,
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
        for request in [
            AppDataRequest::ConfigRead {
                scope: ConfigScope::Public,
                capacity,
            },
            AppDataRequest::PublicRead {
                bundle_id: "os.tairix.terminal",
                capacity,
            },
        ] {
            assert_eq!(
                AppDataRequest::decode(frame(&request).as_slice()),
                Ok(request)
            );
        }
    }
}

#[test]
fn a_capacity_beyond_the_document_bound_is_refused() {
    // No document can be larger, so a larger buffer claim is a malformed
    // request rather than a generous one.
    let mut out = [0u8; APPDATA_MAX_REQUEST];
    let over = u32::try_from(APPDATA_DOCUMENT_MAX + 1).expect("fits a u32");
    assert_eq!(
        AppDataRequest::ConfigRead {
            scope: ConfigScope::Private,
            capacity: over
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        AppDataRequest::PublicRead {
            bundle_id: "os.tairix.terminal",
            capacity: over
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );

    let mut encoded = frame(&AppDataRequest::ConfigRead {
        scope: ConfigScope::Private,
        capacity: 0,
    });
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
        scope: ConfigScope::Private,
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
        AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: &key
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
    let value = "v".repeat(APPDATA_VALUE_MAX + 1);
    assert_eq!(
        AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
            key: "k",
            value: &value,
        }
        .encode(&mut out),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_short_buffer_refuses_the_encode_rather_than_truncating() {
    let request = AppDataRequest::ConfigUnset {
        scope: ConfigScope::Private,
        key: "scheme",
    };
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
    let encoded = frame(&AppDataRequest::ConfigUnset {
        scope: ConfigScope::Private,
        key: "scheme",
    });
    assert_eq!(
        AppDataRequest::decode(&encoded.bytes[..=encoded.len]),
        Err(Errno::BadMagic)
    );
}

#[test]
fn bad_magic_version_and_operation_are_refused() {
    let commit = AppDataRequest::ConfigCommit {
        scope: ConfigScope::Private,
    };
    let mut encoded = frame(&commit);
    encoded.bytes[0] ^= 0xFF;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::BadMagic)
    );

    let mut encoded = frame(&commit);
    put_u16(&mut encoded.bytes, 4, 0xBEEF);
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::AbiVersionUnsupported)
    );

    for op in [0u16, 6, 0xFFFF] {
        let mut encoded = frame(&commit);
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
        AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: "k",
        },
        AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        },
        AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
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
        super::OP_PUBLIC_READ,
    ] {
        let mut encoded = frame(&AppDataRequest::ConfigSet {
            scope: ConfigScope::Private,
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
    for op in [
        super::OP_CONFIG_READ,
        super::OP_CONFIG_COMMIT,
        super::OP_PUBLIC_READ,
    ] {
        let mut encoded = frame(&AppDataRequest::ConfigUnset {
            scope: ConfigScope::Private,
            key: "k",
        });
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
        let mut encoded = frame(&AppDataRequest::ConfigCommit {
            scope: ConfigScope::Private,
        });
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
    let mut encoded = frame(&AppDataRequest::ConfigUnset {
        scope: ConfigScope::Private,
        key: "k",
    });
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
        scope: ConfigScope::Private,
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

    let mut encoded = frame(&AppDataRequest::PublicRead {
        bundle_id: "os.tairix.terminal",
        capacity: 0,
    });
    encoded.bytes[super::BUNDLE_LEN_OFFSET] = u8::try_from(BUNDLE_ID_MAX + 1).expect("fits a u8");
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_declared_length_beyond_the_frame_is_refused() {
    let encoded = frame(&AppDataRequest::ConfigUnset {
        scope: ConfigScope::Private,
        key: "k",
    });
    let mut bytes = encoded.bytes;
    put_u16(&mut bytes, super::KEY_LEN_OFFSET, 64);
    assert_eq!(
        AppDataRequest::decode(&bytes[..encoded.len]),
        Err(Errno::BufferTooSmall)
    );

    let encoded = frame(&AppDataRequest::PublicRead {
        bundle_id: "os.tairix.terminal",
        capacity: 0,
    });
    let mut bytes = encoded.bytes;
    bytes[super::BUNDLE_LEN_OFFSET] = u8::try_from(BUNDLE_ID_MAX).expect("fits a u8");
    assert_eq!(
        AppDataRequest::decode(&bytes[..encoded.len]),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn non_utf8_text_is_refused() {
    let mut encoded = frame(&AppDataRequest::ConfigSet {
        scope: ConfigScope::Private,
        key: "k",
        value: "v",
    });
    encoded.bytes[encoded.len - 1] = 0xFF;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::OutOfRange)
    );

    let mut encoded = frame(&AppDataRequest::ConfigUnset {
        scope: ConfigScope::Private,
        key: "k",
    });
    encoded.bytes[APPDATA_HEADER_LEN] = 0x80;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::OutOfRange)
    );

    let mut encoded = frame(&AppDataRequest::PublicRead {
        bundle_id: "os.tairix.terminal",
        capacity: 0,
    });
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
