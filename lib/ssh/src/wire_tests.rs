extern crate alloc;

use alloc::vec::Vec;

use super::{Mpint, NameList, Reader, WireError, Writer, MAX_NAME_LEN};

fn written(build: impl FnOnce(&mut Writer<'_>)) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = Writer::new(&mut out);
    build(&mut writer);
    writer.finish().expect("a well-formed value writes");
    out
}

/// The five `mpint` examples of RFC 4251 §5, data bytes and wire form.
const RFC_MPINTS: [(&[u8], &[u8]); 5] = [
    (&[], &[0, 0, 0, 0]),
    (
        &[0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7],
        &[0, 0, 0, 8, 0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7],
    ),
    (&[0x00, 0x80], &[0, 0, 0, 2, 0x00, 0x80]),
    (&[0xed, 0xcc], &[0, 0, 0, 2, 0xed, 0xcc]),
    (
        &[0xff, 0x21, 0x52, 0x41, 0x11],
        &[0, 0, 0, 5, 0xff, 0x21, 0x52, 0x41, 0x11],
    ),
];

#[test]
fn the_rfc_mpint_examples_decode_and_encode_exactly() {
    for (data, wire) in RFC_MPINTS {
        let mut reader = Reader::new(wire);
        let value = reader.mpint().expect("an RFC example is canonical");
        reader.finish().expect("nothing follows");
        assert_eq!(value.as_twos_complement(), data);
        assert_eq!(written(|w| w.mpint(value)), wire);
    }
}

#[test]
fn an_mpint_knows_its_sign_and_magnitude() {
    let zero = Mpint::ZERO;
    assert!(zero.is_zero() && !zero.is_negative());
    assert_eq!(zero.magnitude(), Some(&[][..]));
    let padded = Mpint::from_twos_complement(&[0x00, 0x80]).expect("canonical");
    assert_eq!(padded.magnitude(), Some(&[0x80][..]));
    let small = Mpint::from_twos_complement(&[0x7f]).expect("canonical");
    assert_eq!(small.magnitude(), Some(&[0x7f][..]));
    for negative in [
        &[0xed, 0xcc][..],
        &[0xff, 0x21, 0x52, 0x41, 0x11],
        &[0xff],
        &[0x80],
    ] {
        let value = Mpint::from_twos_complement(negative).expect("canonical");
        assert!(value.is_negative());
        assert_eq!(value.magnitude(), None);
    }
}

#[test]
fn a_non_canonical_mpint_is_refused() {
    for data in [
        &[0x00][..],
        &[0x00, 0x7f],
        &[0x00, 0x00, 0x80],
        &[0xff, 0x80],
        &[0xff, 0xff, 0x01],
    ] {
        assert_eq!(
            Mpint::from_twos_complement(data),
            Err(WireError::NonCanonicalMpint),
            "{data:02x?}"
        );
        let mut wire = u32::try_from(data.len())
            .expect("small")
            .to_be_bytes()
            .to_vec();
        wire.extend_from_slice(data);
        assert_eq!(
            Reader::new(&wire).mpint(),
            Err(WireError::NonCanonicalMpint)
        );
    }
}

#[test]
fn an_unsigned_magnitude_is_written_canonically() {
    assert_eq!(written(|w| w.mpint_unsigned(&[])), [0, 0, 0, 0]);
    assert_eq!(written(|w| w.mpint_unsigned(&[0, 0, 0])), [0, 0, 0, 0]);
    assert_eq!(
        written(|w| w.mpint_unsigned(&[0x80])),
        [0, 0, 0, 2, 0x00, 0x80]
    );
    assert_eq!(
        written(|w| w.mpint_unsigned(&[0, 0, 0x80, 1])),
        [0, 0, 0, 3, 0x00, 0x80, 1]
    );
    assert_eq!(
        written(|w| w.mpint_unsigned(&[0, 0x7f, 1])),
        [0, 0, 0, 2, 0x7f, 1]
    );
    // Whatever the magnitude, what is written reads back as that magnitude.
    for top in 0..=255u8 {
        let magnitude = [0, top, 0x55];
        let wire = written(|w| w.mpint_unsigned(&magnitude));
        let value = Reader::new(&wire).mpint().expect("written canonically");
        let expected: &[u8] = if top == 0 {
            &magnitude[2..]
        } else {
            &magnitude[1..]
        };
        assert_eq!(value.magnitude(), Some(expected), "top byte {top:#04x}");
    }
}

#[test]
fn the_rfc_name_list_examples_round_trip() {
    for (text, wire) in [
        ("", &[0, 0, 0, 0][..]),
        ("zlib", &[0, 0, 0, 4, b'z', b'l', b'i', b'b']),
        (
            "zlib,none",
            &[
                0, 0, 0, 9, b'z', b'l', b'i', b'b', b',', b'n', b'o', b'n', b'e',
            ],
        ),
    ] {
        let list = Reader::new(wire).name_list().expect("an RFC example");
        assert_eq!(list.as_str(), text);
        assert_eq!(written(|w| w.name_list(list)), wire);
        assert_eq!(NameList::new(text), Ok(list));
    }
}

#[test]
fn a_name_list_iterates_and_searches_its_names() {
    let list =
        NameList::new("curve25519-sha256,ext-info-c,kex-strict-c-v00@openssh.com").expect("valid");
    let names: Vec<&str> = list.iter().collect();
    assert_eq!(
        names,
        [
            "curve25519-sha256",
            "ext-info-c",
            "kex-strict-c-v00@openssh.com"
        ]
    );
    assert!(list.contains("ext-info-c"));
    assert!(!list.contains("ext-info"));
    assert!(!list.contains(""));
    assert_eq!(NameList::EMPTY.iter().count(), 0);
    assert!(NameList::EMPTY.is_empty());
}

#[test]
fn every_name_rfc_4250_forbids_is_refused() {
    let long = "a".repeat(MAX_NAME_LEN + 1);
    let longest = "a".repeat(MAX_NAME_LEN);
    for bad in [
        ",",
        "a,",
        ",a",
        "a,,b",
        "a b",
        "a\tb",
        "caf\u{e9}",
        "a\x7fb",
        "a\0b",
        "@a",
        "a@",
        "a@b@c",
        long.as_str(),
    ] {
        assert_eq!(NameList::new(bad), Err(WireError::InvalidName), "{bad:?}");
        assert_eq!(
            NameList::parse(bad.as_bytes()),
            Err(WireError::InvalidName),
            "{bad:?}"
        );
    }
    for good in ["a", "a@b", "a-b.c_d@example.org", "3des", longest.as_str()] {
        assert!(NameList::new(good).is_ok(), "{good:?}");
    }
}

#[test]
fn a_name_list_is_checked_at_compile_time_where_it_is_written() {
    const OURS: NameList<'static> = match NameList::new("aes128-ctr,aes256-ctr") {
        Ok(list) => list,
        Err(_) => panic!("our own list is valid"),
    };
    assert_eq!(OURS.iter().count(), 2);
}

