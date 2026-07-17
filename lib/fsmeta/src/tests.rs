//! Host unit tests for the shared extended-metadata model.

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::DriverError;
use tairix_abi::time::Time64;

use crate::key::{AttrKey, Namespace, NamespaceAccess};
use crate::preset::{acorn, amiga, atari, mac};
use crate::{
    AttrFlags, AttrSet, MetadataError, ATTRS_PER_INODE, KEY_MAX, TOTAL_ATTR_BYTES, VALUE_MAX,
};

// --- key grammar ---------------------------------------------------------

#[test]
fn every_namespace_round_trips_its_name() {
    for ns in Namespace::ALL {
        assert_eq!(Namespace::from_name(ns.as_str()), Some(ns));
    }
}

#[test]
fn foreign_and_user_namespaces_are_file_permission() {
    for ns in [
        Namespace::User,
        Namespace::Acorn,
        Namespace::Amiga,
        Namespace::Atari,
        Namespace::Mac,
        Namespace::TAIRiX,
    ] {
        assert_eq!(ns.access(), NamespaceAccess::FilePermission);
        assert!(!ns.is_privileged());
    }
}

#[test]
fn system_and_trusted_namespaces_are_privileged() {
    for ns in [Namespace::System, Namespace::Trusted] {
        assert_eq!(ns.access(), NamespaceAccess::Privileged);
        assert!(ns.is_privileged());
    }
}

#[test]
fn valid_keys_parse_and_expose_their_namespace() {
    let key = AttrKey::parse(b"acorn.filetype").expect("valid");
    assert_eq!(key.namespace(), Namespace::Acorn);
    assert_eq!(key.as_bytes(), b"acorn.filetype");
    assert_eq!(key.access(), NamespaceAccess::FilePermission);
}

#[test]
fn unknown_namespace_is_rejected() {
    assert_eq!(
        AttrKey::parse(b"bogus.key"),
        Err(MetadataError::UnknownNamespace)
    );
}

#[test]
fn malformed_keys_are_rejected() {
    assert_eq!(AttrKey::parse(b""), Err(MetadataError::MalformedKey));
    assert_eq!(AttrKey::parse(b"nodot"), Err(MetadataError::MalformedKey));
    assert_eq!(AttrKey::parse(b"user."), Err(MetadataError::MalformedKey));
    assert_eq!(
        AttrKey::parse(b"user.a/b"),
        Err(MetadataError::MalformedKey)
    );
    assert_eq!(
        AttrKey::parse(b"user.a\0b"),
        Err(MetadataError::MalformedKey)
    );
    assert_eq!(
        AttrKey::parse(&[b'u', b's', b'e', b'r', b'.', 0xff, 0xfe]),
        Err(MetadataError::MalformedKey)
    );
}

#[test]
fn oversize_key_is_rejected() {
    let mut key = b"user.".to_vec();
    key.resize(KEY_MAX + 1, b'a');
    assert_eq!(AttrKey::parse(&key), Err(MetadataError::KeyTooLong));
}

#[test]
fn keys_are_case_sensitive() {
    // Distinct byte sequences are distinct keys.
    let mut set = AttrSet::new();
    set.set(b"user.Name", AttrFlags::empty(), b"a")
        .expect("set");
    set.set(b"user.name", AttrFlags::empty(), b"b")
        .expect("set");
    assert_eq!(set.len(), 2);
    assert_eq!(set.get(b"user.Name"), Some(&b"a"[..]));
    assert_eq!(set.get(b"user.name"), Some(&b"b"[..]));
}

// --- attribute set semantics --------------------------------------------

#[test]
fn set_get_list_remove_round_trip() {
    let mut set = AttrSet::new();
    assert!(set.is_empty());
    set.set(b"user.comment", AttrFlags::empty(), b"hello")
        .expect("set");
    set.set(b"acorn.filetype", AttrFlags::empty(), b"fff")
        .expect("set");
    assert_eq!(set.len(), 2);
    assert_eq!(set.get(b"user.comment"), Some(&b"hello"[..]));

    let keys: Vec<&[u8]> = set.iter().map(|e| e.key().as_bytes()).collect();
    assert_eq!(keys, vec![&b"user.comment"[..], &b"acorn.filetype"[..]]);

    assert!(set.remove(b"user.comment"));
    assert!(!set.remove(b"user.comment"));
    assert_eq!(set.get(b"user.comment"), None);
    assert_eq!(set.len(), 1);
}

