//! WEBP container decoder tests.
//!
//! Every file is assembled here from the two codecs' own fixture writers,
//! so what these tests exercise is the container: the chunk walk, the form
//! rules, the alpha plane, the canvas agreement, and the animation's blend
//! and dispose model.

use alloc::vec;
use alloc::vec::Vec;

use super::{decode, has_signature, probe, RIFF_MAGIC, WEBP_FORM};
use crate::vp8::fixture::{keyframe, Block};
use crate::vp8l::fixture::{flat as flat_lossless, flat_group, header as lossless_header, Bits};
use crate::{sniff, DecodeError, DecodeLimits, ImageFormat, Sequence, SequenceKind, RGBA_BYTES};

/// Generous enough that no fixture here is refused for its size.
fn limits() -> DecodeLimits {
    DecodeLimits::new(256, 256, 256 * 256, 1 << 16)
}

/// One chunk: its identifier, its payload, and the pad byte an odd payload
/// carries.
fn chunk(id: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("a small payload")
            .to_le_bytes(),
    );
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// A whole RIFF form over the chunks given.
fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = WEBP_FORM.to_vec();
    for part in chunks {
        body.extend_from_slice(part);
    }
    let mut out = RIFF_MAGIC.to_vec();
    out.extend_from_slice(
        &u32::try_from(body.len())
            .expect("a small form")
            .to_le_bytes(),
    );
    out.extend_from_slice(&body);
    out
}

/// The extended header's payload: its flags and its canvas.
fn extended(flags: u8, width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![flags, 0, 0, 0];
    out.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    out.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    out
}

/// The animation header's payload: a background colour and a loop count.
fn animation(loop_count: u16) -> Vec<u8> {
    let mut out = vec![0u8; 4];
    out.extend_from_slice(&loop_count.to_le_bytes());
    out
}

/// One animation frame's payload: its placement, its timing, its flags, and
/// its picture's chunks.
fn frame(
    (x, y): (u32, u32),
    (width, height): (u32, u32),
    duration_ms: u32,
    flags: u8,
    body: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::new();
    for value in [x / 2, y / 2, width - 1, height - 1, duration_ms] {
        out.extend_from_slice(&value.to_le_bytes()[..3]);
    }
    out.push(flags);
    for part in body {
        out.extend_from_slice(part);
    }
    out
}

/// A lossless picture of one colour, as the chunk it ships in.
fn lossless(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
    chunk(*b"VP8L", &flat_lossless(width, height, colour))
}

/// A lossy picture, as the chunk it ships in.
fn lossy(width: u32, height: u32) -> Vec<u8> {
    chunk(*b"VP8 ", &keyframe(width, height, Block::flat(0, 0)))
}

/// The colour every pixel of a decoded file must be.
fn expect_flat(bytes: &[u8], width: u32, height: u32, colour: [u8; RGBA_BYTES]) {
    let image = decode(bytes, &limits()).expect("a valid file decodes");
    assert_eq!((image.width(), image.height()), (width, height));
    for (index, pixel) in image
        .pixels()
        .as_chunks::<RGBA_BYTES>()
        .0
        .iter()
        .enumerate()
    {
        assert_eq!(*pixel, colour, "pixel {index}");
    }
}

#[test]
fn the_signature_is_both_halves_of_the_form() {
    let bytes = riff(&[lossless(2, 2, [1, 2, 3, 4])]);
    assert!(has_signature(&bytes));
    assert_eq!(sniff(&bytes), Some(ImageFormat::Webp));
    // A bare RIFF form of some other kind is not a WEBP.
    let mut other = bytes.clone();
    other[8..12].copy_from_slice(b"AVI ");
    assert!(!has_signature(&other));
    assert_eq!(sniff(&other), None);
    // Neither is a file that stops before its form identifier.
    assert!(!has_signature(&bytes[..10]));
}

