use alloc::vec;

use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_cursor::{CursorImage, PlacedCursor, Shape, VectorCursor, Vertex};
use tairix_geometry::{Point, Rect};
use tairix_raster::{Color, Pixel, Surface};

use super::{Present, Scanout, PIXEL_BYTES};

/// A four-pixel-wide mode with one spare pixel of stride padding, so a wrong
/// stride shows up as a shifted row rather than passing by luck.
fn padded_mode(format: DisplayFormat) -> DisplayMode {
    DisplayMode {
        width_px: 4,
        height_px: 3,
        stride_bytes: 5 * 4,
        format,
    }
}

fn pixel(r: u8, g: u8, b: u8) -> Pixel {
    Pixel { r, g, b, a: 255 }
}

/// A surface of `mode`'s extent, every pixel distinct so a misplaced copy is
/// visible.
fn numbered(mode: &DisplayMode) -> Surface {
    let mut surface = Surface::new(mode.width_px, mode.height_px).expect("a tiny surface");
    for y in 0..mode.height_px {
        for x in 0..mode.width_px {
            let index = u8::try_from(y * mode.width_px + x).expect("a tiny surface");
            surface.set(x, y, pixel(index + 1, index + 2, index + 3));
        }
    }
    surface
}

/// A `size`×`size` cursor whose left half is opaque `colour` and whose right
/// half is transparent, with its hotspot at the image's top-left corner so a
/// pointer position is also the origin it draws from.
fn cursor(size: u32, colour: Color) -> CursorImage {
    let s = i32::try_from(size).expect("a tiny cursor");
    let shape = Shape::new(
        colour,
        vec![
            Vertex::new(0, 0),
            Vertex::new(s / 2, 0),
            Vertex::new(s / 2, s),
            Vertex::new(0, s),
        ],
    );
    VectorCursor::new(size, 0, 0, vec![shape])
        .rasterise(100)
        .expect("renderable")
}

/// A region present of exactly that rectangle.
fn region(x: u32, y: u32, width_px: u32, height_px: u32) -> Present {
    Present::Region(DamageRect {
        x,
        y,
        width_px,
        height_px,
    })
}

/// The four bytes frame `frame` holds for pixel `(x, y)` of `mode`.
fn at(frame: &[u8], mode: &DisplayMode, x: u32, y: u32) -> [u8; 4] {
    let stride = mode.stride_bytes as usize;
    let start = y as usize * stride + x as usize * PIXEL_BYTES;
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&frame[start..start + PIXEL_BYTES]);
    bytes
}

#[test]
fn a_mode_with_no_pixels_has_no_frame() {
    let empty = DisplayMode {
        width_px: 0,
        height_px: 0,
        stride_bytes: 0,
        format: DisplayFormat::Rgba8888,
    };
    assert!(Scanout::new(empty).is_none());
}

#[test]
fn a_stride_too_small_for_a_scanline_is_refused() {
    let narrow = DisplayMode {
        width_px: 8,
        height_px: 8,
        stride_bytes: 8,
        format: DisplayFormat::Rgba8888,
    };
    assert!(Scanout::new(narrow).is_none());
}

#[test]
fn the_frame_covers_the_mode() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let scanout = Scanout::new(mode).expect("a valid mode");
    assert_eq!(scanout.screen(), Rect::new(0, 0, 4, 3));
    assert_eq!(scanout.mode(), &mode);
    assert_eq!(scanout.frame().len(), 3 * 5 * 4);
}

#[test]
fn a_whole_screen_composition_presents_the_whole_frame() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let painted = numbered(&mode);
    assert_eq!(scanout.compose(&painted, None, None), Present::Whole);
    assert_eq!(at(scanout.frame(), &mode, 0, 0), [1, 2, 3, 255]);
    assert_eq!(at(scanout.frame(), &mode, 3, 2), [12, 13, 14, 255]);
}

#[test]
fn the_channel_order_follows_the_format() {
    let rgba = padded_mode(DisplayFormat::Rgba8888);
    let mut in_rgba = Scanout::new(rgba).expect("a valid mode");
    in_rgba.compose(&numbered(&rgba), None, None);
    assert_eq!(at(in_rgba.frame(), &rgba, 0, 0), [1, 2, 3, 255]);

    let bgra = padded_mode(DisplayFormat::Bgra8888);
    let mut in_bgra = Scanout::new(bgra).expect("a valid mode");
    in_bgra.compose(&numbered(&bgra), None, None);
    assert_eq!(at(in_bgra.frame(), &bgra, 0, 0), [3, 2, 1, 255]);
}

