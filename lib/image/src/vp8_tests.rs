//! VP8 keyframe decoder tests.
//!
//! Every bitstream is written here rather than shipped. The boolean encoder
//! is the reference *encoder* from the format's own specification, not the
//! inverse of the decoder beside it, so a round trip tests the decoder
//! against the format; the tree writer finds each leaf's path from the same
//! tree arrays the decoder walks, which is the one thing a fixture cannot
//! usefully re-derive.

use alloc::vec;
use alloc::vec::Vec;

use super::fixture::{keyframe, Block, Rng, Writer};
use super::{decode, inverse_dct, inverse_walsh, probe, BLOCK_COEFFS, START_CODE};
use crate::{DecodeError, DecodeLimits, RGBA_BYTES};

/// Generous enough that no fixture here is refused for its size.
fn limits() -> DecodeLimits {
    DecodeLimits::new(256, 256, 256 * 256, 1 << 16)
}

#[test]
fn the_boolean_coder_round_trips_every_probability() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for _ in 0..200 {
        let count = 1 + rng.next() % 400;
        let choices: Vec<(u8, bool)> = (0..count)
            .map(|_| {
                (
                    u8::try_from(rng.next() % 256).expect("one byte"),
                    rng.next() % 2 == 1,
                )
            })
            .collect();
        let mut writer = Writer::new();
        for &(probability, value) in &choices {
            writer.bit(probability, value);
        }
        let bytes = writer.finish();
        let mut reader = super::Bool::new(&bytes);
        for (index, &(probability, value)) in choices.iter().enumerate() {
            assert_eq!(
                reader.bit(probability) != 0,
                value,
                "bit {index} of {count} disagrees"
            );
        }
        assert!(!reader.exhausted(), "the flush pads the lookahead");
    }
}

/// The one colour every pixel of a decoded fixture must be.
fn uniform(bytes: &[u8], width: u32, height: u32) -> [u8; RGBA_BYTES] {
    let image = decode(bytes, &limits()).expect("a valid keyframe decodes");
    assert_eq!((image.width(), image.height()), (width, height));
    let pixels = image.pixels().as_chunks::<RGBA_BYTES>().0;
    let first = pixels[0];
    for (index, pixel) in pixels.iter().enumerate() {
        assert_eq!(*pixel, first, "pixel {index} breaks the uniform picture");
    }
    first
}

#[test]
fn a_flat_keyframe_predicts_the_midpoint_and_converts_to_grey() {
    // With no macroblock above or to the left, the average prediction has
    // nothing to average and the format fills the block with 128; the
    // conversion then carries that to 130 in each channel.
    let bytes = keyframe(16, 16, Block::flat(0, 0));
    assert_eq!(uniform(&bytes, 16, 16), [130, 130, 130, 255]);
}

#[test]
fn a_probe_reads_the_geometry_without_decoding() {
    let bytes = keyframe(16, 16, Block::flat(0, 0));
    assert_eq!(probe(&bytes), Ok((16, 16)));
}

#[test]
fn a_picture_smaller_than_a_macroblock_is_cropped_to_its_own_size() {
    let bytes = keyframe(5, 3, Block::flat(0, 0));
    assert_eq!(uniform(&bytes, 5, 3), [130, 130, 130, 255]);
}

#[test]
fn the_three_directional_modes_predict_from_the_edges_the_format_invents() {
    // On the top-left macroblock the above row reads 127 and the left
    // column 129, so the vertical, horizontal, and averaging modes each
    // reach a different luma and none of them agree.
    let mut seen = Vec::new();
    for mode in [0usize, 1, 2] {
        let bytes = keyframe(16, 16, Block::flat(mode, mode));
        seen.push(uniform(&bytes, 16, 16));
    }
    assert_ne!(seen[0], seen[1]);
    assert_ne!(seen[1], seen[2]);
    assert_ne!(seen[0], seen[2]);
}

#[test]
fn the_true_motion_mode_decodes_to_a_uniform_picture_at_the_frame_corner() {
    // Its prediction is left + above - corner, and at the frame corner all
    // three are the invented edge values, so the result is flat.
    let bytes = keyframe(16, 16, Block::flat(3, 3));
    let _ = uniform(&bytes, 16, 16);
}

#[test]
fn subblock_prediction_reconstructs_each_block_from_the_one_before() {
    // Every subblock averages the four samples above and the four to its
    // left. The top row averages the invented 127 above against a
    // reconstructed 128, giving 128; every later row averages a
    // reconstructed 128 or 129 against the invented 129, giving 129. So the
    // picture is not flat, and where it steps is exactly what proves the
    // sixteen subblocks were predicted and reconstructed in scan order.
    let bytes = keyframe(16, 16, Block::subblocks(0));
    let image = decode(&bytes, &limits()).expect("a valid keyframe decodes");
    let pixels = image.pixels().as_chunks::<RGBA_BYTES>().0;
    for row in 0..16 {
        let expected = if row < 4 {
            [130, 130, 130, 255]
        } else {
            [132, 132, 132, 255]
        };
        for column in 0..16 {
            assert_eq!(pixels[row * 16 + column], expected, "at {column},{row}");
        }
    }
}

#[test]
fn every_subblock_mode_decodes_to_a_picture() {
    // Ten modes, each of which reads a different set of the samples around
    // its subblock; none may refuse or reach outside the frame.
    for bmode in 0..10 {
        let bytes = keyframe(16, 16, Block::subblocks(bmode));
        let image = decode(&bytes, &limits()).expect("a valid keyframe decodes");
        assert_eq!(image.pixels().len(), 16 * 16 * RGBA_BYTES, "mode {bmode}");
    }
}

