//! Wire tests for the font-service protocol: every frame round-trips, and
//! every malformed frame is refused rather than half-decoded.

extern crate alloc;

use super::{
    decode_families_reply, decode_glyph_reply, decode_metrics_reply, encode_families_reply,
    encode_glyph_error_reply, encode_glyph_reply, encode_metrics_reply, FamilyEntry, FamilyKey,
    FamilyKind, FontMetrics, FontRequest, FontWeight, GlyphCoverage, FONT_ENDPOINT,
    FONT_FAMILIES_REPLY_HEADER_LEN, FONT_FAMILY_ENTRY_LEN, FONT_FAMILY_KEY_LEN,
    FONT_FAMILY_LABEL_LEN, FONT_GLYPH_REPLY_HEADER_LEN, FONT_MAX_FAMILIES, FONT_MAX_FAMILIES_REPLY,
    FONT_MAX_GLYPH_REPLY, FONT_MAX_GLYPH_WIDTH, FONT_MAX_PIXEL_HEIGHT, FONT_METRICS_REPLY_LEN,
    FONT_MIN_PIXEL_HEIGHT, FONT_REQUEST_MAGIC,
};
use crate::Errno;
use alloc::vec;
use alloc::vec::Vec;

/// A well-formed key for the tests that are not about key spelling.
fn key(name: &str) -> FamilyKey {
    FamilyKey::new(name).expect("a well-formed family key")
}

#[test]
fn magic_and_endpoint_are_frozen() {
    assert_eq!(FONT_REQUEST_MAGIC, u32::from_le_bytes(*b"FNT1"));
    assert_eq!(FONT_ENDPOINT, 0x464E_5400);
    assert!(crate::ipc::is_reserved_endpoint(FONT_ENDPOINT));
}

#[test]
fn family_key_admits_only_directory_safe_spellings() {
    for name in ["mono", "inter", "noto-sans", "a", "a1", "0123456789abcdef"] {
        assert_eq!(
            FamilyKey::new(name).map(|k| k.as_str().len()),
            Ok(name.len()),
            "{name} should be a valid key"
        );
    }
    // A key can never spell a path escape, a case-folding collision, or an
    // empty directory name.
    for name in [
        "",
        "-lead",
        "Inter",
        "noto sans",
        "../mono",
        "mono/",
        "mono\0",
        "émigré",
        "0123456789abcdefg",
    ] {
        assert_eq!(
            FamilyKey::new(name),
            Err(Errno::OutOfRange),
            "{name} must be refused"
        );
    }
}

