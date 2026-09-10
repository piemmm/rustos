//! VP8L lossless decoder tests.
//!
//! Every stream is written here rather than shipped, and the writers are
//! built from the specification's own rules rather than from the decoder
//! beside them: the bit order, the canonical code assignment, the transform
//! grammar, and the distance mapping are each re-derived, so a round trip
//! tests the decoder against the format rather than against itself.

use alloc::vec;
use alloc::vec::Vec;

use super::fixture::{flat, flat_group, header, prefix_for, simple_one, Bits, Prefix};
use super::{decode, decode_alpha, probe, SIGNATURE};
use crate::{DecodeError, DecodeLimits, RGBA_BYTES};

/// Generous enough that no fixture here is refused for its size.
fn limits() -> DecodeLimits {
    DecodeLimits::new(256, 256, 256 * 256, 1 << 16)
}

/// The colour every pixel of `image` must be.
fn expect_flat(bytes: &[u8], width: u32, height: u32, colour: [u8; 4]) {
    let image = decode(bytes, &limits()).expect("a valid stream decodes");
    assert_eq!((image.width(), image.height()), (width, height));
    for pixel in image.pixels().as_chunks::<RGBA_BYTES>().0 {
        assert_eq!(*pixel, colour);
    }
}

#[test]
fn a_flat_stream_decodes_to_one_colour() {
    let bytes = flat(5, 3, [0x11, 0x22, 0x33, 0xF0]);
    expect_flat(&bytes, 5, 3, [0x11, 0x22, 0x33, 0xF0]);
}

#[test]
fn a_probe_reads_the_geometry_without_decoding() {
    let bytes = flat(9, 7, [1, 2, 3, 4]);
    assert_eq!(probe(&bytes), Ok((9, 7)));
}

#[test]
fn a_one_by_one_stream_is_the_smallest_picture() {
    expect_flat(&flat(1, 1, [9, 8, 7, 6]), 1, 1, [9, 8, 7, 6]);
}

#[test]
fn the_widest_geometry_the_header_can_declare_probes() {
    let bytes = flat(16384, 16384, [0, 0, 0, 0]);
    assert_eq!(probe(&bytes), Ok((16384, 16384)));
    // Its pixel count is far past what the caller allows, and that is
    // settled from the header before anything is reserved.
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WidthExceedsLimit)
    );
}

#[test]
fn a_missing_signature_is_refused() {
    let mut bytes = flat(2, 2, [0; 4]);
    bytes[0] ^= 0xFF;
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpLosslessBadSignature)
    );
}

#[test]
fn a_version_other_than_zero_is_refused() {
    let mut bits = Bits::new();
    bits.put(u32::from(SIGNATURE), 8);
    bits.put(1, 14);
    bits.put(1, 14);
    bits.put(0, 1);
    bits.put(1, 3);
    let bytes = bits.finish();
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpLosslessUnsupportedVersion)
    );
}

#[test]
fn an_empty_stream_is_refused_rather_than_read_as_zeros() {
    assert_eq!(
        decode(&[], &limits()),
        Err(DecodeError::WebpLosslessTruncated)
    );
    assert_eq!(probe(&[SIGNATURE]), Err(DecodeError::WebpLosslessTruncated));
}

#[test]
fn a_truncated_stream_is_refused_rather_than_completed() {
    let bytes = flat(8, 8, [1, 2, 3, 4]);
    for cut in 1..bytes.len() {
        let short = &bytes[..cut];
        // Every prefix either refuses or, where the codes happen to be
        // complete already, decodes: what it must never do is panic.
        let _ = decode(short, &limits());
    }
}

#[test]
fn a_cache_width_outside_the_permitted_range_is_refused() {
    for width in [0u32, 12, 15] {
        let mut bits = Bits::new();
        header(&mut bits, 2, 2);
        bits.put(0, 1);
        bits.put(1, 1);
        bits.put(width, 4);
        let bytes = bits.finish();
        assert_eq!(
            decode(&bytes, &limits()),
            Err(DecodeError::WebpLosslessInvalidCacheBits)
        );
    }
}

