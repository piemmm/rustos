//! Tests for the shared orientation map and the EXIF attribute reader.

use alloc::vec;
use alloc::vec::Vec;

use super::{from_exif, Orientation};

/// Every tag value the format defines, in order.
const ALL: [u32; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

fn tag(code: u32) -> Orientation {
    Orientation::from_tag(code).unwrap_or_else(|| panic!("{code} is a defined orientation"))
}

#[test]
fn only_the_eight_defined_values_are_orientations() {
    for code in ALL {
        assert!(Orientation::from_tag(code).is_some(), "{code}");
    }
    for code in [0, 9, 10, u32::MAX] {
        assert!(Orientation::from_tag(code).is_none(), "{code}");
    }
}

#[test]
fn the_transposing_half_swaps_the_picture_axes() {
    for code in ALL {
        let orientation = tag(code);
        let expected = if code >= 5 { (2, 7) } else { (7, 2) };
        assert_eq!(orientation.picture_size(7, 2), expected, "{code}");
        assert_eq!(orientation.transposes(), code >= 5, "{code}");
    }
}

#[test]
fn every_orientation_permutes_the_picture_exactly() {
    let (stored_w, stored_h) = (5u32, 3u32);
    for code in ALL {
        let orientation = tag(code);
        let (width, height) = orientation.picture_size(stored_w, stored_h);
        let mut seen = vec![false; (width * height) as usize];
        for y in 0..stored_h {
            for x in 0..stored_w {
                let (dx, dy) = orientation.place(x, y, width, height);
                assert!(dx < width && dy < height, "{code} put ({x},{y}) outside");
                let slot = &mut seen[(dy * width + dx) as usize];
                assert!(!*slot, "{code} put two pixels at ({dx},{dy})");
                *slot = true;
            }
        }
        assert!(seen.into_iter().all(|hit| hit), "{code} left a hole");
    }
}

#[test]
fn the_named_corner_of_each_orientation_is_where_the_tag_says() {
    // The stored raster's first pixel, and the corner tag 274 names for it.
    let (width, height) = (4u32, 4u32);
    let corners = [
        (1, (0, 0)),
        (2, (3, 0)),
        (3, (3, 3)),
        (4, (0, 3)),
        (5, (0, 0)),
        (6, (3, 0)),
        (7, (3, 3)),
        (8, (0, 3)),
    ];
    for (code, want) in corners {
        assert_eq!(tag(code).place(0, 0, width, height), want, "{code}");
    }
}

#[test]
fn a_quarter_turn_clockwise_moves_the_top_left_to_the_top_right() {
    // Orientation 6 on a 3-wide, 2-high stored raster: the picture is 2 wide
    // and 3 high, and the stored row runs down the picture's right edge.
    let orientation = tag(6);
    assert_eq!(orientation.picture_size(3, 2), (2, 3));
    assert_eq!(orientation.place(0, 0, 2, 3), (1, 0));
    assert_eq!(orientation.place(2, 0, 2, 3), (1, 2));
    assert_eq!(orientation.place(0, 1, 2, 3), (0, 0));
}

#[test]
fn placing_a_coordinate_outside_the_picture_is_total() {
    // The map is only meaningful inside the picture, but it must answer for
    // anything: a mirrored axis clamps at the near edge instead of wrapping,
    // and the two arms that mirror nothing pass the coordinate through.
    for code in ALL {
        let (x, y) = tag(code).place(u32::MAX, u32::MAX, 4, 4);
        let mirrors_nothing = matches!(code, 1 | 5);
        assert_eq!(
            (x, y) == (u32::MAX, u32::MAX),
            mirrors_nothing,
            "{code} placed ({x},{y})"
        );
        assert_eq!(tag(code).place(0, 0, 0, 0), (0, 0), "{code}");
    }
    assert_eq!(tag(2).place(u32::MAX, 0, 4, 4), (0, 0));
    assert_eq!(tag(3).place(u32::MAX, u32::MAX, 4, 4), (0, 0));
}

/// Build an EXIF `APP1` payload holding `entries`, each a `(tag, type,
/// count, value)` quadruple, in the requested byte order.
fn exif(big: bool, entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
    let u16_bytes = |v: u16| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let u32_bytes = |v: u32| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let mut out = Vec::from(&b"Exif\0\0"[..]);
    out.extend_from_slice(if big { b"MM" } else { b"II" });
    out.extend_from_slice(&u16_bytes(42));
    out.extend_from_slice(&u32_bytes(8));
    let count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&u16_bytes(count));
    for &(tag, kind, count, value) in entries {
        out.extend_from_slice(&u16_bytes(tag));
        out.extend_from_slice(&u16_bytes(kind));
        out.extend_from_slice(&u32_bytes(count));
        // A SHORT is left-justified in the four-byte value field; a LONG
        // fills it.
        if kind == 3 {
            out.extend_from_slice(&u16_bytes(u16::try_from(value).unwrap_or(0)));
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&u32_bytes(value));
        }
    }
    out.extend_from_slice(&u32_bytes(0));
    out
}

