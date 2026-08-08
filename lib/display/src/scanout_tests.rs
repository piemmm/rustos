//! Tests for the shared scan-out encode: the byte order per format, the
//! frame sizing, and the damage conversion.

use super::{scanout_len, sub_screen_damage, ChannelOrder};
use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_geometry::Rect;
use tairix_raster::Pixel;

/// A 4-bytes-per-pixel mode with a tight stride.
fn mode(width_px: u32, height_px: u32, format: DisplayFormat) -> DisplayMode {
    DisplayMode {
        width_px,
        height_px,
        stride_bytes: width_px * 4,
        format,
    }
}

#[test]
fn each_known_format_maps_to_its_byte_order() {
    assert_eq!(
        ChannelOrder::for_format(DisplayFormat::Rgba8888),
        Some(ChannelOrder::Rgba)
    );
    assert_eq!(
        ChannelOrder::for_format(DisplayFormat::Bgra8888),
        Some(ChannelOrder::Bgra)
    );
}

#[test]
fn a_pixel_encodes_into_the_order_its_format_wants() {
    let pixel = Pixel {
        r: 0x10,
        g: 0x20,
        b: 0x30,
        a: 0xFF,
    };
    assert_eq!(ChannelOrder::Rgba.encode(pixel), [0x10, 0x20, 0x30, 0xFF]);
    assert_eq!(ChannelOrder::Bgra.encode(pixel), [0x30, 0x20, 0x10, 0xFF]);
}

#[test]
fn a_frame_is_sized_from_the_stride_and_height() {
    let len = scanout_len(&mode(4, 3, DisplayFormat::Rgba8888)).expect("valid mode");
    assert_eq!(len, 4 * 4 * 3);
}

#[test]
fn a_padded_stride_is_honoured_rather_than_recomputed() {
    let padded = DisplayMode {
        stride_bytes: 40,
        ..mode(4, 2, DisplayFormat::Bgra8888)
    };
    assert_eq!(scanout_len(&padded), Some(80));
}

#[test]
fn a_mode_that_cannot_describe_a_frame_is_refused() {
    // Zero extents, and a stride too small for one scanline.
    assert!(scanout_len(&mode(0, 3, DisplayFormat::Rgba8888)).is_none());
    assert!(scanout_len(&mode(4, 0, DisplayFormat::Rgba8888)).is_none());
    let short = DisplayMode {
        stride_bytes: 15,
        ..mode(4, 3, DisplayFormat::Rgba8888)
    };
    assert!(scanout_len(&short).is_none());
}

#[test]
fn an_enormous_mode_is_refused_rather_than_overflowing() {
    let huge = DisplayMode {
        width_px: u32::MAX,
        height_px: u32::MAX,
        stride_bytes: u32::MAX,
        format: DisplayFormat::Rgba8888,
    };
    assert!(scanout_len(&huge).is_none());
}

#[test]
fn a_sub_region_converts_and_a_full_screen_one_falls_back_to_the_whole_frame() {
    let mode = mode(64, 48, DisplayFormat::Rgba8888);
    let damage = sub_screen_damage(&Rect::new(4, 8, 16, 12), &mode).expect("a sub-region");
    assert_eq!((damage.x, damage.y), (4, 8));
    assert_eq!((damage.width_px, damage.height_px), (16, 12));

    // Covering the screen means the full present path, not a region.
    assert!(sub_screen_damage(&Rect::new(0, 0, 64, 48), &mode).is_none());
}

#[test]
fn an_empty_or_unrepresentable_rectangle_falls_back_to_the_whole_frame() {
    let mode = mode(64, 48, DisplayFormat::Rgba8888);
    assert!(sub_screen_damage(&Rect::new(4, 8, 0, 12), &mode).is_none());
    assert!(sub_screen_damage(&Rect::new(4, 8, 16, 0), &mode).is_none());
    // A negative origin cannot be expressed on the wire; present the whole
    // frame rather than a wrong region.
    assert!(sub_screen_damage(&Rect::new(-1, 8, 16, 12), &mode).is_none());
    assert!(sub_screen_damage(&Rect::new(4, -1, 16, 12), &mode).is_none());
}