/// A stream whose green channel carries real literals through a coded
/// prefix code, so the canonical assignment and the bit order are both
/// exercised.
fn literals(width: u32, height: u32, greens: &[u8]) -> Vec<u8> {
    let mut bits = Bits::new();
    header(&mut bits, width, height);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    let mut distinct: Vec<u8> = greens.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let lengths: Vec<(u16, u8)> = distinct
        .iter()
        .map(|&value| (u16::from(value), 2))
        .collect();
    let green = Prefix::write(&mut bits, &lengths, 280);
    simple_one(&mut bits, 0x40);
    simple_one(&mut bits, 0x50);
    simple_one(&mut bits, 0xFF);
    simple_one(&mut bits, 0);
    for &value in greens {
        green.emit(&mut bits, u16::from(value));
    }
    bits.finish()
}

#[test]
fn coded_literals_decode_to_the_values_written() {
    let greens = [0u8, 1, 2, 3, 3, 2, 1, 0, 0, 3, 1, 2];
    let bytes = literals(4, 3, &greens);
    let image = decode(&bytes, &limits()).expect("a valid stream decodes");
    let decoded: Vec<u8> = image
        .pixels()
        .as_chunks::<RGBA_BYTES>()
        .0
        .iter()
        .map(|pixel| pixel[1])
        .collect();
    assert_eq!(decoded, greens);
    for pixel in image.pixels().as_chunks::<RGBA_BYTES>().0 {
        assert_eq!([pixel[0], pixel[2], pixel[3]], [0x40, 0x50, 0xFF]);
    }
}

#[test]
fn a_prefix_code_that_leaves_the_space_unspent_is_refused() {
    let mut bits = Bits::new();
    header(&mut bits, 2, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    // Three symbols of length two spends three quarters of the code space,
    // which describes no full decision tree.
    Prefix::write(&mut bits, &[(0, 2), (1, 2), (2, 2)], 280);
    let bytes = bits.finish();
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpLosslessInvalidCode)
    );
}

/// A stream that writes one literal and then copies it with a backward
/// reference of the distance given.
fn reference(width: u32, height: u32, distance: u32, length: u32) -> Vec<u8> {
    let mut bits = Bits::new();
    header(&mut bits, width, height);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    // Green codes one literal value and one length prefix; the length
    // prefixes follow the 256 literals.
    // Four copied pixels, and the distance named as a plain scan-line one.
    let (length_symbol, length_extra, length_bits) = prefix_for(4);
    let (distance_symbol, distance_extra, distance_bits) = prefix_for(distance + 120);
    let green = Prefix::write(&mut bits, &[(7, 1), (256 + length_symbol, 1)], 280);
    simple_one(&mut bits, 0x20);
    simple_one(&mut bits, 0x30);
    simple_one(&mut bits, 0xFF);
    let distances = Prefix::write(&mut bits, &[(0, 1), (distance_symbol, 1)], 40);
    for _ in 0..length {
        green.emit(&mut bits, 7);
    }
    green.emit(&mut bits, 256 + length_symbol);
    bits.put(length_extra, length_bits);
    distances.emit(&mut bits, distance_symbol);
    bits.put(distance_extra, distance_bits);
    bits.finish()
}

#[test]
fn a_backward_reference_repeats_the_pixels_it_names() {
    // Four literals then a four-pixel copy from four back.
    let bytes = reference(8, 1, 4, 4);
    let image = decode(&bytes, &limits()).expect("a valid stream decodes");
    for pixel in image.pixels().as_chunks::<RGBA_BYTES>().0 {
        assert_eq!(*pixel, [0x20, 7, 0x30, 0xFF]);
    }
}

#[test]
fn a_reference_reaching_before_the_first_pixel_is_refused() {
    // One literal, then a copy from four pixels back: three of them do not
    // exist yet.
    let bytes = reference(8, 1, 4, 1);
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpLosslessInvalidReference)
    );
}

/// A stream using the colour cache: one literal, then a cache hit naming it.
fn cached(colour: [u8; 4]) -> Vec<u8> {
    let mut bits = Bits::new();
    header(&mut bits, 2, 1);
    bits.put(0, 1);
    bits.put(1, 1);
    bits.put(1, 4);
    bits.put(0, 1);
    let argb = (u32::from(colour[3]) << 24)
        | (u32::from(colour[0]) << 16)
        | (u32::from(colour[1]) << 8)
        | u32::from(colour[2]);
    let key = argb.wrapping_mul(0x1E35_A7BD) >> (32 - 1);
    let cache_symbol = 256 + 24 + u16::try_from(key).expect("one bit");
    let green = Prefix::write(
        &mut bits,
        &[(u16::from(colour[1]), 1), (cache_symbol, 1)],
        280 + 2,
    );
    simple_one(&mut bits, u32::from(colour[0]));
    simple_one(&mut bits, u32::from(colour[2]));
    simple_one(&mut bits, u32::from(colour[3]));
    simple_one(&mut bits, 0);
    green.emit(&mut bits, u16::from(colour[1]));
    green.emit(&mut bits, cache_symbol);
    bits.finish()
}