#[test]
fn an_orientation_reads_back_in_either_byte_order() {
    for big in [false, true] {
        for code in ALL {
            let payload = exif(big, &[(274, 3, 1, code)]);
            assert_eq!(from_exif(&payload), Orientation::from_tag(code), "{code}");
        }
    }
}

#[test]
fn a_long_typed_orientation_is_read_too() {
    for big in [false, true] {
        let payload = exif(big, &[(274, 4, 1, 6)]);
        assert_eq!(from_exif(&payload), Orientation::from_tag(6));
    }
}

#[test]
fn the_orientation_is_found_among_other_attributes() {
    let payload = exif(false, &[(256, 3, 1, 640), (274, 3, 1, 8), (257, 3, 1, 480)]);
    assert_eq!(from_exif(&payload), Orientation::from_tag(8));
}

#[test]
fn a_block_that_states_no_usable_orientation_reads_as_absent() {
    // Absent, wrong identifier, wrong byte order, wrong magic, no such tag,
    // an undefined value, an unexpected field type, and a count that is not
    // the single value the tag is defined to carry.
    assert_eq!(from_exif(&[]), None);
    assert_eq!(from_exif(b"JFIF\0\0II\x2a\0\x08\0\0\0"), None);

    let mut wrong_order = exif(false, &[(274, 3, 1, 6)]);
    wrong_order[6] = b'X';
    assert_eq!(from_exif(&wrong_order), None);

    let mut wrong_magic = exif(false, &[(274, 3, 1, 6)]);
    wrong_magic[8] = 43;
    assert_eq!(from_exif(&wrong_magic), None);

    assert_eq!(from_exif(&exif(false, &[(256, 3, 1, 640)])), None);
    for bad in [0u32, 9, 255] {
        assert_eq!(from_exif(&exif(false, &[(274, 3, 1, bad)])), None, "{bad}");
    }
    assert_eq!(from_exif(&exif(false, &[(274, 5, 1, 6)])), None);
    assert_eq!(from_exif(&exif(false, &[(274, 3, 2, 6)])), None);
}

#[test]
fn a_truncated_block_never_reads_a_different_orientation() {
    let whole = exif(false, &[(274, 3, 1, 6)]);
    assert_eq!(from_exif(&whole), Orientation::from_tag(6));
    // The entry the tag sits in ends before the block does — the trailing
    // next-directory pointer is not followed — so a cut after the entry
    // still reads the orientation and one before it reads none. Neither may
    // ever produce a *different* value.
    let entry_end = 16 + 12;
    for end in 0..whole.len() {
        let want = if end >= entry_end {
            Orientation::from_tag(6)
        } else {
            None
        };
        assert_eq!(from_exif(&whole[..end]), want, "truncated to {end}");
    }
}

#[test]
fn a_declared_entry_count_the_block_does_not_hold_is_not_walked() {
    let mut payload = exif(false, &[(274, 3, 1, 6)]);
    // Claim a thousand entries in a block holding one: the walk is bounded
    // by the bytes actually present, and the real entry is still found.
    payload[14] = 0xE8;
    payload[15] = 0x03;
    assert_eq!(from_exif(&payload), Orientation::from_tag(6));

    let mut empty = exif(false, &[]);
    empty[14] = 0xFF;
    empty[15] = 0xFF;
    assert_eq!(from_exif(&empty), None);
}

#[test]
fn a_directory_offset_past_the_block_reads_as_absent() {
    let mut payload = exif(false, &[(274, 3, 1, 6)]);
    payload[10] = 0xFF;
    payload[11] = 0xFF;
    payload[12] = 0xFF;
    payload[13] = 0x7F;
    assert_eq!(from_exif(&payload), None);
}
