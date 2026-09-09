//! ICO and CUR decoder tests: the directory, the mask, the alpha channel
//! that supersedes it, PNG-compressed entries, and every refusal.
//!
//! Every input is built here, entry by entry — the crate ships no fixtures.
//! [`Entry`] carries one picture and the directory row describing it, so a
//! test states only what it is about; [`icon`] lays a set of them out.

use alloc::vec;
use alloc::vec::Vec;

use crate::{
    decode, decode_fitted, probe, DecodeError, DecodeLimits, FitBox, ImageFormat, Sequence,
    SequenceKind,
};

/// Limits generous enough for every fixture here.
fn limits() -> DecodeLimits {
    DecodeLimits::new(256, 256, 256 * 256, 0)
}

const ICON: u16 = 1;
const CURSOR: u16 = 2;

/// One picture and the size the directory should claim for it.
struct Entry {
    width: u8,
    height: u8,
    picture: Vec<u8>,
}

/// Lay a directory and its pictures out.
fn container(kind: u16, entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap_or(0).to_le_bytes());
    let mut at = 6 + entries.len() * 16;
    for entry in entries {
        out.push(entry.width);
        out.push(entry.height);
        out.push(0); // colour count
        out.push(0); // reserved
        out.extend_from_slice(&0u16.to_le_bytes()); // planes, or a hot spot
        out.extend_from_slice(&0u16.to_le_bytes()); // bit count, or a hot spot
        out.extend_from_slice(
            &u32::try_from(entry.picture.len())
                .unwrap_or(0)
                .to_le_bytes(),
        );
        out.extend_from_slice(&u32::try_from(at).unwrap_or(0).to_le_bytes());
        at += entry.picture.len();
    }
    for entry in entries {
        out.extend_from_slice(&entry.picture);
    }
    out
}

/// An icon of one entry.
fn icon(entry: Entry) -> Vec<u8> {
    container(ICON, &[entry])
}

/// A `BITMAPINFOHEADER` for an icon: `height` is doubled, because the mask
/// follows the colour rows.
fn dib_header(width: i32, height: i32, bits: u16, clr_used: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&(height * 2).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&clr_used.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// Bytes one row of `width` pixels at `bits` occupies once padded out.
fn stride(width: usize, bits: usize) -> usize {
    (width * bits).div_ceil(32) * 4
}

/// A 1-bit AND mask, given top first as one flag per pixel, laid out
/// bottom-up like the colour rows above it.
fn mask_rows(width: usize, rows: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows.iter().rev() {
        let start = out.len();
        for chunk in row.chunks(8) {
            let mut byte = 0u8;
            for (bit, &flag) in chunk.iter().enumerate() {
                if flag != 0 {
                    byte |= 0x80 >> bit;
                }
            }
            out.push(byte);
        }
        while out.len() - start < stride(width, 1) {
            out.push(0);
        }
    }
    out
}

/// A 32-bit icon entry: `colours` are `0xAARRGGBB` words given top first.
fn bgra_entry(width: u8, height: u8, colours: &[Vec<u32>], mask: &[Vec<u8>]) -> Entry {
    let mut picture = dib_header(i32::from(width), i32::from(height), 32, 0);
    for row in colours.iter().rev() {
        for &word in row {
            picture.extend_from_slice(&word.to_le_bytes());
        }
    }
    picture.extend_from_slice(&mask_rows(usize::from(width), mask));
    Entry {
        width,
        height,
        picture,
    }
}

/// An 8-bit indexed icon entry over a two-colour table: black then white.
fn indexed_entry(width: u8, height: u8, indices: &[Vec<u8>], mask: &[Vec<u8>]) -> Entry {
    let mut picture = dib_header(i32::from(width), i32::from(height), 8, 2);
    picture.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    picture.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);
    for row in indices.iter().rev() {
        let start = picture.len();
        picture.extend_from_slice(row);
        while picture.len() - start < stride(usize::from(width), 8) {
            picture.push(0);
        }
    }
    picture.extend_from_slice(&mask_rows(usize::from(width), mask));
    Entry {
        width,
        height,
        picture,
    }
}

/// The decoded pixels of `bytes`, as one RGBA quad per pixel.
fn pixels_of(bytes: &[u8]) -> Vec<[u8; 4]> {
    let image = decode(bytes, &limits()).expect("fixture failed to decode");
    image.pixels().as_chunks::<4>().0.to_vec()
}

