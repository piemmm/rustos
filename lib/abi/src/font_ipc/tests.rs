//! Wire tests for the font-service protocol: every frame round-trips, and
//! every malformed frame is refused rather than half-decoded.

extern crate alloc;

use super::{
    decode_families_reply, decode_glyphs_reply, decode_metrics_reply, decode_outlines_reply,
    encode_batch_error_reply, encode_families_reply, encode_metrics_reply, ContourSource,
    FamilyEntry, FamilyKey, FamilyKind, FontMetrics, FontRequest, FontStretch, FontStyle,
    FontUnits, FontWeight, GlyphBatchWriter, GlyphCoverage, GlyphRun, GlyphSegment,
    OutlineBatchWriter, OutlineSource, Synthesis, FONT_ENDPOINT, FONT_FAMILIES_REPLY_HEADER_LEN,
    FONT_FAMILY_ENTRY_LEN, FONT_FAMILY_KEY_LEN, FONT_FAMILY_LABEL_LEN,
    FONT_GLYPHS_REPLY_HEADER_LEN, FONT_GLYPH_RECORD_HEADER_LEN, FONT_MAX_COVERAGE_LEN,
    FONT_MAX_FAMILIES, FONT_MAX_FAMILIES_REPLY, FONT_MAX_GLYPH_REPLY, FONT_MAX_GLYPH_RUN,
    FONT_MAX_GLYPH_WIDTH, FONT_MAX_OUTLINE_POINTS, FONT_MAX_OUTLINE_REPLY, FONT_MAX_PIXEL_HEIGHT,
    FONT_MAX_STRETCH, FONT_MAX_SYNTH_BOLD, FONT_MAX_SYNTH_SHEAR, FONT_MAX_WEIGHT,
    FONT_METRICS_REPLY_LEN, FONT_MIN_PIXEL_HEIGHT, FONT_MIN_STRETCH, FONT_MIN_WEIGHT,
    FONT_OUTLINE_CONTOUR_HEADER_LEN, FONT_OUTLINE_RECORD_HEADER_LEN, FONT_OUTLINE_REPLY_HEADER_LEN,
    FONT_REQUEST_MAGIC,
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
            weight: FontWeight::REGULAR,
        },
        FontRequest::Glyphs {
            family: key("mono"),
            // U+0000 is a legal scalar, so a run of it must survive the
            // padding rule that zeroes every unasked-for slot.
            scalars: run(&['\0', '\u{FFFD}']),
            pixel_height: FONT_MIN_PIXEL_HEIGHT,
            weight: FontWeight::MEDIUM,
        },
        FontRequest::Glyphs {
            family: key("noto-serif"),
            scalars: run(&['\u{10FFFF}']),
            pixel_height: FONT_MAX_PIXEL_HEIGHT,
            weight: FontWeight::BOLD,
        },
        FontRequest::Glyphs {
            family: key("inter"),
            scalars: run(&longest),
            pixel_height: 16,
            weight: FontWeight::REGULAR,
        },
        FontRequest::Metrics {
            family: key("inter"),
            pixel_height: 16,
            weight: FontWeight::BOLD,
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
        weight: FontWeight::REGULAR,
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
    // A weight off the `wght` axis is refused rather than clamped onto it.
    let mut bad_weight = good;
    bad_weight[8..10].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(FontRequest::from_bytes(&bad_weight), Err(Errno::OutOfRange));
    bad_weight[8..10].copy_from_slice(&(FONT_MAX_WEIGHT + 1).to_le_bytes());
    assert_eq!(FontRequest::from_bytes(&bad_weight), Err(Errno::OutOfRange));
    // A smuggled field in the reserved halfword is wire corruption.
    let mut dirty_reserved = good;
    dirty_reserved[14] = 1;
    assert_eq!(
        FontRequest::from_bytes(&dirty_reserved),
        Err(Errno::BadMagic)
    );
    // A malformed family key never reaches the service.
    let mut bad_family = good;
    bad_family[24] = b'/';
    assert_eq!(FontRequest::from_bytes(&bad_family), Err(Errno::OutOfRange));
}

