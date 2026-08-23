//! Wire tests for the app-data channel: every operation round-trips, and
//! every malformed frame is refused rather than guessed at.

use super::{
    decode_blob_list_reply, decode_document_reply, decode_grant_reply, decode_quota_reply,
    encode_blob_entry, encode_blob_list_reply, encode_document_reply, encode_grant_reply,
    encode_quota_reply, validate_blob_name, AppDataRequest, BlobEntry, BlobListing, BlobMode,
    BlobQuota, ConfigDocument, ConfigScope, APPDATA_BLOB_ENTRY_LEN, APPDATA_BLOB_LIST_HEADER_LEN,
    APPDATA_BLOB_LIST_MAX, APPDATA_BLOB_MAX_BYTES, APPDATA_BLOB_MAX_COUNT,
    APPDATA_DOCUMENT_HEADER_LEN, APPDATA_DOCUMENT_MAX, APPDATA_GRANT_REPLY_LEN, APPDATA_HEADER_LEN,
    APPDATA_KEY_MAX, APPDATA_MAX_REPLY, APPDATA_MAX_REQUEST, APPDATA_NAME_MAX,
    APPDATA_QUOTA_REPLY_LEN, APPDATA_VALUE_MAX,
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

/// Every operation, one representative each; the configuration ones on the
/// private scope.
fn every_operation() -> [AppDataRequest<'static>; 12] {
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
        AppDataRequest::VaultRead { capacity: 4096 },
        AppDataRequest::VaultSet {
            key: "imap.password",
            value: "hunter2",
        },
        AppDataRequest::VaultUnset {
            key: "smtp.password",
        },
        AppDataRequest::BlobOpen {
            name: "mail.index",
            mode: BlobMode::ReadWrite,
        },
        AppDataRequest::BlobDelete { name: "thumbnails" },
        AppDataRequest::BlobList { capacity: 4096 },
        AppDataRequest::QuotaGet {},
    ]
}

/// The four blob-scope operations, which name no scope and no setting.
fn every_blob_operation() -> [AppDataRequest<'static>; 4] {
    [
        AppDataRequest::BlobOpen {
            name: "index",
            mode: BlobMode::Read,
        },
        AppDataRequest::BlobDelete { name: "index" },
        AppDataRequest::BlobList { capacity: 64 },
        AppDataRequest::QuotaGet {},
    ]
}