#[test]
fn a_thirty_two_bit_entry_reads_its_fourth_byte_as_alpha() {
    // The same bytes in a plain BMP file would be opaque; in an icon they
    // are the alpha channel every writer since Windows XP fills.
    let bytes = icon(bgra_entry(
        2,
        1,
        &[vec![0x80FF_0000, 0xFF00_FF00]],
        &[vec![0, 0]],
    ));
    assert_eq!(crate::sniff(&bytes), Some(ImageFormat::Ico));
    assert_eq!(
        pixels_of(&bytes),
        vec![[0xFF, 0x00, 0x00, 0x80], [0x00, 0xFF, 0x00, 0xFF]]
    );
}

#[test]
fn an_alpha_channel_supersedes_the_mask() {
    // A writer that fills alpha leaves the mask zero; one that fills both
    // must not have its opaque pixels erased by a stale mask bit.
    let bytes = icon(bgra_entry(
        2,
        1,
        &[vec![0xFFFF_0000, 0xFF00_FF00]],
        &[vec![1, 1]],
    ));
    assert_eq!(
        pixels_of(&bytes),
        vec![[0xFF, 0x00, 0x00, 0xFF], [0x00, 0xFF, 0x00, 0xFF]]
    );
}

#[test]
fn an_all_zero_alpha_channel_falls_back_to_the_mask() {
    // A pre-XP writer left the fourth byte alone, so honouring it would show
    // nothing at all; the mask is what says which pixels are absent.
    let bytes = icon(bgra_entry(
        2,
        1,
        &[vec![0x0000_00FF, 0x0000_FF00]],
        &[vec![0, 1]],
    ));
    assert_eq!(
        pixels_of(&bytes),
        vec![[0x00, 0x00, 0xFF, 0xFF], [0x00, 0xFF, 0x00, 0x00]]
    );
}

#[test]
fn an_indexed_entry_takes_its_transparency_from_the_mask() {
    let bytes = icon(indexed_entry(
        2,
        2,
        &[vec![0, 1], vec![1, 0]],
        &[vec![0, 1], vec![1, 0]],
    ));
    assert_eq!(
        pixels_of(&bytes),
        vec![
            [0x00, 0x00, 0x00, 0xFF],
            [0xFF, 0xFF, 0xFF, 0x00],
            [0xFF, 0xFF, 0xFF, 0x00],
            [0x00, 0x00, 0x00, 0xFF],
        ]
    );
}

#[test]
fn a_cursor_directory_decodes_like_an_icon() {
    // The two 16-bit fields hold a hot spot rather than planes and a bit
    // count, and neither is what says how to decode the picture.
    let entry = bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]);
    let bytes = container(CURSOR, &[entry]);
    assert_eq!(crate::sniff(&bytes), Some(ImageFormat::Ico));
    assert_eq!(pixels_of(&bytes), vec![[0x11, 0x22, 0x33, 0xFF]]);
}

#[test]
fn a_top_down_entry_applies_its_mask_in_the_same_order() {
    // A negative height flips the colour rows and the mask rows together.
    let mut picture = dib_header(1, -2, 32, 0);
    for word in [0xFFFF_0000u32, 0xFF00_FF00] {
        picture.extend_from_slice(&word.to_le_bytes());
    }
    // Two mask rows, top first: the top pixel is absent.
    picture.extend_from_slice(&[0x80, 0, 0, 0, 0x00, 0, 0, 0]);
    let bytes = icon(Entry {
        width: 1,
        height: 2,
        picture,
    });
    // The alpha channel is not all zero, so the mask does not apply.
    assert_eq!(
        pixels_of(&bytes),
        vec![[0xFF, 0x00, 0x00, 0xFF], [0x00, 0xFF, 0x00, 0xFF]]
    );
}

#[test]
fn a_top_down_indexed_entry_applies_its_mask_in_the_same_order() {
    let mut picture = dib_header(1, -2, 8, 2);
    picture.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    picture.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);
    picture.extend_from_slice(&[0, 0, 0, 0]); // top row: index 0
    picture.extend_from_slice(&[1, 0, 0, 0]); // bottom row: index 1
    picture.extend_from_slice(&[0x80, 0, 0, 0]); // top row masked out
    picture.extend_from_slice(&[0x00, 0, 0, 0]);
    let bytes = icon(Entry {
        width: 1,
        height: 2,
        picture,
    });
    assert_eq!(
        pixels_of(&bytes),
        vec![[0x00, 0x00, 0x00, 0x00], [0xFF, 0xFF, 0xFF, 0xFF]]
    );
}