#[test]
fn no_other_signature_in_the_sniff_order_opens_with_the_riff_magic() {
    // Nothing can shadow the form, because nothing else here begins with
    // its first byte.
    for other in [
        &[0x89u8, b'P', b'N', b'G'][..],
        &[0xFF, 0xD8, 0xFF][..],
        b"GIF",
        b"BM",
        &[0, 0, 1, 0][..],
        b"II*\0",
        b"MM\0*",
    ] {
        assert_ne!(other.first(), RIFF_MAGIC.first(), "{other:?}");
    }
}

#[test]
fn a_simple_lossless_file_decodes() {
    let bytes = riff(&[lossless(5, 3, [0x11, 0x22, 0x33, 0xF0])]);
    assert_eq!(probe(&bytes), Ok((5, 3)));
    expect_flat(&bytes, 5, 3, [0x11, 0x22, 0x33, 0xF0]);
}

#[test]
fn a_simple_lossy_file_decodes() {
    let bytes = riff(&[lossy(16, 16)]);
    assert_eq!(probe(&bytes), Ok((16, 16)));
    expect_flat(&bytes, 16, 16, [130, 130, 130, 255]);
}

#[test]
fn an_extended_still_file_decodes_at_its_declared_canvas() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0, 5, 3)),
        lossless(5, 3, [9, 8, 7, 6]),
    ]);
    assert_eq!(probe(&bytes), Ok((5, 3)));
    expect_flat(&bytes, 5, 3, [9, 8, 7, 6]);
}

#[test]
fn a_bitstream_disagreeing_with_the_canvas_is_refused() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0, 6, 3)),
        lossless(5, 3, [9, 8, 7, 6]),
    ]);
    assert_eq!(probe(&bytes), Err(DecodeError::WebpFrameGeometryMismatch));
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpFrameGeometryMismatch)
    );
}

#[test]
fn the_metadata_chunks_are_read_past() {
    // A colour profile and both metadata chunks are skipped like any chunk
    // this decoder does not act on.
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x2C, 5, 3)),
        chunk(*b"ICCP", &[1, 2, 3]),
        lossless(5, 3, [1, 1, 1, 1]),
        chunk(*b"EXIF", &[4, 5]),
        chunk(*b"XMP ", &[6]),
    ]);
    expect_flat(&bytes, 5, 3, [1, 1, 1, 1]);
}

#[test]
fn a_reserved_extended_flag_or_field_is_refused() {
    for payload in [
        extended(0x01, 4, 4),
        extended(0x40, 4, 4),
        extended(0x80, 4, 4),
        {
            let mut reserved = extended(0, 4, 4);
            reserved[2] = 1;
            reserved
        },
    ] {
        let bytes = riff(&[chunk(*b"VP8X", &payload), lossless(4, 4, [0; 4])]);
        assert_eq!(probe(&bytes), Err(DecodeError::WebpInvalidCanvas));
    }
}

#[test]
fn a_declared_riff_region_the_file_does_not_hold_is_refused() {
    let mut bytes = riff(&[lossless(4, 4, [0; 4])]);
    let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    bytes[4..8].copy_from_slice(&(declared + 16).to_le_bytes());
    assert_eq!(probe(&bytes), Err(DecodeError::WebpTruncated));
}

#[test]
fn a_form_with_no_bitstream_is_refused() {
    let bytes = riff(&[chunk(*b"ICCP", &[1, 2, 3])]);
    assert_eq!(probe(&bytes), Err(DecodeError::WebpInvalidChunkLayout));
    assert_eq!(probe(&riff(&[])), Err(DecodeError::WebpInvalidChunkLayout));
}

#[test]
fn the_extended_forms_own_chunks_cannot_appear_in_the_simple_form() {
    for id in [b"ALPH", b"ANIM", b"ANMF"] {
        let bytes = riff(&[chunk(*id, &[0, 0, 0, 0, 0, 0])]);
        assert_eq!(
            probe(&bytes),
            Err(DecodeError::WebpInvalidChunkLayout),
            "{:?}",
            core::str::from_utf8(id)
        );
    }
}