#[test]
fn set_replaces_existing_key_in_place() {
    let mut set = AttrSet::new();
    set.set(b"user.k", AttrFlags::empty(), b"one").expect("set");
    set.set(b"user.k", AttrFlags::SYSTEM, b"two").expect("set");
    assert_eq!(set.len(), 1);
    assert_eq!(set.get(b"user.k"), Some(&b"two"[..]));
    let entry = set.iter().next().expect("entry");
    assert!(entry.flags().contains(AttrFlags::SYSTEM));
}

#[test]
fn oversize_value_is_rejected() {
    let mut set = AttrSet::new();
    let value = vec![0u8; VALUE_MAX + 1];
    assert_eq!(
        set.set(b"user.k", AttrFlags::empty(), &value),
        Err(MetadataError::ValueTooLong)
    );
}

#[test]
fn attribute_count_is_bounded() {
    let mut set = AttrSet::new();
    for i in 0..ATTRS_PER_INODE {
        let mut key = b"user.".to_vec();
        key.extend_from_slice(format_index(i).as_bytes());
        set.set(&key, AttrFlags::empty(), b"").expect("set");
    }
    assert_eq!(set.len(), ATTRS_PER_INODE);
    assert_eq!(
        set.set(b"user.overflow", AttrFlags::empty(), b""),
        Err(MetadataError::TooManyAttributes)
    );
}

#[test]
fn total_bytes_is_bounded() {
    let mut set = AttrSet::new();
    // Two of these plus their keys exceed the summed-bytes cap; one fits.
    let big = vec![b'x'; TOTAL_ATTR_BYTES / 2];
    set.set(b"user.a", AttrFlags::empty(), &big).expect("set");
    assert_eq!(
        set.set(b"user.b", AttrFlags::empty(), &big),
        Err(MetadataError::TotalBytesExceeded)
    );
    assert!(set.total_bytes() <= TOTAL_ATTR_BYTES);
}

// --- encode / decode -----------------------------------------------------

#[test]
fn encode_decode_round_trips() {
    let mut set = AttrSet::new();
    set.set(b"user.comment", AttrFlags::NO_BACKUP, b"note")
        .expect("set");
    set.set(b"mac.type", AttrFlags::empty(), b"TEXT")
        .expect("set");
    let encoded = set.encode();
    let decoded = AttrSet::decode(&encoded).expect("decode");
    assert_eq!(decoded, set);
}

#[test]
fn decode_ignores_trailing_block_padding() {
    let mut set = AttrSet::new();
    set.set(b"user.k", AttrFlags::empty(), b"v").expect("set");
    let mut encoded = set.encode();
    encoded.resize(4096, 0); // simulate a zero-padded metadata block
    let decoded = AttrSet::decode(&encoded).expect("decode");
    assert_eq!(decoded, set);
}

#[test]
fn decode_rejects_corrupt_encodings() {
    assert_eq!(AttrSet::decode(&[]), Err(MetadataError::Corrupt));
    assert_eq!(AttrSet::decode(&[0u8; 8]), Err(MetadataError::Corrupt));

    let mut set = AttrSet::new();
    set.set(b"user.k", AttrFlags::empty(), b"vv").expect("set");
    let good = set.encode();

    // Truncated mid-entry.
    assert_eq!(
        AttrSet::decode(&good[..good.len() - 1]),
        Err(MetadataError::Corrupt)
    );

    // Flipped version.
    let mut bad_version = good.clone();
    bad_version[4] ^= 0xff;
    assert_eq!(AttrSet::decode(&bad_version), Err(MetadataError::Corrupt));

    // A count claiming more entries than the bytes hold.
    let mut bad_count = good.clone();
    bad_count[6] = 0xff;
    assert_eq!(AttrSet::decode(&bad_count), Err(MetadataError::Corrupt));
}