/// A black greyscale PNG, which is what a PNG-compressed entry carries in
/// place of a bitmap. Built through the same writer the PNG tests use, so
/// the entry is a real file rather than a shape.
fn png_entry(width: u8, height: u8) -> Entry {
    let scanlines = vec![0u8; (usize::from(width) + 1) * usize::from(height)];
    Entry {
        width,
        height,
        picture: crate::png_fixture::build_png(
            u32::from(width),
            u32::from(height),
            8,
            0,
            0,
            None,
            None,
            &scanlines,
        ),
    }
}

#[test]
fn a_png_compressed_entry_decodes_through_the_png_decoder() {
    let bytes = icon(png_entry(4, 4));
    let image = decode(&bytes, &limits()).expect("a PNG entry failed to decode");
    assert_eq!((image.width(), image.height()), (4, 4));
    let info = probe(&bytes).expect("a PNG entry failed to probe");
    assert_eq!((info.width(), info.height()), (4, 4));
    assert_eq!(info.format(), ImageFormat::Ico);
}

#[test]
fn a_plain_decode_answers_the_largest_entry() {
    let bytes = container(
        ICON,
        &[
            bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]),
            bgra_entry(4, 4, &vec![vec![0xFF44_5566; 4]; 4], &vec![vec![0; 4]; 4]),
            bgra_entry(2, 2, &vec![vec![0xFF77_8899; 2]; 2], &vec![vec![0; 2]; 2]),
        ],
    );
    let image = decode(&bytes, &limits()).expect("a valid icon failed to decode");
    assert_eq!((image.width(), image.height()), (4, 4));
    let info = probe(&bytes).expect("a valid icon failed to probe");
    assert_eq!((info.width(), info.height()), (4, 4));
}

#[test]
fn a_fitted_decode_takes_the_smallest_covering_entry() {
    let bytes = container(
        ICON,
        &[
            bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]),
            bgra_entry(4, 4, &vec![vec![0xFF44_5566; 4]; 4], &vec![vec![0; 4]; 4]),
            bgra_entry(2, 2, &vec![vec![0xFF77_8899; 2]; 2], &vec![vec![0; 2]; 2]),
        ],
    );
    for (want, expect) in [(1, 1), (2, 2), (3, 4), (4, 4)] {
        let image = decode_fitted(&bytes, &limits(), FitBox::new(want, want))
            .expect("a valid icon failed to decode");
        assert_eq!(
            (image.width(), image.height()),
            (expect, expect),
            "a fit of {want} took the wrong entry"
        );
    }
    // Nothing covers a box larger than every entry, so the largest stands.
    let image = decode_fitted(&bytes, &limits(), FitBox::new(64, 64))
        .expect("a valid icon failed to decode");
    assert_eq!((image.width(), image.height()), (4, 4));
}

#[test]
fn a_fitted_decode_falls_back_to_the_largest_the_limits_allow() {
    let bytes = container(
        ICON,
        &[
            bgra_entry(2, 2, &vec![vec![0xFF11_2233; 2]; 2], &vec![vec![0; 2]; 2]),
            bgra_entry(8, 8, &vec![vec![0xFF44_5566; 8]; 8], &vec![vec![0; 8]; 8]),
        ],
    );
    let tight = DecodeLimits::new(4, 4, 16, 0);
    let image = decode_fitted(&bytes, &tight, FitBox::new(8, 8))
        .expect("a covering entry within the limits was refused");
    assert_eq!((image.width(), image.height()), (2, 2));
}

#[test]
fn a_fitted_decode_refuses_when_no_entry_fits_the_limits() {
    let bytes = icon(bgra_entry(
        8,
        8,
        &vec![vec![0xFF44_5566; 8]; 8],
        &vec![vec![0; 8]; 8],
    ));
    let tight = DecodeLimits::new(4, 4, 16, 0);
    assert_eq!(
        decode_fitted(&bytes, &tight, FitBox::new(2, 2)),
        Err(DecodeError::WidthExceedsLimit)
    );
}