#[test]
fn a_truncated_file_is_refused_rather_than_completed() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x10, 4, 2)),
        chunk(*b"ALPH", &alpha_chunk(0, 0, &[0x40; 8])),
        lossy(4, 2),
    ]);
    for cut in 0..bytes.len() {
        // Every prefix must refuse or decode, and none may panic.
        let _ = decode(&bytes[..cut], &limits());
    }
}

/// An alpha chunk's payload: its declaration byte and its plane.
fn alpha_chunk(method: u8, filter: u8, plane: &[u8]) -> Vec<u8> {
    let mut out = vec![method | (filter << 2)];
    out.extend_from_slice(plane);
    out
}

#[test]
fn an_uncompressed_alpha_plane_fills_the_pictures_alpha_channel() {
    let plane: Vec<u8> = (0..16u8).map(|value| value * 16).collect();
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x10, 16, 1)),
        chunk(*b"ALPH", &alpha_chunk(0, 0, &[0x40; 16])),
        lossy(16, 1),
    ]);
    let image = decode(&bytes, &limits()).expect("a valid file decodes");
    for pixel in image.pixels().as_chunks::<RGBA_BYTES>().0 {
        assert_eq!(pixel[3], 0x40);
    }
    assert_eq!(plane.len(), 16);
}

#[test]
fn each_alpha_filter_undoes_the_prediction_it_names() {
    // A horizontally filtered row of deltas restores to a running sum; a
    // vertically filtered one restores each row from the row above; and a
    // gradient one from all three neighbours. The first row of every method
    // predicts along itself, so one row of deltas is enough to tell them
    // apart from an unfiltered plane.
    let deltas = [10u8, 10, 10, 10, 10, 10, 10, 10];
    let mut seen = Vec::new();
    for filter in 0..4u8 {
        let bytes = riff(&[
            chunk(*b"VP8X", &extended(0x10, 8, 1)),
            chunk(*b"ALPH", &alpha_chunk(0, filter, &deltas)),
            lossy(8, 1),
        ]);
        let image = decode(&bytes, &limits()).expect("a valid file decodes");
        let alphas: Vec<u8> = image
            .pixels()
            .as_chunks::<RGBA_BYTES>()
            .0
            .iter()
            .map(|pixel| pixel[3])
            .collect();
        seen.push(alphas);
    }
    assert_eq!(seen[0], vec![10u8; 8], "no filtering copies the plane");
    for filtered in &seen[1..] {
        assert_eq!(
            *filtered,
            vec![10u8, 20, 30, 40, 50, 60, 70, 80],
            "the first row predicts from its own left neighbour"
        );
    }
}

#[test]
fn a_compressed_alpha_plane_decodes_through_the_lossless_codec() {
    // The compressed method is a lossless stream over the plane, whose
    // samples are its green channel.
    let mut bits = Bits::new();
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    flat_group(&mut bits, [0, 0x5A, 0, 0]);
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x10, 16, 1)),
        chunk(*b"ALPH", &alpha_chunk(1, 0, &bits.finish())),
        lossy(16, 1),
    ]);
    let image = decode(&bytes, &limits()).expect("a valid file decodes");
    for pixel in image.pixels().as_chunks::<RGBA_BYTES>().0 {
        assert_eq!(pixel[3], 0x5A);
    }
}

#[test]
fn an_alpha_chunk_declaring_a_reserved_value_is_refused() {
    for declaration in [0x02u8, 0x03, 0x20, 0x30, 0x40, 0x80] {
        let mut payload = vec![declaration];
        payload.extend_from_slice(&[0x40; 16]);
        let bytes = riff(&[
            chunk(*b"VP8X", &extended(0x10, 16, 1)),
            chunk(*b"ALPH", &payload),
            lossy(16, 1),
        ]);
        assert_eq!(
            decode(&bytes, &limits()),
            Err(DecodeError::WebpUnsupportedAlpha),
            "declaration {declaration:#04x}"
        );
    }
}

#[test]
fn an_alpha_plane_shorter_than_its_picture_is_refused() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x10, 16, 1)),
        chunk(*b"ALPH", &alpha_chunk(0, 0, &[0x40; 4])),
        lossy(16, 1),
    ]);
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpAlphaGeometryMismatch)
    );
}