#[test]
fn the_stride_padding_is_never_written() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    scanout.compose(&numbered(&mode), None, None);
    for y in 0..mode.height_px {
        assert_eq!(
            at(scanout.frame(), &mode, 4, y),
            [0, 0, 0, 0],
            "row {y}'s padding was written"
        );
    }
}

#[test]
fn a_damaged_region_copies_only_itself() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let damage = Rect::new(1, 1, 2, 1);

    let present = scanout.compose(&numbered(&mode), None, Some(damage));
    let Present::Region(region) = present else {
        panic!("a sub-screen rectangle is a region present, got {present:?}");
    };
    assert_eq!((region.x, region.y), (1, 1));
    assert_eq!((region.width_px, region.height_px), (2, 1));

    assert_eq!(at(scanout.frame(), &mode, 1, 1), [6, 7, 8, 255]);
    assert_eq!(at(scanout.frame(), &mode, 2, 1), [7, 8, 9, 255]);
    assert_eq!(
        at(scanout.frame(), &mode, 0, 0),
        [0, 0, 0, 0],
        "a pixel outside the damage was copied"
    );
    assert_eq!(at(scanout.frame(), &mode, 3, 1), [0, 0, 0, 0]);
}

#[test]
fn a_damage_covering_the_screen_is_a_whole_present() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let whole = Rect::new(0, 0, 4, 3);
    assert_eq!(
        scanout.compose(&numbered(&mode), None, Some(whole)),
        Present::Whole
    );
}

#[test]
fn an_empty_damage_presents_nothing_and_writes_nothing() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let nothing = Rect::new(2, 2, 0, 0);
    assert_eq!(
        scanout.compose(&numbered(&mode), None, Some(nothing)),
        Present::Nothing
    );
    assert!(scanout.frame().iter().all(|byte| *byte == 0));
}

#[test]
fn a_damage_outside_the_screen_is_clipped_not_a_panic() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");

    let beyond = Rect::new(64, 64, 8, 8);
    assert_eq!(
        scanout.compose(&numbered(&mode), None, Some(beyond)),
        Present::Nothing
    );

    let straddling = Rect::new(3, 2, 8, 8);
    let present = scanout.compose(&numbered(&mode), None, Some(straddling));
    let Present::Region(region) = present else {
        panic!("the overlap is a region present, got {present:?}");
    };
    assert_eq!((region.width_px, region.height_px), (1, 1));
    assert_eq!(at(scanout.frame(), &mode, 3, 2), [12, 13, 14, 255]);
}

#[test]
fn a_negative_origin_is_clipped_to_the_screen() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let present = scanout.compose(&numbered(&mode), None, Some(Rect::new(-4, -4, 6, 6)));
    let Present::Region(region) = present else {
        panic!("the overlap is a region present, got {present:?}");
    };
    assert_eq!((region.x, region.y), (0, 0));
    assert_eq!((region.width_px, region.height_px), (2, 2));
    assert_eq!(at(scanout.frame(), &mode, 0, 0), [1, 2, 3, 255]);
    assert_eq!(at(scanout.frame(), &mode, 2, 0), [0, 0, 0, 0]);
}

#[test]
fn a_surface_smaller_than_the_screen_leaves_the_rest_alone() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let mut small = Surface::new(2, 2).expect("a tiny surface");
    small.set(0, 0, pixel(9, 9, 9));
    small.set(1, 1, pixel(8, 8, 8));

    assert_eq!(scanout.compose(&small, None, None), Present::Whole);
    assert_eq!(at(scanout.frame(), &mode, 0, 0), [9, 9, 9, 255]);
    assert_eq!(at(scanout.frame(), &mode, 1, 1), [8, 8, 8, 255]);
    assert_eq!(at(scanout.frame(), &mode, 3, 2), [0, 0, 0, 0]);
}

