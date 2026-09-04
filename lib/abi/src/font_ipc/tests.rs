//! Wire tests for the font-service protocol: every frame round-trips, and
//! every malformed frame is refused rather than half-decoded.

extern crate alloc;

use super::{
    decode_families_reply, decode_glyphs_reply, decode_metrics_reply, encode_families_reply,
    encode_glyph_error_reply, encode_metrics_reply, FamilyEntry, FamilyKey, FamilyKind,
    FontMetrics, FontRequest, FontWeight, GlyphBatchWriter, GlyphCoverage, GlyphRun, FONT_ENDPOINT,
    FONT_FAMILIES_REPLY_HEADER_LEN, FONT_FAMILY_ENTRY_LEN, FONT_FAMILY_KEY_LEN,
    FONT_FAMILY_LABEL_LEN, FONT_GLYPHS_REPLY_HEADER_LEN, FONT_GLYPH_RECORD_HEADER_LEN,
    FONT_MAX_COVERAGE_LEN, FONT_MAX_FAMILIES, FONT_MAX_FAMILIES_REPLY, FONT_MAX_GLYPH_REPLY,
    FONT_MAX_GLYPH_RUN, FONT_MAX_GLYPH_WIDTH, FONT_MAX_PIXEL_HEIGHT, FONT_METRICS_REPLY_LEN,
    FONT_MIN_PIXEL_HEIGHT, FONT_REQUEST_MAGIC,
};
use crate::Errno;
use alloc::vec;
use alloc::vec::Vec;

/// A well-formed key for the tests that are not about key spelling.
fn key(name: &str) -> FamilyKey {
    FamilyKey::new(name).expect("a well-formed family key")
}