#[test]
fn an_alpha_chunk_under_a_clear_flag_still_decodes() {
    // The alpha flag says what a file contains; the chunk present is the
    // fact, and refusing over a disagreement that costs nothing would refuse
    // a picture whose alpha is right there.
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0, 8, 1)),
        chunk(*b"ALPH", &alpha_chunk(0, 0, &[0x33; 8])),
        lossy(8, 1),
    ]);
    let image = decode(&bytes, &limits()).expect("a valid file decodes");
    for pixel in image.pixels().as_chunks::<RGBA_BYTES>().0 {
        assert_eq!(pixel[3], 0x33);
    }
}

#[test]
fn a_set_alpha_flag_with_no_alpha_chunk_decodes_opaque() {
    let bytes = riff(&[chunk(*b"VP8X", &extended(0x10, 8, 1)), lossy(8, 1)]);
    let image = decode(&bytes, &limits()).expect("a valid file decodes");
    for pixel in image.pixels().as_chunks::<RGBA_BYTES>().0 {
        assert_eq!(pixel[3], 0xFF);
    }
}

#[test]
fn an_animation_chunk_under_a_clear_flag_is_refused() {
    // Unlike the alpha flag, this one is what says the file is an animation
    // at all, so the two cannot be allowed to disagree.
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0, 4, 4)),
        chunk(*b"ANIM", &animation(1)),
        chunk(
            *b"ANMF",
            &frame((0, 0), (4, 4), 40, 1, &[lossless(4, 4, [0; 4])]),
        ),
    ]);
    assert_eq!(probe(&bytes), Err(DecodeError::WebpInvalidChunkLayout));
}

#[test]
fn an_alpha_chunk_beside_a_lossless_bitstream_is_refused() {
    // A lossless stream carries its own alpha channel, so the two are two
    // answers to one question.
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x10, 4, 4)),
        chunk(*b"ALPH", &alpha_chunk(0, 0, &[0xFF; 16])),
        lossless(4, 4, [1, 2, 3, 4]),
    ]);
    assert_eq!(probe(&bytes), Err(DecodeError::WebpInvalidChunkLayout));
}

#[test]
fn a_second_alpha_chunk_is_refused() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x10, 4, 2)),
        chunk(*b"ALPH", &alpha_chunk(0, 0, &[0xFF; 8])),
        chunk(*b"ALPH", &alpha_chunk(0, 0, &[0x00; 8])),
        lossy(4, 2),
    ]);
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::WebpInvalidChunkLayout)
    );
}

/// An animation of `frames` lossless pictures, each covering the canvas.
fn animated(canvas: (u32, u32), loop_count: u16, frames: &[([u8; 4], u8, u32)]) -> Vec<u8> {
    let mut chunks = vec![
        chunk(*b"VP8X", &extended(0x02, canvas.0, canvas.1)),
        chunk(*b"ANIM", &animation(loop_count)),
    ];
    for &(colour, flags, duration) in frames {
        chunks.push(chunk(
            *b"ANMF",
            &frame(
                (0, 0),
                canvas,
                duration,
                flags,
                &[lossless(canvas.0, canvas.1, colour)],
            ),
        ));
    }
    riff(&chunks)
}

#[test]
fn an_animation_reports_itself_as_one_and_carries_its_loop_count() {
    let bytes = animated((4, 4), 3, &[([1, 2, 3, 0xFF], 0, 40)]);
    let sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    assert_eq!(sequence.info().format(), ImageFormat::Webp);
    assert_eq!(
        sequence.info().kind(),
        SequenceKind::Animation {
            loop_count: Some(3)
        }
    );
    assert_eq!(sequence.info().count(), 1);
    assert_eq!((sequence.info().width(), sequence.info().height()), (4, 4));
}

#[test]
fn a_loop_count_of_zero_means_for_ever() {
    let bytes = animated((4, 4), 0, &[([1, 2, 3, 0xFF], 0, 40)]);
    let sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    assert_eq!(
        sequence.info().kind(),
        SequenceKind::Animation { loop_count: None }
    );
}