#[test]
fn a_luma_dc_coefficient_moves_the_whole_macroblock() {
    // The Walsh-Hadamard transform spreads the luma DC block's one
    // coefficient across all sixteen luma blocks, so a token of four lifts
    // every sample by one and the conversion carries that to 132.
    let mut block = Block::flat(0, 0);
    block.y2_dc = 4;
    let bytes = keyframe(16, 16, block);
    assert_eq!(uniform(&bytes, 16, 16), [132, 132, 132, 255]);
}

#[test]
fn an_interframe_is_refused_rather_than_predicted_from_nothing() {
    let mut bytes = keyframe(16, 16, Block::flat(0, 0));
    bytes[0] |= 1;
    assert_eq!(probe(&bytes), Err(DecodeError::WebpLossyInterframe));
}

#[test]
fn a_profile_the_format_does_not_define_is_refused() {
    let mut bytes = keyframe(16, 16, Block::flat(0, 0));
    bytes[0] |= 0b0000_1110;
    assert_eq!(probe(&bytes), Err(DecodeError::WebpLossyUnsupportedProfile));
}

#[test]
fn a_missing_start_code_is_refused() {
    let mut bytes = keyframe(16, 16, Block::flat(0, 0));
    bytes[4] ^= 0xFF;
    assert_eq!(probe(&bytes), Err(DecodeError::WebpLossyBadStartCode));
}

#[test]
fn a_zero_sided_picture_is_refused() {
    let mut bytes = keyframe(16, 16, Block::flat(0, 0));
    bytes[6] = 0;
    bytes[7] = 0;
    assert_eq!(probe(&bytes), Err(DecodeError::WebpLossyInvalidGeometry));
}

#[test]
fn a_reserved_colour_space_is_refused_rather_than_guessed() {
    // The first bit of the compressed header selects the colour space, and
    // its one reserved value names a space this decoder cannot convert.
    let mut head = Writer::new();
    head.flag(true);
    let first_part = head.finish();
    let mut bytes = vec![
        u8::try_from((u32::try_from(first_part.len()).expect("small") << 5) & 0xFF)
            .expect("one byte"),
        0,
        0,
    ];
    bytes.extend_from_slice(&START_CODE);
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(&first_part);
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpLossyReservedColourSpace)
    );
}

#[test]
fn a_truncated_bitstream_is_refused_rather_than_completed() {
    let bytes = keyframe(16, 16, Block::flat(0, 0));
    for cut in 0..bytes.len() {
        // Every prefix must refuse or decode; none may panic, and none may
        // hand out a picture read from bytes that are not there.
        let _ = decode(&bytes[..cut], &limits());
    }
    assert!(decode(&bytes[..UNCOMPRESSED_ONLY], &limits()).is_err());
}

/// A prefix holding only the uncompressed header, whose partition is empty.
const UNCOMPRESSED_ONLY: usize = 10;

#[test]
fn a_geometry_past_the_callers_limit_is_refused_before_anything_is_reserved() {
    let bytes = keyframe(64, 64, Block::flat(0, 0));
    let tight = DecodeLimits::new(32, 32, 32 * 32, 0);
    assert_eq!(decode(&bytes, &tight), Err(DecodeError::WidthExceedsLimit));
}

#[test]
fn the_direct_current_shortcut_matches_the_whole_inverse_transform() {
    // A block whose only non-zero coefficient is its first has a constant
    // residual, and the reconstruction takes that shortcut; it must agree
    // with the transform it skips at every value.
    for value in [-2048i16, -257, -8, -1, 0, 1, 8, 257, 2047] {
        let mut coeffs = [0i16; BLOCK_COEFFS];
        coeffs[0] = value;
        let residual = inverse_dct(&coeffs);
        let flat = (i32::from(value) + 4) >> 3;
        for (index, sample) in residual.iter().enumerate() {
            assert_eq!(*sample, flat, "coefficient {value}, sample {index}");
        }
    }
}

#[test]
fn the_inverse_transforms_leave_an_empty_block_empty() {
    let empty = [0i16; BLOCK_COEFFS];
    assert_eq!(inverse_dct(&empty), [0i32; BLOCK_COEFFS]);
    assert_eq!(inverse_walsh(&empty), empty);
}

#[test]
fn the_walsh_transform_spreads_one_coefficient_over_every_output() {
    for value in [8i16, 32, 256, -32] {
        let mut coeffs = [0i16; BLOCK_COEFFS];
        coeffs[0] = value;
        let spread = inverse_walsh(&coeffs);
        let expected = i16::try_from((i32::from(value) + 3) >> 3).expect("in range");
        assert_eq!(spread, [expected; BLOCK_COEFFS], "coefficient {value}");
    }
}

#[test]
fn the_inverse_transforms_are_linear_in_their_input() {
    // Doubling every coefficient doubles the residual, within the rounding
    // the format's own arithmetic performs.
    let mut coeffs = [0i16; BLOCK_COEFFS];
    for (index, value) in coeffs.iter_mut().enumerate() {
        *value = i16::try_from(index * 16).expect("in range");
    }
    let single = inverse_dct(&coeffs);
    let mut doubled = coeffs;
    for value in &mut doubled {
        *value *= 2;
    }
    let twice = inverse_dct(&doubled);
    for (one, two) in single.iter().zip(&twice) {
        assert!((two - 2 * one).abs() <= 1, "{one} doubled is {two}");
    }
}