#[test]
fn fixed_width_values_decode_big_endian() {
    let wire = [
        0x01, 0x00, 0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xaa,
        0xbb,
    ];
    let mut reader = Reader::new(&wire);
    assert_eq!(reader.boolean(), Ok(true));
    assert_eq!(reader.boolean(), Ok(false));
    assert_eq!(reader.uint32(), Ok(0xdead_beef));
    assert_eq!(reader.uint64(), Ok(0x0102_0304_0506_0708));
    assert_eq!(reader.fixed::<2>(), Ok(&[0xaa, 0xbb]));
    assert_eq!(reader.remaining(), 0);
    reader.finish().expect("consumed exactly");
    assert_eq!(
        written(|w| {
            w.boolean(true);
            w.boolean(false);
            w.uint32(0xdead_beef);
            w.uint64(0x0102_0304_0506_0708);
            w.fixed(&[0xaa, 0xbb]);
        }),
        wire
    );
}

#[test]
fn any_non_zero_boolean_reads_true_and_true_writes_one() {
    for byte in 1..=255u8 {
        assert_eq!(Reader::new(&[byte]).boolean(), Ok(true));
    }
    assert_eq!(written(|w| w.boolean(true)), [1]);
}

#[test]
fn every_truncation_is_refused_rather_than_read_past() {
    let wire = written(|w| {
        w.uint32(7);
        w.string(b"hello");
        w.uint64(9);
    });
    for cut in 0..wire.len() {
        let mut reader = Reader::new(&wire[..cut]);
        let result = reader
            .uint32()
            .and_then(|_| reader.string())
            .and_then(|_| reader.uint64());
        assert_eq!(result.err(), Some(WireError::Truncated), "cut at {cut}");
    }
    // A declared length past the input, however large, is a truncation.
    assert_eq!(
        Reader::new(&[0xff, 0xff, 0xff, 0xff, 1]).string(),
        Err(WireError::Truncated)
    );
}

#[test]
fn trailing_bytes_are_refused_by_finish() {
    let mut reader = Reader::new(&[0, 0, 0, 1, b'x', 0]);
    assert_eq!(reader.string(), Ok(&b"x"[..]));
    assert_eq!(reader.finish(), Err(WireError::TrailingData));
}

#[test]
fn a_utf8_string_is_checked() {
    assert_eq!(Reader::new(&[0, 0, 0, 2, 0xc3, 0xa9]).utf8(), Ok("\u{e9}"));
    assert_eq!(
        Reader::new(&[0, 0, 0, 1, 0xff]).utf8(),
        Err(WireError::InvalidUtf8)
    );
}

#[test]
fn a_writer_latches_its_first_failure() {
    let mut out = Vec::new();
    let mut writer = Writer::new(&mut out);
    writer.byte(1);
    writer.failed = Some(WireError::TooLong);
    writer.uint32(2);
    assert_eq!(writer.finish(), Err(WireError::TooLong));
    assert_eq!(out, [1], "nothing is written after the failure");
}