/// The three sealed-scope operations, which name no scope and no application.
fn every_vault_operation() -> [AppDataRequest<'static>; 3] {
    [
        AppDataRequest::VaultRead { capacity: 64 },
        AppDataRequest::VaultSet {
            key: "k",
            value: "v",
        },
        AppDataRequest::VaultUnset { key: "k" },
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
    let own = every_scoped_operation(ConfigScope::Private)
        .into_iter()
        .chain(every_vault_operation());
    for request in own {
        let mut bytes = [0u8; APPDATA_MAX_REQUEST];
        let len = request.encode(&mut bytes).expect("encodes");
        let id = b"os.tairix.terminal";
        // Splice the identifier in ahead of the key and value the record
        // already carries, exactly where the wire puts it.
        let mut spliced = [0u8; APPDATA_MAX_REQUEST];
        spliced[..APPDATA_HEADER_LEN].copy_from_slice(&bytes[..APPDATA_HEADER_LEN]);
        spliced[super::NAME_LEN_OFFSET] = u8::try_from(id.len()).expect("fits a u8");
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

    // Zero, one past the highest defined operation, and the far end of the
    // space. Derived rather than written out, so adding an operation cannot
    // quietly turn this case into a live one.
    for op in [0u16, super::OP_QUOTA_GET + 1, 0xFFFF] {
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
        AppDataRequest::VaultSet {
            key: "k",
            value: "v",
        },
        AppDataRequest::VaultUnset { key: "k" },
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
        super::OP_VAULT_READ,
        super::OP_VAULT_UNSET,
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
        super::OP_VAULT_READ,
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
    // Each family is re-labelled from a keyless frame of its *own* family, so
    // the empty key is what the decoder refuses rather than the scope byte a
    // configuration frame carries and a sealed one does not.
    let scoped = AppDataRequest::ConfigCommit {
        scope: ConfigScope::Private,
    };
    let sealed = AppDataRequest::VaultRead { capacity: 0 };
    for (base, op) in [
        (&scoped, super::OP_CONFIG_SET),
        (&scoped, super::OP_CONFIG_UNSET),
        (&sealed, super::OP_VAULT_SET),
        (&sealed, super::OP_VAULT_UNSET),
    ] {
        let mut encoded = frame(base);
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
    encoded.bytes[super::NAME_LEN_OFFSET] = u8::try_from(BUNDLE_ID_MAX + 1).expect("fits a u8");
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
    bytes[super::NAME_LEN_OFFSET] = u8::try_from(BUNDLE_ID_MAX).expect("fits a u8");
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

#[test]
fn every_sealed_operation_round_trips() {
    for request in every_vault_operation() {
        assert_eq!(
            AppDataRequest::decode(frame(&request).as_slice()),
            Ok(request),
            "{request:?} must survive the wire"
        );
    }
}

#[test]
fn a_sealed_operation_cannot_name_a_scope() {
    // The sealed scope is not a `ConfigScope`, so no vault frame carries one:
    // a configuration frame cannot name a secret and a vault frame cannot name
    // a configuration document, in either direction, by construction.
    for request in every_vault_operation() {
        let mut encoded = frame(&request);
        for wire in [
            ConfigScope::Private.as_wire(),
            ConfigScope::Public.as_wire(),
            0xFF,
        ] {
            encoded.bytes[super::SCOPE_OFFSET] = wire;
            assert_eq!(
                AppDataRequest::decode(encoded.as_slice()),
                Err(Errno::BadMagic),
                "{request:?} must carry no scope byte"
            );
        }
    }
}

#[test]
fn a_configuration_operation_cannot_be_reinterpreted_as_a_sealed_one() {
    // The other direction of the same property: a `ConfigSet`'s frame carries a
    // scope byte, so re-labelling it as a `VaultSet` is refused rather than
    // silently sealing what the caller meant to publish in the clear.
    for op in [
        super::OP_VAULT_READ,
        super::OP_VAULT_SET,
        super::OP_VAULT_UNSET,
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
            "operation {op} must carry no scope"
        );
    }
}

#[test]
fn the_widest_secret_is_the_widest_value() {
    // A secret crosses the wire as a setting's value, so the sealed scope adds
    // no width of its own: the request bound is still the widest configuration
    // write. A secret larger than a value is the blob scope's business.
    let request = AppDataRequest::VaultSet {
        key: &String::from_iter(core::iter::repeat_n('k', APPDATA_KEY_MAX)),
        value: &String::from_iter(core::iter::repeat_n('v', APPDATA_VALUE_MAX)),
    };
    let encoded = frame(&request);
    assert_eq!(encoded.len, APPDATA_MAX_REQUEST);
    assert_eq!(AppDataRequest::decode(encoded.as_slice()), Ok(request));
}

#[test]
fn a_blob_operation_carries_no_scope_and_no_setting() {
    // The blob family and the configuration family have no field in common
    // that could select the other's target, so neither can be re-labelled as
    // the other: a blob frame with a scope, or one with a setting spliced in,
    // is refused rather than reaching a document.
    for request in every_blob_operation() {
        let mut encoded = frame(&request);
        encoded.bytes[super::SCOPE_OFFSET] = ConfigScope::Private.as_wire();
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "a blob operation must carry no scope byte"
        );

        let mut encoded = frame(&request);
        put_u16(&mut encoded.bytes, super::KEY_LEN_OFFSET, 1);
        encoded.bytes[encoded.len] = b'k';
        assert_eq!(
            AppDataRequest::decode(&encoded.bytes[..=encoded.len]),
            Err(Errno::BadMagic),
            "a blob operation must carry no key"
        );
    }
}

#[test]
fn a_configuration_operation_cannot_be_reinterpreted_as_a_blob_one() {
    // And the other direction: a `ConfigCommit`'s frame carries a scope, so
    // re-labelling it as a blob delete cannot make the daemon unlink anything.
    for op in [
        super::OP_BLOB_OPEN,
        super::OP_BLOB_DELETE,
        super::OP_BLOB_LIST,
        super::OP_QUOTA_GET,
    ] {
        let mut encoded = frame(&AppDataRequest::ConfigCommit {
            scope: ConfigScope::Public,
        });
        put_u16(&mut encoded.bytes, super::OP_OFFSET, op);
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "operation {op} must carry no scope"
        );
    }
}

#[test]
fn the_blob_modes_are_distinct_and_a_mode_outside_the_set_is_refused() {
    for mode in [BlobMode::Read, BlobMode::ReadWrite] {
        assert_eq!(BlobMode::from_wire(mode.as_wire()), Ok(mode));
        assert_ne!(mode.as_wire(), 0, "an all-zero frame must not open a blob");
    }
    assert!(!BlobMode::Read.is_write());
    assert!(BlobMode::ReadWrite.is_write());

    let mut encoded = frame(&AppDataRequest::BlobOpen {
        name: "index",
        mode: BlobMode::Read,
    });
    for wire in [0u8, 3, 0xFF] {
        encoded.bytes[super::MODE_OFFSET] = wire;
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::OutOfRange),
            "mode {wire} is outside the closed set"
        );
    }
}