#[test]
fn a_still_file_is_a_one_page_sequence_rather_than_an_animation() {
    // A file carrying no animation header has no loop count to report, and
    // answering "for ever" would fabricate a declaration it never made.
    for bytes in [
        riff(&[lossless(4, 4, [1, 2, 3, 4])]),
        riff(&[
            chunk(*b"VP8X", &extended(0, 4, 4)),
            lossless(4, 4, [1, 2, 3, 4]),
        ]),
    ] {
        let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid file opens");
        assert_eq!(sequence.info().kind(), SequenceKind::Pages);
        assert_eq!(sequence.info().count(), 1);
        let frame = sequence
            .next_frame()
            .expect("the one page decodes")
            .expect("a page is there");
        assert_eq!((frame.width(), frame.height()), (4, 4));
        assert!(sequence.next_frame().expect("the walk ends").is_none());
    }
}

#[test]
fn an_animations_frames_step_in_order_with_their_declared_delays() {
    let bytes = animated(
        (4, 4),
        1,
        &[
            ([0x10, 0x20, 0x30, 0xFF], 1, 40),
            ([0x40, 0x50, 0x60, 0xFF], 1, 70),
        ],
    );
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    assert_eq!(sequence.info().count(), 2);
    let first = sequence
        .next_frame()
        .expect("the first frame decodes")
        .expect("a frame is there");
    assert_eq!(first.index(), 0);
    assert_eq!(first.delay_ns(), 40_000_000);
    assert_eq!(
        first.pixels().as_chunks::<RGBA_BYTES>().0[0],
        [0x10, 0x20, 0x30, 0xFF]
    );
    let second = sequence
        .next_frame()
        .expect("the second frame decodes")
        .expect("a frame is there");
    assert_eq!(second.index(), 1);
    assert_eq!(second.delay_ns(), 70_000_000);
    assert_eq!(
        second.pixels().as_chunks::<RGBA_BYTES>().0[0],
        [0x40, 0x50, 0x60, 0xFF]
    );
    assert!(sequence.next_frame().expect("the walk ends").is_none());
}

#[test]
fn a_rewind_replays_an_animation_from_a_cleared_canvas() {
    let bytes = animated((4, 4), 1, &[([0x10, 0x20, 0x30, 0xFF], 1, 40)]);
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    let first = sequence
        .next_frame()
        .expect("the frame decodes")
        .expect("a frame is there")
        .pixels()
        .to_vec();
    assert!(sequence.next_frame().expect("the walk ends").is_none());
    sequence.rewind();
    let again = sequence
        .next_frame()
        .expect("the frame decodes again")
        .expect("a frame is there");
    assert_eq!(again.pixels(), first.as_slice());
    assert_eq!(again.index(), 0);
}

#[test]
fn addressing_a_frame_composites_forward_to_it() {
    let bytes = animated(
        (4, 4),
        1,
        &[
            ([0x10, 0x20, 0x30, 0xFF], 1, 40),
            ([0x40, 0x50, 0x60, 0xFF], 1, 40),
        ],
    );
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    let second = sequence
        .page(1)
        .expect("the frame decodes")
        .expect("a frame is there");
    assert_eq!(
        second.pixels().as_chunks::<RGBA_BYTES>().0[0],
        [0x40, 0x50, 0x60, 0xFF]
    );
    assert!(sequence.page(2).expect("past the last frame").is_none());
}

#[test]
fn a_still_decode_of_an_animation_answers_its_first_composited_frame() {
    let bytes = animated(
        (4, 4),
        1,
        &[
            ([0x10, 0x20, 0x30, 0xFF], 1, 40),
            ([0x40, 0x50, 0x60, 0xFF], 1, 40),
        ],
    );
    expect_flat(&bytes, 4, 4, [0x10, 0x20, 0x30, 0xFF]);
}

