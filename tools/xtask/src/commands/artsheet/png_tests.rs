//! The encoder is proven by the decoder the desktop itself uses, so a
//! malformed stream fails here rather than in a viewer.

use tairix_image::{decode, DecodeLimits};

use super::encode;

fn limits(width: u32, height: u32) -> DecodeLimits {
    DecodeLimits::new(width, height, u64::from(width) * u64::from(height), 0)
}

/// A picture written and read back is the picture that went in.
#[test]
fn what_it_writes_decodes_back_to_the_same_pixels() {
    for (width, height) in [(1, 1), (7, 3), (64, 64), (259, 5)] {
        let mut rgba = Vec::with_capacity(usize::try_from(width * height * 4).expect("a size"));
        for index in 0u32..width * height {
            rgba.extend_from_slice(&index.wrapping_mul(2_654_435_761).to_be_bytes());
        }
        let png = encode(width, height, &rgba).expect("it encodes");
        let decoded = decode(&png, &limits(width, height)).expect("it decodes");
        assert_eq!((decoded.width(), decoded.height()), (width, height));
        assert_eq!(decoded.pixels(), &rgba[..], "{width}x{height} round trip");
    }
}

/// A picture past one stored block, so the block chaining and the final-block
/// flag are both exercised rather than assumed.
#[test]
fn a_picture_past_one_block_round_trips_too() {
    let (width, height) = (200, 120);
    let rgba: Vec<u8> = (0..width * height * 4)
        .map(|index: u32| u8::try_from(index % 251).expect("inside a byte"))
        .collect();
    let png = encode(width, height, &rgba).expect("it encodes");
    let decoded = decode(&png, &limits(width, height)).expect("it decodes");
    assert_eq!(decoded.pixels(), &rgba[..]);
}

/// The same pixels give the same bytes, which is what lets a sheet be
/// regenerated rather than kept.
#[test]
fn the_same_picture_encodes_to_the_same_bytes() {
    let rgba = vec![0x5Au8; 16 * 16 * 4];
    assert_eq!(
        encode(16, 16, &rgba).expect("it encodes"),
        encode(16, 16, &rgba).expect("it encodes")
    );
}

/// Geometry that does not match its pixels is refused rather than writing a
/// truncated picture.
#[test]
fn a_mismatched_geometry_is_refused() {
    assert!(encode(4, 4, &[0; 12]).is_err());
    assert!(encode(0, 4, &[]).is_err());
    assert!(encode(4, 0, &[]).is_err());
}