#[test]
fn only_a_blob_open_may_name_a_mode() {
    // A mode on an operation that opens nothing is a frame that does not mean
    // what its operation says — including on the foreign read, so a mode
    // cannot be smuggled onto the one shape that names another application.
    let others = every_scoped_operation(ConfigScope::Private)
        .into_iter()
        .chain(every_vault_operation())
        .chain([
            AppDataRequest::BlobDelete { name: "index" },
            AppDataRequest::BlobList { capacity: 32 },
            AppDataRequest::QuotaGet {},
            AppDataRequest::PublicRead {
                bundle_id: "os.tairix.terminal",
                capacity: 32,
            },
        ]);
    for request in others {
        let mut encoded = frame(&request);
        encoded.bytes[super::MODE_OFFSET] = BlobMode::ReadWrite.as_wire();
        assert_eq!(
            AppDataRequest::decode(encoded.as_slice()),
            Err(Errno::BadMagic),
            "{request:?} must carry no mode byte"
        );
    }
}

#[test]
fn a_blob_name_applies_the_store_name_grammar() {
    // A blob name becomes one path component in the application's own blob
    // directory, so it is the same security question a bundle identifier
    // poses and gets the same answer — nothing that could traverse, hide, or
    // case-fold is a name at all.
    for hostile in [
        "", "..", ".", "a/b", "/etc", "A", ".hidden", "a b", "a..b", "a.", ".a", "a\nb",
    ] {
        assert!(
            validate_blob_name(hostile).is_err(),
            "`{hostile}` must never be a blob name"
        );
        let mut bytes = [0u8; APPDATA_MAX_REQUEST];
        let len = AppDataRequest::BlobOpen {
            name: "placeholder",
            mode: BlobMode::Read,
        }
        .encode(&mut bytes)
        .expect("encodes");
        // Splice the hostile name in where the wire puts it, at its own
        // length, so the decoder judges it rather than the encoder.
        let mut spliced = [0u8; APPDATA_MAX_REQUEST];
        spliced[..APPDATA_HEADER_LEN].copy_from_slice(&bytes[..APPDATA_HEADER_LEN]);
        spliced[super::NAME_LEN_OFFSET] = u8::try_from(hostile.len()).expect("fits a u8");
        spliced[APPDATA_HEADER_LEN..APPDATA_HEADER_LEN + hostile.len()]
            .copy_from_slice(hostile.as_bytes());
        let spliced_len = APPDATA_HEADER_LEN + hostile.len();
        assert!(
            AppDataRequest::decode(&spliced[..spliced_len]).is_err(),
            "`{hostile}` must never decode as a blob name"
        );
        let _ = len;
    }
    for legal in ["index", "mail.index", "thumbnails-v2", "a", "a_b.c0"] {
        assert_eq!(validate_blob_name(legal), Ok(()));
    }
}