#[test]
fn a_plain_decode_refuses_a_largest_entry_beyond_the_limits() {
    // A plain decode always means the picture the container is, so a
    // too-large one is refused rather than quietly swapped for a smaller.
    let bytes = container(
        ICON,
        &[
            bgra_entry(2, 2, &vec![vec![0xFF11_2233; 2]; 2], &vec![vec![0; 2]; 2]),
            bgra_entry(8, 8, &vec![vec![0xFF44_5566; 8]; 8], &vec![vec![0; 8]; 8]),
        ],
    );
    let tight = DecodeLimits::new(4, 4, 16, 0);
    assert_eq!(decode(&bytes, &tight), Err(DecodeError::WidthExceedsLimit));
}

#[test]
fn a_sequence_walks_every_page_and_addresses_them_independently() {
    let bytes = container(
        ICON,
        &[
            bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]),
            bgra_entry(2, 2, &vec![vec![0xFF44_5566; 2]; 2], &vec![vec![0; 2]; 2]),
        ],
    );
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid icon failed to open");
    assert_eq!(sequence.info().format(), ImageFormat::Ico);
    assert_eq!(sequence.info().count(), 2);
    assert_eq!(sequence.info().kind(), SequenceKind::Pages);
    // The container's own geometry is its largest page's.
    assert_eq!((sequence.info().width(), sequence.info().height()), (2, 2));

    let mut seen = Vec::new();
    while let Some(frame) = sequence.next_frame().expect("a page failed to decode") {
        seen.push((
            frame.index(),
            frame.width(),
            frame.height(),
            frame.delay_ns(),
        ));
    }
    assert_eq!(seen, vec![(0, 1, 1, 0), (1, 2, 2, 0)]);

    // Addressed access reaches any page, in any order, more than once.
    for (index, side) in [(1u32, 2u32), (0, 1), (1, 2)] {
        let frame = sequence
            .page(index)
            .expect("a page failed to decode")
            .expect("a page in range answered none");
        assert_eq!(
            (frame.index(), frame.width(), frame.height()),
            (index, side, side)
        );
    }
    assert!(sequence
        .page(2)
        .expect("a page past the last failed")
        .is_none());
    assert!(sequence
        .page(u32::MAX)
        .expect("a page past the last failed")
        .is_none());
}

#[test]
fn a_page_refusal_is_remembered_until_rewind() {
    let good = bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]);
    let mut broken = bgra_entry(1, 1, &[vec![0xFF44_5566]], &[vec![0]]);
    broken.picture.truncate(40);
    let bytes = container(ICON, &[good, broken]);
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a valid icon failed to open");
    assert!(sequence
        .next_frame()
        .expect("the first page failed")
        .is_some());
    let refusal = sequence.next_frame().expect_err("a broken page decoded");
    assert_eq!(
        sequence.next_frame().expect_err("a refusal was forgotten"),
        refusal
    );
    sequence.rewind();
    assert!(sequence
        .next_frame()
        .expect("a rewind did not clear the refusal")
        .is_some());
    // The good page is still reachable by address across the refusal.
    assert!(sequence.page(0).expect("addressing failed").is_some());
}

#[test]
fn one_unreadable_page_does_not_refuse_the_readable_ones() {
    let mut broken = bgra_entry(1, 1, &[vec![0]], &[vec![0]]);
    broken.picture.truncate(4);
    let bytes = container(
        ICON,
        &[
            broken,
            bgra_entry(2, 2, &vec![vec![0xFF44_5566; 2]; 2], &vec![vec![0; 2]; 2]),
        ],
    );
    let image = decode(&bytes, &limits()).expect("a readable page was refused");
    assert_eq!((image.width(), image.height()), (2, 2));
}

#[test]
fn a_directory_of_only_unreadable_pages_reports_the_first_refusal() {
    let mut broken = bgra_entry(1, 1, &[vec![0]], &[vec![0]]);
    broken.picture.truncate(4);
    let bytes = icon(broken);
    assert_eq!(decode(&bytes, &limits()), Err(DecodeError::BmpTruncated));
}