#[test]
fn decode_rejects_unknown_flag_bits() {
    let mut set = AttrSet::new();
    set.set(b"user.k", AttrFlags::empty(), b"v").expect("set");
    let mut encoded = set.encode();
    // The flags byte sits at set-header(8) + key_len(2) + value_len(2) = 12.
    encoded[12] = 0x80;
    assert_eq!(AttrSet::decode(&encoded), Err(MetadataError::Corrupt));
}

#[test]
fn decode_rejects_duplicate_keys() {
    // Hand-build an encoding with two identical keys.
    let mut set = AttrSet::new();
    set.set(b"user.k", AttrFlags::empty(), b"a").expect("set");
    let single = set.encode();
    let mut doubled = single.clone();
    // count = 2
    doubled[6] = 2;
    // append a copy of the one entry (everything after the 8-byte set header)
    let entry = single[8..].to_vec();
    doubled.extend_from_slice(&entry);
    assert_eq!(AttrSet::decode(&doubled), Err(MetadataError::Corrupt));
}

// --- error mapping -------------------------------------------------------

#[test]
fn metadata_errors_map_to_fail_closed_driver_errors() {
    assert_eq!(
        DriverError::from(MetadataError::KeyTooLong),
        DriverError::LengthOutOfRange
    );
    assert_eq!(
        DriverError::from(MetadataError::ValueTooLong),
        DriverError::LengthOutOfRange
    );
    assert_eq!(
        DriverError::from(MetadataError::UnknownNamespace),
        DriverError::OutOfRange
    );
    assert_eq!(
        DriverError::from(MetadataError::NotRepresentable),
        DriverError::OutOfRange
    );
    assert_eq!(
        DriverError::from(MetadataError::TooManyAttributes),
        DriverError::NoSpace
    );
    assert_eq!(
        DriverError::from(MetadataError::Corrupt),
        DriverError::DeviceFault
    );
}

// --- preset: acorn -------------------------------------------------------

#[test]
fn acorn_filetype_round_trips() {
    assert_eq!(&acorn::filetype_to_value(0xFFF).expect("enc"), b"fff");
    assert_eq!(&acorn::filetype_to_value(0x0FA).expect("enc"), b"0fa");
    assert_eq!(acorn::filetype_from_value(b"fff").expect("dec"), 0xFFF);
    assert_eq!(acorn::filetype_from_value(b"FFF").expect("dec"), 0xFFF);
    assert_eq!(
        acorn::filetype_to_value(0x1000),
        Err(MetadataError::NotRepresentable)
    );
    assert_eq!(
        acorn::filetype_from_value(b"ffff"),
        Err(MetadataError::NotRepresentable)
    );
}

#[test]
fn acorn_load_exec_decodes_typed_object() {
    // A Text file (&FFF) with a load/exec-encoded type + timestamp.
    // High 12 bits FFF (typed marker), filetype FFF, top centisecond byte 0x12.
    let load = 0xFFFF_FF12u32;
    let exec = 0x3456_789A;
    match acorn::decode_load_exec(load, exec) {
        acorn::LoadExec::Typed {
            filetype,
            centiseconds,
        } => {
            assert_eq!(filetype, 0xFFF);
            assert_eq!(centiseconds, (0x12u64 << 32) | 0x3456_789A);
            let (rload, rexec) = acorn::encode_typed(filetype, centiseconds).expect("re-encode");
            assert_eq!((rload, rexec), (load, exec));
        }
        acorn::LoadExec::Untyped { .. } => panic!("expected typed"),
    }
}

#[test]
fn acorn_load_exec_keeps_untyped_addresses() {
    match acorn::decode_load_exec(0x8000_0000, 0x0000_8000) {
        acorn::LoadExec::Untyped { load, exec } => {
            assert_eq!(load, 0x8000_0000);
            assert_eq!(exec, 0x0000_8000);
        }
        acorn::LoadExec::Typed { .. } => panic!("expected untyped"),
    }
}

#[test]
fn acorn_addr_round_trips() {
    assert_eq!(&acorn::addr_to_value(0xDEAD_BEEF), b"deadbeef");
    assert_eq!(
        acorn::addr_from_value(b"deadbeef").expect("dec"),
        0xDEAD_BEEF
    );
    assert_eq!(
        acorn::addr_from_value(b"short"),
        Err(MetadataError::NotRepresentable)
    );
}