/// A well-formed run for the tests that are not about run framing.
fn run(scalars: &[char]) -> GlyphRun {
    GlyphRun::new(scalars).expect("a well-formed glyph run")
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
    let longest: Vec<char> = (0..FONT_MAX_GLYPH_RUN)
        .map(|index| char::from(b'a' + u8::try_from(index).expect("the run bound fits a u8")))
        .collect();
    for request in [
        FontRequest::Glyphs {
            family: key("inter"),
            scalars: run(&['A']),
            pixel_height: 28,
            weight: FontWeight::Regular,
        },
        FontRequest::Glyphs {
            family: key("mono"),
            // U+0000 is a legal scalar, so a run of it must survive the
            // padding rule that zeroes every unasked-for slot.
            scalars: run(&['\0', '\u{FFFD}']),
            pixel_height: FONT_MIN_PIXEL_HEIGHT,
            weight: FontWeight::Medium,
        },
        FontRequest::Glyphs {
            family: key("noto-serif"),
            scalars: run(&['\u{10FFFF}']),
            pixel_height: FONT_MAX_PIXEL_HEIGHT,
            weight: FontWeight::Bold,
        },
        FontRequest::Glyphs {
            family: key("inter"),
            scalars: run(&longest),
            pixel_height: 16,
            weight: FontWeight::Regular,
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
fn glyph_run_admits_only_a_bounded_non_empty_sequence() {
    assert_eq!(run(&['a', 'b']).scalars(), &['a', 'b']);
    assert_eq!(GlyphRun::new(&[]), Err(Errno::LengthOutOfRange));
    let too_long = vec!['x'; FONT_MAX_GLYPH_RUN + 1];
    assert_eq!(GlyphRun::new(&too_long), Err(Errno::LengthOutOfRange));
    // Two runs with the same scalars are the same run however they were
    // built, so the unasked-for slots can never make them differ.
    assert_eq!(run(&['a']), run(&['a']));
    assert_ne!(run(&['a']), run(&['a', 'b']));
}

#[test]
fn request_decode_fails_closed_on_malformed_framing() {
    let good = FontRequest::Glyphs {
        family: key("inter"),
        scalars: run(&['x']),
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
    // The run length and every run slot belong to `Glyphs` alone.
    for dirty in [16usize, 36, FontRequest::WIRE_LEN - 1] {
        let mut metrics = FontRequest::Metrics {
            family: key("mono"),
            pixel_height: 20,
            weight: FontWeight::Regular,
        }
        .to_le_bytes();
        metrics[dirty] = 1;
        assert_eq!(
            FontRequest::from_bytes(&metrics),
            Err(Errno::BadMagic),
            "byte {dirty} must not be smuggled into a Metrics request"
        );
    }

    for dirty in [8usize, 12, 16, 20, 36, FontRequest::WIRE_LEN - 1] {
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
fn request_decode_bounds_the_run_length_and_refuses_a_dirty_run_pad() {
    let good = FontRequest::Glyphs {
        family: key("inter"),
        scalars: run(&['a', 'b']),
        pixel_height: 20,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();

    // A run that asks for nothing is malformed: a reply must always answer
    // at least one glyph for the client to make progress.
    for count in [0u32, 33, u32::MAX] {
        let mut bad = good;
        bad[16..20].copy_from_slice(&count.to_le_bytes());
        assert_eq!(
            FontRequest::from_bytes(&bad),
            Err(Errno::LengthOutOfRange),
            "a run length of {count} must be refused"
        );
    }

    // A scalar beyond the run length is a smuggled field, never ignored.
    let mut smuggled = good;
    smuggled[44..48].copy_from_slice(&u32::from('z').to_le_bytes());
    assert_eq!(FontRequest::from_bytes(&smuggled), Err(Errno::BadMagic));
}

#[test]
fn request_decode_rejects_a_non_scalar_and_a_bad_pixel_height() {
    let good = FontRequest::Glyphs {
        family: key("inter"),
        scalars: run(&['A', 'B']),
        pixel_height: 20,
        weight: FontWeight::Regular,
    }
    .to_le_bytes();

    // A UTF-16 surrogate (U+D800) is not a Unicode scalar value, and neither
    // slot of the run may carry one.
    for slot in [36usize, 40] {
        for wire in [0xD800u32, 0x11_0000, u32::MAX] {
            let mut bad = good;
            bad[slot..slot + 4].copy_from_slice(&wire.to_le_bytes());
            assert_eq!(
                FontRequest::from_bytes(&bad),
                Err(Errno::OutOfRange),
                "{wire:#x} in run slot at byte {slot} must be refused"
            );
        }
    }

    for op in [
        FontRequest::Glyphs {
            family: key("inter"),
            scalars: run(&['A']),
            pixel_height: FONT_MIN_PIXEL_HEIGHT - 1,
            weight: FontWeight::Regular,
        },
        FontRequest::Glyphs {
            family: key("inter"),
            scalars: run(&['A']),
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

/// A glyph record carrying `width * height` distinguishable coverage bytes.
fn glyph(width: u32, height: u32, advance: u32, left: i32, coverage: &[u8]) -> GlyphCoverage<'_> {
    GlyphCoverage {
        width,
        height,
        advance,
        left,
        coverage,
    }
}

/// Frame `glyphs` as a batch reply in `buf`, returning its length.
fn encode_batch(buf: &mut [u8], glyphs: &[GlyphCoverage<'_>]) -> Result<usize, Errno> {
    let mut writer = GlyphBatchWriter::new(buf)?;
    for glyph in glyphs {
        assert!(writer.push(glyph)?, "the test frame holds every record");
    }
    writer.finish()
}

#[test]
fn glyph_batch_round_trips_in_order_including_bearings_and_inkless_glyphs() {
    let height = 12u32;
    let inked: Vec<u8> = (0..6 * height)
        .map(|i| u8::try_from(i % 256).expect("a value modulo 256 fits a u8"))
        .collect();
    let mark_coverage = vec![9u8; 3 * height as usize];
    // A glyph that reaches back over the preceding one, a space with no
    // samples at all, and a combining mark that occupies no space of its own.
    let sent = [
        glyph(6, height, 7, -2, &inked),
        glyph(0, height, 5, 0, &[]),
        glyph(3, height, 0, -3, &mark_coverage),
    ];

    let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let n = encode_batch(&mut buf, &sent).expect("encodes");
    assert_eq!(
        n,
        FONT_GLYPHS_REPLY_HEADER_LEN
            + 3 * FONT_GLYPH_RECORD_HEADER_LEN
            + inked.len()
            + mark_coverage.len()
    );
    let batch = decode_glyphs_reply(&buf[..n]).expect("decodes");
    assert_eq!(batch.glyphs(), &sent);
}

#[test]
fn glyph_batch_writer_rejects_bad_geometry_and_mismatched_coverage() {
    let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let mut writer = GlyphBatchWriter::new(&mut buf).expect("the frame holds a header");
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
        assert_eq!(writer.push(&bad), Err(Errno::LengthOutOfRange));
    }
    assert_eq!(writer.count(), 0);
    assert_eq!(
        GlyphBatchWriter::new(&mut [0u8; FONT_GLYPHS_REPLY_HEADER_LEN - 1]).err(),
        Some(Errno::BufferTooSmall)
    );
}

#[test]
fn glyph_batch_answers_a_prefix_when_the_frame_or_the_run_bound_fills() {
    // One glyph at the extreme of the coverage bound fills the frame on its
    // own, which is why a batch is a prefix rather than a whole-run promise.
    let widest = vec![0x5Au8; FONT_MAX_COVERAGE_LEN];
    let extreme = glyph(
        FONT_MAX_GLYPH_WIDTH,
        FONT_MAX_PIXEL_HEIGHT,
        FONT_MAX_GLYPH_WIDTH,
        0,
        &widest,
    );
    let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let mut writer = GlyphBatchWriter::new(&mut buf).expect("the frame holds a header");
    assert_eq!(writer.push(&extreme), Ok(true));
    let tiny = glyph(1, 8, 1, 0, &[0u8; 8]);
    assert_eq!(writer.push(&tiny), Ok(false));
    let n = writer.finish().expect("one record fitted");
    assert_eq!(n, FONT_MAX_GLYPH_REPLY);
    let batch = decode_glyphs_reply(&buf[..n]).expect("decodes");
    assert_eq!(batch.glyphs(), &[extreme]);

    // The run bound caps the batch even when the frame has room to spare.
    let mut roomy = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let mut writer = GlyphBatchWriter::new(&mut roomy).expect("the frame holds a header");
    for _ in 0..FONT_MAX_GLYPH_RUN {
        assert_eq!(writer.push(&tiny), Ok(true));
    }
    assert_eq!(writer.push(&tiny), Ok(false));
    let n = writer
        .finish()
        .expect("the run bound worth of records fitted");
    let batch = decode_glyphs_reply(&roomy[..n]).expect("decodes");
    assert_eq!(batch.glyphs().len(), FONT_MAX_GLYPH_RUN);
}

#[test]
fn glyph_batch_never_successfully_answers_nothing() {
    // A batch that answered nothing would leave a client asking again for a
    // remainder it can never be told about, so neither side admits one.
    let mut buf = vec![0u8; FONT_GLYPHS_REPLY_HEADER_LEN + 4];
    let writer = GlyphBatchWriter::new(&mut buf).expect("the frame holds a header");
    assert_eq!(writer.finish(), Err(Errno::BufferTooSmall));

    let mut empty = vec![0u8; FONT_GLYPHS_REPLY_HEADER_LEN];
    empty[4..8].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_glyphs_reply(&empty).err(),
        Some(Errno::LengthOutOfRange)
    );
}

#[test]
fn glyph_batch_error_frame_surfaces_its_errno() {
    let mut buf = [0u8; FONT_GLYPHS_REPLY_HEADER_LEN];
    let n = encode_glyph_error_reply(&mut buf, Errno::NotFound).expect("encodes");
    assert_eq!(n, 4);
    assert_eq!(decode_glyphs_reply(&buf[..n]).err(), Some(Errno::NotFound));
}

#[test]
fn glyph_batch_decode_fails_closed() {
    let height = 10u32;
    let coverage = vec![0xABu8; 4 * height as usize];
    let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
    let sent = [
        glyph(4, height, 4, 0, &coverage),
        glyph(0, height, 3, 0, &[]),
    ];
    let n = encode_batch(&mut buf, &sent).expect("encodes");

    for truncated in [3usize, FONT_GLYPHS_REPLY_HEADER_LEN - 1, n - 1] {
        assert_eq!(
            decode_glyphs_reply(&buf[..truncated]).err(),
            Some(Errno::BufferTooSmall),
            "a frame cut to {truncated} bytes must be refused"
        );
    }
    let mut bad_status = buf.clone();
    bad_status[0] = 1;
    assert_eq!(
        decode_glyphs_reply(&bad_status).err(),
        Some(Errno::OutOfRange)
    );
    let mut past_bound = buf.clone();
    past_bound[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        decode_glyphs_reply(&past_bound).err(),
        Some(Errno::LengthOutOfRange)
    );
    // A zero height in the first record's geometry: refused before its
    // coverage length is believed.
    let mut bad_geometry = buf.clone();
    bad_geometry[FONT_GLYPHS_REPLY_HEADER_LEN + 4..FONT_GLYPHS_REPLY_HEADER_LEN + 8]
        .copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_glyphs_reply(&bad_geometry).err(),
        Some(Errno::LengthOutOfRange)
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