#[test]
fn request_decode_refuses_fields_an_operation_does_not_use() {
    // The run length and every run slot belong to the run ops alone, and a
    // posture and a width belong to the outline op alone.
    for dirty in [10usize, 12, 20, 40, FontRequest::WIRE_LEN - 1] {
        let mut metrics = FontRequest::Metrics {
            family: key("mono"),
            pixel_height: 20,
            weight: FontWeight::REGULAR,
        }
        .to_le_bytes();
        metrics[dirty] = 1;
        assert_eq!(
            FontRequest::from_bytes(&metrics),
            Err(Errno::BadMagic),
            "byte {dirty} must not be smuggled into a Metrics request"
        );
    }

    for dirty in [8usize, 10, 12, 16, 20, 24, 40, FontRequest::WIRE_LEN - 1] {
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
        weight: FontWeight::REGULAR,
    }
    .to_le_bytes();

    // A run that asks for nothing is malformed: a reply must always answer
    // at least one glyph for the client to make progress.
    for count in [0u32, 33, u32::MAX] {
        let mut bad = good;
        bad[20..24].copy_from_slice(&count.to_le_bytes());
        assert_eq!(
            FontRequest::from_bytes(&bad),
            Err(Errno::LengthOutOfRange),
            "a run length of {count} must be refused"
        );
    }

    // A scalar beyond the run length is a smuggled field, never ignored.
    let mut smuggled = good;
    smuggled[48..52].copy_from_slice(&u32::from('z').to_le_bytes());
    assert_eq!(FontRequest::from_bytes(&smuggled), Err(Errno::BadMagic));
}

#[test]
fn request_decode_rejects_a_non_scalar_and_a_bad_pixel_height() {
    let good = FontRequest::Glyphs {
        family: key("inter"),
        scalars: run(&['A', 'B']),
        pixel_height: 20,
        weight: FontWeight::REGULAR,
    }
    .to_le_bytes();

    // A UTF-16 surrogate (U+D800) is not a Unicode scalar value, and neither
    // slot of the run may carry one.
    for slot in [40usize, 44] {
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
            weight: FontWeight::REGULAR,
        },
        FontRequest::Glyphs {
            family: key("inter"),
            scalars: run(&['A']),
            pixel_height: FONT_MAX_PIXEL_HEIGHT + 1,
            weight: FontWeight::BOLD,
        },
        FontRequest::Metrics {
            family: key("mono"),
            pixel_height: 0,
            weight: FontWeight::REGULAR,
        },
    ] {
        assert_eq!(
            FontRequest::from_bytes(&op.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );
    }
}

#[test]
fn the_weight_axis_is_a_number_bounded_at_both_ends() {
    for weight in [FontWeight::REGULAR, FontWeight::MEDIUM, FontWeight::BOLD] {
        assert_eq!(FontWeight::from_wire(weight.to_wire()), Ok(weight));
    }
    assert_eq!(FontWeight::default(), FontWeight::REGULAR);
    // The named points are the standard OpenType coordinates, and every
    // point between them is a weight in its own right — a document setting
    // `font-weight: 250` means 250.
    assert_eq!(FontWeight::REGULAR.axis_value(), 400);
    assert_eq!(FontWeight::MEDIUM.axis_value(), 500);
    assert_eq!(FontWeight::BOLD.axis_value(), 700);
    assert_eq!(FontWeight::new(250).map(FontWeight::axis_value), Ok(250));
    for axis in [FONT_MIN_WEIGHT, 250, 999, FONT_MAX_WEIGHT] {
        assert_eq!(FontWeight::new(axis).map(FontWeight::axis_value), Ok(axis));
    }
    for off_axis in [0u16, FONT_MAX_WEIGHT + 1, u16::MAX] {
        assert_eq!(FontWeight::new(off_axis), Err(Errno::OutOfRange));
    }
}

#[test]
fn the_posture_wire_is_a_closed_set() {
    for style in [FontStyle::Normal, FontStyle::Italic, FontStyle::Oblique] {
        assert_eq!(FontStyle::from_wire(style.to_wire()), Ok(style));
    }
    assert_eq!(FontStyle::default(), FontStyle::Normal);
    for wire in [0u16, 4, u16::MAX] {
        assert_eq!(FontStyle::from_wire(wire), Err(Errno::OutOfRange));
    }
}