#[test]
fn acorn_datestamp_round_trips_across_boundaries() {
    // A pre-1970 instant (1950) is still after the 1900 RISC OS epoch, so it
    // is representable.
    for time in [
        Time64::from_secs(-631_152_000),                         // ~1950
        Time64::from_secs(0),                                    // 1970 epoch
        Time64::from_secs(2_147_483_648),                        // just past the 2038 i32 boundary
        Time64::new(1_000_000_000, 250_000_000).expect("valid"), // .25s -> 25 centis
    ] {
        let centis = acorn::time64_to_centiseconds(time).expect("to centis");
        let back = acorn::centiseconds_to_time64(centis).expect("from centis");
        assert_eq!(back, time);
    }
}

#[test]
fn acorn_datestamp_fails_closed_out_of_range() {
    // Sub-centisecond precision cannot be represented.
    let sub = Time64::new(0, 1).expect("valid");
    assert_eq!(
        acorn::time64_to_centiseconds(sub),
        Err(MetadataError::TimestampOutOfRange)
    );
    // Before 1900 cannot be represented.
    let ancient = Time64::from_secs(-3_000_000_000);
    assert_eq!(
        acorn::time64_to_centiseconds(ancient),
        Err(MetadataError::TimestampOutOfRange)
    );
    // Far future beyond the 40-bit centisecond range.
    let far = Time64::from_secs(20_000_000_000);
    assert_eq!(
        acorn::time64_to_centiseconds(far),
        Err(MetadataError::TimestampOutOfRange)
    );
}

// --- preset: amiga -------------------------------------------------------

#[test]
fn amiga_protection_round_trips() {
    assert_eq!(&amiga::protection_to_value(0b1000_0000), b"h-------");
    assert_eq!(&amiga::protection_to_value(0b0000_0001), b"-------d");
    assert_eq!(&amiga::protection_to_value(0xFF), b"hsparwed");
    for bits in [0u8, 1, 0x55, 0xAA, 0xFF] {
        let value = amiga::protection_to_value(bits);
        assert_eq!(amiga::protection_from_value(&value).expect("parse"), bits);
    }
    assert_eq!(
        amiga::protection_from_value(b"xxxxxxxx"),
        Err(MetadataError::NotRepresentable)
    );
}

#[test]
fn amiga_comment_is_length_bounded() {
    assert!(amiga::validate_comment(&[b'x'; amiga::MAX_COMMENT_LEN]).is_ok());
    assert_eq!(
        amiga::validate_comment(&[b'x'; amiga::MAX_COMMENT_LEN + 1]),
        Err(MetadataError::ValueTooLong)
    );
}

// --- preset: atari -------------------------------------------------------

#[test]
fn atari_attributes_reject_unknown_bits() {
    assert!(atari::validate_attributes(atari::ATTR_READ_ONLY | atari::ATTR_ARCHIVE).is_ok());
    assert_eq!(
        atari::validate_attributes(0x80),
        Err(MetadataError::NotRepresentable)
    );
}

#[test]
fn atari_datetime_round_trips() {
    // 1980-01-01 00:00:00 is the FAT epoch.
    let (date, time) = (0b0000_0000_0010_0001, 0);
    let t = atari::datetime_to_time64(date, time).expect("to time");
    assert_eq!(
        atari::time64_to_datetime(t).expect("from time"),
        (date, time)
    );

    // A concrete instant: 2023-05-27 14:06:30.
    let t2 = atari::datetime_to_time64(0b0101_0110_1011_1011, 0b0111_0000_1100_1111);
    let t2 = t2.expect("valid");
    let (d2, tm2) = atari::time64_to_datetime(t2).expect("round trip");
    assert_eq!(atari::datetime_to_time64(d2, tm2).expect("valid"), t2);
}