#[test]
fn a_frame_that_does_not_blend_overwrites_what_the_canvas_held() {
    // An opaque red frame that is kept, then a half-transparent green one
    // that declines to blend: the green replaces the red outright.
    let bytes = animated(
        (4, 4),
        1,
        &[
            ([0xFF, 0x00, 0x00, 0xFF], 2, 40),
            ([0x00, 0xFF, 0x00, 0x80], 2, 40),
        ],
    );
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    let second = sequence.page(1).expect("decodes").expect("a frame");
    assert_eq!(
        second.pixels().as_chunks::<RGBA_BYTES>().0[0],
        [0x00, 0xFF, 0x00, 0x80]
    );
}

#[test]
fn a_frame_that_blends_composites_over_what_the_canvas_held() {
    // An opaque red frame, then a half-transparent green one over it: the
    // result is opaque and mixes the two.
    let bytes = animated(
        (4, 4),
        1,
        &[
            ([0xFF, 0x00, 0x00, 0xFF], 2, 40),
            ([0x00, 0xFF, 0x00, 0x80], 0, 40),
        ],
    );
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    let blended = sequence.page(1).expect("decodes").expect("a frame");
    let pixel = blended.pixels().as_chunks::<RGBA_BYTES>().0[0];
    assert_eq!(pixel[3], 0xFF, "an opaque backdrop stays opaque");
    assert!(pixel[0] > 0 && pixel[0] < 0xFF, "red is mixed, not kept");
    assert!(pixel[1] > 0 && pixel[1] < 0xFF, "green is mixed, not kept");
    assert_eq!(pixel[2], 0);
}

#[test]
fn a_disposing_frame_clears_its_rectangle_to_transparent() {
    // The first frame covers only part of the canvas and asks to be
    // disposed; the second covers the rest, so the disposed area is
    // transparent rather than the first frame's colour.
    let canvas = (8u32, 4);
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x02, canvas.0, canvas.1)),
        chunk(*b"ANIM", &animation(1)),
        chunk(
            *b"ANMF",
            &frame(
                (0, 0),
                (4, 4),
                40,
                0x03,
                &[lossless(4, 4, [0xFF, 0, 0, 0xFF])],
            ),
        ),
        chunk(
            *b"ANMF",
            &frame(
                (4, 0),
                (4, 4),
                40,
                0x01,
                &[lossless(4, 4, [0, 0xFF, 0, 0xFF])],
            ),
        ),
    ]);
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid animation opens");
    let second = sequence.page(1).expect("decodes").expect("a frame");
    let pixels = second.pixels().as_chunks::<RGBA_BYTES>().0;
    assert_eq!(pixels[0], [0, 0, 0, 0], "the disposed area is transparent");
    assert_eq!(pixels[4], [0, 0xFF, 0, 0xFF], "the new frame is drawn");
}

#[test]
fn a_frame_reaching_outside_the_canvas_is_refused() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x02, 4, 4)),
        chunk(*b"ANIM", &animation(1)),
        chunk(
            *b"ANMF",
            &frame((2, 0), (4, 4), 40, 1, &[lossless(4, 4, [0; 4])]),
        ),
    ]);
    assert_eq!(
        Sequence::open(&bytes, &limits()).err(),
        Some(DecodeError::WebpFrameOutsideCanvas)
    );
}

#[test]
fn a_frame_whose_payload_disagrees_with_its_rectangle_is_refused() {
    // Nothing is sized from the frame's own declaration: the payload is
    // decoded at the size *it* declares and then refused for disagreeing.
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x02, 8, 8)),
        chunk(*b"ANIM", &animation(1)),
        chunk(
            *b"ANMF",
            &frame((0, 0), (8, 8), 40, 1, &[lossless(4, 4, [0; 4])]),
        ),
    ]);
    let mut sequence = Sequence::open(&bytes, &limits()).expect("the structure is valid");
    assert_eq!(
        sequence.next_frame().err(),
        Some(DecodeError::WebpFrameGeometryMismatch)
    );
    // The refusal is remembered, so no later frame composites onto a canvas
    // that describes no whole frame.
    assert_eq!(
        sequence.next_frame().err(),
        Some(DecodeError::WebpFrameGeometryMismatch)
    );
}