#[test]
fn an_odd_bitmap_height_is_refused() {
    // An icon's bitmap is the colour rows and the mask over them, so its
    // declared height is always even.
    let mut picture = dib_header(1, 1, 32, 0);
    // `dib_header` doubled the height, so put an odd one back.
    picture[8..12].copy_from_slice(&3i32.to_le_bytes());
    picture.extend_from_slice(&[0; 16]);
    let bytes = icon(Entry {
        width: 1,
        height: 1,
        picture,
    });
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::IcoInvalidMaskHeight)
    );
}

#[test]
fn a_bad_signature_is_refused() {
    let mut bytes = icon(bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]));
    for (at, value) in [(0usize, 1u8), (2, 3), (3, 1)] {
        let mut broken = bytes.clone();
        broken[at] = value;
        assert_eq!(crate::sniff(&broken), None);
        assert_eq!(
            crate::decode(&broken, &limits()),
            Err(DecodeError::UnknownFormat),
            "byte {at} set to {value} was still sniffed as an icon"
        );
    }
    // A recognisable header whose count is zero reaches the decoder and is
    // refused there, with the reason.
    bytes[4] = 0;
    bytes[5] = 0;
    assert_eq!(crate::sniff(&bytes), Some(ImageFormat::Ico));
    assert_eq!(decode(&bytes, &limits()), Err(DecodeError::IcoNoEntries));
    assert_eq!(probe(&bytes), Err(DecodeError::IcoNoEntries));
}

#[test]
fn an_entry_reaching_past_the_input_is_refused() {
    let mut bytes = icon(bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]));
    // Entry zero's declared length starts at directory offset 6 + 8.
    bytes[14..18].copy_from_slice(&0xFFFF_u32.to_le_bytes());
    assert_eq!(decode(&bytes, &limits()), Err(DecodeError::IcoTruncated));

    let mut moved = icon(bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]));
    moved[18..22].copy_from_slice(&0xFFFF_FFFF_u32.to_le_bytes());
    assert_eq!(decode(&moved, &limits()), Err(DecodeError::IcoTruncated));
}

#[test]
fn a_truncated_directory_is_refused() {
    let bytes = icon(bgra_entry(1, 1, &[vec![0xFF11_2233]], &[vec![0]]));
    for cut in 4..22 {
        assert_eq!(
            decode(&bytes[..cut], &limits()),
            Err(DecodeError::IcoTruncated),
            "a directory cut at {cut} was not refused"
        );
    }
}

#[test]
fn the_bitmap_rather_than_the_directory_says_how_big_a_page_is() {
    // A 256-pixel side is spelled zero in the directory, and real files get
    // the field wrong besides, so nothing acts on it.
    let mut entry = bgra_entry(2, 2, &vec![vec![0xFF11_2233; 2]; 2], &vec![vec![0; 2]; 2]);
    entry.width = 0;
    entry.height = 99;
    let bytes = icon(entry);
    let image = decode(&bytes, &limits()).expect("a valid icon failed to decode");
    assert_eq!((image.width(), image.height()), (2, 2));
}

#[test]
fn every_prefix_of_a_one_page_icon_is_refused_rather_than_half_decoded() {
    let bytes = icon(indexed_entry(
        3,
        2,
        &[vec![0, 1, 0], vec![1, 0, 1]],
        &vec![vec![0; 3]; 2],
    ));
    assert!(decode(&bytes, &limits()).is_ok());
    for cut in 0..bytes.len() {
        assert!(
            decode(&bytes[..cut], &limits()).is_err(),
            "an icon cut at {cut} decoded"
        );
    }
}

#[test]
fn a_cut_that_loses_one_page_still_answers_a_whole_one() {
    // Passing over an unreadable page is the point: a file truncated past a
    // complete page answers that page rather than nothing, and never
    // anything that is not one of the pages it really holds.
    let bytes = container(
        ICON,
        &[
            bgra_entry(
                2,
                2,
                &vec![vec![0xFF11_2233; 2]; 2],
                &[vec![0, 1], vec![1, 0]],
            ),
            indexed_entry(3, 2, &[vec![0, 1, 0], vec![1, 0, 1]], &vec![vec![0; 3]; 2]),
        ],
    );
    for cut in 0..bytes.len() {
        if let Ok(image) = decode(&bytes[..cut], &limits()) {
            assert!(
                matches!((image.width(), image.height()), (2 | 3, 2)),
                "a cut at {cut} answered a picture the file does not hold"
            );
        }
    }
}