#[test]
fn the_width_axis_carries_the_half_percents_css_names() {
    assert_eq!(FontStretch::default(), FontStretch::NORMAL);
    assert_eq!(FontStretch::NORMAL.hundredths(), 10_000);
    // `extra-condensed` is 62.5%, which only a sub-percent unit can state.
    assert_eq!(
        FontStretch::new(6250).map(FontStretch::hundredths),
        Ok(6250)
    );
    for off_axis in [0u16, FONT_MIN_STRETCH - 1, FONT_MAX_STRETCH + 1] {
        assert_eq!(FontStretch::new(off_axis), Err(Errno::OutOfRange));
    }
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
    let n = encode_batch_error_reply(&mut buf, Errno::NotFound).expect("encodes");
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

/// A contour of one line and one quadratic, in whole font units.
fn square_contour() -> ([GlyphSegment; 3], (FontUnits, FontUnits)) {
    let at = |x: f64, y: f64| {
        (
            FontUnits::from_f64(x).expect("a representable coordinate"),
            FontUnits::from_f64(y).expect("a representable coordinate"),
        )
    };
    (
        [
            GlyphSegment::Line { to: at(500.0, 0.0) },
            GlyphSegment::Quadratic {
                control: at(600.5, 350.25),
                to: at(500.0, 700.0),
            },
            GlyphSegment::Line { to: at(0.0, 0.0) },
        ],
        at(0.0, 0.0),
    )
}

#[test]
fn a_font_unit_is_exact_to_a_sixty_fourth_and_refuses_the_non_finite() {
    assert_eq!(FontUnits::from_f64(1.0).map(FontUnits::raw), Ok(64));
    assert_eq!(FontUnits::from_f64(-1.5).map(FontUnits::raw), Ok(-96));
    assert_eq!(FontUnits::from_f64(0.015_625).map(FontUnits::raw), Ok(1));
    for broken in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e30, -1e30] {
        assert_eq!(FontUnits::from_f64(broken), Err(Errno::OutOfRange));
    }
    // Round-tripping a representable coordinate is exact, which is what
    // makes a non-finite unrepresentable rather than merely refused.
    let unit = FontUnits::from_f64(123.5).expect("representable");
    assert!((unit.to_f64() - 123.5).abs() < f64::EPSILON);
}

#[test]
fn the_synthesis_report_packs_both_halves_and_bounds_each() {
    let synth = Synthesis::new(2730, 4085).expect("within bounds");
    assert_eq!(Synthesis::from_wire(synth.to_wire()), Ok(synth));
    assert!(!synth.is_none());
    assert!(Synthesis::NONE.is_none());
    // A quarter-em stroke and a 45-degree lean are the far ends.
    assert!(Synthesis::new(FONT_MAX_SYNTH_BOLD, FONT_MAX_SYNTH_SHEAR).is_ok());
    assert!(Synthesis::new(FONT_MAX_SYNTH_BOLD + 1, 0).is_err());
    assert!(Synthesis::new(0, FONT_MAX_SYNTH_SHEAR + 1).is_err());
    assert!(Synthesis::new(0, -FONT_MAX_SYNTH_SHEAR - 1).is_err());
    // A negative lean survives the packing, which a naive `u32` cast would
    // have turned into an enormous positive one.
    let leaning = Synthesis::new(0, -4085).expect("within bounds");
    assert!(leaning.shear() < 0.0);
    assert_eq!(Synthesis::from_wire(leaning.to_wire()), Ok(leaning));
}

#[test]
fn an_outline_request_round_trips_with_every_axis() {
    let request = FontRequest::Outlines {
        family: key("noto-serif"),
        scalars: run(&['A', 'v']),
        weight: FontWeight::new(250).expect("on the axis"),
        style: FontStyle::Oblique,
        stretch: FontStretch::new(6250).expect("on the axis"),
    };
    assert_eq!(
        FontRequest::from_bytes(&request.to_le_bytes()),
        Ok(request),
        "every design axis must survive the wire"
    );
}

#[test]
fn an_outline_request_carries_no_pixel_height() {
    let mut frame = FontRequest::Outlines {
        family: key("inter"),
        scalars: run(&['A']),
        weight: FontWeight::REGULAR,
        style: FontStyle::Normal,
        stretch: FontStretch::NORMAL,
    }
    .to_le_bytes();
    // A drawing has no resolution, so the height field is the one this
    // operation does not use and may not smuggle a value through.
    frame[16..20].copy_from_slice(&20u32.to_le_bytes());
    assert_eq!(FontRequest::from_bytes(&frame), Err(Errno::BadMagic));
}