#[test]
fn the_widest_legal_blob_name_round_trips() {
    let name = String::from_iter(core::iter::repeat_n('b', APPDATA_NAME_MAX));
    let request = AppDataRequest::BlobOpen {
        name: &name,
        mode: BlobMode::ReadWrite,
    };
    let encoded = frame(&request);
    assert_eq!(AppDataRequest::decode(encoded.as_slice()), Ok(request));
    // One byte more cannot even be described: the length prefix is a byte and
    // the bound is checked before it is written.
    let over = String::from_iter(core::iter::repeat_n('b', APPDATA_NAME_MAX + 1));
    assert_eq!(
        AppDataRequest::BlobOpen {
            name: &over,
            mode: BlobMode::ReadWrite,
        }
        .encode(&mut [0u8; APPDATA_MAX_REQUEST]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_reserved_header_byte_must_be_zero() {
    // A frame that filled the reserved byte means something this version
    // cannot read, so it is refused rather than read as though the byte were
    // absent.
    let mut encoded = frame(&AppDataRequest::QuotaGet {});
    encoded.bytes[super::RESERVED_OFFSET] = 1;
    assert_eq!(
        AppDataRequest::decode(encoded.as_slice()),
        Err(Errno::BadMagic)
    );
}

#[test]
fn a_grant_reply_round_trips_and_never_carries_the_invalid_handle() {
    let mut out = [0u8; APPDATA_GRANT_REPLY_LEN];
    assert_eq!(
        encode_grant_reply(0x1234_5678_9ABC_DEF0, &mut out),
        Ok(APPDATA_GRANT_REPLY_LEN)
    );
    assert_eq!(decode_grant_reply(&out), Ok(0x1234_5678_9ABC_DEF0));

    // Handle zero is the reserved invalid value, so a reply carrying it is a
    // success frame that grants nothing — refused rather than redeemed.
    let mut out = [0u8; APPDATA_GRANT_REPLY_LEN];
    assert_eq!(encode_grant_reply(0, &mut out), Ok(APPDATA_GRANT_REPLY_LEN));
    assert_eq!(decode_grant_reply(&out), Err(Errno::BadMagic));

    // The daemon's own refusal comes through as itself.
    let refusal = encode_status_reply(Err(Errno::LimitExceeded));
    assert_eq!(decode_grant_reply(&refusal), Err(Errno::LimitExceeded));

    // A short frame is refused rather than read past.
    let mut out = [0u8; APPDATA_GRANT_REPLY_LEN];
    let _ = encode_grant_reply(7, &mut out);
    assert_eq!(
        decode_grant_reply(&out[..APPDATA_GRANT_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(
        encode_grant_reply(7, &mut out[..APPDATA_GRANT_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn a_blob_listing_is_whole_or_nothing() {
    let entries = [
        BlobEntry {
            name: "mail.index",
            len: 4096,
        },
        BlobEntry {
            name: "thumbnails",
            len: APPDATA_BLOB_MAX_BYTES,
        },
    ];
    let mut listing = vec![0u8; entries.len() * APPDATA_BLOB_ENTRY_LEN];
    for (slot, entry) in listing.chunks_mut(APPDATA_BLOB_ENTRY_LEN).zip(&entries) {
        encode_blob_entry(entry, slot).expect("a legal entry encodes");
    }

    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    let len = encode_blob_list_reply(&listing, u32::MAX, &mut out).expect("encodes");
    let decoded = decode_blob_list_reply(&out[..len]).expect("decodes");
    assert_eq!(
        decoded.entries().collect::<alloc::vec::Vec<_>>(),
        entries.to_vec()
    );

    // A listing that does not fit comes back as the length it needs, with no
    // body at all — so no caller can act on a listing missing entries.
    let len = encode_blob_list_reply(&listing, 1, &mut out).expect("encodes");
    assert_eq!(
        decode_blob_list_reply(&out[..len]),
        Ok(BlobListing::NeedsCapacity(listing.len()))
    );
    assert_eq!(
        decode_blob_list_reply(&out[..len])
            .expect("decodes")
            .entries()
            .count(),
        0
    );

    // The empty listing is a whole listing, not a capacity refusal.
    let len = encode_blob_list_reply(&[], 0, &mut out).expect("encodes");
    assert_eq!(
        decode_blob_list_reply(&out[..len]),
        Ok(BlobListing::Whole(&[]))
    );
}

#[test]
fn a_blob_listing_refuses_a_body_that_is_not_whole_entries() {
    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    // A partial entry cannot be encoded...
    assert_eq!(
        encode_blob_list_reply(&[0u8; APPDATA_BLOB_ENTRY_LEN - 1], u32::MAX, &mut out),
        Err(Errno::LengthOutOfRange)
    );
    // ...nor can a listing wider than the bound that lets it be whole.
    assert_eq!(
        encode_blob_list_reply(
            &vec![0u8; APPDATA_BLOB_LIST_MAX + APPDATA_BLOB_ENTRY_LEN],
            u32::MAX,
            &mut out
        ),
        Err(Errno::LengthOutOfRange)
    );
    // ...and a declared length that is not whole entries is refused on the
    // way in, so a walk can never run off the end of a partial record.
    let len = encode_blob_list_reply(&[], 0, &mut out).expect("encodes");
    put_u32(&mut out, STATUS_REPLY_LEN, 1);
    assert_eq!(
        decode_blob_list_reply(&out[..len]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_listing_entry_that_hides_bytes_past_its_name_is_refused() {
    // The name field is fixed-width, so a shorter name leaves padding. A
    // walker that ignored it would let two different frames decode to the
    // same listing, and one of them could carry a name a caller never sees.
    let mut listing = vec![0u8; APPDATA_BLOB_ENTRY_LEN];
    encode_blob_entry(
        &BlobEntry {
            name: "index",
            len: 1,
        },
        &mut listing,
    )
    .expect("encodes");
    listing[1 + APPDATA_NAME_MAX - 1] = b'x';

    let mut out = vec![0u8; APPDATA_MAX_REPLY];
    let len = encode_blob_list_reply(&listing, u32::MAX, &mut out).expect("encodes");
    assert_eq!(
        decode_blob_list_reply(&out[..len])
            .expect("decodes")
            .entries()
            .count(),
        0,
        "an entry with bytes hiding past its name ends the walk"
    );
}

#[test]
fn a_listing_entry_refuses_a_name_outside_the_grammar() {
    let mut slot = [0u8; APPDATA_BLOB_ENTRY_LEN];
    assert!(encode_blob_entry(&BlobEntry { name: "..", len: 0 }, &mut slot).is_err());
    assert!(encode_blob_entry(&BlobEntry { name: "", len: 0 }, &mut slot).is_err());
    assert_eq!(
        encode_blob_entry(
            &BlobEntry {
                name: "index",
                len: 0
            },
            &mut slot[..APPDATA_BLOB_ENTRY_LEN - 1]
        ),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn a_whole_listing_of_every_blob_fits_one_reply() {
    // This is what lets `BlobList` have no cursor: the count bound and the
    // entry width together make the widest possible answer one frame, so a
    // listing is never spliced out of two snapshots of a changing store.
    assert_eq!(
        APPDATA_BLOB_LIST_MAX,
        APPDATA_BLOB_MAX_COUNT * APPDATA_BLOB_ENTRY_LEN
    );
    const {
        assert!(
            STATUS_REPLY_LEN + APPDATA_BLOB_LIST_HEADER_LEN + APPDATA_BLOB_LIST_MAX
                <= APPDATA_MAX_REPLY,
            "the widest listing must fit the endpoint's reply bound"
        );
    }
    // And a capacity past that bound is refused, so a caller cannot ask the
    // daemon to size a buffer for a listing that cannot exist.
    assert_eq!(
        AppDataRequest::BlobList {
            capacity: u32::try_from(APPDATA_BLOB_LIST_MAX + 1).expect("fits"),
        }
        .encode(&mut [0u8; APPDATA_MAX_REQUEST]),
        Err(Errno::LengthOutOfRange)
    );
    assert!(AppDataRequest::BlobList {
        capacity: u32::try_from(APPDATA_BLOB_LIST_MAX).expect("fits"),
    }
    .encode(&mut [0u8; APPDATA_MAX_REQUEST])
    .is_ok());
}

#[test]
fn a_quota_reply_round_trips_and_refuses_a_usage_past_its_own_ceiling() {
    let quota = BlobQuota {
        blobs: 3,
        bytes: 8192,
        blob_max: u64::try_from(APPDATA_BLOB_MAX_COUNT).expect("fits"),
        blob_bytes_max: APPDATA_BLOB_MAX_BYTES,
    };
    let mut out = [0u8; APPDATA_QUOTA_REPLY_LEN];
    assert_eq!(
        encode_quota_reply(&quota, &mut out),
        Ok(APPDATA_QUOTA_REPLY_LEN)
    );
    assert_eq!(decode_quota_reply(&out), Ok(quota));

    // A count or a byte total past its own ceiling is not a state the daemon
    // can be in, so it is refused rather than shown to a caller that would
    // draw a bar past the end of its gauge.
    for broken in [
        BlobQuota {
            blobs: quota.blob_max + 1,
            ..quota
        },
        BlobQuota {
            bytes: quota.blob_max * quota.blob_bytes_max + 1,
            ..quota
        },
    ] {
        let mut out = [0u8; APPDATA_QUOTA_REPLY_LEN];
        encode_quota_reply(&broken, &mut out).expect("encodes");
        assert_eq!(decode_quota_reply(&out), Err(Errno::OutOfRange));
    }

    let refusal = encode_status_reply(Err(Errno::DeviceOffline));
    assert_eq!(decode_quota_reply(&refusal), Err(Errno::DeviceOffline));
    assert_eq!(
        decode_quota_reply(&out[..APPDATA_QUOTA_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(
        encode_quota_reply(&quota, &mut out[..APPDATA_QUOTA_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
}