#[test]
fn family_key_round_trips_and_refuses_a_dirty_pad() {
    let mono = key("mono");
    assert_eq!(FamilyKey::from_wire(mono.to_wire()), Ok(mono));
    assert_eq!(mono.as_str(), "mono");
    assert_eq!(FamilyKey::MONO, mono);

    let mut smuggled = mono.to_wire();
    smuggled[FONT_FAMILY_KEY_LEN - 1] = b'x';
    assert_eq!(FamilyKey::from_wire(smuggled), Err(Errno::BadMagic));

    assert_eq!(
        FamilyKey::from_wire([0u8; FONT_FAMILY_KEY_LEN]),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn requests_round_trip() {
    for request in [
        FontRequest::Glyph {
            family: key("inter"),
            scalar: 'A',
            pixel_height: 28,
            weight: FontWeight::Regular,
        },
        FontRequest::Glyph {
            family: key("mono"),
            scalar: '\u{FFFD}',
            pixel_height: FONT_MIN_PIXEL_HEIGHT,
            weight: FontWeight::Medium,
        },
        FontRequest::Glyph {
            family: key("noto-serif"),
            scalar: '\u{10FFFF}',
            pixel_height: FONT_MAX_PIXEL_HEIGHT,
            weight: FontWeight::Bold,
        },
        FontRequest::Metrics {
            family: key("inter"),
            pixel_height: 16,
            weight: FontWeight::Bold,
        },
        FontRequest::Families,
    ] {
        let bytes = request.to_le_bytes();
        assert_eq!(FontRequest::from_bytes(&bytes), Ok(request));
    }
}

#[test]
fn request_decode_fails_closed_on_malformed_framing() {
    let good = FontRequest::Glyph {
        family: key("inter"),
        scalar: 'x',
        pixel_height: 20,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();

    assert_eq!(
        FontRequest::from_bytes(&good[..FontRequest::WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
    let mut bad_magic = good;
    bad_magic[0] ^= 0xFF;
    assert_eq!(FontRequest::from_bytes(&bad_magic), Err(Errno::BadMagic));
    let mut bad_version = good;
    bad_version[4] = 9;
    assert_eq!(
        FontRequest::from_bytes(&bad_version),
        Err(Errno::AbiVersionUnsupported)
    );
    let mut bad_op = good;
    bad_op[6] = 9;
    assert_eq!(FontRequest::from_bytes(&bad_op), Err(Errno::OutOfRange));
    // An unknown weight is refused rather than rendered as Regular.
    let mut bad_weight = good;
    bad_weight[8] = 9;
    assert_eq!(FontRequest::from_bytes(&bad_weight), Err(Errno::OutOfRange));
    // A smuggled field in the reserved halfword is wire corruption.
    let mut dirty_reserved = good;
    dirty_reserved[10] = 1;
    assert_eq!(
        FontRequest::from_bytes(&dirty_reserved),
        Err(Errno::BadMagic)
    );
    // A malformed family key never reaches the service.
    let mut bad_family = good;
    bad_family[20] = b'/';
    assert_eq!(FontRequest::from_bytes(&bad_family), Err(Errno::OutOfRange));
}

#[test]
fn request_decode_refuses_fields_an_operation_does_not_use() {
    let mut metrics = FontRequest::Metrics {
        family: key("mono"),
        pixel_height: 20,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();
    metrics[16] = 1;
    assert_eq!(FontRequest::from_bytes(&metrics), Err(Errno::BadMagic));

    for dirty in [8usize, 12, 16, 20] {
        let mut families = FontRequest::Families.to_le_bytes();
        families[dirty] = 1;
        assert_eq!(
            FontRequest::from_bytes(&families),
            Err(Errno::BadMagic),
            "byte {dirty} must not be smuggled into a Families request"
        );
    }
}

#[test]
fn request_decode_rejects_a_non_scalar_and_a_bad_pixel_height() {
    // A UTF-16 surrogate (U+D800) is not a Unicode scalar value.
    let mut surrogate = FontRequest::Glyph {
        family: key("inter"),
        scalar: 'A',
        pixel_height: 20,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();
    surrogate[16..20].copy_from_slice(&0xD800u32.to_le_bytes());
    assert_eq!(FontRequest::from_bytes(&surrogate), Err(Errno::OutOfRange));

    let mut past_max = surrogate;
    past_max[16..20].copy_from_slice(&0x11_0000u32.to_le_bytes());
    assert_eq!(FontRequest::from_bytes(&past_max), Err(Errno::OutOfRange));

    for op in [
        FontRequest::Glyph {
            family: key("inter"),
            scalar: 'A',
            pixel_height: FONT_MIN_PIXEL_HEIGHT - 1,
            weight: FontWeight::Regular,
        },
        FontRequest::Glyph {
            family: key("inter"),
            scalar: 'A',
            pixel_height: FONT_MAX_PIXEL_HEIGHT + 1,
            weight: FontWeight::Bold,
        },
        FontRequest::Metrics {
            family: key("mono"),
            pixel_height: 0,
            weight: FontWeight::Regular,
        },
    ] {
        assert_eq!(
            FontRequest::from_bytes(&op.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );
    }
}

#[test]
fn weight_wire_discriminants_are_a_closed_set() {
    for weight in [FontWeight::Regular, FontWeight::Medium, FontWeight::Bold] {
        assert_eq!(FontWeight::from_wire(weight.to_wire()), Ok(weight));
    }
    assert_eq!(FontWeight::default(), FontWeight::Regular);
    for wire in [0u16, 4, u16::MAX] {
        assert_eq!(FontWeight::from_wire(wire), Err(Errno::OutOfRange));
    }
    // The design-axis coordinates are the standard OpenType weights and
    // increase with the weight, so a heavier role never instantiates lighter.
    assert_eq!(FontWeight::Regular.axis_value(), 400);
    assert_eq!(FontWeight::Medium.axis_value(), 500);
    assert_eq!(FontWeight::Bold.axis_value(), 700);
}

#[test]
fn family_kind_wire_is_a_closed_set() {
    for kind in [FamilyKind::Monospace, FamilyKind::Proportional] {
        assert_eq!(FamilyKind::from_wire(kind.to_wire()), Ok(kind));
    }
    for wire in [0u8, 3, u8::MAX] {
        assert_eq!(FamilyKind::from_wire(wire), Err(Errno::OutOfRange));
    }
}

/// A glyph reply carrying `width * height` distinguishable coverage bytes.
fn glyph(width: u32, height: u32, advance: u32, left: i32, coverage: &[u8]) -> GlyphCoverage<'_> {
    GlyphCoverage {
        width,
        height,
        advance,
        left,
        coverage,
    }
}

#[test]
fn glyph_reply_round_trips_including_bearings_and_inkless_glyphs() {
    let width = 6u32;
    let height = 12u32;
    let coverage: Vec<u8> = (0..width * height)
        .map(|i| u8::try_from(i % 256).expect("a value modulo 256 fits a u8"))
        .collect();
    let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];

    // A glyph that reaches back over the preceding one keeps its negative
    // bearing across the wire.
    let sent = glyph(width, height, 7, -2, &coverage);
    let n = encode_glyph_reply(&mut buf, &sent).expect("encodes");
    assert_eq!(n, FONT_GLYPH_REPLY_HEADER_LEN + coverage.len());
    assert_eq!(decode_glyph_reply(&buf[..n]), Ok(sent));

    // A space carries no samples at all: the pen still advances.
    let space = glyph(0, height, 5, 0, &[]);
    let n = encode_glyph_reply(&mut buf, &space).expect("encodes");
    assert_eq!(n, FONT_GLYPH_REPLY_HEADER_LEN);
    assert_eq!(decode_glyph_reply(&buf[..n]), Ok(space));

    // A combining mark occupies no space of its own.
    let mark_coverage = vec![9u8; 3 * height as usize];
    let mark = glyph(3, height, 0, -3, &mark_coverage);
    let n = encode_glyph_reply(&mut buf, &mark).expect("encodes");
    assert_eq!(decode_glyph_reply(&buf[..n]), Ok(mark));
}

#[test]
fn glyph_reply_encode_rejects_bad_geometry_and_mismatched_coverage() {
    let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let span = i32::try_from(FONT_MAX_GLYPH_WIDTH).expect("the bound fits an i32");
    for bad in [
        glyph(4, 10, 4, 0, &[0u8; 39]),
        glyph(FONT_MAX_GLYPH_WIDTH + 1, 10, 4, 0, &[]),
        glyph(4, FONT_MIN_PIXEL_HEIGHT - 1, 4, 0, &[]),
        glyph(4, FONT_MAX_PIXEL_HEIGHT + 1, 4, 0, &[]),
        glyph(4, 10, FONT_MAX_GLYPH_WIDTH + 1, 0, &[]),
        glyph(4, 10, 4, span + 1, &[]),
        glyph(4, 10, 4, -span - 1, &[]),
    ] {
        assert_eq!(
            encode_glyph_reply(&mut buf, &bad),
            Err(Errno::LengthOutOfRange)
        );
    }
    let mut tiny = [0u8; FONT_GLYPH_REPLY_HEADER_LEN + 3];
    assert_eq!(
        encode_glyph_reply(&mut tiny, &glyph(2, 8, 2, 0, &[0u8; 16])),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn glyph_reply_error_frame_surfaces_its_errno() {
    let mut buf = [0u8; FONT_GLYPH_REPLY_HEADER_LEN];
    let n = encode_glyph_error_reply(&mut buf, Errno::NotFound).expect("encodes");
    assert_eq!(n, 4);
    assert_eq!(decode_glyph_reply(&buf[..n]), Err(Errno::NotFound));
}

#[test]
fn glyph_reply_decode_fails_closed() {
    let width = 4u32;
    let height = 10u32;
    let coverage = vec![0xABu8; (width * height) as usize];
    let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n =
        encode_glyph_reply(&mut buf, &glyph(width, height, width, 0, &coverage)).expect("encodes");

    assert_eq!(decode_glyph_reply(&buf[..3]), Err(Errno::BufferTooSmall));
    assert_eq!(
        decode_glyph_reply(&buf[..FONT_GLYPH_REPLY_HEADER_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(
        decode_glyph_reply(&buf[..n - 1]),
        Err(Errno::BufferTooSmall)
    );
    let mut bad_status = buf.clone();
    bad_status[0] = 1;
    assert_eq!(decode_glyph_reply(&bad_status), Err(Errno::OutOfRange));
    let mut bad_geometry = buf.clone();
    bad_geometry[8..12].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_glyph_reply(&bad_geometry),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn metrics_reply_round_trips_for_both_family_kinds() {
    for metrics in [
        FontMetrics {
            pixel_height: 28,
            baseline: 23,
            line_height: 33,
            monospace_advance: 15,
        },
        FontMetrics {
            pixel_height: 20,
            baseline: 16,
            line_height: 24,
            monospace_advance: 0,
        },
    ] {
        assert_eq!(
            decode_metrics_reply(&encode_metrics_reply(Ok(metrics))),
            Ok(metrics)
        );
    }
    assert_eq!(
        decode_metrics_reply(&encode_metrics_reply(Err(Errno::NotFound))),
        Err(Errno::NotFound)
    );
}

#[test]
fn metrics_reply_decode_fails_closed() {
    let good = encode_metrics_reply(Ok(FontMetrics {
        pixel_height: 28,
        baseline: 23,
        line_height: 33,
        monospace_advance: 15,
    }));

    assert_eq!(
        decode_metrics_reply(&good[..FONT_METRICS_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
    let mut bad_status = good;
    bad_status[0] = 1;
    assert_eq!(decode_metrics_reply(&bad_status), Err(Errno::OutOfRange));

    // A pixel height out of range, a baseline below the box, a line height
    // of zero or implausibly tall, and an impossible monospace advance are
    // each refused rather than laid text out with.
    for (at, value) in [
        (4usize, 0u32),
        (8, 99),
        (12, 0),
        (12, 28 * 4 + 1),
        (16, FONT_MAX_GLYPH_WIDTH + 1),
    ] {
        let mut bad = good;
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert_eq!(
            decode_metrics_reply(&bad),
            Err(Errno::LengthOutOfRange),
            "field at {at} = {value} must be refused"
        );
    }
}

/// The family list a store of `count` families would report.
fn entries(count: usize) -> Vec<FamilyEntry> {
    (0..count)
        .map(|i| {
            let name = alloc::format!("family{i}");
            FamilyEntry::new(
                FamilyKey::new(&name).expect("a well-formed key"),
                "A Font Family",
                if i % 2 == 0 {
                    FamilyKind::Proportional
                } else {
                    FamilyKind::Monospace
                },
            )
            .expect("a well-formed entry")
        })
        .collect()
}

#[test]
fn families_reply_round_trips_from_empty_to_full() {
    let mut buf = vec![0u8; FONT_MAX_FAMILIES_REPLY];
    for count in [0usize, 1, FONT_MAX_FAMILIES] {
        let sent = entries(count);
        let n = encode_families_reply(&mut buf, Ok(&sent)).expect("encodes");
        assert_eq!(
            n,
            FONT_FAMILIES_REPLY_HEADER_LEN + count * FONT_FAMILY_ENTRY_LEN
        );
        let list = decode_families_reply(&buf[..n]).expect("decodes");
        assert_eq!(list.entries().len(), count);
        for (got, want) in list.entries().iter().zip(sent.iter()) {
            assert_eq!(got.key, want.key);
            assert_eq!(got.label(), want.label());
            assert_eq!(got.kind, want.kind);
        }
    }
}

#[test]
fn families_reply_refuses_an_overlong_list_and_a_short_buffer() {
    let mut buf = vec![0u8; FONT_MAX_FAMILIES_REPLY];
    let too_many = entries(FONT_MAX_FAMILIES + 1);
    assert_eq!(
        encode_families_reply(&mut buf, Ok(&too_many)),
        Err(Errno::LengthOutOfRange)
    );
    let mut tiny = [0u8; FONT_FAMILIES_REPLY_HEADER_LEN + 1];
    assert_eq!(
        encode_families_reply(&mut tiny, Ok(&entries(1))),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn families_reply_error_frame_surfaces_its_errno() {
    let mut buf = vec![0u8; FONT_MAX_FAMILIES_REPLY];
    let n = encode_families_reply(&mut buf, Err(Errno::NotFound)).expect("encodes");
    assert_eq!(n, 4);
    assert_eq!(decode_families_reply(&buf[..n]), Err(Errno::NotFound));
}

#[test]
fn families_reply_decode_fails_closed() {
    let mut buf = vec![0u8; FONT_MAX_FAMILIES_REPLY];
    let n = encode_families_reply(&mut buf, Ok(&entries(2))).expect("encodes");

    assert_eq!(
        decode_families_reply(&buf[..n - 1]),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(decode_families_reply(&buf[..3]), Err(Errno::BufferTooSmall));

    let mut over_count = buf.clone();
    let over = u32::try_from(FONT_MAX_FAMILIES + 1).expect("the bound fits a u32");
    over_count[4..8].copy_from_slice(&over.to_le_bytes());
    assert_eq!(
        decode_families_reply(&over_count),
        Err(Errno::LengthOutOfRange)
    );

    let kind_at = FONT_FAMILIES_REPLY_HEADER_LEN + FONT_FAMILY_KEY_LEN + FONT_FAMILY_LABEL_LEN;
    let mut bad_kind = buf.clone();
    bad_kind[kind_at] = 7;
    assert_eq!(decode_families_reply(&bad_kind), Err(Errno::OutOfRange));

    let mut dirty_pad = buf.clone();
    dirty_pad[kind_at + 1] = 1;
    assert_eq!(decode_families_reply(&dirty_pad), Err(Errno::BadMagic));

    // A NUL followed by more label bytes is a smuggled second field.
    let label_at = FONT_FAMILIES_REPLY_HEADER_LEN + FONT_FAMILY_KEY_LEN;
    let mut truncated_label = buf.clone();
    truncated_label[label_at] = 0;
    assert_eq!(
        decode_families_reply(&truncated_label),
        Err(Errno::BadMagic)
    );

    // A wholly empty label leaves a picker with nothing to draw.
    let mut empty_label = buf.clone();
    empty_label[label_at..label_at + FONT_FAMILY_LABEL_LEN].fill(0);
    assert_eq!(
        decode_families_reply(&empty_label),
        Err(Errno::LengthOutOfRange)
    );

    let mut bad_key = buf;
    bad_key[FONT_FAMILIES_REPLY_HEADER_LEN] = b'/';
    assert_eq!(decode_families_reply(&bad_key), Err(Errno::OutOfRange));
}

#[test]
fn family_entry_label_is_bounded_and_printable() {
    let mono = FamilyKey::MONO;
    assert_eq!(
        FamilyEntry::new(mono, "", FamilyKind::Monospace),
        Err(Errno::LengthOutOfRange)
    );
    let overlong = "x".repeat(FONT_FAMILY_LABEL_LEN + 1);
    assert_eq!(
        FamilyEntry::new(mono, &overlong, FamilyKind::Monospace),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        FamilyEntry::new(mono, "two\nlines", FamilyKind::Monospace),
        Err(Errno::OutOfRange)
    );
    let exact = "x".repeat(FONT_FAMILY_LABEL_LEN);
    let entry = FamilyEntry::new(mono, &exact, FamilyKind::Monospace).expect("encodes");
    assert_eq!(entry.label(), exact);
}
