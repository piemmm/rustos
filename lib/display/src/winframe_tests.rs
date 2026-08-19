//! Unit tests for the window-frame pixel codec.
//!
//! Two properties carry the module: a frame round-trips an app's pixels
//! including their alpha, and splitting the conversion across bands changes the
//! wall-clock cost and nothing else. Both are asserted against a runner that
//! reports a width it does not have and runs its pieces backwards, so the order
//! dependence a real pool could expose is exercised with no thread involved.

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_abi::Errno;
use tairix_geometry::Rect;
use tairix_parallel::{JobRunner, Reversed, SERIAL};
use tairix_raster::{Color, Pixel, Surface};

use super::{decode, encode};

const WIDTH: u32 = 40;
const HEIGHT: u32 = 24;

/// A runner wide enough that a `WIDTH`×`HEIGHT` rectangle really is split.
fn splitting() -> Reversed {
    Reversed::new(HEIGHT as usize)
}

fn mode(format: DisplayFormat, stride_bytes: u32) -> DisplayMode {
    DisplayMode {
        width_px: WIDTH,
        height_px: HEIGHT,
        stride_bytes,
        format,
    }
}

fn packed(format: DisplayFormat) -> DisplayMode {
    mode(format, WIDTH * 4)
}

fn frame_for(mode: &DisplayMode) -> Vec<u8> {
    vec![0u8; (mode.stride_bytes as usize) * (mode.height_px as usize)]
}

/// A deterministic picture with a different colour and alpha per pixel, so a
/// channel swap, an alpha loss, or a transposed row all show up.
fn painted() -> Surface {
    let mut surface =
        Surface::filled(WIDTH, HEIGHT, Color::rgba(0, 0, 0, 255).premultiply()).expect("surface");
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            surface.set(x, y, sample(x, y));
        }
    }
    surface
}

/// The pixel `painted` puts at `(x, y)`.
///
/// Alpha is kept at or above the largest channel so un-premultiplying and
/// premultiplying again is exact: a straight-alpha frame cannot represent a
/// channel brighter than its own alpha, and a test that asked it to would be
/// asserting against the format rather than against this code.
fn sample(x: u32, y: u32) -> Pixel {
    let a = 128 + u8::try_from((x * 3 + y) % 128).unwrap_or(0);
    let scale = |v: u32| u8::try_from(u32::from(a) * (v % 64) / 64).unwrap_or(0);
    Color::rgba(scale(x), scale(y * 2), scale(x + y), a).premultiply()
}

fn blank() -> Surface {
    Surface::filled(WIDTH, HEIGHT, Color::rgba(0, 0, 0, 0).premultiply()).expect("surface")
}

fn whole() -> DamageRect {
    DamageRect {
        x: 0,
        y: 0,
        width_px: WIDTH,
        height_px: HEIGHT,
    }
}

/// Encode `source` and decode it straight back, answering the recovered surface
/// and the rectangle the decode reported as changed.
fn round_trip(source: &Surface, mode: &DisplayMode, runner: &dyn JobRunner) -> (Surface, Rect) {
    let mut frame = frame_for(mode);
    encode(source, &mut frame, mode, whole(), runner).expect("encode");
    let mut target = blank();
    let changed = decode(&frame, &mut target, mode, whole(), runner).expect("decode");
    (target, changed)
}

#[test]
fn a_frame_round_trips_every_pixel_in_both_channel_orders() {
    let source = painted();
    for format in [DisplayFormat::Rgba8888, DisplayFormat::Bgra8888] {
        let mode = packed(format);
        let (recovered, changed) = round_trip(&source, &mode, &SERIAL);
        assert_eq!(
            recovered.pixels(),
            source.pixels(),
            "{format:?} lost or reordered a channel"
        );
        assert_eq!(changed, Rect::new(0, 0, WIDTH, HEIGHT));
    }
}

#[test]
fn a_stride_wider_than_the_row_leaves_its_padding_alone() {
    let mode = mode(DisplayFormat::Rgba8888, WIDTH * 4 + 16);
    let source = painted();
    let mut frame = frame_for(&mode);
    encode(&source, &mut frame, &mode, whole(), &SERIAL).expect("encode");
    for y in 0..HEIGHT as usize {
        let row = y * mode.stride_bytes as usize;
        let padding = &frame[row + (WIDTH as usize) * 4..row + mode.stride_bytes as usize];
        assert!(padding.iter().all(|&byte| byte == 0), "padding was written");
    }
    let mut target = blank();
    decode(&frame, &mut target, &mode, whole(), &SERIAL).expect("decode");
    assert_eq!(target.pixels(), source.pixels());
}