#[test]
fn a_colour_cache_hit_answers_the_colour_it_stored() {
    let colour = [0x81u8, 0x42, 0x23, 0xC0];
    let bytes = cached(colour);
    let image = decode(&bytes, &limits()).expect("a valid stream decodes");
    let pixels = image.pixels().as_chunks::<RGBA_BYTES>().0;
    assert_eq!(pixels[0], colour);
    assert_eq!(pixels[1], colour);
}

/// A stream carrying one transform over a flat picture.
fn transformed(kind: u32, extra: impl FnOnce(&mut Bits), colour: [u8; 4]) -> Vec<u8> {
    let mut bits = Bits::new();
    header(&mut bits, 4, 4);
    bits.put(1, 1);
    bits.put(kind, 2);
    extra(&mut bits);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    flat_group(&mut bits, colour);
    bits.finish()
}

#[test]
fn subtract_green_adds_the_green_channel_back() {
    // Every residual pixel is (r, g, b) = (0x10, 0x20, 0x30); the inverse
    // adds green into red and blue.
    let bytes = transformed(2, |_| {}, [0x10, 0x20, 0x30, 0xFF]);
    expect_flat(&bytes, 4, 4, [0x30, 0x20, 0x50, 0xFF]);
}

#[test]
fn a_repeated_transform_is_refused() {
    let mut bits = Bits::new();
    header(&mut bits, 4, 4);
    bits.put(1, 1);
    bits.put(2, 2);
    bits.put(1, 1);
    bits.put(2, 2);
    let bytes = bits.finish();
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpLosslessInvalidTransform)
    );
}

#[test]
fn the_predictor_transform_restores_from_its_declared_mode() {
    // A whole-picture predictor block naming mode one, over residuals that
    // are all zero: the first pixel predicts opaque black, the rest of the
    // top row copy the left, and each later row copies the row above's
    // first pixel down through mode two.
    let bytes = transformed(
        0,
        |bits| {
            // A block covering the whole four-pixel width, whose one pixel's
            // green channel names the mode.
            bits.put(7, 3);
            bits.put(0, 1);
            flat_group(bits, [0, 1, 0, 0]);
        },
        [0, 0, 0, 0],
    );
    expect_flat(&bytes, 4, 4, [0, 0, 0, 0xFF]);
}

#[test]
fn the_colour_transform_adds_its_deltas_back() {
    // Multipliers of zero leave the picture alone, which is what proves the
    // transform's own image is read at the right size and position.
    let bytes = transformed(
        1,
        |bits| {
            bits.put(7, 3);
            bits.put(0, 1);
            flat_group(bits, [0, 0, 0, 0xFF]);
        },
        [0x40, 0x50, 0x60, 0xFF],
    );
    expect_flat(&bytes, 4, 4, [0x40, 0x50, 0x60, 0xFF]);
}

/// A colour-indexing stream over a palette of `colours`, every pixel being
/// index zero.
fn indexed(colours: &[[u8; 4]]) -> Vec<u8> {
    let mut bits = Bits::new();
    header(&mut bits, 8, 1);
    bits.put(1, 1);
    bits.put(3, 2);
    bits.put(
        u32::try_from(colours.len()).expect("a small palette") - 1,
        8,
    );
    // The palette is a one-row picture, subtraction-coded per channel.
    bits.put(0, 1);
    let mut greens = Vec::new();
    let mut reds = Vec::new();
    let mut blues = Vec::new();
    let mut alphas = Vec::new();
    let mut previous = [0u8; 4];
    for colour in colours {
        reds.push(colour[0].wrapping_sub(previous[0]));
        greens.push(colour[1].wrapping_sub(previous[1]));
        blues.push(colour[2].wrapping_sub(previous[2]));
        alphas.push(colour[3].wrapping_sub(previous[3]));
        previous = *colour;
    }
    let write_channel = |bits: &mut Bits, values: &[u8]| -> Option<Prefix> {
        let mut distinct = values.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        if distinct.len() == 1 {
            simple_one(bits, u32::from(distinct[0]));
            return None;
        }
        let width = u8::try_from(distinct.len().next_power_of_two().trailing_zeros())
            .expect("a small width");
        let lengths: Vec<(u16, u8)> = distinct.iter().map(|&v| (u16::from(v), width)).collect();
        Some(Prefix::write(bits, &lengths, 280))
    };
    let green = write_channel(&mut bits, &greens);
    let red = write_channel(&mut bits, &reds);
    let blue = write_channel(&mut bits, &blues);
    let alpha = write_channel(&mut bits, &alphas);
    simple_one(&mut bits, 0);
    for index in 0..colours.len() {
        for (code, values) in [
            (&green, &greens),
            (&red, &reds),
            (&blue, &blues),
            (&alpha, &alphas),
        ] {
            if let Some(code) = code {
                code.emit(&mut bits, u16::from(values[index]));
            }
        }
    }
    // The bundled picture: eight pixels of one-bit indices pack into one.
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    flat_group(&mut bits, [0, 0, 0, 0]);
    bits.finish()
}