#[test]
fn the_coverage_ops_refuse_a_posture_or_a_width_they_do_not_draw() {
    for op in [
        FontRequest::Glyphs {
            family: key("inter"),
            scalars: run(&['A']),
            pixel_height: 20,
            weight: FontWeight::REGULAR,
        },
        FontRequest::Metrics {
            family: key("inter"),
            pixel_height: 20,
            weight: FontWeight::REGULAR,
        },
    ] {
        for dirty in [10usize, 12] {
            let mut frame = op.to_le_bytes();
            frame[dirty] = 1;
            assert_eq!(
                FontRequest::from_bytes(&frame),
                Err(Errno::BadMagic),
                "byte {dirty} must not be smuggled into a coverage request"
            );
        }
    }
}

#[test]
fn an_outline_batch_round_trips_its_geometry_and_its_header() {
    let (segments, start) = square_contour();
    let contours = [ContourSource {
        start,
        segments: &segments,
    }];
    let synth = Synthesis::new(2730, 4085).expect("within bounds");
    let mut buf = vec![0u8; FONT_MAX_OUTLINE_REPLY];
    let len = {
        let mut writer = OutlineBatchWriter::new(&mut buf, 2048, 1600, 400, 90).expect("a writer");
        assert!(writer
            .push(&OutlineSource {
                units_per_em: 1000,
                advance: FontUnits::from_f64(512.5).expect("representable"),
                synth,
                contours: &contours,
            })
            .expect("a well-formed record"));
        // A second record from another face, as a per-scalar fallback makes.
        assert!(writer
            .push(&OutlineSource {
                units_per_em: 2048,
                advance: FontUnits::from_f64(1024.0).expect("representable"),
                synth: Synthesis::NONE,
                contours: &[],
            })
            .expect("an ink-less record"));
        assert_eq!(writer.count(), 2);
        writer.finish().expect("a sealed batch")
    };

    let batch = decode_outlines_reply(&buf[..len]).expect("a decode");
    assert_eq!(batch.units_per_em, 2048);
    assert_eq!(
        (batch.ascent, batch.descent, batch.line_gap),
        (1600, 400, 90)
    );
    let glyphs = batch.glyphs();
    assert_eq!(glyphs.len(), 2);

    // The resolved face's own em travels per record, because a fallback
    // crosses faces that need not share one.
    assert_eq!(glyphs[0].units_per_em, 1000);
    assert_eq!(glyphs[1].units_per_em, 2048);
    assert_eq!(glyphs[0].synth, synth);
    assert!(glyphs[1].synth.is_none());
    assert!((glyphs[0].advance.to_f64() - 512.5).abs() < f64::EPSILON);

    let walked: Vec<_> = glyphs[0].contours().collect();
    assert_eq!(walked.len(), 1);
    assert_eq!(walked[0].len(), 3);
    assert_eq!(walked[0].start, start);
    assert_eq!(walked[0].segments().collect::<Vec<_>>(), segments.to_vec());

    // An ink-less glyph walks to nothing and is drawn by advancing the pen.
    assert_eq!(glyphs[1].contours, 0);
    assert_eq!(glyphs[1].contours().count(), 0);
}

#[test]
fn an_outline_batch_answers_a_prefix_when_the_frame_fills() {
    let (segments, start) = square_contour();
    let contours = [ContourSource {
        start,
        segments: &segments,
    }];
    let record = OutlineSource {
        units_per_em: 1000,
        advance: FontUnits::from_raw(0),
        synth: Synthesis::NONE,
        contours: &contours,
    };
    // A frame with room for one record takes one and reports the second
    // did not fit, exactly as the coverage batch does.
    let mut buf = vec![0u8; 120];
    let mut writer = OutlineBatchWriter::new(&mut buf, 1000, 800, 200, 0).expect("a writer");
    assert!(writer.push(&record).expect("the first fits"));
    let mut filled = 1;
    while writer.push(&record).expect("a well-formed record") {
        filled += 1;
    }
    let len = writer.finish().expect("a sealed batch");
    assert_eq!(
        decode_outlines_reply(&buf[..len])
            .expect("a decode")
            .glyphs()
            .len(),
        filled
    );
}

#[test]
fn an_outline_batch_that_answered_nothing_is_never_a_successful_reply() {
    let mut buf = vec![0u8; FONT_MAX_OUTLINE_REPLY];
    let writer = OutlineBatchWriter::new(&mut buf, 1000, 800, 200, 0).expect("a writer");
    assert_eq!(writer.finish(), Err(Errno::BufferTooSmall));
}