#[test]
fn the_cursor_is_blended_over_the_surface_where_it_draws_and_nowhere_else() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let placed = PlacedCursor::new(cursor(2, Color::rgb(200, 10, 20)), Point::new(1, 1));

    assert_eq!(
        scanout.compose(&numbered(&mode), Some(&placed), None),
        Present::Whole
    );
    assert_eq!(at(scanout.frame(), &mode, 1, 1), [200, 10, 20, 255]);
    assert_eq!(at(scanout.frame(), &mode, 1, 2), [200, 10, 20, 255]);
    assert_eq!(
        at(scanout.frame(), &mode, 2, 1),
        [7, 8, 9, 255],
        "the cursor's transparent half left the surface showing"
    );
    assert_eq!(
        at(scanout.frame(), &mode, 0, 0),
        [1, 2, 3, 255],
        "a pixel outside the cursor was blended"
    );
}

#[test]
fn a_composition_that_misses_the_cursor_copies_the_surface_alone() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let placed = PlacedCursor::new(cursor(2, Color::rgb(200, 10, 20)), Point::new(1, 1));

    scanout.compose(&numbered(&mode), Some(&placed), Some(Rect::new(0, 0, 4, 1)));
    assert_eq!(at(scanout.frame(), &mode, 1, 0), [2, 3, 4, 255]);
    assert_eq!(
        at(scanout.frame(), &mode, 1, 1),
        [0, 0, 0, 0],
        "a row outside the damage was written"
    );
}

#[test]
fn a_cursor_overhanging_the_screen_draws_only_the_part_that_fits() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    let placed = PlacedCursor::new(cursor(2, Color::rgb(200, 10, 20)), Point::new(-1, 2));

    assert_eq!(
        scanout.compose(&numbered(&mode), Some(&placed), None),
        Present::Whole
    );
    assert_eq!(
        at(scanout.frame(), &mode, 0, 2),
        [9, 10, 11, 255],
        "the cursor's transparent column landed on the only visible one"
    );
    assert_eq!(at(scanout.frame(), &mode, 0, 0), [1, 2, 3, 255]);
}

#[test]
fn nothing_merged_with_a_present_is_that_present() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let some = region(1, 1, 2, 1);
    assert_eq!(
        Present::Nothing.merged(Present::Nothing, &mode),
        Present::Nothing
    );
    assert_eq!(Present::Nothing.merged(some, &mode), some);
    assert_eq!(some.merged(Present::Nothing, &mode), some);
    assert_eq!(
        Present::Nothing.merged(Present::Whole, &mode),
        Present::Whole
    );
}

#[test]
fn a_whole_present_swallows_whatever_it_meets() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    assert_eq!(
        Present::Whole.merged(region(1, 1, 2, 1), &mode),
        Present::Whole
    );
    assert_eq!(
        region(1, 1, 2, 1).merged(Present::Whole, &mode),
        Present::Whole
    );
    assert_eq!(Present::Whole.merged(Present::Whole, &mode), Present::Whole);
}

#[test]
fn two_regions_merge_into_the_one_rectangle_that_contains_both() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    assert_eq!(
        region(0, 0, 1, 1).merged(region(2, 1, 1, 1), &mode),
        region(0, 0, 3, 2)
    );
    assert_eq!(
        region(2, 1, 1, 1).merged(region(0, 0, 1, 1), &mode),
        region(0, 0, 3, 2),
        "merging is the same rectangle either way round"
    );
}

#[test]
fn regions_that_together_cover_the_screen_merge_into_a_whole_present() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    assert_eq!(
        region(0, 0, 4, 1).merged(region(0, 2, 4, 1), &mode),
        Present::Whole
    );
}

#[test]
fn a_second_composition_keeps_the_pixels_the_first_wrote() {
    let mode = padded_mode(DisplayFormat::Rgba8888);
    let mut scanout = Scanout::new(mode).expect("a valid mode");
    scanout.compose(&numbered(&mode), None, None);

    let mut changed = numbered(&mode);
    changed.set(0, 0, pixel(200, 201, 202));
    scanout.compose(&changed, None, Some(Rect::new(0, 0, 1, 1)));

    assert_eq!(at(scanout.frame(), &mode, 0, 0), [200, 201, 202, 255]);
    assert_eq!(
        at(scanout.frame(), &mode, 3, 2),
        [12, 13, 14, 255],
        "the untouched pixels are still the first frame's"
    );
}