#[test]
fn splitting_the_conversion_changes_nothing_it_produces() {
    let source = painted();
    let mode = packed(DisplayFormat::Bgra8888);

    let mut serial_frame = frame_for(&mode);
    encode(&source, &mut serial_frame, &mode, whole(), &SERIAL).expect("encode");
    let mut split_frame = frame_for(&mode);
    encode(&source, &mut split_frame, &mode, whole(), &splitting()).expect("encode");
    assert_eq!(serial_frame, split_frame, "a band wrote different bytes");

    // A decode over an already-populated surface, so the change detection —
    // which is what the bands have to agree on — is exercised rather than a
    // first fill.
    let mut serial_target = painted();
    serial_target.set(3, 5, Color::rgba(1, 2, 3, 255).premultiply());
    let mut split_target = serial_target.clone();
    let serial_changed =
        decode(&serial_frame, &mut serial_target, &mode, whole(), &SERIAL).expect("decode");
    let split_changed = decode(
        &split_frame,
        &mut split_target,
        &mode,
        whole(),
        &splitting(),
    )
    .expect("decode");
    assert_eq!(serial_changed, split_changed, "the bands' union disagreed");
    assert_eq!(serial_target.pixels(), split_target.pixels());
}

#[test]
fn an_unchanged_frame_reports_no_damage() {
    let source = painted();
    let mode = packed(DisplayFormat::Rgba8888);
    let mut frame = frame_for(&mode);
    encode(&source, &mut frame, &mode, whole(), &SERIAL).expect("encode");

    let mut target = painted();
    for runner in [&SERIAL as &dyn JobRunner, &splitting()] {
        let changed = decode(&frame, &mut target, &mode, whole(), runner).expect("decode");
        assert_eq!(changed, Rect::EMPTY, "an identical present reported damage");
    }
}

#[test]
fn the_reported_damage_is_the_box_of_the_pixels_that_really_changed() {
    let mode = packed(DisplayFormat::Rgba8888);
    let source = painted();
    let mut frame = frame_for(&mode);
    encode(&source, &mut frame, &mode, whole(), &SERIAL).expect("encode");

    // Two far-apart differences: their bounding box is the answer, and it is
    // deliberately much smaller than the whole-window damage declared.
    let mut target = painted();
    target.set(4, 6, Color::rgba(255, 255, 255, 255).premultiply());
    target.set(9, 11, Color::rgba(255, 255, 255, 255).premultiply());
    for runner in [&SERIAL as &dyn JobRunner, &splitting()] {
        let mut probe = target.clone();
        let changed = decode(&frame, &mut probe, &mode, whole(), runner).expect("decode");
        assert_eq!(changed, Rect::new(4, 6, 6, 6));
        assert_eq!(probe.pixels(), source.pixels(), "the frame did not land");
    }
}

#[test]
fn a_sub_rectangle_touches_nothing_outside_itself() {
    let mode = packed(DisplayFormat::Rgba8888);
    let source = painted();
    let damage = DamageRect {
        x: 8,
        y: 4,
        width_px: 6,
        height_px: 3,
    };

    let mut frame = frame_for(&mode);
    encode(&source, &mut frame, &mode, damage, &splitting()).expect("encode");
    let mut target = blank();
    let changed = decode(&frame, &mut target, &mode, damage, &splitting()).expect("decode");
    assert_eq!(changed, Rect::new(8, 4, 6, 3));
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let inside = (8..14).contains(&x) && (4..7).contains(&y);
            let want = if inside {
                sample(x, y)
            } else {
                Color::rgba(0, 0, 0, 0).premultiply()
            };
            assert_eq!(target.get(x, y), Some(want), "at ({x}, {y})");
        }
    }
}

/// Every refusal shape, asserted to leave the target exactly as it was: a
/// hostile geometry must never half-convert a window.
#[test]
fn a_refused_geometry_writes_nothing() {
    let source = painted();
    let good = packed(DisplayFormat::Rgba8888);
    let cases: [(DisplayMode, DamageRect, usize); 4] = [
        // A rectangle reaching past the surface.
        (
            good,
            DamageRect {
                x: 0,
                y: 0,
                width_px: WIDTH + 1,
                height_px: HEIGHT,
            },
            frame_for(&good).len(),
        ),
        // A row span wider than the stride claims.
        (
            mode(DisplayFormat::Rgba8888, WIDTH * 4 - 4),
            whole(),
            frame_for(&good).len(),
        ),
        // A frame too short for the rows named.
        (good, whole(), frame_for(&good).len() - 1),
        // An overflowing origin.
        (
            good,
            DamageRect {
                x: u32::MAX,
                y: 0,
                width_px: 2,
                height_px: 1,
            },
            frame_for(&good).len(),
        ),
    ];
    for (mode, damage, frame_len) in cases {
        let mut frame = vec![0u8; frame_len];
        assert_eq!(
            encode(&source, &mut frame, &mode, damage, &splitting()),
            Err(Errno::OutOfRange)
        );
        assert!(frame.iter().all(|&byte| byte == 0), "a refusal wrote bytes");

        let mut target = blank();
        assert_eq!(
            decode(&frame, &mut target, &mode, damage, &splitting()),
            Err(Errno::OutOfRange)
        );
        assert_eq!(target.pixels(), blank().pixels(), "a refusal wrote pixels");
    }
}