#[test]
fn an_animation_declaring_no_frames_is_refused() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x02, 4, 4)),
        chunk(*b"ANIM", &animation(1)),
    ]);
    assert_eq!(probe(&bytes), Err(DecodeError::WebpNoFrames));
}

#[test]
fn a_bitstream_beside_an_animation_header_is_refused() {
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0x02, 4, 4)),
        chunk(*b"ANIM", &animation(1)),
        lossless(4, 4, [0; 4]),
    ]);
    assert_eq!(probe(&bytes), Err(DecodeError::WebpInvalidChunkLayout));
}

#[test]
fn more_frames_than_the_decoder_walks_are_refused() {
    let mut chunks = vec![
        chunk(*b"VP8X", &extended(0x02, 2, 2)),
        chunk(*b"ANIM", &animation(1)),
    ];
    // An animation frame costs a handful of bytes to declare, so a small
    // file can name a great many; the bound is on the count, before
    // anything is decoded.
    let body = frame((0, 0), (2, 2), 0, 1, &[lossless(2, 2, [0; 4])]);
    for _ in 0..=crate::MAX_ANIMATION_FRAMES {
        chunks.push(chunk(*b"ANMF", &body));
    }
    let bytes = riff(&chunks);
    assert_eq!(probe(&bytes), Err(DecodeError::WebpTooManyFrames));
}

#[test]
fn a_canvas_past_the_callers_limit_is_refused_before_it_is_allocated() {
    let bytes = animated((4, 4), 1, &[([1, 2, 3, 4], 1, 40)]);
    let tight = DecodeLimits::new(2, 2, 4, 0);
    assert_eq!(
        Sequence::open(&bytes, &tight).err(),
        Some(DecodeError::WidthExceedsLimit)
    );
}

#[test]
fn a_fitted_decode_of_a_webp_is_its_natural_size() {
    // Neither codec has a reduced-scale decode process, so there is nothing
    // to choose between: VP8's intra prediction reads full-resolution
    // neighbours and VP8L's predictors read the pixels already produced.
    let bytes = riff(&[lossless(8, 8, [1, 2, 3, 4])]);
    let fitted = crate::decode_fitted(&bytes, &limits(), crate::FitBox::new(2, 2))
        .expect("a valid file decodes");
    assert_eq!((fitted.width(), fitted.height()), (8, 8));
}

#[test]
fn naming_the_format_reaches_the_same_decoder_as_sniffing_it() {
    let bytes = riff(&[lossless(4, 4, [5, 6, 7, 8])]);
    assert_eq!(
        crate::probe_as(ImageFormat::Webp, &bytes),
        crate::probe(&bytes)
    );
    let named =
        crate::decode_as(ImageFormat::Webp, &bytes, &limits()).expect("a valid file decodes");
    let sniffed = decode(&bytes, &limits()).expect("a valid file decodes");
    assert_eq!(named.pixels(), sniffed.pixels());
}

#[test]
fn a_chunk_declaring_more_bytes_than_the_form_holds_is_refused() {
    let mut bytes = riff(&[lossless(4, 4, [0; 4])]);
    let at = RIFF_MAGIC.len() + 4 + WEBP_FORM.len() + 4;
    bytes[at..at + 4].copy_from_slice(&0xFFFF_u32.to_le_bytes());
    assert_eq!(probe(&bytes), Err(DecodeError::WebpTruncated));
}

#[test]
fn an_odd_payload_is_padded_and_the_pad_is_not_part_of_it() {
    // The chunk after an odd-length one is still found, which is only true
    // if the pad byte is walked past rather than read as payload.
    let mut header = Bits::new();
    lossless_header(&mut header, 4, 4);
    header.put(0, 1);
    header.put(0, 1);
    header.put(0, 1);
    flat_group(&mut header, [3, 4, 5, 6]);
    let stream = header.finish();
    let bytes = riff(&[
        chunk(*b"VP8X", &extended(0, 4, 4)),
        chunk(*b"ICCP", &[1, 2, 3]),
        chunk(*b"VP8L", &stream),
    ]);
    expect_flat(&bytes, 4, 4, [3, 4, 5, 6]);
}