#[test]
fn colour_indexing_expands_its_bundled_indices_through_the_palette() {
    let palette = [[0x10u8, 0x20, 0x30, 0xFF], [0x40, 0x50, 0x60, 0x80]];
    let bytes = indexed(&palette);
    expect_flat(&bytes, 8, 1, palette[0]);
}

#[test]
fn the_alpha_entry_point_answers_the_green_channel_as_a_plane() {
    let mut bits = Bits::new();
    // The alpha form carries no signature and no geometry of its own.
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    flat_group(&mut bits, [0, 0x7F, 0, 0]);
    let bytes = bits.finish();
    let plane = decode_alpha(&bytes, 4, 2).expect("a valid alpha plane decodes");
    assert_eq!(plane, vec![0x7Fu8; 8]);
}

#[test]
fn an_alpha_plane_that_ends_early_is_refused() {
    assert_eq!(
        decode_alpha(&[], 4, 2),
        Err(DecodeError::WebpLosslessTruncated)
    );
}

#[test]
fn a_zero_sided_picture_is_refused() {
    // The geometry fields are one less than the size, so a zero side cannot
    // be spelled in a whole stream; the alpha entry point takes its size
    // from its caller and can be handed one.
    assert_eq!(
        decode_alpha(&[0, 0, 0, 0], 0, 4),
        Err(DecodeError::WebpLosslessInvalidGeometry)
    );
}

#[test]
fn a_meta_prefix_arrangement_chooses_a_group_per_region() {
    // Two groups over a two-block-wide picture: the left half is one green
    // value and the right half another.
    let mut bits = Bits::new();
    header(&mut bits, 8, 4);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(1, 1);
    // A block size of four pixels gives two blocks across and one down.
    bits.put(0, 3);
    // The entropy image: two pixels whose red and green name groups 0 and 1.
    bits.put(0, 1);
    let entropy_green = Prefix::write(&mut bits, &[(0, 1), (1, 1)], 280);
    simple_one(&mut bits, 0);
    simple_one(&mut bits, 0);
    simple_one(&mut bits, 0);
    simple_one(&mut bits, 0);
    entropy_green.emit(&mut bits, 0);
    entropy_green.emit(&mut bits, 1);
    // Two groups, each flat.
    flat_group(&mut bits, [0x11, 0x22, 0x33, 0xFF]);
    flat_group(&mut bits, [0x44, 0x55, 0x66, 0xFF]);
    let bytes = bits.finish();
    let image = decode(&bytes, &limits()).expect("a valid stream decodes");
    let pixels = image.pixels().as_chunks::<RGBA_BYTES>().0;
    for row in 0..4 {
        for column in 0..8 {
            let expected = if column < 4 {
                [0x11, 0x22, 0x33, 0xFF]
            } else {
                [0x44, 0x55, 0x66, 0xFF]
            };
            assert_eq!(pixels[row * 8 + column], expected, "at {column},{row}");
        }
    }
}

#[test]
fn a_group_index_the_stream_never_described_is_refused() {
    let mut bits = Bits::new();
    header(&mut bits, 8, 4);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(1, 1);
    bits.put(0, 3);
    bits.put(0, 1);
    // An entropy image naming group 200 while the stream carries only what
    // its remaining bytes could describe.
    simple_one(&mut bits, 200);
    simple_one(&mut bits, 0);
    simple_one(&mut bits, 0);
    simple_one(&mut bits, 0);
    simple_one(&mut bits, 0);
    flat_group(&mut bits, [0, 0, 0, 0]);
    let bytes = bits.finish();
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpLosslessTruncated)
    );
}
