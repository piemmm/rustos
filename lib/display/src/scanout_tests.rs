//! Tests for the shared scan-out encode: the byte order per format, the
//! per-pixel and run encoders agreeing byte for byte, the frame sizing,
//! and the damage conversion.

use super::{damage_list, scanout_len, sub_screen_damage, ChannelOrder};
use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode, MAX_DAMAGE_RECTS};
use tairix_geometry::{Rect, Region};
use tairix_raster::Pixel;

/// Every order the encoder knows; each run test proves all of them.
const ORDERS: [ChannelOrder; 2] = [ChannelOrder::Rgba, ChannelOrder::Bgra];

/// The longest run the tests encode: odd, and well past any unrolling or
/// vector width a compiler might choose, so a mishandled tail shows up.
const MAX_RUN: usize = 1021;

/// A byte no encode produces here, so an untouched slot is recognisable.
const UNTOUCHED: u8 = 0x5A;

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

/// One step of the run's channel sequence.
fn step(seed: &mut u8) -> u8 {
    *seed = seed.wrapping_mul(37).wrapping_add(11);
    *seed
}

/// A run whose colour channels differ pixel by pixel, so a swapped
/// channel cannot pass by coincidence. Opaque, as a composited screen
/// pixel is.
fn sample_run() -> [Pixel; MAX_RUN] {
    let mut run = [Pixel {
        r: 0,
        g: 0,
        b: 0,
        a: 0xFF,
    }; MAX_RUN];
    let mut seed = 1u8;
    for pixel in &mut run {
        pixel.r = step(&mut seed);
        pixel.g = step(&mut seed);
        pixel.b = step(&mut seed);
    }
    run
}

/// The per-pixel reference the run encoder must match byte for byte.
fn encode_one_by_one(order: ChannelOrder, pixels: &[Pixel], out: &mut [u8]) -> usize {
    let mut written = 0;
    for pixel in pixels {
        let Some(slot) = out.get_mut(written * 4..written * 4 + 4) else {
            break;
        };
        slot.copy_from_slice(&order.encode(*pixel));
        written += 1;
    }
    written
}

#[test]
fn a_run_encodes_byte_for_byte_like_the_per_pixel_loop() {
    let run = sample_run();
    for order in ORDERS {
        for len in [0, 1, 2, 3, 4, 5, 16, 17, MAX_RUN] {
            let pixels = &run[..len];
            let bytes = len * 4;
            let mut bulk = [UNTOUCHED; MAX_RUN * 4];
            let mut one_by_one = [UNTOUCHED; MAX_RUN * 4];
            assert_eq!(order.encode_run(pixels, &mut bulk[..bytes]), len);
            assert_eq!(
                encode_one_by_one(order, pixels, &mut one_by_one[..bytes]),
                len
            );
            assert_eq!(bulk, one_by_one);
        }
    }
}

#[test]
fn a_short_out_truncates_and_never_half_writes_a_pixel() {
    let run = sample_run();
    for order in ORDERS {
        // Not even one whole pixel fits.
        let mut stub = [UNTOUCHED; 3];
        assert_eq!(order.encode_run(&run[..4], &mut stub), 0);
        assert_eq!(stub, [UNTOUCHED; 3]);

        // Room for two whole pixels and three spare bytes: the two are
        // encoded and the partial group is left as it was.
        let mut out = [UNTOUCHED; 11];
        assert_eq!(order.encode_run(&run[..5], &mut out), 2);
        let mut expected = [UNTOUCHED; 11];
        assert_eq!(encode_one_by_one(order, &run[..2], &mut expected), 2);
        assert_eq!(out, expected);
    }
}

#[test]
fn an_over_long_out_keeps_its_tail_untouched() {
    let run = sample_run();
    for order in ORDERS {
        let pixels = &run[..3];
        let mut out = [UNTOUCHED; 3 * 4 + 9];
        assert_eq!(order.encode_run(pixels, &mut out), pixels.len());
        let mut expected = [UNTOUCHED; 3 * 4 + 9];
        assert_eq!(
            encode_one_by_one(order, pixels, &mut expected),
            pixels.len()
        );
        assert_eq!(out, expected);
        assert!(out[12..].iter().all(|&byte| byte == UNTOUCHED));
    }
}

#[test]
fn a_uniform_run_encodes_uniformly_in_every_order() {
    for order in ORDERS {
        for fill in [0x00, 0xFF] {
            let pixels = [Pixel {
                r: fill,
                g: fill,
                b: fill,
                a: fill,
            }; 7];
            let mut out = [UNTOUCHED; 7 * 4];
            assert_eq!(order.encode_run(&pixels, &mut out), pixels.len());
            assert!(out.iter().all(|&byte| byte == fill));
        }
    }
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

/// A scratch list, filled as a caller's is: every slot holds a real
/// rectangle, so an unfilled one can never be mistaken for damage.
fn scratch(mode: &DisplayMode) -> [DamageRect; MAX_DAMAGE_RECTS] {
    [DamageRect::full(mode); MAX_DAMAGE_RECTS]
}

/// A scattered frame's rectangles reach the list as themselves — the whole
/// point of a list present, since their bounding box here **is** the screen
/// while the damage is thirty-two pixels. Answering on the box was what made
/// two far-apart corners cost a whole-screen present.
#[test]
fn corners_that_span_the_screen_are_named_not_collapsed() {
    let mode = mode(64, 48, DisplayFormat::Rgba8888);
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 4, 4));
    region.add(Rect::new(60, 44, 4, 4));
    assert_eq!(
        sub_screen_damage(&region.bounds(), &mode),
        None,
        "the box around them really does cover the screen"
    );

    let mut out = scratch(&mode);
    assert_eq!(
        damage_list(&region, &mode, &mut out),
        Some(
            [
                DamageRect {
                    x: 0,
                    y: 0,
                    width_px: 4,
                    height_px: 4
                },
                DamageRect {
                    x: 60,
                    y: 44,
                    width_px: 4,
                    height_px: 4
                },
            ]
            .as_slice()
        )
    );
}

/// Damage that really is the surface presents the whole frame instead.
#[test]
fn whole_surface_damage_asks_for_the_whole_frame() {
    let mode = mode(64, 48, DisplayFormat::Rgba8888);
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 64, 48));
    let mut out = scratch(&mode);
    assert_eq!(damage_list(&region, &mode, &mut out), None);
}

/// Past the list's bound the answer is the bounding box alone: over-covering
/// costs pixels, dropping a rectangle would leave stale ones on screen.
#[test]
fn a_region_past_the_bound_degrades_to_its_bounding_box() {
    let mode = mode(64, 48, DisplayFormat::Rgba8888);
    let scattered = |count: usize| {
        let mut region = Region::new();
        for step in 0..count {
            let x = i32::try_from(step * 6).expect("on screen");
            region.add(Rect::new(x, 0, 4, 4));
        }
        region
    };

    let region = scattered(MAX_DAMAGE_RECTS + 1);
    assert!(region.rects().len() > MAX_DAMAGE_RECTS);
    let bounds = sub_screen_damage(&region.bounds(), &mode).expect("a sub-region");
    let mut out = scratch(&mode);
    assert_eq!(
        damage_list(&region, &mode, &mut out),
        Some([bounds].as_slice())
    );

    // Exactly the bound is still named rectangle by rectangle: the
    // degradation begins one past it, not at it.
    let region = scattered(MAX_DAMAGE_RECTS);
    let mut out = scratch(&mode);
    assert_eq!(
        damage_list(&region, &mode, &mut out).map(<[DamageRect]>::len),
        Some(MAX_DAMAGE_RECTS)
    );
}