#[test]
fn an_outline_writer_refuses_an_em_or_a_glyph_outside_its_bounds() {
    let mut buf = vec![0u8; FONT_MAX_OUTLINE_REPLY];
    assert_eq!(
        OutlineBatchWriter::new(&mut buf, 8, 0, 0, 0).err(),
        Some(Errno::OutOfRange),
        "an em TrueType does not define is refused before a record is written"
    );

    let mut writer = OutlineBatchWriter::new(&mut buf, 1000, 800, 200, 0).expect("a writer");
    let segments = vec![
        GlyphSegment::Line {
            to: (FontUnits::from_raw(0), FontUnits::from_raw(0)),
        };
        FONT_MAX_OUTLINE_POINTS as usize
    ];
    let contours = [ContourSource {
        start: (FontUnits::from_raw(0), FontUnits::from_raw(0)),
        segments: &segments,
    }];
    assert_eq!(
        writer
            .push(&OutlineSource {
                units_per_em: 1000,
                advance: FontUnits::from_raw(0),
                synth: Synthesis::NONE,
                contours: &contours,
            })
            .err(),
        Some(Errno::LengthOutOfRange),
        "a glyph past the point bound is refused however full the batch is"
    );
}

#[test]
fn an_outline_reply_fails_closed_on_every_malformed_frame() {
    let (segments, start) = square_contour();
    let contours = [ContourSource {
        start,
        segments: &segments,
    }];
    let mut buf = vec![0u8; FONT_MAX_OUTLINE_REPLY];
    let len = {
        let mut writer = OutlineBatchWriter::new(&mut buf, 1000, 800, 200, 0).expect("a writer");
        writer
            .push(&OutlineSource {
                units_per_em: 1000,
                advance: FontUnits::from_raw(0),
                synth: Synthesis::NONE,
                contours: &contours,
            })
            .expect("a record");
        writer.finish().expect("a sealed batch")
    };
    let good = buf[..len].to_vec();

    assert_eq!(
        decode_outlines_reply(&good[..3]),
        Err(Errno::BufferTooSmall)
    );
    assert_eq!(
        decode_outlines_reply(&good[..len - 1]),
        Err(Errno::BufferTooSmall),
        "a truncated frame is refused, never read past its bytes"
    );

    let mut refused = good.clone();
    refused[..4].copy_from_slice(&(-Errno::NotFound.as_i32()).to_le_bytes());
    assert_eq!(decode_outlines_reply(&refused), Err(Errno::NotFound));

    let mut no_glyphs = good.clone();
    no_glyphs[4..8].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_outlines_reply(&no_glyphs),
        Err(Errno::LengthOutOfRange),
        "a batch answering nothing leaves a client with no way forward"
    );

    let mut bad_em = good.clone();
    bad_em[8..12].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(decode_outlines_reply(&bad_em), Err(Errno::OutOfRange));

    // A record whose contours do not account for its stated segments is
    // refused rather than read to whichever count is smaller.
    let mut miscounted = good.clone();
    let record = FONT_OUTLINE_REPLY_HEADER_LEN;
    miscounted[record + 12..record + 16].copy_from_slice(&9u32.to_le_bytes());
    assert_eq!(
        decode_outlines_reply(&miscounted),
        Err(Errno::LengthOutOfRange)
    );

    // A segment kind outside the closed set is wire corruption.
    let mut bad_kind = good;
    let first = FONT_OUTLINE_REPLY_HEADER_LEN
        + FONT_OUTLINE_RECORD_HEADER_LEN
        + FONT_OUTLINE_CONTOUR_HEADER_LEN;
    bad_kind[first..first + 4].copy_from_slice(&7u32.to_le_bytes());
    assert_eq!(decode_outlines_reply(&bad_kind), Err(Errno::OutOfRange));
}

#[test]
fn the_outline_reply_bound_stays_under_the_coverage_one() {
    // Serving geometry moves no existing bound and grows no receive buffer.
    // Read through bindings so this states a relation between two derived
    // numbers rather than a constant the compiler can fold away.
    let outline = FONT_MAX_OUTLINE_REPLY;
    let coverage = FONT_MAX_GLYPH_REPLY;
    assert!(
        outline < coverage / 3,
        "{outline} is not comfortably under {coverage}"
    );
    assert_eq!(outline, 163_876);
}