#[test]
fn atari_datetime_fails_closed() {
    // Odd (non-two-second) seconds cannot be represented.
    let odd = Time64::from_secs(atari_epoch_secs() + 1);
    assert_eq!(
        atari::time64_to_datetime(odd),
        Err(MetadataError::TimestampOutOfRange)
    );
    // Sub-second precision cannot be represented.
    let sub = Time64::new(atari_epoch_secs(), 1).expect("valid");
    assert_eq!(
        atari::time64_to_datetime(sub),
        Err(MetadataError::TimestampOutOfRange)
    );
    // Before 1980 cannot be represented.
    let before = Time64::from_secs(0);
    assert_eq!(
        atari::time64_to_datetime(before),
        Err(MetadataError::TimestampOutOfRange)
    );
    // An out-of-range packed month is rejected on decode.
    assert_eq!(
        atari::datetime_to_time64(0, 0),
        Err(MetadataError::NotRepresentable)
    );
}

// --- preset: mac ---------------------------------------------------------

#[test]
fn mac_ostype_is_four_bytes() {
    assert!(mac::validate_ostype(b"TEXT").is_ok());
    assert_eq!(
        mac::validate_ostype(b"TOOLONG"),
        Err(MetadataError::NotRepresentable)
    );
}

#[test]
fn mac_finderflags_round_trip() {
    let value = mac::finderflags_to_value(0x1234);
    assert_eq!(value, [0x12, 0x34]);
    assert_eq!(mac::finderflags_from_value(&value).expect("parse"), 0x1234);
    assert_eq!(
        mac::finderflags_from_value(b"x"),
        Err(MetadataError::NotRepresentable)
    );
}

#[test]
fn acorn_attr_round_trips() {
    // A locked, publicly readable directory in canonical order.
    let attr = 0b0010_1101; // R, L, D, r
    let (value, len) = acorn::attr_to_value(attr).expect("encode");
    assert_eq!(&value[..len], b"RLD/r");
    assert_eq!(acorn::attr_from_value(&value[..len]).expect("parse"), attr);
    // Letters parse in any order; duplicates and unknowns fail closed.
    assert_eq!(acorn::attr_from_value(b"DLR/r").expect("parse"), attr);
    assert_eq!(
        acorn::attr_from_value(b"RR/"),
        Err(MetadataError::NotRepresentable)
    );
    assert_eq!(
        acorn::attr_from_value(b"Q/"),
        Err(MetadataError::NotRepresentable)
    );
    assert_eq!(
        acorn::attr_from_value(b"RW"),
        Err(MetadataError::NotRepresentable)
    );
    assert_eq!(
        acorn::attr_from_value(b"R/w/e"),
        Err(MetadataError::NotRepresentable)
    );
    // Every defined bit survives the round trip.
    let (value, len) = acorn::attr_to_value(acorn::ATTR_BITS).expect("encode");
    assert_eq!(&value[..len], b"RWLDEP/rwe");
    assert_eq!(
        acorn::attr_from_value(&value[..len]).expect("parse"),
        acorn::ATTR_BITS
    );
    // Bits outside the defined set are not representable.
    assert_eq!(
        acorn::attr_to_value(0x0200),
        Err(MetadataError::NotRepresentable)
    );
}

#[test]
fn acorn_datestamp_round_trips() {
    let stamp = 0x0000_A1B2_C3D4_u64;
    let value = acorn::datestamp_to_value(stamp).expect("encode");
    assert_eq!(&value, b"00a1b2c3d4");
    assert_eq!(acorn::datestamp_from_value(&value).expect("parse"), stamp);
    // The stamp is 40 bits, the value exactly ten hex digits.
    assert_eq!(
        acorn::datestamp_to_value(1 << 40),
        Err(MetadataError::NotRepresentable)
    );
    assert_eq!(
        acorn::datestamp_from_value(b"123"),
        Err(MetadataError::NotRepresentable)
    );
    assert_eq!(
        acorn::datestamp_from_value(b"00a1b2c3dg"),
        Err(MetadataError::NotRepresentable)
    );
}

// --- helpers -------------------------------------------------------------

fn atari_epoch_secs() -> i64 {
    // 1980-01-01 00:00:00 UTC as a whole-second, even Unix timestamp.
    atari::datetime_to_time64(0b0000_0000_0010_0001, 0)
        .expect("epoch")
        .secs()
}

fn format_index(i: usize) -> alloc::string::String {
    use alloc::string::ToString;
    i.to_string()
}
