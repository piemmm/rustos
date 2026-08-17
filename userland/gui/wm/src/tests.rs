//! Headless unit tests for the compositor core.

extern crate alloc;

use tairix_abi::driver::display::{
    AccelCaps, AccelLayer, AcceleratedDisplay, DamageRect, Display, DisplayFormat, DisplayMode,
};
use tairix_abi::DriverError;

use crate::color::{div255, Color, Pixel};
use crate::compositor::MAX_PRESENT_REGIONS;
use crate::corner::Corners;
use crate::geometry::{Point, Rect, Region};
use crate::surface::Surface;
use crate::{Compositor, WindowId};

use tairix_cursor::CursorImage;
use tairix_log::{Event, Sink};
use tairix_reclaim::{CachedBytes, PressureBand, ReclaimCache, ReportedPressure};
use tairix_theme::{Contrast, Theme, ThemeId, ThemeRegistry};

use crate::chrome::{chrome_cache, ChromeEpoch, WindowChrome};
use crate::frost::{frost_cache, FrostEpoch, FrostedBackdrop};
use crate::select::{cursor_cache, CursorEpoch};

use crate::{
    IconKind, WindowActivationState, WindowControlKind, WindowFrame, WindowFurnitureState,
    WindowSizeState,
};

pub(crate) fn mode(w: u32, h: u32) -> DisplayMode {
    DisplayMode {
        width_px: w,
        height_px: h,
        stride_bytes: w * 4,
        format: DisplayFormat::Rgba8888,
    }
}

pub(crate) fn opaque(w: u32, h: u32, color: Color) -> Surface {
    Surface::filled(w, h, color.premultiply()).expect("surface allocates")
}

/// A compositor for `mode` over `background`, holding a window-furniture and a
/// frosted-backdrop cache at normal pressure sized from a 1080p output — the
/// one place these tests assemble the caches the embedder would otherwise
/// inject, so a test that cares about a budget or the band builds its own
/// instead.
pub(crate) fn new_compositor(mode: DisplayMode, background: Color) -> Option<Compositor> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    Compositor::new(
        mode,
        background,
        test_chrome_cache(),
        test_frost_cache(),
        &NORMAL_PRESSURE,
    )
}

/// Convert a client present at the window's *current* client size, as the
/// session's present bridge does, and return the conversion's own value.
///
/// A test that presents at a size the window is not laid out for asks for
/// that size explicitly instead, so a stale-geometry present is always
/// visible at the call site rather than hidden in a helper.
fn present_content<T>(
    comp: &mut Compositor,
    id: WindowId,
    convert: impl FnOnce(&mut Surface) -> (T, Rect),
) -> Option<T> {
    let (w, h) = comp.window(id)?.client_size();
    comp.present_window_content(id, w, h, convert)
}

/// Read the RGBA scan-out bytes of frame pixel `(x, y)`.
fn frame_pixel(comp: &Compositor, x: u32, y: u32) -> [u8; 4] {
    let info = comp.mode();
    let off = (y * info.stride_bytes + x * 4) as usize;
    let frame = comp.frame();
    [frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]
}

/// A display seam that records the last presented frame and, separately,
/// how many times each of [`Display::present`] (a whole-frame present) and
/// [`Display::present_region`] (naming the exact rectangle presented) were
/// called, or always fails when `fail` is set.
struct MockDisplay {
    mode: DisplayMode,
    last: alloc::vec::Vec<u8>,
    fail: bool,
    full_presents: usize,
    regions: alloc::vec::Vec<DamageRect>,
}

impl MockDisplay {
    fn new(mode: DisplayMode) -> Self {
        Self {
            mode,
            last: alloc::vec::Vec::new(),
            fail: false,
            full_presents: 0,
            regions: alloc::vec::Vec::new(),
        }
    }

    /// Record `frame` as the latest presented bytes, or fail when `fail`
    /// is set. Shared by both [`Display`] methods so only the call-count
    /// bookkeeping differs between a whole-frame and a region present.
    fn record(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        if self.fail {
            return Err(DriverError::DeviceFault);
        }
        self.last = frame.to_vec();
        Ok(())
    }
}

impl Display for MockDisplay {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }

    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        self.record(frame)?;
        self.full_presents += 1;
        Ok(())
    }

    fn present_region(&mut self, frame: &[u8], damage: DamageRect) -> Result<(), DriverError> {
        self.record(frame)?;
        self.regions.push(damage);
        Ok(())
    }
}

const BLUE: Color = Color::rgb(0, 0, 255);
const RED: Color = Color::rgb(255, 0, 0);
const GREEN: Color = Color::rgb(0, 255, 0);

// The colour algebra (`Color`/`Pixel`, premultiply, `over`, `scale_alpha`)
// and the `Surface` pixel buffer are unit-tested in their own crate
// (`lib/raster`); likewise the `Point`/`Rect` primitives in `lib/geometry`.
// The compositor tests below exercise how the window manager *uses* them
// (rounded corners, damage, z-order, blending, hit-testing).

// ---- rounded corners -------------------------------------------------

#[test]
fn square_corners_fully_cover() {
    assert_eq!(Corners::Square.coverage(0, 0, 10, 10), 255);
    assert_eq!(Corners::Square.coverage(9, 9, 10, 10), 255);
}

#[test]
fn zero_radius_fully_covers() {
    assert_eq!(Corners::Rounded { radius: 0 }.coverage(0, 0, 10, 10), 255);
}

#[test]
fn rounded_corner_pixel_is_clipped() {
    // The extreme corner of a generous radius is entirely outside.
    assert_eq!(Corners::Rounded { radius: 8 }.coverage(0, 0, 20, 20), 0);
}

#[test]
fn only_the_corner_bands_clip_a_row() {
    // What a per-row decision may skip: a row no arc reaches is fully covered
    // at every column, so the compositor pays for coverage at the corners only.
    let rounded = Corners::Rounded { radius: 8 };
    assert!(rounded.clips_row(0, 20, 20));
    assert!(rounded.clips_row(7, 20, 20));
    assert!(!rounded.clips_row(8, 20, 20));
    assert!(!rounded.clips_row(11, 20, 20));
    assert!(rounded.clips_row(12, 20, 20));
    assert!(rounded.clips_row(19, 20, 20));
    // A square window, and a radius clamped away by a short side, clip nothing.
    assert!(!Corners::Square.clips_row(0, 20, 20));
    assert!(!Corners::Rounded { radius: 8 }.clips_row(0, 20, 1));
}

#[test]
fn rounded_centre_is_opaque() {
    assert_eq!(Corners::Rounded { radius: 8 }.coverage(10, 10, 20, 20), 255);
}

#[test]
fn rounded_edge_midpoints_are_opaque() {
    // The middle of each edge lies on the straight section, not an arc.
    let c = Corners::Rounded { radius: 6 };
    assert_eq!(c.coverage(10, 0, 20, 20), 255);
    assert_eq!(c.coverage(0, 10, 20, 20), 255);
}

#[test]
fn rounded_arc_has_partial_coverage() {
    // A pixel straddling the arc is neither fully in nor fully out.
    let c = Corners::Rounded { radius: 8 };
    let cov = c.coverage(2, 2, 20, 20);
    assert!(cov > 0 && cov < 255, "expected partial coverage, got {cov}");
}

#[test]
fn radius_is_clamped_to_half_side() {
    // A radius larger than half the height is clamped, so the surface
    // becomes a capsule whose centre row is fully covered.
    let huge = Corners::Rounded { radius: 1000 };
    assert_eq!(huge.coverage(10, 5, 20, 10), 255);
}

// ---- damage ----------------------------------------------------------
//
// The region type itself is `tairix_geometry::Region` and is tested there;
// what belongs here is how the compositor uses it.

#[test]
fn duplicate_damage_composites_a_window_once_and_correctly() {
    // Marking a window's rectangle dirty repeatedly must not change the
    // composited result (and, with coalescing, composites it once): the
    // frame is identical to a single clean composite.
    let mut once = new_compositor(mode(4, 4), BLUE).expect("compositor");
    once.add_window(Point::new(1, 1), opaque(2, 2, RED));
    once.composite();

    let mut many = new_compositor(mode(4, 4), BLUE).expect("compositor");
    let id = many.add_window(Point::new(1, 1), opaque(2, 2, RED));
    // Extra identical damage on top of the add's own damage: each
    // surface replacement re-dirties the same window rectangle.
    for _ in 0..5 {
        assert!(many.set_surface(id, opaque(2, 2, RED)));
    }
    many.composite();

    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(
                frame_pixel(&once, x, y),
                frame_pixel(&many, x, y),
                "pixel ({x},{y}) differs after duplicated damage"
            );
        }
    }
}

// ---- compositor ------------------------------------------------------

#[test]
fn new_rejects_zero_size() {
    assert!(new_compositor(mode(0, 4), BLUE).is_none());
    assert!(new_compositor(mode(4, 0), BLUE).is_none());
}

#[test]
fn new_rejects_short_stride() {
    let bad = DisplayMode {
        stride_bytes: 4,
        ..mode(4, 4)
    };
    assert!(new_compositor(bad, BLUE).is_none());
}

#[test]
fn background_fills_screen() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]);
    assert_eq!(frame_pixel(&c, 1, 1), [0, 0, 255, 255]);
}

#[test]
fn set_background_repaints_the_whole_screen() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    c.composite();

    assert!(c.set_background(RED));
    assert_eq!(c.background(), RED);
    assert!(c.has_damage(), "a changed background dirties the screen");
    assert_eq!(c.composite().bounds(), Rect::new(0, 0, 2, 2));
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 1, 1), [255, 0, 0, 255]);
}

#[test]
fn set_background_same_colour_is_a_no_op() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    c.composite();

    assert!(!c.set_background(BLUE));
    assert!(!c.has_damage(), "an unchanged background dirties nothing");
}

#[test]
fn set_background_forces_opaque() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    c.composite();

    // A translucent spelling of the current colour is the same opaque
    // background, so nothing changes; a translucent new colour lands opaque.
    assert!(!c.set_background(Color { a: 9, ..BLUE }));
    assert!(c.set_background(Color { a: 0, ..RED }));
    assert_eq!(c.background(), RED);
}

#[test]
fn set_background_keeps_windows_on_top() {
    let mut c = new_compositor(mode(4, 4), BLUE).expect("compositor");
    c.add_window(Point::new(0, 0), opaque(2, 2, RED));
    c.composite();

    assert!(c.set_background(Color::rgb(0, 255, 0)));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 3, 3), [0, 255, 0, 255]);
}

#[test]
fn bgra_channel_order_is_honoured() {
    let m = DisplayMode {
        format: DisplayFormat::Bgra8888,
        ..mode(2, 2)
    };
    let mut c = new_compositor(m, BLUE).expect("compositor");
    c.composite();
    // Blue in BGRA is byte order B,G,R,A.
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]);
}

#[test]
fn opaque_window_overwrites_background() {
    let mut c = new_compositor(mode(4, 4), BLUE).expect("compositor");
    c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]); // background
    assert_eq!(frame_pixel(&c, 1, 1), [255, 0, 0, 255]); // window
    assert_eq!(frame_pixel(&c, 2, 2), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 3, 3), [0, 0, 255, 255]); // background again
}

/// A picture that ramps *slowly* down the screen — one level per row, the
/// tonal shape of a wallpaper's sky and the one a translucent field over it
/// can flatten into bands. A steep ramp cannot band: it outruns the levels
/// the blend takes away.
fn ramp_surface(side: u32) -> Surface {
    let mut surface = Surface::new(side, side).expect("surface allocates");
    for y in 0..side {
        let level = u8::try_from(y).unwrap_or(u8::MAX);
        surface.fill_rect(0, y, side, 1, Color::rgb(level, level, level));
    }
    surface
}

/// The longest run of consecutive frame rows carrying the same tone across
/// `columns`, and how many distinct tones those rows hold.
///
/// A row's tone is the sum of its green scan-out bytes, so it resolves the row
/// to a fraction of a level rather than to the one level a single pixel can
/// hold. The scene below ramps in one direction and a blend is monotone in
/// what it covers, so a tone that changes is a tone not seen before.
fn frame_bands(comp: &Compositor, rows: core::ops::Range<u32>, columns: u32) -> (u32, u32) {
    let tone = |y: u32| -> u32 {
        (0..columns)
            .map(|x| u32::from(frame_pixel(comp, x, y)[1]))
            .sum()
    };
    let mut previous = tone(rows.start);
    let (mut longest, mut run, mut tones) = (1, 1, 1);
    for y in rows.skip(1) {
        let here = tone(y);
        if here == previous {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 1;
            tones += 1;
            previous = here;
        }
    }
    (longest, tones)
}

/// A translucent window over a smoothly shaded desktop must not band it.
///
/// The blend has only `256 - alpha` output levels for the 256 the picture
/// beneath it held, so rounding every pixel alike answers a whole ramp with a
/// handful of tones: at alpha 224 this scene came out as eight plateaus eight
/// rows deep. The composite spreads that rounding over the area instead, and
/// the ramp survives.
#[test]
fn a_translucent_window_does_not_band_the_desktop_beneath_it() {
    const SIDE: u32 = 64;
    let mut c = new_compositor(mode(SIDE, SIDE), BLUE).expect("compositor");
    c.set_desktop(ramp_surface(SIDE));
    let veil = Surface::filled(SIDE, SIDE, Color::rgba(11, 14, 16, 224).premultiply())
        .expect("surface allocates");
    c.add_window(Point::ORIGIN, veil);

    c.composite();

    let (band, tones) = frame_bands(&c, 0..SIDE, SIDE);
    assert!(band <= 3, "the window flattened {band} rows into one tone");
    assert!(
        tones >= 32,
        "only {tones} of {SIDE} tones survived the window"
    );
}

/// The same guarantee for the desktop layer's own blend: a translucent
/// wallpaper over the root fill is the same shape of composite.
#[test]
fn a_translucent_desktop_layer_does_not_band_over_the_background() {
    const SIDE: u32 = 64;
    let mut c = new_compositor(mode(SIDE, SIDE), BLUE).expect("compositor");
    let mut ramp = ramp_surface(SIDE);
    ramp.fill_round_rect(0, 0, SIDE, SIDE, 0, Color::rgba(11, 14, 16, 224));
    c.set_desktop(ramp);

    c.composite();

    let (band, tones) = frame_bands(&c, 0..SIDE, SIDE);
    assert!(band <= 3, "the desktop layer flattened {band} rows");
    assert!(tones >= 32, "only {tones} of {SIDE} tones survived");
}

#[test]
fn top_window_wins_z_order() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(2, 2, RED));
    c.add_window(Point::ORIGIN, opaque(2, 2, Color::rgb(0, 255, 0)));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 255, 0, 255]); // green on top
}

#[test]
fn raise_changes_z_order() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    let bottom = c.add_window(Point::ORIGIN, opaque(2, 2, RED));
    c.add_window(Point::ORIGIN, opaque(2, 2, Color::rgb(0, 255, 0)));
    assert!(c.raise(bottom));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]); // red raised to top
}

#[test]
fn raising_the_topmost_window_repaints_nothing() {
    // The session re-asserts a popup's stacking before every composite, so a
    // raise of a window already at the front is the common case, not a rare
    // one: it must cost nothing at all.
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    let under = c.add_window(Point::ORIGIN, opaque(8, 8, RED));
    let top = c.add_window(Point::ORIGIN, opaque(4, 4, GREEN));
    composite_checked(&mut c);

    assert!(c.raise(top), "a topmost window is raised");
    assert!(!c.has_damage());
    assert!(composite_checked(&mut c).is_empty());
    assert!(c.frame_stats().is_idle());
    // The stack is exactly as it was: a raise from the front passes nobody.
    assert_eq!(c.window_at(Point::ORIGIN), Some(top));
    assert_eq!(c.window_at(Point::new(6, 6)), Some(under));
}

#[test]
fn raising_a_covered_window_still_restacks_and_repaints_it() {
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    let under = c.add_window(Point::ORIGIN, opaque(6, 6, RED));
    c.add_window(Point::ORIGIN, opaque(4, 4, GREEN));
    composite_checked(&mut c);

    assert!(c.raise(under));
    assert_eq!(c.window_at(Point::ORIGIN), Some(under));
    assert_eq!(composite_checked(&mut c).rects(), &[Rect::new(0, 0, 6, 6)]);
}

#[test]
fn semi_transparent_window_blends_with_background() {
    let mut c = new_compositor(mode(1, 1), BLUE).expect("compositor");
    let surface =
        Surface::filled(1, 1, Color::rgba(255, 0, 0, 128).premultiply()).expect("allocates");
    c.add_window(Point::ORIGIN, surface);
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [128, 0, 127, 255]);
}

#[test]
fn per_region_alpha_blends_each_pixel() {
    // A 1x2 surface: opaque red on top, half-alpha red below.
    let mut surface = Surface::new(1, 2).expect("allocates");
    surface.set(0, 0, RED.premultiply());
    surface.set(0, 1, Color::rgba(255, 0, 0, 128).premultiply());
    let mut c = new_compositor(mode(1, 2), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, surface);
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]); // opaque row
    assert_eq!(frame_pixel(&c, 0, 1), [128, 0, 127, 255]); // blended row
}

#[test]
fn set_opacity_makes_window_translucent() {
    let mut c = new_compositor(mode(1, 1), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.set_opacity(id, 128));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [128, 0, 127, 255]);
}

#[test]
fn rounded_window_shows_background_at_corner() {
    let mut c = new_compositor(mode(20, 20), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(20, 20, RED));
    assert!(c.set_corners(id, Corners::Rounded { radius: 8 }));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]); // corner clipped to bg
    assert_eq!(frame_pixel(&c, 10, 10), [255, 0, 0, 255]); // centre opaque
}

#[test]
fn hidden_window_is_not_composited() {
    let mut c = new_compositor(mode(1, 1), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.set_visible(id, false));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]);
}

#[test]
fn removed_window_disappears() {
    let mut c = new_compositor(mode(1, 1), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.remove(id));
    assert_eq!(c.window_count(), 0);
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]);
}

#[test]
fn move_window_repaints_old_and_new() {
    let mut c = new_compositor(mode(4, 1), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]);
    assert!(c.move_window(id, Point::new(3, 0)));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]); // vacated: background
    assert_eq!(frame_pixel(&c, 3, 0), [255, 0, 0, 255]); // new location
}

#[test]
fn composite_clears_damage_and_is_idempotent() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.has_damage());
    c.composite();
    assert!(!c.has_damage());
    let before = c.frame().to_vec();
    c.composite(); // no damage: a no-op
    assert_eq!(c.frame(), before.as_slice());
}

#[test]
fn unknown_window_operations_return_false() {
    let mut c = new_compositor(mode(2, 2), BLUE).expect("compositor");
    let ghost = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    c.remove(ghost);
    assert!(!c.move_window(ghost, Point::new(1, 1)));
    assert!(!c.set_opacity(ghost, 0));
    assert!(!c.raise(ghost));
}

#[test]
fn back_buffer_holds_premultiplied_pixels() {
    let mut c = new_compositor(mode(1, 1), BLUE).expect("compositor");
    c.composite();
    assert_eq!(c.back_buffer().get(0, 0), Some(BLUE.premultiply()));
}

// ---- pending work: has_damage agrees with composite ------------------

/// Composite `c`, first asserting [`Compositor::has_damage`] answered
/// exactly what that composite produces, and return the region.
///
/// A caller skips a frame entirely when `has_damage` is `false`, so a
/// disagreement either drops a repaint the user is waiting for or burns a
/// wake compositing nothing.
fn composite_checked(c: &mut Compositor) -> Region {
    let claimed = c.has_damage();
    let region = c.composite();
    assert_eq!(
        claimed,
        !region.is_empty(),
        "has_damage promised {claimed}, composite produced {region:?}"
    );
    region
}

#[test]
fn has_damage_answers_exactly_what_the_next_composite_produces() {
    let mut c = new_compositor(mode(24, 24), BLUE).expect("compositor");
    // The very first frame: a new compositor marks the whole screen.
    assert!(!composite_checked(&mut c).is_empty());
    assert!(composite_checked(&mut c).is_empty());

    let id = c.add_window(Point::new(2, 2), opaque(4, 4, RED));
    assert!(!composite_checked(&mut c).is_empty());
    assert!(c.move_window(id, Point::new(2, 2)));
    assert!(composite_checked(&mut c).is_empty());
    assert!(c.move_window(id, Point::new(6, 6)));
    assert!(!composite_checked(&mut c).is_empty());

    c.set_cursor(solid_cursor(4, RED), Point::new(10, 10));
    assert!(!composite_checked(&mut c).is_empty());
    assert!(c.move_cursor(Point::new(14, 14)));
    assert!(!composite_checked(&mut c).is_empty());
    assert!(c.hide_cursor());
    assert!(!composite_checked(&mut c).is_empty());
    assert!(composite_checked(&mut c).is_empty());
}

#[test]
fn a_cursor_move_landing_on_the_same_rectangle_is_no_work() {
    let mut c = new_compositor(mode(24, 24), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(4, RED), Point::new(5, 5));
    c.composite();

    assert!(c.move_cursor(Point::new(5, 5)));
    assert!(!c.has_damage(), "the pointer did not actually move");
    assert!(composite_checked(&mut c).is_empty());
}

#[test]
fn replacing_the_cursor_artwork_repaints_its_unchanged_rectangle() {
    // A hover shape change (arrow -> text) installs a same-size image at the
    // same pointer: the rectangle is identical, the pixels are not.
    let mut c = new_compositor(mode(24, 24), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(4, RED), Point::new(5, 5));
    c.composite();
    assert_eq!(frame_pixel(&c, 6, 6), [255, 0, 0, 255]);

    c.set_cursor(solid_cursor(4, Color::rgb(0, 255, 0)), Point::new(5, 5));
    assert!(c.has_damage());
    assert_eq!(composite_checked(&mut c).rects(), &[Rect::new(5, 5, 4, 4)]);
    assert_eq!(frame_pixel(&c, 6, 6), [0, 255, 0, 255]);
}

#[test]
fn damage_marked_entirely_off_screen_is_no_pending_work() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    let offscreen = c.add_window(Point::new(100, 100), opaque(4, 4, RED));
    c.composite();

    // Nothing this window does reaches a pixel, so waking to composite it
    // would be a frame spent on nothing.
    assert!(c.move_window(offscreen, Point::new(120, 120)));
    assert!(!c.has_damage());
    assert!(composite_checked(&mut c).is_empty());
}

// ---- no-op updates and reported content damage -----------------------

#[test]
fn no_op_window_updates_mark_no_damage_and_still_report_success() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    let id = c.add_window(Point::new(2, 2), opaque(4, 4, RED));
    assert!(c.set_corners(id, Corners::Rounded { radius: 2 }));
    assert!(c.set_opacity(id, 200));
    c.composite();

    // The taskbar presenter re-issues exactly these on every frame, so a
    // repaint here is a whole window recomposited for nothing.
    assert!(c.move_window(id, Point::new(2, 2)));
    assert!(c.set_corners(id, Corners::Rounded { radius: 2 }));
    assert!(c.set_visible(id, true));
    assert!(c.set_opacity(id, 200));
    assert!(!c.has_damage(), "an unchanged window repaints nothing");
    assert!(composite_checked(&mut c).is_empty());
}

#[test]
fn a_genuine_move_still_damages_the_vacated_and_the_new_rectangle() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(4, 4, RED));
    c.composite();

    assert!(c.move_window(id, Point::new(8, 8)));
    let region = composite_checked(&mut c);
    assert_eq!(region.rects().len(), 2);
    assert!(region.rects().contains(&Rect::new(0, 0, 4, 4)));
    assert!(region.rects().contains(&Rect::new(8, 8, 4, 4)));
}

#[test]
fn replacing_a_surface_always_marks_damage() {
    // Comparing two whole buffers costs more than recompositing the window,
    // so a replacement is assumed to differ.
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    let id = c.add_window(Point::new(1, 1), opaque(4, 4, RED));
    c.composite();

    assert!(c.set_surface(id, opaque(4, 4, RED)));
    assert_eq!(composite_checked(&mut c).rects(), &[Rect::new(1, 1, 4, 4)]);
}

#[test]
fn a_content_edit_marks_only_the_rectangle_it_reports() {
    let mut c = new_compositor(mode(32, 32), BLUE).expect("compositor");
    let id = c.add_window(Point::new(4, 4), opaque(16, 16, RED));
    c.composite();

    let green = Color::rgb(0, 255, 0);
    let edited = present_content(&mut c, id, |surface| {
        surface.set(2, 3, green.premultiply());
        (green, Rect::new(2, 3, 1, 1))
    });
    assert_eq!(edited, Some(green));

    // Content-local (2, 3) is screen (6, 7) for a window at (4, 4).
    assert_eq!(composite_checked(&mut c).rects(), &[Rect::new(6, 7, 1, 1)]);
    assert_eq!(frame_pixel(&c, 6, 7), [0, 255, 0, 255]);
    assert_eq!(frame_pixel(&c, 7, 7), [255, 0, 0, 255]);
}

#[test]
fn content_damage_is_offset_by_a_decorated_window_frame() {
    let (mut c, id) = decorated_compositor();
    c.composite();
    let client = c.window_client_rect(id).expect("decorated client");
    let outer = c.window(id).expect("window").bounds();
    assert!(client.left() > outer.left() && client.top() > outer.top());

    let edit = present_content(&mut c, id, |surface| {
        surface.set(0, 0, Color::rgb(0, 255, 0).premultiply());
        ((), Rect::new(0, 0, 2, 2))
    });
    assert_eq!(edit, Some(()));

    // The reported rectangle is content-local, so it lands at the client's
    // top-left inside the furniture band, never at the outer origin.
    let expected = Rect::new(client.left(), client.top(), 2, 2);
    assert_eq!(composite_checked(&mut c).rects(), &[expected]);
}

#[test]
fn content_damage_larger_than_the_window_is_clipped_to_its_client() {
    let mut c = new_compositor(mode(32, 32), BLUE).expect("compositor");
    let id = c.add_window(Point::new(20, 20), opaque(8, 8, RED));
    c.add_window(Point::new(0, 0), opaque(8, 8, BLUE));
    c.composite();
    let client = c.window_client_rect(id).expect("client");

    // An over-large report must never reach a neighbouring window's pixels.
    let edit = present_content(&mut c, id, |_surface| ((), Rect::new(0, 0, 1_000, 1_000)));
    assert_eq!(edit, Some(()));
    assert_eq!(composite_checked(&mut c).rects(), &[client]);
}

#[test]
fn an_empty_content_damage_marks_nothing_although_the_edit_ran() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    let id = c.add_window(Point::new(2, 2), opaque(4, 4, RED));
    c.composite();

    let mut ran = false;
    let edited = present_content(&mut c, id, |_surface| {
        ran = true;
        (42_u8, Rect::EMPTY)
    });
    assert_eq!(edited, Some(42));
    assert!(ran, "the edit still runs and reports its value");
    assert!(
        !c.has_damage(),
        "an edit that changed nothing repaints nothing"
    );
    assert!(composite_checked(&mut c).is_empty());
}

#[test]
fn editing_an_unknown_window_never_runs_the_edit() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    let mut ran = false;
    let edited = c.present_window_content(WindowId(9_999), 4, 4, |_surface| {
        ran = true;
        ((), Rect::new(0, 0, 4, 4))
    });
    assert_eq!(edited, None);
    assert!(!ran);
}

// ---- present seam ----------------------------------------------------

#[test]
fn present_composites_then_writes_frame() {
    let m = mode(2, 2);
    let mut c = new_compositor(m, BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(2, 2, RED));
    let mut display = MockDisplay::new(m);
    assert!(c.present(&mut display).is_ok());
    assert_eq!(display.last, c.frame());
    assert!(!c.has_damage());
}

#[test]
fn present_propagates_driver_error() {
    let m = mode(2, 2);
    let mut c = new_compositor(m, BLUE).expect("compositor");
    let mut display = MockDisplay::new(m);
    display.fail = true;
    assert_eq!(c.present(&mut display), Err(DriverError::DeviceFault));
}

#[test]
fn a_present_with_no_damage_never_touches_the_display() {
    let m = mode(8, 8);
    let mut c = new_compositor(m, BLUE).expect("compositor");
    let mut display = MockDisplay::new(m);

    // A new compositor marks the whole screen, so the desktop's very first
    // present still reaches the driver.
    assert!(c.present(&mut display).is_ok());
    assert_eq!(display.full_presents, 1);
    assert_eq!(display.last, c.frame());

    // A wake that changed nothing must cost neither the scan-out copy nor
    // the driver blit.
    display.last.clear();
    assert!(c.present(&mut display).is_ok());
    assert_eq!(display.full_presents, 1);
    assert!(display.regions.is_empty());
    assert!(display.last.is_empty(), "the driver was not called at all");
}

#[test]
fn two_disjoint_dirty_rectangles_present_exactly_those_rectangles() {
    let m = mode(64, 64);
    let mut c = new_compositor(m, BLUE).expect("compositor");
    let mut display = MockDisplay::new(m);
    assert!(c.present(&mut display).is_ok());
    display.full_presents = 0;

    // A repaint at the top-left and one at the bottom-right: their bounding
    // box is nearly the whole screen, the changed pixels are two small
    // rectangles.
    c.add_window(Point::ORIGIN, opaque(4, 4, RED));
    c.add_window(Point::new(56, 58), opaque(4, 4, RED));
    assert!(c.present(&mut display).is_ok());

    assert_eq!(
        display.full_presents, 0,
        "a partial frame is not a full present"
    );
    assert_eq!(display.regions.len(), 2);
    assert!(display.regions.contains(&DamageRect {
        x: 0,
        y: 0,
        width_px: 4,
        height_px: 4,
    }));
    assert!(display.regions.contains(&DamageRect {
        x: 56,
        y: 58,
        width_px: 4,
        height_px: 4,
    }));
}

#[test]
fn whole_screen_damage_presents_the_frame_once() {
    let m = mode(32, 32);
    let mut c = new_compositor(m, BLUE).expect("compositor");
    let mut display = MockDisplay::new(m);
    assert!(c.present(&mut display).is_ok());
    display.full_presents = 0;

    assert!(c.set_background(RED));
    assert!(c.present(&mut display).is_ok());
    assert_eq!(display.full_presents, 1);
    assert!(
        display.regions.is_empty(),
        "a whole-screen region is one full present, never a region blit"
    );
}

#[test]
fn more_dirty_rectangles_than_the_limit_collapse_to_one_bounding_present() {
    let m = mode(64, 64);
    let mut c = new_compositor(m, BLUE).expect("compositor");
    let mut display = MockDisplay::new(m);
    assert!(c.present(&mut display).is_ok());
    display.full_presents = 0;

    // One more scattered rectangle than the per-rectangle path carries:
    // their round trips would cost more than one call copying their box.
    let count = i32::try_from(MAX_PRESENT_REGIONS + 1).expect("a small limit");
    for step in 0..count {
        c.add_window(Point::new(step * 6, 0), opaque(4, 4, RED));
    }
    assert!(c.present(&mut display).is_ok());

    assert_eq!(display.full_presents, 0);
    let width = u32::try_from((count - 1) * 6 + 4).expect("on screen");
    assert_eq!(
        display.regions,
        [DamageRect {
            x: 0,
            y: 0,
            width_px: width,
            height_px: 4,
        }]
    );
}

// ---- shared theme integration (lib/theme) ---------------------------

#[test]
fn active_theme_drives_compositor_background() {
    // The compositor sources its root background from the active theme,
    // and a runtime theme switch (dark -> light) changes the colour the
    // screen clears to. One shared definition drives the WM.
    let mut themes = ThemeRegistry::with_builtins();
    let dark_bg = themes.active().palette().desktop;
    let mut c = new_compositor(mode(2, 2), dark_bg.into()).expect("compositor");
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), dark_bg.to_array());

    themes
        .set_active(ThemeId::LIGHT)
        .expect("light is built in");
    let light_bg = themes.active().palette().desktop;
    assert_ne!(light_bg, dark_bg);
    let mut c = new_compositor(mode(2, 2), light_bg.into()).expect("compositor");
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), light_bg.to_array());
}

#[test]
fn theme_corner_radius_shapes_windows() {
    // A window takes its corner radius from the active theme's metrics
    // through the single compositor rounded-corner path: the
    // rounded corner still reveals the background behind it.
    let themes = ThemeRegistry::with_builtins();
    let radius = themes.active().metrics().window_corner_radius;
    let corners = Corners::from_radius(radius);
    assert_eq!(corners, Corners::Rounded { radius });
    assert_eq!(Corners::from_radius(0), Corners::Square);

    let mut c = new_compositor(mode(20, 20), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(20, 20, RED));
    assert!(c.set_corners(id, corners));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]); // corner clipped to bg
    assert_eq!(frame_pixel(&c, 10, 10), [255, 0, 0, 255]); // centre opaque
}

// ---- input routing ---------------------------------------------------

use crate::input::{
    InputEvent, InputResponse, InputRouter, Key, Modifiers, NamedKey, PointerButton,
};

fn press_primary() -> InputEvent {
    InputEvent::PointerPressed {
        button: PointerButton::Primary,
    }
}

fn key_pressed(key: Key) -> InputEvent {
    InputEvent::KeyPressed {
        key,
        modifiers: Modifiers::default(),
    }
}

fn release_primary() -> InputEvent {
    InputEvent::PointerReleased {
        button: PointerButton::Primary,
    }
}

fn press_secondary() -> InputEvent {
    InputEvent::PointerPressed {
        button: PointerButton::Secondary,
    }
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

#[test]
fn hit_test_picks_top_most_visible_window() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let bottom = c.add_window(Point::new(0, 0), opaque(20, 20, RED));
    let top = c.add_window(Point::new(10, 10), opaque(20, 20, RED));

    // Overlap region resolves to the higher window.
    assert_eq!(c.window_at(Point::new(15, 15)), Some(top));
    // Only the bottom window covers this point.
    assert_eq!(c.window_at(Point::new(2, 2)), Some(bottom));
    // Background.
    assert_eq!(c.window_at(Point::new(35, 35)), None);

    // A hidden window is not hit even where it lies on top.
    assert!(c.set_visible(top, false));
    assert_eq!(c.window_at(Point::new(15, 15)), Some(bottom));
}

#[test]
fn press_activates_raises_and_focuses() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let bottom = c.add_window(Point::new(0, 0), opaque(30, 30, RED));
    let top = c.add_window(Point::new(20, 0), opaque(20, 30, RED));
    let mut router = InputRouter::new();

    // Press on the bottom window where the top does not cover it. A hover
    // over client content is delivered to that window (undecorated test
    // windows are all client) so its in-content controls can track the
    // pointer.
    let r = router.handle(moved(5, 5), &mut c);
    assert_eq!(
        r,
        InputResponse::ClientPointerMoved {
            window: bottom,
            local: Point::new(5, 5),
        }
    );
    let r = router.handle(press_primary(), &mut c);
    assert_eq!(
        r,
        InputResponse::Activated {
            window: bottom,
            local: Point::new(5, 5),
        }
    );
    assert_eq!(router.focused(), Some(bottom));
    // The activated window is now the top of the z-order: in the
    // overlap it wins, while a point only `top` covers still hits `top`.
    assert_eq!(c.window_at(Point::new(22, 5)), Some(bottom)); // overlap now bottom-on-top
    assert_eq!(c.window_at(Point::new(35, 5)), Some(top)); // only top covers here
}

#[test]
fn secondary_press_activates_and_delivers_to_the_client() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let bottom = c.add_window(Point::new(0, 0), opaque(30, 30, RED));
    let top = c.add_window(Point::new(20, 0), opaque(20, 30, RED));
    let mut router = InputRouter::new();

    // A right-click on the bottom window (where the top does not cover it)
    // raises+focuses it exactly as a primary press does, and is delivered to
    // the client as a secondary press — the event a client uses to open its
    // context menu (undecorated test windows have no furniture, so the client
    // area is the whole window).
    router.handle(moved(5, 5), &mut c);
    assert_eq!(
        router.handle(press_secondary(), &mut c),
        InputResponse::SecondaryActivated {
            window: bottom,
            local: Point::new(5, 5),
        }
    );
    assert_eq!(router.focused(), Some(bottom));
    assert_eq!(c.window_at(Point::new(22, 5)), Some(bottom)); // raised over top
    let _ = top;
}

#[test]
fn secondary_press_on_desktop_is_reported_to_the_desktop_and_changes_nothing() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::new(0, 0), opaque(10, 10, RED));
    let mut router = InputRouter::new();
    assert!(router.focus(win, &c));

    // A right-click on the bare desktop is the desktop's own question to
    // answer: the window manager reports it and synthesises no menu itself.
    router.handle(moved(30, 30), &mut c);
    assert_eq!(
        router.handle(press_secondary(), &mut c),
        InputResponse::DesktopSecondaryPressed
    );
    // Unlike the primary press, it activates nothing: the focused window
    // keeps the keyboard and the z-order is untouched.
    assert_eq!(router.focused(), Some(win));
    assert_eq!(c.window_at(Point::new(5, 5)), Some(win));
}

#[test]
fn press_on_desktop_clears_focus() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::new(0, 0), opaque(10, 10, RED));
    let mut router = InputRouter::new();

    router.handle(moved(5, 5), &mut c);
    assert!(matches!(
        router.handle(press_primary(), &mut c),
        InputResponse::Activated { window, .. } if window == win
    ));
    assert_eq!(router.focused(), Some(win));

    // Click the background.
    router.handle(moved(30, 30), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::DesktopPressed
    );
    assert_eq!(router.focused(), None);
}

#[test]
fn focus_gives_keyboard_focus_to_a_known_window() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::ORIGIN, opaque(10, 10, RED));
    let mut router = InputRouter::new();

    assert_eq!(router.focused(), None);
    assert!(router.focus(win, &c), "a known window can be focused");
    assert_eq!(router.focused(), Some(win));

    router.unfocus();
    assert_eq!(router.focused(), None, "unfocus drops keyboard focus");
}

#[test]
fn focus_fails_closed_for_an_unknown_window() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::ORIGIN, opaque(10, 10, RED));
    assert!(c.remove(win), "the window is removed");
    let mut router = InputRouter::new();

    assert!(
        !router.focus(win, &c),
        "focusing a window the compositor no longer knows fails closed"
    );
    assert_eq!(router.focused(), None);
}

#[test]
fn key_is_delivered_to_the_focused_window() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::ORIGIN, opaque(10, 10, RED));
    let mut router = InputRouter::new();
    assert!(router.focus(win, &c));

    assert_eq!(
        router.handle(key_pressed(Key::Char('k')), &mut c),
        InputResponse::Key {
            window: win,
            key: Key::Char('k'),
            modifiers: Modifiers::default(),
            pressed: true,
        }
    );
    assert_eq!(
        router.handle(
            InputEvent::KeyReleased {
                key: Key::Named(NamedKey::Enter),
                modifiers: Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
            },
            &mut c
        ),
        InputResponse::Key {
            window: win,
            key: Key::Named(NamedKey::Enter),
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            pressed: false,
        }
    );
}

#[test]
fn key_without_focus_goes_to_the_desktop_not_to_a_window() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(10, 10, RED));
    let mut router = InputRouter::new();

    assert_eq!(router.focused(), None);
    assert_eq!(
        router.handle(key_pressed(Key::Char('a')), &mut c),
        InputResponse::DesktopKey {
            key: Key::Char('a'),
            modifiers: Modifiers::default(),
            pressed: true,
        }
    );
}

#[test]
fn key_to_a_vanished_focus_falls_back_to_the_desktop_and_drops_focus() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::ORIGIN, opaque(10, 10, RED));
    let mut router = InputRouter::new();
    assert!(router.focus(win, &c));

    assert!(c.remove(win), "the focused window is removed");
    assert_eq!(
        router.handle(key_pressed(Key::Char('a')), &mut c),
        InputResponse::DesktopKey {
            key: Key::Char('a'),
            modifiers: Modifiers::default(),
            pressed: true,
        }
    );
    assert_eq!(
        router.focused(),
        None,
        "a key to a vanished window drops stale focus"
    );
}

#[test]
fn an_unhandled_button_does_not_change_focus() {
    // The primary button activates and the secondary opens a context menu
    // (both raise+focus); the middle button carries no window-manager meaning,
    // so it is consumed without changing focus.
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    c.add_window(Point::new(0, 0), opaque(10, 10, RED));
    let mut router = InputRouter::new();

    router.handle(moved(5, 5), &mut c);
    let r = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Middle,
        },
        &mut c,
    );
    assert_eq!(r, InputResponse::Ignored);
    assert_eq!(router.focused(), None);
}

#[test]
fn move_grab_drags_focused_window() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    let mut router = InputRouter::new();

    // Activate, then begin a move-grab (as decorations would on a
    // title-bar press) and drag.
    router.handle(moved(15, 12), &mut c);
    router.handle(press_primary(), &mut c);
    assert!(router.begin_move(&c));
    assert!(router.is_moving());

    // Pointer moves by (+20, +8); window tracks it, grab offset (5, 2)
    // preserved.
    let r = router.handle(moved(35, 20), &mut c);
    assert_eq!(
        r,
        InputResponse::Moved {
            window: win,
            origin: Point::new(30, 18),
        }
    );
    assert_eq!(
        c.window(win).map(super::window::Window::origin),
        Some(Point::new(30, 18))
    );

    // Release ends the grab; further motion no longer moves the window.
    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::MoveEnded { window: win }
    );
    assert!(!router.is_moving());
    assert_eq!(
        router.handle(moved(60, 60), &mut c),
        InputResponse::DesktopPointerMoved
    );
    assert_eq!(
        c.window(win).map(super::window::Window::origin),
        Some(Point::new(30, 18))
    );
}

#[test]
fn begin_move_fails_closed_without_focus() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    c.add_window(Point::new(0, 0), opaque(10, 10, RED));
    let mut router = InputRouter::new();

    assert!(!router.begin_move(&c));
    assert!(!router.is_moving());
}

#[test]
fn drag_ends_if_grabbed_window_removed() {
    let mut c = new_compositor(mode(60, 60), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    let mut router = InputRouter::new();

    router.handle(moved(15, 15), &mut c);
    router.handle(press_primary(), &mut c);
    assert!(router.begin_move(&c));

    assert!(c.remove(win));
    assert_eq!(
        router.handle(moved(40, 40), &mut c),
        InputResponse::MoveEnded { window: win }
    );
    assert!(!router.is_moving());
}

#[test]
fn client_hover_moves_route_to_the_window_under_the_pointer() {
    let mut c = new_compositor(mode(60, 60), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    let mut router = InputRouter::new();

    // A hover over client content is delivered window-local so the client's
    // in-content controls (a scrollbar, a menu) can track the pointer.
    assert_eq!(
        router.handle(moved(15, 12), &mut c),
        InputResponse::ClientPointerMoved {
            window: win,
            local: Point::new(5, 2),
        }
    );
    // A hover over the desktop belongs to no client — it belongs to the
    // desktop layer's owner, which is told rather than left guessing.
    assert_eq!(
        router.handle(moved(50, 50), &mut c),
        InputResponse::DesktopPointerMoved
    );
}

#[test]
fn client_press_captures_the_pointer_until_release() {
    let mut c = new_compositor(mode(60, 60), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    let mut router = InputRouter::new();

    // Press on the client content: activates the window and takes the
    // implicit pointer grab.
    router.handle(moved(15, 15), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::Activated {
            window: win,
            local: Point::new(5, 5),
        }
    );

    // A move during the grab is delivered to the grabbed window as an
    // in-content drag, even once the pointer leaves the window: the position
    // is clamped into the client so the drag keeps tracking rather than
    // wrapping or jumping.
    assert_eq!(
        router.handle(moved(25, 25), &mut c),
        InputResponse::ClientPointerMoved {
            window: win,
            local: Point::new(15, 15),
        }
    );
    assert_eq!(
        router.handle(moved(100, 100), &mut c),
        InputResponse::ClientPointerMoved {
            window: win,
            local: Point::new(19, 19),
        }
    );

    // The release completes the in-content click/drag on the grabbed window
    // and ends the grab; a later move is a plain hover again.
    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::ClientPointerReleased {
            window: win,
            local: Point::new(19, 19),
        }
    );
    assert_eq!(
        router.handle(moved(15, 15), &mut c),
        InputResponse::ClientPointerMoved {
            window: win,
            local: Point::new(5, 5),
        }
    );
}

#[test]
fn client_grab_ends_if_grabbed_window_removed() {
    let mut c = new_compositor(mode(60, 60), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    let mut router = InputRouter::new();

    router.handle(moved(15, 15), &mut c);
    router.handle(press_primary(), &mut c);
    assert!(c.remove(win));
    // With the grabbed window gone, the drag fails closed rather than naming
    // a window that no longer exists: neither the motion nor the release
    // names a recipient.
    assert_eq!(router.handle(moved(20, 20), &mut c), InputResponse::Ignored);
    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::Ignored
    );
}

#[test]
fn pointer_position_tracks_motion() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    assert_eq!(router.pointer(), Point::ORIGIN);
    router.handle(moved(7, 9), &mut c);
    assert_eq!(router.pointer(), Point::new(7, 9));
}

/// A solid opaque `size`×`size` cursor image in `color`, hotspot at the
/// top-left, built through the shared cursor library.
pub(crate) fn solid_cursor(size: u32, color: Color) -> tairix_cursor::CursorImage {
    use tairix_cursor::{Shape, VectorCursor, Vertex};
    let s = i32::try_from(size).expect("small");
    let shape = Shape::new(
        color,
        alloc::vec![
            Vertex::new(0, 0),
            Vertex::new(s, 0),
            Vertex::new(s, s),
            Vertex::new(0, s),
        ],
    );
    VectorCursor::new(size, 0, 0, alloc::vec![shape])
        .rasterise(100)
        .expect("renderable")
}

#[test]
fn cursor_overlay_composites_over_the_desktop() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(8, RED), Point::new(10, 10));
    assert_eq!(c.cursor_bounds(), Some(Rect::new(10, 10, 8, 8)));
    c.composite();
    // Under the cursor: red. Away from it: the blue desktop.
    assert_eq!(frame_pixel(&c, 12, 12), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 30, 30), [0, 0, 255, 255]);
}

#[test]
fn cursor_overlay_draws_above_windows() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let green = Color::rgb(0, 255, 0);
    c.add_window(Point::new(0, 0), opaque(40, 40, green));
    c.set_cursor(solid_cursor(8, RED), Point::new(4, 4));
    c.composite();
    assert_eq!(frame_pixel(&c, 5, 5), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 30, 30), [0, 255, 0, 255]);
}

#[test]
fn moving_the_cursor_restores_pixels_behind_it() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(8, RED), Point::new(2, 2));
    c.composite();
    assert_eq!(frame_pixel(&c, 4, 4), [255, 0, 0, 255]);

    assert!(c.move_cursor(Point::new(20, 20)));
    c.composite();
    // The vacated area is the blue desktop again; the new area is red.
    assert_eq!(frame_pixel(&c, 4, 4), [0, 0, 255, 255]);
    assert_eq!(frame_pixel(&c, 22, 22), [255, 0, 0, 255]);
}

#[test]
fn hiding_the_cursor_restores_the_pixels_beneath() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(8, RED), Point::new(2, 2));
    c.composite();
    assert_eq!(frame_pixel(&c, 4, 4), [255, 0, 0, 255]);

    assert!(c.hide_cursor());
    assert_eq!(c.cursor_bounds(), None);
    c.composite();
    assert_eq!(frame_pixel(&c, 4, 4), [0, 0, 255, 255]);
}

#[test]
fn move_and_hide_cursor_fail_closed_without_a_cursor() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    assert!(!c.move_cursor(Point::new(5, 5)));
    assert!(!c.hide_cursor());
    assert_eq!(c.cursor_bounds(), None);
}

#[test]
fn replacing_the_cursor_image_marks_both_footprints_dirty() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(8, RED), Point::new(2, 2));
    c.composite();
    assert!(!c.has_damage());

    // A larger cursor at the same hotspot: setting it must re-dirty the area.
    c.set_cursor(solid_cursor(12, RED), Point::new(2, 2));
    assert!(c.has_damage());
    c.composite();
    assert_eq!(c.cursor_bounds(), Some(Rect::new(2, 2, 12, 12)));
    assert_eq!(frame_pixel(&c, 12, 12), [255, 0, 0, 255]);
}

#[test]
fn a_cursor_sweep_damages_only_the_rectangle_it_left_and_the_one_it_reached() {
    let mut c = new_compositor(mode(64, 64), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(4, RED), Point::ORIGIN);
    c.composite();

    // A whole batch of pointer samples pumped between two composites: the
    // intermediate positions were never drawn, so their pixels are already
    // correct and recompositing them is pure waste.
    for x in 1..=8 {
        assert!(c.move_cursor(Point::new(x, 0)));
    }
    let region = composite_checked(&mut c);
    assert_eq!(
        region.rects().len(),
        2,
        "one rectangle per sample would be 9"
    );
    assert!(region.rects().contains(&Rect::new(0, 0, 4, 4)));
    assert!(region.rects().contains(&Rect::new(8, 0, 4, 4)));
}

#[test]
fn a_single_cursor_move_damages_both_of_its_rectangles() {
    let mut c = new_compositor(mode(64, 64), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(4, RED), Point::ORIGIN);
    c.composite();

    assert!(c.move_cursor(Point::new(8, 0)));
    let region = composite_checked(&mut c);
    assert_eq!(region.rects().len(), 2);
    assert!(region.rects().contains(&Rect::new(0, 0, 4, 4)));
    assert!(region.rects().contains(&Rect::new(8, 0, 4, 4)));
}

#[test]
fn hiding_and_reshowing_the_cursor_damages_one_rectangle_each() {
    let mut c = new_compositor(mode(64, 64), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(4, RED), Point::new(10, 10));
    c.composite();

    // Hiding restores what the cursor covered and touches nothing else...
    assert!(c.hide_cursor());
    assert_eq!(
        composite_checked(&mut c).rects(),
        &[Rect::new(10, 10, 4, 4)]
    );

    // ...and showing it elsewhere paints only where it now is.
    c.set_cursor(solid_cursor(4, RED), Point::new(30, 30));
    assert_eq!(
        composite_checked(&mut c).rects(),
        &[Rect::new(30, 30, 4, 4)]
    );
    assert_eq!(frame_pixel(&c, 31, 31), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 11, 11), [0, 0, 255, 255]);
}

/// Composite a small scene with a window under the pointer, apply every
/// `sample` to the cursor, composite once more, and return the resulting
/// scan-out frame — the shape of a desktop wake that pumps a batch of
/// pointer motion before repainting.
fn cursor_sweep_frame(samples: &[Point]) -> alloc::vec::Vec<u8> {
    let mut c = new_compositor(mode(24, 24), BLUE).expect("compositor");
    c.add_window(Point::new(4, 4), opaque(12, 12, RED));
    c.set_cursor(solid_cursor(6, Color::rgb(0, 255, 0)), Point::ORIGIN);
    c.composite();
    for &sample in samples {
        assert!(c.move_cursor(sample));
    }
    c.composite();
    c.frame().to_vec()
}

#[test]
fn a_swept_cursor_composites_the_frame_a_single_move_would() {
    // The decisive check that damaging only the leaving and arriving
    // rectangles is not lossy: a sweep of samples must leave the screen
    // byte-for-byte where one move to the same place leaves it.
    let sweep: alloc::vec::Vec<Point> = (1..=10).map(|step| Point::new(step, step)).collect();
    let swept = cursor_sweep_frame(&sweep);
    let direct = cursor_sweep_frame(&[Point::new(10, 10)]);
    assert_eq!(swept, direct);

    // ...and the sweep really did move the cursor rather than draw nothing.
    assert_ne!(swept, cursor_sweep_frame(&[Point::ORIGIN]));
}

// ---- cursor selection from interaction state -------------------------

use crate::select::{desired_cursor, CursorController};
use tairix_cursor::{CursorRegistry, CursorSetId, CursorTheme};
use tairix_geometry::Scale;
use tairix_theme::CursorKind;

#[test]
fn window_cursor_hint_round_trips_and_unknown_id_fails_closed() {
    let mut c = new_compositor(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::new(0, 0), opaque(10, 10, RED));

    // Default hint is the plain arrow.
    assert_eq!(c.window_cursor(win), Some(CursorKind::Arrow));
    assert!(c.set_window_cursor(win, CursorKind::Text));
    assert_eq!(c.window_cursor(win), Some(CursorKind::Text));

    // An unknown window changes nothing.
    let ghost = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.remove(ghost));
    assert!(!c.set_window_cursor(ghost, CursorKind::Pointer));
    assert_eq!(c.window_cursor(ghost), None);
}

#[test]
fn desired_cursor_reflects_the_window_under_the_pointer() {
    let mut c = new_compositor(mode(60, 60), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    let mut router = InputRouter::new();

    // Over the desktop background: the plain arrow.
    router.handle(moved(50, 50), &mut c);
    assert_eq!(desired_cursor(&router, &c), CursorKind::Arrow);

    // Over a default window: still the arrow.
    router.handle(moved(15, 15), &mut c);
    assert_eq!(desired_cursor(&router, &c), CursorKind::Arrow);

    // The window advertises a text cursor over its content.
    assert!(c.set_window_cursor(win, CursorKind::Text));
    assert_eq!(desired_cursor(&router, &c), CursorKind::Text);

    // Moving back to the background returns to the arrow.
    router.handle(moved(50, 50), &mut c);
    assert_eq!(desired_cursor(&router, &c), CursorKind::Arrow);
}

#[test]
fn move_grab_outranks_the_window_hint() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    let mut router = InputRouter::new();
    assert!(c.set_window_cursor(win, CursorKind::Text));

    router.handle(moved(15, 15), &mut c);
    router.handle(press_primary(), &mut c);
    assert!(router.begin_move(&c));

    // While dragging, the move cursor wins over the window's text hint.
    assert!(router.is_moving());
    assert_eq!(desired_cursor(&router, &c), CursorKind::Move);

    router.handle(release_primary(), &mut c);
    assert_eq!(desired_cursor(&router, &c), CursorKind::Text);
}

#[test]
fn controller_installs_and_switches_the_cursor_shape() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(30, 30, RED));
    assert!(c.set_window_cursor(win, CursorKind::Text));
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());

    // First refresh over the desktop installs the arrow.
    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(ctrl.kind(), CursorKind::Arrow);
    assert!(c.cursor_bounds().is_some());

    // A repeat refresh with the same kind does no work.
    assert!(!ctrl.refresh(&router, &mut c));

    // Moving over the text window switches the shape.
    router.handle(moved(20, 20), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(ctrl.kind(), CursorKind::Text);
}

#[test]
fn controller_reuses_a_cached_kind_when_it_recurs() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(30, 30, RED));
    assert!(c.set_window_cursor(win, CursorKind::Text));
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());

    // Arrow over the background, then Text over the window.
    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    let arrow_bounds = c.cursor_bounds().expect("arrow shown");
    router.handle(moved(20, 20), &mut c);
    assert!(ctrl.refresh(&router, &mut c));

    // Returning to the background re-shows the cached arrow unchanged: same
    // kind and same footprint as the first time it was rasterised.
    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(ctrl.kind(), CursorKind::Arrow);
    assert_eq!(
        c.cursor_bounds().map(|b| (b.width, b.height)),
        Some((arrow_bounds.width, arrow_bounds.height))
    );
}

#[test]
fn refresh_without_a_cursor_after_a_scale_change_draws_nothing() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());

    // No cursor shown yet and the policy has not run: raising the output
    // scale damages the screen but there is nothing to install, and a
    // refresh over the desktop installs the arrow at the new density.
    let bigger = Scale::from_percent(200).expect("valid scale");
    assert!(c.set_scale(bigger));
    assert!(ctrl.refresh(&router, &mut c));
    assert!(c.cursor_bounds().is_some());
}

#[test]
fn controller_re_renders_on_scale_change() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());

    // Show a cursor at 1:1, then raise the output scale: a refresh sees the
    // new density and re-rasterises, so the footprint enlarges even though
    // the chosen kind is unchanged.
    router.handle(moved(10, 10), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    let small = c.cursor_bounds().expect("cursor shown");
    let bigger = Scale::from_percent(200).expect("valid scale");
    assert!(c.set_scale(bigger));
    assert!(ctrl.refresh(&router, &mut c));
    let large = c.cursor_bounds().expect("cursor shown");
    assert!(large.width > small.width);
    assert!(large.height > small.height);
}

#[test]
fn controller_re_renders_on_registry_swap() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());

    router.handle(moved(10, 10), &mut c);
    assert!(ctrl.refresh(&router, &mut c));

    // A registry that selects an alternative set re-renders the cursor.
    let mut registry = CursorRegistry::with_builtin();
    let custom = CursorSetId::new("alt");
    registry
        .register(custom, CursorTheme::builtin())
        .expect("register");
    registry.set_active(custom).expect("activate");
    assert!(ctrl.set_registry(registry, &router, &mut c));
    assert_eq!(ctrl.registry().active_id(), custom);
    assert!(c.cursor_bounds().is_some());
}

#[test]
fn output_scale_starts_at_one_and_is_settable() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    assert_eq!(c.scale(), Scale::ONE);

    let bigger = Scale::from_percent(200).expect("valid scale");
    assert!(c.set_scale(bigger), "a new scale changes the output");
    assert_eq!(c.scale(), bigger);

    // Setting the scale already in effect is a no-op the embedder can skip.
    assert!(!c.set_scale(bigger));
}

#[test]
fn setting_the_output_scale_marks_the_whole_screen_dirty() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    c.composite();
    assert!(!c.has_damage(), "a fresh composite clears the damage");

    let bigger = Scale::from_percent(150).expect("valid scale");
    assert!(c.set_scale(bigger));
    assert!(
        c.has_damage(),
        "a scale change re-rasterises every window next composite"
    );
}

#[test]
fn window_scale_reports_the_output_scale_for_a_known_window() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(20, 20, RED));
    assert_eq!(c.window_scale(win), Some(Scale::ONE));

    let bigger = Scale::from_percent(200).expect("valid scale");
    c.set_scale(bigger);
    assert_eq!(
        c.window_scale(win),
        Some(bigger),
        "an app reads its window's output density here"
    );

    assert_eq!(c.window_scale(WindowId(9_999)), None, "unknown id is None");
}

// ---- hardware-accelerated present ------------------------------------

/// One layer the mock engine was asked to composite, captured by value
/// so the test can inspect it after the borrowed slice is gone.
struct CapturedLayer {
    pixels: alloc::vec::Vec<u8>,
    width: u32,
    height: u32,
    dst_x: i32,
    dst_y: i32,
}

/// A hardware-layer engine seam that records the layer stack handed to
/// it, the software frame presented on fallback, and reports a
/// configurable [`AccelCaps`].
struct MockAccel {
    mode: DisplayMode,
    caps: AccelCaps,
    layers: alloc::vec::Vec<CapturedLayer>,
    software_frame: alloc::vec::Vec<u8>,
}

impl MockAccel {
    fn new(mode: DisplayMode, caps: AccelCaps) -> Self {
        Self {
            mode,
            caps,
            layers: alloc::vec::Vec::new(),
            software_frame: alloc::vec::Vec::new(),
        }
    }
}

impl Display for MockAccel {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }
    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        self.software_frame = frame.to_vec();
        Ok(())
    }
}

impl AcceleratedDisplay for MockAccel {
    fn accel_caps(&self) -> Result<AccelCaps, DriverError> {
        Ok(self.caps)
    }
    fn present_layers(&mut self, layers: &[AccelLayer<'_>]) -> Result<(), DriverError> {
        self.layers = layers
            .iter()
            .map(|l| CapturedLayer {
                pixels: l.pixels.to_vec(),
                width: l.width_px,
                height: l.height_px,
                dst_x: l.dst_x,
                dst_y: l.dst_y,
            })
            .collect();
        Ok(())
    }
}

fn generous_caps() -> AccelCaps {
    AccelCaps {
        max_layers: 8,
        max_width_px: 1024,
        max_height_px: 1024,
        per_layer_opacity: true,
    }
}

/// Read the RGBA bytes of pixel `(x, y)` in a captured layer.
fn layer_pixel(layer: &CapturedLayer, x: u32, y: u32) -> [u8; 4] {
    let off = usize::try_from((y * layer.width + x) * 4).expect("offset");
    [
        layer.pixels[off],
        layer.pixels[off + 1],
        layer.pixels[off + 2],
        layer.pixels[off + 3],
    ]
}

#[test]
fn accelerated_present_encodes_background_and_window_layers() {
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    let mut display = MockAccel::new(mode(8, 8), generous_caps());

    c.present_accelerated(&mut display)
        .expect("accelerated present");

    // Background layer + one window layer, in back-to-front order.
    assert_eq!(display.layers.len(), 2, "background + window");
    let bg = &display.layers[0];
    assert_eq!((bg.width, bg.height, bg.dst_x, bg.dst_y), (8, 8, 0, 0));
    assert_eq!(
        layer_pixel(bg, 4, 4),
        [0, 0, 255, 255],
        "background is blue"
    );

    let win = &display.layers[1];
    assert_eq!((win.width, win.height, win.dst_x, win.dst_y), (2, 2, 1, 1));
    assert_eq!(layer_pixel(win, 0, 0), [255, 0, 0, 255], "window is red");
    assert_eq!(layer_pixel(win, 1, 1), [255, 0, 0, 255]);

    // The software fallback was not taken.
    assert!(display.software_frame.is_empty());
}

#[test]
fn hidden_window_is_omitted_from_the_layer_stack() {
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    let win = c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    assert!(c.set_visible(win, false));
    let mut display = MockAccel::new(mode(8, 8), generous_caps());

    c.present_accelerated(&mut display).expect("present");
    assert_eq!(display.layers.len(), 1, "only the background remains");
}

/// An engine blends a layer over what is beneath it in the scan-out's own 8
/// bits with a fixed rounding, which is exactly what bands a picture under a
/// translucent field. The compositor keeps such a scene for itself, where the
/// blend can spread its rounding across the area, as it already does for a
/// backdrop blur.
#[test]
fn a_translucent_window_takes_the_software_path_the_engine_cannot_dither() {
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    let win = c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    assert!(c.set_opacity(win, 200));
    let mut display = MockAccel::new(mode(8, 8), generous_caps());

    c.present_accelerated(&mut display).expect("present");

    assert!(display.layers.is_empty(), "no layer stack was handed over");
    assert!(
        !display.software_frame.is_empty(),
        "the frame went through the software composite"
    );

    // Opaque again, and the engine serves the scene as before.
    assert!(c.set_opacity(win, 255));
    c.present_accelerated(&mut display).expect("present");
    assert_eq!(display.layers.len(), 2, "background + window");
}

#[test]
fn accelerated_present_falls_back_when_over_layer_budget() {
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    // One plane only: background + window needs two, so the engine cannot
    // serve the scene and the compositor uses the software path.
    let caps = AccelCaps {
        max_layers: 1,
        ..generous_caps()
    };
    let mut display = MockAccel::new(mode(8, 8), caps);

    c.present_accelerated(&mut display).expect("present");
    assert!(display.layers.is_empty(), "no hardware layers used");
    assert_eq!(
        display.software_frame.len(),
        8 * 8 * 4,
        "software frame sent"
    );
    // The window's red is composited into the software frame at (1,1).
    let off: usize = (8 + 1) * 4; // pixel (1,1) in an 8-wide RGBA frame
    assert_eq!(
        &display.software_frame[off..off + 4],
        &[255, 0, 0, 255],
        "software fallback composited the window"
    );
}

#[test]
fn accelerated_present_falls_back_when_a_layer_is_too_large() {
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    // The background layer is the full 8×8 screen; an engine that can
    // only source 4-px-wide planes cannot take it.
    let caps = AccelCaps {
        max_width_px: 4,
        ..generous_caps()
    };
    let mut display = MockAccel::new(mode(8, 8), caps);

    c.present_accelerated(&mut display).expect("present");
    assert!(display.layers.is_empty(), "no hardware layers used");
    assert_eq!(
        display.software_frame.len(),
        8 * 8 * 4,
        "software frame sent"
    );
}

// ---- backdrop blur ---------------------------------------------------

/// A fully transparent surface: a window made of it draws nothing of its
/// own, so the composited pixels under it *are* its backdrop and a test can
/// read the blur's own output rather than a blend of it.
fn clear(w: u32, h: u32) -> Surface {
    Surface::filled(w, h, Pixel::TRANSPARENT).expect("surface allocates")
}

/// A 12×6 screen holding a hard red/blue vertical edge at column 6 (an
/// opaque window over the left half of a blue background) with a
/// transparent `radius`-blurred window over the whole of it, composited
/// from scratch.
fn frosted_edge(radius: u16) -> Compositor {
    let mut c = new_compositor(mode(12, 6), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(6, 6, RED));
    let glass = c.add_window(Point::ORIGIN, clear(12, 6));
    assert!(c.set_backdrop_blur(glass, radius));
    c.composite();
    c
}

#[test]
fn a_blurred_window_spreads_the_backdrop_behind_it() {
    let plain = frosted_edge(0);
    assert_eq!(frame_pixel(&plain, 5, 3), [255, 0, 0, 255], "hard edge");
    assert_eq!(frame_pixel(&plain, 6, 3), [0, 0, 255, 255], "hard edge");

    let frosted = frosted_edge(2);
    for x in [5, 6] {
        let [r, _, b, a] = frame_pixel(&frosted, x, 3);
        assert!(
            r > 0 && r < 255 && b > 0 && b < 255,
            "column {x} mixes both sides of the edge, got {r},{b}"
        );
        assert_eq!(a, 255, "the screen stays opaque");
    }
    assert_eq!(
        frame_pixel(&frosted, 0, 3),
        [255, 0, 0, 255],
        "a clamped edge keeps the far side of the window pure"
    );
    assert_eq!(frame_pixel(&frosted, 11, 3), [0, 0, 255, 255]);
}

#[test]
fn a_zero_radius_blur_is_a_no_op() {
    let mut unset = new_compositor(mode(12, 6), BLUE).expect("compositor");
    unset.add_window(Point::ORIGIN, opaque(6, 6, RED));
    unset.add_window(Point::ORIGIN, clear(12, 6));
    unset.composite();

    assert_eq!(
        frosted_edge(0).frame(),
        unset.frame(),
        "radius 0 composites exactly as a window that never asked to frost"
    );
}

#[test]
fn the_blur_is_confined_to_the_window_rectangle() {
    let mut plain = new_compositor(mode(12, 6), BLUE).expect("compositor");
    plain.add_window(Point::ORIGIN, opaque(6, 6, RED));
    plain.add_window(Point::new(4, 1), clear(4, 4));
    plain.composite();

    let mut frosted = new_compositor(mode(12, 6), BLUE).expect("compositor");
    frosted.add_window(Point::ORIGIN, opaque(6, 6, RED));
    let glass = frosted.add_window(Point::new(4, 1), clear(4, 4));
    assert!(frosted.set_backdrop_blur(glass, 2));
    frosted.composite();

    for y in 0..6 {
        for x in 0..12 {
            let inside = (4..8).contains(&x) && (1..5).contains(&y);
            let plain_pixel = frame_pixel(&plain, x, y);
            let frosted_pixel = frame_pixel(&frosted, x, y);
            if !inside {
                assert_eq!(
                    frosted_pixel, plain_pixel,
                    "({x},{y}) is outside the window and must be untouched"
                );
            }
        }
    }
    let [r, _, b, _] = frame_pixel(&frosted, 5, 2);
    assert!(r > 0 && b > 0, "inside the window the edge is spread");
}

/// Fill a window's whole content with `color` and report all of it as changed:
/// the largest damage an application can present.
fn repaint_all(color: Color) -> impl FnOnce(&mut Surface) -> (bool, Rect) {
    move |content| {
        content.fill(color);
        let damage = Rect::new(0, 0, content.width(), content.height());
        (true, damage)
    }
}

/// Paint one green pixel into a window's content and report exactly that
/// pixel as changed: the smallest damage an application can present.
fn paint_dot(content: &mut Surface) -> (bool, Rect) {
    content.set(1, 4, GREEN.premultiply());
    (true, Rect::new(1, 4, 1, 1))
}

/// A 16×8 screen with a three-column opaque block at `x` behind a
/// transparent blurred window covering columns 4..12, composited from
/// scratch. The block is narrower than the blur's reach, so where it sits
/// changes the frosted pixels right across the window.
///
/// `dot` paints [`paint_dot`] into the block *before* that first
/// whole-screen composite, so a caller can compare an incremental repaint
/// against a from-scratch one of the very same scene.
fn frosted_block(x: i32, dot: bool) -> (Compositor, WindowId) {
    let mut c = new_compositor(mode(16, 8), BLUE).expect("compositor");
    let block = c.add_window(Point::new(x, 0), opaque(3, 8, RED));
    let glass = c.add_window(Point::new(4, 0), clear(8, 8));
    assert!(c.set_backdrop_blur(glass, 3));
    if dot {
        assert_eq!(present_content(&mut c, block, paint_dot), Some(true));
    }
    c.composite();
    (c, block)
}

#[test]
fn a_change_behind_a_blurred_window_repaints_all_of_it() {
    let (mut moved, block) = frosted_block(2, false);
    // The move damages only the block's old and new rectangles — a strip
    // narrower than the frosted window — so the frame can only match a
    // from-scratch composite if the whole window was refrosted.
    assert!(moved.move_window(block, Point::new(6, 0)));
    assert!(!composite_checked(&mut moved).is_empty());

    let (fresh, _) = frosted_block(6, false);
    assert_eq!(
        moved.frame(),
        fresh.frame(),
        "a partial repaint of a blurred window matches a whole-screen one"
    );
}

#[test]
fn a_frosted_window_repaints_whole_however_little_is_damaged() {
    let (mut c, block) = frosted_block(6, false);
    // A single presented pixel behind the frosting: the damage widens to
    // the frosted window's whole rectangle, so every row of it is
    // recomposited and the result matches a from-scratch composite.
    assert_eq!(present_content(&mut c, block, paint_dot), Some(true));
    let repainted = composite_checked(&mut c);
    for y in 0..8 {
        assert!(
            repainted.contains(Point::new(4, y)) && repainted.contains(Point::new(11, y)),
            "row {y} of the frosted window was recomposited end to end"
        );
    }

    let (fresh, _) = frosted_block(6, true);
    assert_eq!(
        c.frame(),
        fresh.frame(),
        "one damaged pixel refrosts the window exactly as a full composite"
    );
}

#[test]
fn damage_beside_a_frosted_window_is_not_swallowed_by_it() {
    let (mut c, block) = frosted_block(6, false);
    // The block sits under the frosted window, so its repaint promotes the
    // window whole; the taskbar-like strip in the far corner is unrelated.
    let strip = c.add_window(Point::new(14, 6), opaque(2, 2, GREEN));
    assert!(!composite_checked(&mut c).is_empty());
    assert_eq!(present_content(&mut c, block, paint_dot), Some(true));
    assert!(c.move_window(strip, Point::new(14, 4)));

    let repainted = composite_checked(&mut c);
    for y in 0..8 {
        assert!(
            repainted.contains(Point::new(4, y)) && repainted.contains(Point::new(11, y)),
            "row {y} of the frosted window was recomposited end to end"
        );
    }
    // Growing the strip's damage to reach the frosted window would have
    // recomposited the columns between them too.
    let touched: u32 = repainted
        .rects()
        .iter()
        .map(|r| r.width * r.height)
        .sum::<u32>();
    assert_eq!(
        touched,
        8 * 8 + 2 * 2 + 2 * 2,
        "only the window and the strip's two positions were recomposed"
    );
}

#[test]
fn two_overlapping_frosted_windows_recompose_as_one_rectangle() {
    let mut c = new_compositor(mode(20, 10), BLUE).expect("compositor");
    let block = c.add_window(Point::ORIGIN, opaque(3, 10, RED));
    let left = c.add_window(Point::ORIGIN, clear(8, 10));
    let right = c.add_window(Point::new(6, 0), clear(8, 10));
    assert!(c.set_backdrop_blur(left, 2));
    assert!(c.set_backdrop_blur(right, 2));
    c.composite();

    // Each frosted window reads what the other wrote, so damage touching
    // either must recompose both at once or the overlap seams.
    assert_eq!(present_content(&mut c, block, paint_dot), Some(true));
    let repainted = composite_checked(&mut c);
    assert_eq!(repainted.rects(), &[Rect::new(0, 0, 14, 10)]);
}

#[test]
fn the_blur_radius_is_a_logical_length() {
    let mut coarse = new_compositor(mode(12, 6), BLUE).expect("compositor");
    coarse.add_window(Point::ORIGIN, opaque(6, 6, RED));
    let glass = coarse.add_window(Point::ORIGIN, clear(12, 6));
    assert!(coarse.set_backdrop_blur(glass, 1));
    assert!(coarse.set_scale(Scale::from_percent(300).expect("scale")));
    coarse.composite();

    // At 100% a one-pixel radius cannot reach column 8; tripled by the
    // output's density it does.
    let [r, _, _, _] = frame_pixel(&coarse, 8, 3);
    assert!(r > 0, "the physical radius follows the output scale");
    assert_eq!(
        frame_pixel(&frosted_edge(1), 8, 3),
        [0, 0, 255, 255],
        "at 100% the same logical radius stays clear of column 8"
    );
}

#[test]
fn a_rounded_frosted_window_leaves_its_corners_alone() {
    let mut plain = new_compositor(mode(20, 20), BLUE).expect("compositor");
    plain.add_window(Point::ORIGIN, opaque(10, 20, RED));
    plain.add_window(Point::ORIGIN, clear(20, 20));
    plain.composite();

    let mut frosted = new_compositor(mode(20, 20), BLUE).expect("compositor");
    frosted.add_window(Point::ORIGIN, opaque(10, 20, RED));
    let glass = frosted.add_window(Point::ORIGIN, clear(20, 20));
    assert!(frosted.set_corners(glass, Corners::Rounded { radius: 8 }));
    assert!(frosted.set_backdrop_blur(glass, 3));
    frosted.composite();

    assert_eq!(
        frame_pixel(&frosted, 0, 0),
        frame_pixel(&plain, 0, 0),
        "the corner the window does not cover keeps its unfrosted pixel"
    );
    let [r, _, b, _] = frame_pixel(&frosted, 10, 10);
    assert!(r > 0 && b > 0, "the covered centre is frosted");
}

#[test]
fn an_unknown_or_hidden_window_asks_for_no_blur() {
    let mut c = new_compositor(mode(4, 4), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(2, 2, RED));
    assert!(!c.has_backdrop_blur());
    assert!(c.set_backdrop_blur(id, 4));
    assert!(c.has_backdrop_blur());
    assert!(c.set_visible(id, false));
    assert!(
        !c.has_backdrop_blur(),
        "a hidden window frosts nothing, so the hardware path stays open"
    );
    assert!(c.remove(id));
    assert!(
        !c.set_backdrop_blur(id, 4),
        "an unknown window fails closed"
    );
}

#[test]
fn accelerated_present_falls_back_for_a_backdrop_blur() {
    let mut c = new_compositor(mode(8, 8), BLUE).expect("compositor");
    let id = c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    assert!(c.set_backdrop_blur(id, 3));
    let mut display = MockAccel::new(mode(8, 8), generous_caps());

    c.present_accelerated(&mut display).expect("present");
    assert!(
        display.layers.is_empty(),
        "a hardware layer cannot sample what is behind it"
    );
    assert_eq!(
        display.software_frame.len(),
        8 * 8 * 4,
        "software frame sent"
    );

    // Dropping the blur puts the scene back within the engine's reach.
    assert!(c.set_backdrop_blur(id, 0));
    let mut display = MockAccel::new(mode(8, 8), generous_caps());
    c.present_accelerated(&mut display).expect("present");
    assert_eq!(display.layers.len(), 2, "background + window");
    assert!(display.software_frame.is_empty());
}

// ---- root-viewport scrollbars ----------------------------------------

use crate::{RootViewport, ScrollModel, ScrollOrientation, ScrollPolicy, ScrollRange};

fn scrolled(dx: i32, dy: i32) -> InputEvent {
    InputEvent::PointerScrolled { dx, dy }
}

/// A window with a vertical root viewport: 1000 units of content in a
/// 100-unit viewport, 10-unit lines, 100-unit pages, 14px breadth, 24px
/// minimum thumb.
fn with_vertical_viewport(c: &mut Compositor) -> WindowId {
    let id = c.add_window(Point::ORIGIN, opaque(100, 100, RED));
    let viewport = RootViewport::new(ScrollPolicy::ReservedGutter, 14, 24)
        .with_vertical(ScrollModel::new(ScrollRange::new(1000, 100, 0), 10, 100));
    assert!(c.set_root_viewport(id, viewport));
    id
}

fn vertical_offset(c: &Compositor, id: WindowId) -> u64 {
    c.root_viewport(id)
        .and_then(RootViewport::vertical)
        .expect("vertical bar")
        .offset()
}

#[test]
fn wheel_scrolls_the_viewport_under_the_pointer() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = with_vertical_viewport(&mut c);
    let mut router = InputRouter::new();

    // Pointer over the client: a wheel tick moves one line step per tick.
    router.handle(moved(10, 10), &mut c);
    assert_eq!(
        router.handle(scrolled(0, 3), &mut c),
        InputResponse::Scrolled { window: id }
    );
    assert_eq!(vertical_offset(&c, id), 30);

    // Pointer off the window: the wheel has no viewport to scroll.
    router.handle(moved(150, 150), &mut c);
    assert_eq!(
        router.handle(scrolled(0, 5), &mut c),
        InputResponse::Ignored
    );
    assert_eq!(vertical_offset(&c, id), 30);
}

#[test]
fn wheel_over_a_window_without_a_root_viewport_is_forwarded_to_the_app() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    // A plain window that owns its own content scrolling: no root viewport.
    let id = c.add_window(Point::ORIGIN, opaque(100, 100, RED));
    let mut router = InputRouter::new();

    // A wheel over it consumes no furniture; the ticks belong to the app,
    // reported verbatim (both axes, signed) for the session to forward.
    router.handle(moved(10, 10), &mut c);
    assert_eq!(
        router.handle(scrolled(-2, 3), &mut c),
        InputResponse::AppScroll {
            window: id,
            dx: -2,
            dy: 3,
        }
    );

    // Off the window there is nothing to forward.
    router.handle(moved(150, 150), &mut c);
    assert_eq!(
        router.handle(scrolled(0, 5), &mut c),
        InputResponse::Ignored
    );
}

#[test]
fn furniture_press_is_not_delivered_to_the_client() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = with_vertical_viewport(&mut c);
    let mut router = InputRouter::new();

    // A press in the reserved vertical gutter (x in [86, 100)) is furniture,
    // never an Activated delivered to the client.
    router.handle(moved(93, 5), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::FurniturePressed { window: id }
    );
    // But the press still focused the window (it is the window's furniture).
    assert_eq!(router.focused(), Some(id));

    // A press in the client area is a normal activation.
    router.handle(moved(10, 10), &mut c);
    assert!(matches!(
        router.handle(press_primary(), &mut c),
        InputResponse::Activated { window, .. } if window == id
    ));
}

#[test]
fn thumb_drag_captures_tracks_and_releases() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = with_vertical_viewport(&mut c);
    let mut router = InputRouter::new();

    // Grab the thumb near its top (offset 0 → thumb starts at 0).
    router.handle(moved(93, 5), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::FurniturePressed { window: id }
    );
    assert!(router.is_scrolling());
    // Grabbing does not move the content.
    assert_eq!(vertical_offset(&c, id), 0);

    // Dragging down scrolls forward, tracking the pointer.
    assert_eq!(
        router.handle(moved(93, 45), &mut c),
        InputResponse::Scrolled { window: id }
    );
    let dragged = vertical_offset(&c, id);
    assert!(dragged > 0, "drag moved the offset forward");

    // Release ends the capture; a later move no longer scrolls.
    router.handle(release_primary(), &mut c);
    assert!(!router.is_scrolling());
    assert_eq!(router.handle(moved(93, 80), &mut c), InputResponse::Ignored);
    assert_eq!(vertical_offset(&c, id), dragged, "no scroll after release");
}

#[test]
fn content_shrinking_mid_drag_reclamps_the_offset() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = with_vertical_viewport(&mut c);
    let mut router = InputRouter::new();

    router.handle(moved(93, 5), &mut c);
    router.handle(press_primary(), &mut c);
    router.handle(moved(93, 45), &mut c);
    assert!(vertical_offset(&c, id) > 100);

    // Content shrinks under the live drag: the viewport re-expresses its
    // range, and the next drag move produces a valid, clamped offset.
    c.scroll_root(id, |vp| vp.resize(ScrollOrientation::Vertical, 200, 100));
    router.handle(moved(93, 90), &mut c);
    assert!(
        vertical_offset(&c, id) <= 100,
        "offset stays within the new 200-100 range"
    );
}

#[test]
fn track_press_below_the_thumb_pages_forward() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = with_vertical_viewport(&mut c);
    let mut router = InputRouter::new();

    // The thumb sits at the top (offset 0); a press well below it is the
    // after-thumb region and pages one page (100) forward.
    router.handle(moved(93, 80), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::FurniturePressed { window: id }
    );
    assert!(!router.is_scrolling(), "a track press does not capture");
    assert_eq!(vertical_offset(&c, id), 100);
}

#[test]
fn client_pixels_are_clipped_out_of_the_reserved_gutter() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let _ = with_vertical_viewport(&mut c);
    c.composite();
    // The client fills 0..86 with red; the reserved 14px gutter shows the
    // desktop background instead (the client cannot paint into the furniture).
    assert_eq!(frame_pixel(&c, 10, 10), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 90, 10), [0, 0, 255, 255]);
}

#[test]
fn clearing_a_root_viewport_removes_the_furniture() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = with_vertical_viewport(&mut c);
    assert!(c.root_viewport(id).is_some());

    // Clearing drops the viewport, so no furniture is composed and the client
    // reclaims the gutter.
    assert!(c.clear_root_viewport(id));
    assert!(c.root_viewport(id).is_none());

    // A clear against an unknown id is refused.
    assert!(!c.clear_root_viewport(WindowId(9_999)));
}

// ---- server-side window decorations (Stage A geometry) ---------------

/// A movable, resizable, active furniture state for a decorated test window.
fn decorated() -> WindowFurnitureState {
    WindowFurnitureState {
        activation: WindowActivationState::Active,
        size: WindowSizeState::Restored,
        movable: true,
        resizable: true,
    }
}

#[test]
fn decorating_a_window_reserves_a_band_around_the_client() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = c.add_window(Point::new(10, 10), opaque(40, 30, RED));

    // Undecorated: outer bounds are the bare content surface and the client is
    // the whole window.
    assert_eq!(c.window(id).unwrap().bounds(), Rect::new(10, 10, 40, 30));
    assert_eq!(c.window_client_rect(id), Some(Rect::new(10, 10, 40, 30)));

    assert!(c.set_window_frame(id, WindowFrame::new(decorated())));

    // The outer bounds grow to hold the band; the content keeps its own size.
    let bounds = c.window(id).unwrap().bounds();
    let client = c.window_client_rect(id).expect("decorated client");
    assert_eq!(client.width, 40);
    assert_eq!(client.height, 30);
    assert!(bounds.width > 40 && bounds.height > 30);

    // The client sits strictly inside the outer bounds on every edge, and the
    // top band (title bar) is thicker than the others.
    let left = client.left() - bounds.left();
    let right = bounds.right() - client.right();
    let top = client.top() - bounds.top();
    let bottom = bounds.bottom() - client.bottom();
    assert!(left > 0 && right > 0 && top > 0 && bottom > 0);
    assert!(top > bottom, "the title band is the thickest edge");
    assert!(bounds.contains(client.origin));
}

#[test]
fn decorated_client_shows_content_and_the_band_shows_furniture_chrome() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = c.add_window(Point::new(10, 10), opaque(40, 30, RED));
    assert!(c.set_window_frame(id, WindowFrame::new(decorated())));
    let bounds = c.window(id).unwrap().bounds();
    let client = c.window_client_rect(id).expect("client");
    let rim_color = c.theme().palette().frame.to_array();
    let surface = c.theme().palette().surface.to_array();
    c.composite();

    // A pixel inside the client shows the application content.
    let cx = u32::try_from(client.left() + 2).unwrap();
    let cy = u32::try_from(client.top() + 2).unwrap();
    assert_eq!(frame_pixel(&c, cx, cy), [255, 0, 0, 255]);

    // Stage B paints the furniture in the reserved band, not the desktop
    // background: the outer top-edge rim shows the frame colour...
    let rim_x = u32::try_from(bounds.left() + i32::try_from(bounds.width / 2).unwrap()).unwrap();
    let rim_y = u32::try_from(bounds.top()).unwrap();
    assert_eq!(frame_pixel(&c, rim_x, rim_y), rim_color);
    assert_ne!(rim_color, [0, 0, 255, 255], "chrome is not the background");

    // ...and the title-bar interior above the client shows the window surface.
    let by = u32::try_from(client.top() - 1).unwrap();
    assert_eq!(frame_pixel(&c, cx, by), surface);
}

#[test]
fn clearing_the_frame_restores_the_bare_bounds() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = c.add_window(Point::new(10, 10), opaque(40, 30, RED));
    assert!(c.set_window_frame(id, WindowFrame::new(decorated())));
    assert!(c.window(id).unwrap().bounds().width > 40);

    assert!(c.clear_window_frame(id));
    assert_eq!(c.window(id).unwrap().bounds(), Rect::new(10, 10, 40, 30));
    assert_eq!(c.window_client_rect(id), Some(Rect::new(10, 10, 40, 30)));
    assert!(c.window_frame(id).is_none());

    // Operations against an unknown id are refused.
    assert!(!c.set_window_frame(WindowId(9_999), WindowFrame::new(decorated())));
    assert!(!c.clear_window_frame(WindowId(9_999)));
    assert!(c.window_client_rect(WindowId(9_999)).is_none());
}

#[test]
fn rescaling_grows_the_reserved_band() {
    let mut c = new_compositor(mode(400, 400), BLUE).expect("compositor");
    let id = c.add_window(Point::new(10, 10), opaque(40, 30, RED));
    assert!(c.set_window_frame(id, WindowFrame::new(decorated())));
    let before = c.window(id).unwrap().bounds();

    assert!(c.set_scale(Scale::from_percent(200).expect("scale")));
    let after = c.window(id).unwrap().bounds();

    // The band scales with density, so the outer bounds grow while the client
    // content keeps its pixel size.
    assert!(after.width > before.width);
    assert!(after.height > before.height);
    assert_eq!(c.window_client_rect(id).unwrap().width, 40);
}

#[test]
fn switching_theme_is_reported_and_keeps_decorated_windows() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = c.add_window(Point::new(10, 10), opaque(40, 30, RED));
    assert!(c.set_window_frame(id, WindowFrame::new(decorated())));

    // Switching to a different theme reports the change and leaves the window
    // decorated; re-applying the same theme is a no-op.
    assert!(c.set_theme(Theme::light()));
    assert!(c.window_frame(id).is_some());
    assert!(!c.set_theme(Theme::light()));
}

#[test]
fn an_undecorated_window_keeps_its_surface_bounds() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let id = c.add_window(Point::new(5, 7), opaque(40, 30, RED));
    // No frame: bounds and client are the bare surface, unchanged by this work.
    assert_eq!(c.window(id).unwrap().bounds(), Rect::new(5, 7, 40, 30));
    assert_eq!(c.window_client_rect(id), Some(Rect::new(5, 7, 40, 30)));
    assert!(c.window_frame(id).is_none());
}

// ---- server-side window decorations (Stage B rendering) --------------

/// The centre point of a rectangle, in screen coordinates.
fn centre(r: Rect) -> Point {
    Point::new(
        r.left() + i32::try_from(r.width / 2).unwrap(),
        r.top() + i32::try_from(r.height / 2).unwrap(),
    )
}

/// A copy of `base` with a different [`Contrast`] policy (same palette,
/// metrics, and motion), so a test can render the furniture under high
/// contrast without a second built-in theme.
fn with_contrast(base: &Theme, contrast: Contrast) -> Theme {
    Theme::new(
        base.id(),
        base.name(),
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        contrast,
    )
}

/// A copy of `base` with reduced motion enabled; everything else is identical,
/// so a reduced-motion render must be pixel-identical to the full-motion one
/// (the furniture is animation-free).
fn with_reduced_motion(base: &Theme) -> Theme {
    Theme::new(
        base.id(),
        base.name(),
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion().with_reduced_motion(true),
        base.density(),
        base.contrast(),
    )
}

/// A compositor with one decorated window whose content is wide enough to hold
/// a full title bar (identity text plus the four command controls) and a
/// resize grabber, so the furniture renders as it would on a real desktop.
fn decorated_compositor() -> (Compositor, WindowId) {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = c.add_window(Point::new(20, 20), opaque(240, 150, RED));
    assert!(c.set_window_frame(id, WindowFrame::new(decorated())));
    (c, id)
}

#[test]
fn the_frame_rim_is_one_quiet_tone_at_either_activation() {
    let (mut active, id) = decorated_compositor();
    assert!(active.set_window_title(id, "Documents"));
    let quiet = active.theme().palette().frame.to_array();
    active.composite();

    let (mut inactive, other) = decorated_compositor();
    assert!(inactive.set_window_title(other, "Documents"));
    assert!(inactive.set_active_frame(other, false));
    inactive.composite();

    let bounds = active.window(id).unwrap().bounds();
    let rim_x = u32::try_from(centre(bounds).x).unwrap();
    let rim_y = u32::try_from(bounds.top()).unwrap();

    // The rim is the one quiet neutral either way: a window's edge does not
    // change when focus moves elsewhere.
    assert_eq!(frame_pixel(&active, rim_x, rim_y), quiet);
    assert_eq!(frame_pixel(&inactive, rim_x, rim_y), quiet);

    // Focus stays visible all the same — the title bar dims its text — so the
    // two frames are not identical.
    assert_ne!(
        active.frame(),
        inactive.frame(),
        "the title bar must still show which window holds focus"
    );

    // Toggling activation leaves the rim exactly as it was, both ways.
    assert!(active.set_active_frame(id, false));
    active.composite();
    assert_eq!(frame_pixel(&active, rim_x, rim_y), quiet);
    assert!(active.set_active_frame(id, true));
    active.composite();
    assert_eq!(frame_pixel(&active, rim_x, rim_y), quiet);

    // An undecorated or unknown window has no frame to activate.
    let plain = active.add_window(Point::new(150, 150), opaque(10, 10, RED));
    assert!(!active.set_active_frame(plain, false));
    assert!(!active.set_active_frame(WindowId(9_999), false));
}

#[test]
fn a_focus_flip_repaints_only_the_furniture() {
    let (mut c, id) = decorated_compositor();
    c.composite();
    assert!(!c.has_damage(), "composite clears damage");

    let client = c.window_client_rect(id).unwrap();
    let bounds = c.window(id).unwrap().bounds();

    // Flipping focus repaints the furniture and marks it dirty...
    assert!(c.set_active_frame(id, false));
    assert!(c.has_damage());

    // ...but the client interior is never in the damage — a focus change does
    // not touch the application content, so it is not recomposited.
    assert!(!c.damage_covers(centre(client)));
    // The title rim, by contrast, is dirty.
    assert!(c.damage_covers(Point::new(centre(bounds).x, bounds.top())));

    // The furniture bands reach into the client only where the rim's own curve
    // does: a corner arc is drawn as frame, and everything further in than the
    // radius belongs to the client alone (the separate furniture paint/hit map
    // the design language requires).
    let radius = c
        .scale()
        .scale_length(c.theme().metrics().window_corner_radius);
    let interior = Rect::new(
        client.left().saturating_add_unsigned(radius),
        client.top().saturating_add_unsigned(radius),
        client.width.saturating_sub(radius.saturating_mul(2)),
        client.height.saturating_sub(radius.saturating_mul(2)),
    );
    assert!(
        !interior.is_empty(),
        "the test window must have an interior"
    );
    for band in c.window(id).unwrap().furniture_bands() {
        assert!(band.intersection(&interior).is_empty());
    }
}

#[test]
fn a_decorated_windows_content_cannot_square_off_its_rounded_corner() {
    // What the window composites to is exactly the shape its rim traces: a
    // pixel the shape does not reach shows the desktop, and every pixel it does
    // reach is drawn. The application's own rows are square, so without the
    // clip they reached the bottom corners and covered the curve.
    let (mut c, id) = decorated_compositor();
    c.composite();
    let bounds = c.window(id).expect("window").bounds();
    let shape = c
        .window(id)
        .expect("window")
        .shape()
        .expect("a decorated window is rounded");
    let desktop = [BLUE.r, BLUE.g, BLUE.b, 255];
    for ly in 0..bounds.height {
        for lx in 0..bounds.width {
            let px = frame_pixel(
                &c,
                bounds.left().cast_unsigned() + lx,
                bounds.top().cast_unsigned() + ly,
            );
            assert_eq!(
                px != desktop,
                shape.coverage(lx, ly) > 0,
                "({lx}, {ly}) is not the shape the rim traces"
            );
        }
    }
}

#[test]
fn clipping_the_client_to_the_curve_costs_it_nothing_else() {
    // The clip takes the corner arcs and nothing more: every client pixel
    // further in than the radius is still the application's own, whatever the
    // theme's band and radius are.
    for theme in [Theme::dark(), Theme::light()] {
        let (mut c, id) = decorated_compositor();
        c.set_theme(theme.clone());
        assert_eq!(c.theme().id(), theme.id(), "the theme is in effect");
        c.composite();
        let client = c.window_client_rect(id).expect("client rect");
        for y in client.top()..client.bottom() {
            for x in client.left()..client.right() {
                if in_a_client_corner(&c, client, Point::new(x, y)) {
                    continue;
                }
                assert_eq!(
                    frame_pixel(&c, x.cast_unsigned(), y.cast_unsigned()),
                    [RED.r, RED.g, RED.b, 255],
                    "the client's own pixel at ({x}, {y}) must survive the clip"
                );
            }
        }
    }
}

#[test]
fn a_decorated_frosted_window_leaves_the_corner_outside_its_rim_alone() {
    // The frost is confined to the shape the rim traces, not to the window's
    // rectangle, so a corner the window does not cover keeps the desktop it
    // had. The backdrop changes colour under the corner, so a blur that
    // reached it could not go unnoticed.
    let scene = |blur: u16| {
        let mut c = new_compositor(mode(64, 64), BLUE).expect("compositor");
        c.add_window(Point::ORIGIN, opaque(30, 64, RED));
        let glass = c.add_window(Point::new(28, 0), clear(30, 40));
        assert!(c.set_window_frame(glass, WindowFrame::new(decorated())));
        if blur > 0 {
            assert!(c.set_backdrop_blur(glass, blur));
        }
        c.composite();
        c
    };
    let plain = scene(0);
    let frosted = scene(3);
    assert_eq!(
        frame_pixel(&frosted, 28, 0),
        frame_pixel(&plain, 28, 0),
        "the corner the rim curves away from keeps its unfrosted pixel"
    );
    assert_ne!(
        frame_pixel(&frosted, 30, 30),
        frame_pixel(&plain, 30, 30),
        "the covered client is frosted"
    );
}

#[test]
fn setting_a_title_repaints_only_the_title_band() {
    let (mut c, id) = decorated_compositor();
    c.composite();
    assert!(!c.has_damage());

    assert!(c.set_window_title(id, "Documents"));
    assert!(c.has_damage());

    let client = c.window_client_rect(id).unwrap();
    let bounds = c.window(id).unwrap().bounds();
    // The client and the bottom edge are untouched; only the top title band is
    // dirty.
    assert!(!c.damage_covers(centre(client)));
    assert!(!c.damage_covers(Point::new(centre(bounds).x, bounds.bottom() - 1)));
    assert!(c.damage_covers(Point::new(centre(bounds).x, bounds.top())));

    // Refused for an undecorated or unknown window.
    let plain = c.add_window(Point::new(150, 150), opaque(10, 10, RED));
    assert!(!c.set_window_title(plain, "x"));
    assert!(!c.set_window_title(WindowId(9_999), "x"));
}

#[test]
fn the_window_title_is_rendered_in_the_title_bar() {
    // Two identical decorated windows differing only in their title must
    // composite to different frames — the title is drawn, not merely stored.
    let (mut blank, _) = decorated_compositor();
    blank.composite();

    let (mut titled, id) = decorated_compositor();
    assert!(titled.set_window_title(id, "TAIRiX Files"));
    titled.composite();

    assert_ne!(
        blank.frame(),
        titled.frame(),
        "the title text changes the rendered title bar"
    );
}

#[test]
fn setting_an_identity_repaints_only_the_title_band() {
    let (mut c, id) = decorated_compositor();
    assert!(c.set_window_title(id, "Documents"));
    c.composite();
    assert!(!c.has_damage());

    assert!(c.set_window_identity(id, IconKind::AppBundle, None));
    assert!(c.has_damage());

    let client = c.window_client_rect(id).unwrap();
    let bounds = c.window(id).unwrap().bounds();
    assert!(!c.damage_covers(centre(client)));
    assert!(!c.damage_covers(Point::new(centre(bounds).x, bounds.bottom() - 1)));
    assert!(c.damage_covers(Point::new(centre(bounds).x, bounds.top())));

    // Refused for an undecorated or unknown window, which therefore has no
    // slot side to report either.
    let plain = c.add_window(Point::new(150, 150), opaque(10, 10, RED));
    assert!(!c.set_window_identity(plain, IconKind::AppBundle, None));
    assert!(!c.set_window_identity(WindowId(9_999), IconKind::AppBundle, None));
    assert_eq!(c.window_title_icon_side(plain), None);
    assert_eq!(c.window_title_icon_side(WindowId(9_999)), None);
}

#[test]
fn the_owning_applications_icon_is_drawn_in_the_title_bar() {
    // Two identical titled windows differing only in whether they carry an
    // identity must composite to different frames: the icon is drawn, and its
    // artwork is drawn in place of the built-in glyph.
    let (mut bare, bare_id) = decorated_compositor();
    assert!(bare.set_window_title(bare_id, "Documents"));
    bare.composite();

    let (mut glyphed, id) = decorated_compositor();
    assert!(glyphed.set_window_title(id, "Documents"));
    let side = glyphed
        .window_title_icon_side(id)
        .expect("a decorated window reports its slot side");
    assert!(side > 0);
    assert!(glyphed.set_window_identity(id, IconKind::AppBundle, None));
    glyphed.composite();
    assert_ne!(
        bare.frame(),
        glyphed.frame(),
        "an identity with no artwork still draws its built-in glyph"
    );

    let (mut arted, other) = decorated_compositor();
    assert!(arted.set_window_title(other, "Documents"));
    assert!(arted.set_window_identity(other, IconKind::AppBundle, Some(opaque(side, side, GREEN))));
    arted.composite();
    assert_ne!(
        glyphed.frame(),
        arted.frame(),
        "the owner's artwork replaces the glyph"
    );
}

#[test]
fn setting_an_identity_evicts_only_that_windows_chrome() {
    let (mut c, first) = decorated_compositor();
    let second = c.add_window(Point::new(120, 120), opaque(60, 40, RED));
    assert!(c.set_window_frame(second, WindowFrame::new(decorated())));
    c.composite();
    assert!(c.chrome_resident(first));
    assert!(c.chrome_resident(second));

    assert!(c.set_window_identity(second, IconKind::AppBundle, None));
    assert!(
        c.chrome_resident(first),
        "the sibling window's furniture is still valid"
    );
    assert!(!c.chrome_resident(second));
}

#[test]
fn the_light_theme_draws_the_furniture_chrome() {
    let (mut c, id) = decorated_compositor();
    assert!(c.set_theme(Theme::light()));
    let bounds = c.window(id).unwrap().bounds();
    let client = c.window_client_rect(id).unwrap();
    let rim_color = c.theme().palette().frame.to_array();
    let surface = c.theme().palette().surface.to_array();
    let desktop = c.theme().palette().desktop.to_array();
    c.composite();

    // The light theme paints its own rim and title-bar surface, distinct from
    // the desktop background.
    let rim = Point::new(centre(bounds).x, bounds.top());
    assert_eq!(
        frame_pixel(
            &c,
            u32::try_from(rim.x).unwrap(),
            u32::try_from(rim.y).unwrap()
        ),
        rim_color
    );
    assert_ne!(rim_color, desktop);
    let by = u32::try_from(client.top() - 1).unwrap();
    let cx = u32::try_from(client.left() + 2).unwrap();
    assert_eq!(frame_pixel(&c, cx, by), surface);
    // The client still shows its content.
    let cc = centre(client);
    assert_eq!(
        frame_pixel(
            &c,
            u32::try_from(cc.x).unwrap(),
            u32::try_from(cc.y).unwrap()
        ),
        [255, 0, 0, 255]
    );
}

#[test]
fn reduced_motion_renders_furniture_identically() {
    // The furniture is animation-free, so a reduced-motion theme must produce
    // the exact same pixels as the full-motion theme (reduced-motion correct
    // by construction).
    let (mut full, _) = decorated_compositor();
    full.composite();

    let (mut reduced, _) = decorated_compositor();
    assert!(reduced.set_theme(with_reduced_motion(&Theme::dark())));
    reduced.composite();

    assert_eq!(full.frame(), reduced.frame());
}

#[test]
fn high_contrast_thickens_the_furniture_glyphs() {
    // High contrast keeps the same palette but thickens the command-glyph and
    // grip strokes, so the rendered furniture differs from normal contrast.
    let (mut normal, _) = decorated_compositor();
    normal.composite();

    let (mut heavy, id) = decorated_compositor();
    assert!(heavy.set_theme(with_contrast(&Theme::dark(), Contrast::High)));
    heavy.composite();

    assert_ne!(
        normal.frame(),
        heavy.frame(),
        "high contrast changes the glyph rendering"
    );

    // The chrome is still correct: the rim is drawn.
    let bounds = heavy.window(id).unwrap().bounds();
    let rim = Point::new(centre(bounds).x, bounds.top());
    assert_eq!(
        frame_pixel(
            &heavy,
            u32::try_from(rim.x).unwrap(),
            u32::try_from(rim.y).unwrap()
        ),
        heavy.theme().palette().frame.to_array()
    );
}

// ---- server-side window decorations (Stage C input) ------------------

use tairix_controls::{FurniturePart, ResizeEdge};

/// The mid-height of the title band of decorated window `id`, in screen
/// coordinates — a y that lands inside the title bar (above the client).
fn title_y(c: &Compositor, id: WindowId) -> i32 {
    let bounds = c.window(id).unwrap().bounds();
    let client = c.window_client_rect(id).unwrap();
    i32::midpoint(bounds.top(), client.top())
}

/// The first screen point on the title band (scanning left→right at
/// [`title_y`]) whose [`Compositor::frame_hit`] satisfies `pred`.
fn scan_title(c: &Compositor, id: WindowId, pred: impl Fn(FurniturePart) -> bool) -> Option<Point> {
    let bounds = c.window(id).unwrap().bounds();
    let y = title_y(c, id);
    (bounds.left()..bounds.right()).find_map(|x| {
        let point = Point::new(x, y);
        match c.frame_hit(id, point) {
            Some(part) if pred(part) => Some(point),
            _ => None,
        }
    })
}

#[test]
fn frame_hit_classifies_furniture_and_never_the_client() {
    let (c, id) = decorated_compositor();
    let bounds = c.window(id).unwrap().bounds();
    let client = c.window_client_rect(id).unwrap();

    // A client interior point is the client; a point beyond the window is
    // outside; the bottom-right corner is a resize edge on a resizable window.
    assert_eq!(c.frame_hit(id, centre(client)), Some(FurniturePart::Client));
    assert_eq!(
        c.frame_hit(id, Point::new(bounds.right() + 5, bounds.bottom() + 5)),
        Some(FurniturePart::Outside)
    );
    assert_eq!(
        c.frame_hit(id, Point::new(bounds.right() - 1, bounds.bottom() - 1)),
        Some(FurniturePart::ResizeEdge(ResizeEdge::BottomRight))
    );

    // The title band carries both a draggable region and command controls, and
    // no point on it ever classifies as the client — the frame hit map keeps
    // furniture strictly separate from the application surface.
    assert!(
        scan_title(&c, id, |p| matches!(p, FurniturePart::TitleBar)).is_some(),
        "the title bar has a draggable region"
    );
    assert!(
        scan_title(&c, id, |p| matches!(p, FurniturePart::WindowControl(_))).is_some(),
        "the title bar has command controls"
    );
    let y = title_y(&c, id);
    for x in bounds.left()..bounds.right() {
        assert_ne!(
            c.frame_hit(id, Point::new(x, y)),
            Some(FurniturePart::Client),
            "no point on the title band is the client"
        );
    }

    // An unknown window has no frame hit map (fail closed).
    assert_eq!(c.frame_hit(WindowId(9_999), centre(client)), None);
}

#[test]
fn a_title_bar_drag_moves_the_window() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let start = c.window(id).unwrap().origin();
    let drag = scan_title(&c, id, |p| matches!(p, FurniturePart::TitleBar)).expect("drag region");

    router.handle(moved(drag.x, drag.y), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::FurniturePressed { window: id }
    );
    assert!(router.is_moving(), "a title-bar press begins a move-grab");

    // Motion drags the window's outer origin; the press is never the client's.
    let response = router.handle(moved(drag.x + 15, drag.y + 10), &mut c);
    assert!(matches!(response, InputResponse::Moved { window, .. } if window == id));
    assert_eq!(
        c.window(id).unwrap().origin(),
        Point::new(start.x + 15, start.y + 10)
    );

    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::MoveEnded { window: id }
    );
    assert!(!router.is_moving());
}

#[test]
fn a_title_bar_drag_keeps_a_grabbable_patch_of_the_bar_on_screen() {
    // A window may hang off any edge — that is normal on a desktop — but never
    // so far that nothing is left to drag it back by.
    let (mut c, id) = decorated_compositor();
    let screen = c.screen_rect();
    let mut router = InputRouter::new();
    let drag = scan_title(&c, id, |p| matches!(p, FurniturePart::TitleBar)).expect("drag region");
    router.handle(moved(drag.x, drag.y), &mut c);
    router.handle(press_primary(), &mut c);

    // The last pair is a pointer sample at the far end of the coordinate
    // space: the clamp must saturate rather than overflow into a window
    // parked somewhere impossible.
    for (dx, dy) in [
        (-4000, 0),
        (4000, 0),
        (0, -4000),
        (0, 4000),
        (i32::MIN / 2, i32::MAX / 2),
    ] {
        router.handle(moved(drag.x + dx, drag.y + dy), &mut c);
        let surface = c.window_drag_surface(id).expect("decorated");
        assert!(
            surface.top() >= screen.top() && surface.bottom() <= screen.bottom(),
            "the whole band stays on screen along its height ({dx},{dy})"
        );
        let visible = Rect::new(
            surface.left().max(screen.left()),
            surface.top(),
            u32::try_from(surface.right().min(screen.right()) - surface.left().max(screen.left()))
                .unwrap_or(0),
            surface.height,
        );
        assert!(
            visible.width >= surface.height.min(surface.width),
            "a patch at least as wide as the band is tall is still reachable ({dx},{dy})"
        );
        let probe = Point::new(
            i32::midpoint(visible.left(), visible.right()),
            i32::midpoint(visible.top(), visible.bottom()),
        );
        assert_eq!(
            c.frame_hit(id, probe),
            Some(FurniturePart::TitleBar),
            "and a press there would move the window ({dx},{dy})"
        );
    }
}

#[test]
fn a_title_bar_drag_still_hangs_the_window_off_an_edge() {
    // The clamp bounds the extreme, it does not glue windows to the screen:
    // dragging a window part-way off an edge must still work.
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let drag = scan_title(&c, id, |p| matches!(p, FurniturePart::TitleBar)).expect("drag region");
    router.handle(moved(drag.x, drag.y), &mut c);
    router.handle(press_primary(), &mut c);
    router.handle(moved(drag.x - 60, drag.y), &mut c);

    let bounds = c.window(id).expect("window").bounds();
    assert!(
        bounds.left() < c.screen_rect().left(),
        "the window hangs off the leading edge"
    );
    assert_eq!(bounds.left(), 20 - 60, "by exactly the pointer delta");
}

#[test]
fn hovering_a_window_command_lights_it_and_leaving_puts_it_out() {
    // Pointer motion over a decoration is the window manager's own: nothing
    // else can see it, so if the router does not hand it to the frame the
    // buttons never respond to the pointer at all.
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = titled_window(&mut c, 10, 10, 180, "Documents");
    composite_checked(&mut c);
    let mut router = InputRouter::new();

    let close = command_rect(&c, id, WindowControlKind::Close);
    let over = inside(close);
    assert_eq!(
        router.handle(moved(over.x, over.y), &mut c),
        InputResponse::Ignored,
        "a hover over furniture is no client's"
    );
    assert_eq!(
        composite_checked(&mut c).rects(),
        [close],
        "exactly the command under the pointer lit up"
    );

    // Moving along the bar puts it out again, and costs only that control.
    let drag = title_layout(&c, id).drag;
    let away = Point::new(drag.left() + 1, i32::midpoint(drag.top(), drag.bottom()));
    router.handle(moved(away.x, away.y), &mut c);
    assert_eq!(
        composite_checked(&mut c).rects(),
        [close],
        "the command the pointer left, and nothing else"
    );

    // And a sample that stays on the drag region costs nothing at all.
    router.handle(moved(away.x + 1, away.y), &mut c);
    assert!(!c.has_damage());
    assert!(c.chrome_resident(id));
}

#[test]
fn a_hover_leaving_a_window_for_another_puts_the_first_one_out() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let first = titled_window(&mut c, 10, 10, 140, "first");
    let second = titled_window(&mut c, 170, 10, 140, "second");
    composite_checked(&mut c);
    let mut router = InputRouter::new();

    let lit = command_rect(&c, first, WindowControlKind::Close);
    let over = inside(lit);
    router.handle(moved(over.x, over.y), &mut c);
    composite_checked(&mut c);

    let next = command_rect(&c, second, WindowControlKind::Close);
    let onto = inside(next);
    router.handle(moved(onto.x, onto.y), &mut c);
    let region = composite_checked(&mut c);
    assert!(
        region.rects().contains(&lit) && region.rects().contains(&next),
        "the command left goes out as the one arrived at lights up"
    );
}

#[test]
fn a_resize_grab_resizes_the_window_from_the_corner() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let before = c.window(id).unwrap().bounds();
    let corner = Point::new(before.right() - 1, before.bottom() - 1);

    router.handle(moved(corner.x, corner.y), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::FurniturePressed { window: id }
    );
    assert!(router.is_resizing(), "a corner press begins a resize-grab");

    // Dragging out grows the window's outer bounds by the pointer delta.
    let response = router.handle(moved(corner.x + 40, corner.y + 30), &mut c);
    assert!(matches!(response, InputResponse::Resized { window } if window == id));
    let grown = c.window(id).unwrap().bounds();
    assert_eq!(grown.width, before.width + 40);
    assert_eq!(grown.height, before.height + 30);

    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::ResizeEnded { window: id }
    );
    assert!(!router.is_resizing());
}

#[test]
fn a_resize_grab_clamps_where_the_title_bar_still_works_and_escape_restores() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let before = c.window(id).unwrap().bounds();
    let floor = c.window_min_outer_size(id).expect("decorated");
    let corner = Point::new(before.right() - 1, before.bottom() - 1);

    router.handle(moved(corner.x, corner.y), &mut c);
    router.handle(press_primary(), &mut c);

    // Dragging far past the top-left cannot shrink the window below the floor.
    router.handle(moved(before.left(), before.top()), &mut c);
    let shrunk = c.window(id).unwrap().bounds();
    assert!(shrunk.width < before.width && shrunk.height < before.height);
    assert_eq!((shrunk.width, shrunk.height), floor);

    // What the floor is *for*: at it the band still seats all four commands
    // side by side, inside the window, with a drag surface left between them.
    let layout = title_layout(&c, id);
    let mut previous = shrunk.left();
    for (kind, rect) in layout.controls {
        assert!(
            rect.width > 0 && rect.left() >= previous && rect.right() <= shrunk.right(),
            "{kind:?} is seated inside the window and clear of the command before it"
        );
        previous = rect.right();
    }
    assert!(
        layout.drag.width > 0 && layout.drag.height > 0,
        "and the window can still be dragged by its title bar"
    );

    // Escape cancels the gesture and restores the exact pre-drag geometry.
    assert_eq!(
        router.handle(key_pressed(Key::Named(NamedKey::Escape)), &mut c),
        InputResponse::ResizeEnded { window: id }
    );
    assert_eq!(c.window(id).unwrap().bounds(), before);
    assert!(!router.is_resizing());
}

#[test]
fn an_application_s_declared_minimum_raises_the_resize_floor() {
    // The application states the smallest client it can lay out at; below it
    // the app would either be squeezed into nonsense or fight the drag by
    // resizing itself back, which is the bounce this closes.
    let (mut c, id) = decorated_compositor();
    let furniture = c.window_min_outer_size(id).expect("decorated");
    let declared = (furniture.0 + 60, furniture.1 + 40);
    assert!(c.set_window_min_client_size(id, declared.0, declared.1));
    let raised = c.window_min_outer_size(id).expect("decorated");
    assert!(
        raised.0 > furniture.0 && raised.1 > furniture.1,
        "a minimum larger than the furniture's own raises the floor"
    );

    let mut router = InputRouter::new();
    let before = c.window(id).unwrap().bounds();
    let corner = Point::new(before.right() - 1, before.bottom() - 1);
    router.handle(moved(corner.x, corner.y), &mut c);
    router.handle(press_primary(), &mut c);
    router.handle(moved(before.left(), before.top()), &mut c);

    let shrunk = c.window(id).unwrap().bounds();
    assert_eq!((shrunk.width, shrunk.height), raised);
    let client = c.window_client_rect(id).expect("decorated");
    assert!(
        client.width >= declared.0 && client.height >= declared.1,
        "the client never shrinks below what its application declared"
    );
}

#[test]
fn a_declared_minimum_under_the_furniture_s_own_floor_cannot_lower_it() {
    // Both floors are real, and the furniture's holds even for an application
    // that asks for less: the title bar's commands must stay usable whatever
    // the application would settle for.
    let (mut c, id) = decorated_compositor();
    let furniture = c.window_min_outer_size(id).expect("decorated");
    assert!(c.set_window_min_client_size(id, 1, 1));
    assert_eq!(c.window_min_outer_size(id), Some(furniture));

    assert!(
        !c.set_window_min_client_size(WindowId(9999), 200, 200),
        "a minimum for a window the compositor does not know changes nothing"
    );
}

#[test]
fn a_resize_grab_leaves_the_clients_own_pixels_alone() {
    // The window manager resizes the frame it draws on every motion of a
    // resize-grab, while the client is told its new size once, when the drag
    // settles. Reshaping the client's buffer under it would make every
    // present in between describe a geometry the compositor had already
    // discarded — a refusal an app cannot tell from a dead session — so the
    // frame is the window manager's and the pixels stay the client's.
    let (mut c, id) = decorated_compositor();
    present_full(&mut c, id, GREEN);
    let client = c.window(id).expect("window").client_size();
    let mut router = InputRouter::new();
    let outer = c.window(id).expect("window").bounds();
    let corner = Point::new(outer.right() - 1, outer.bottom() - 1);
    router.handle(moved(corner.x, corner.y), &mut c);
    router.handle(press_primary(), &mut c);

    // Shrunk well inside the client, then grown past it again.
    for delta in [-40, -80, 20] {
        router.handle(moved(corner.x + delta, corner.y + delta), &mut c);
        let window = c.window(id).expect("window");
        assert_ne!(
            window.client_size(),
            client,
            "the frame tracks the pointer, or this proves nothing"
        );
        let content = window.content().expect("the client keeps its pixels");
        assert_eq!(
            (content.width(), content.height()),
            client,
            "the frame resized; the client's buffer did not"
        );
        assert!(
            content.pixels().iter().all(|p| *p == GREEN.premultiply()),
            "not one client pixel is disturbed by a frame resize"
        );
    }
    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::ResizeEnded { window: id },
        "the drag settles, which is when the client is told its new size"
    );
}

#[test]
fn a_present_at_a_new_size_re_establishes_the_buffer_and_repaints_the_client() {
    let mut c = new_compositor(mode(64, 64), BLUE).expect("compositor");
    let id = c.add_window(Point::new(4, 4), opaque(8, 8, RED));
    c.composite();
    // The resize has settled: the frame reserves the new client size and the
    // client re-renders at it.
    assert!(c.resize_window_client(id, 12, 12));
    c.composite();
    let client = c.window_client_rect(id).expect("client");
    assert_eq!(client, Rect::new(4, 4, 12, 12));

    let blank = c.present_window_content(id, 12, 12, |surface| {
        let blank = surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT);
        (blank, Rect::EMPTY)
    });
    assert_eq!(
        blank,
        Some(true),
        "a buffer established for a new size carries nothing over"
    );
    assert_eq!(
        composite_checked(&mut c).rects(),
        &[client],
        "every client pixel now comes from the fresh buffer, so the empty \
         rectangle the conversion reported cannot be taken at face value"
    );
    assert_eq!(
        c.window(id).expect("window").client_size(),
        (12, 12),
        "the frame and the buffer agree once the resize has settled"
    );
}

#[test]
fn a_command_control_click_emits_its_typed_action() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let control =
        scan_title(&c, id, |p| matches!(p, FurniturePart::WindowControl(_))).expect("a control");
    let kind = match c.frame_hit(id, control) {
        Some(FurniturePart::WindowControl(kind)) => kind,
        other => panic!("expected a control, found {other:?}"),
    };

    router.handle(moved(control.x, control.y), &mut c);
    assert_eq!(
        router.handle(press_primary(), &mut c),
        InputResponse::FurniturePressed { window: id }
    );
    // Releasing over the same control completes the click (a click activates on
    // release), emitting the typed command — never delivered to the client.
    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::WindowControl {
            window: id,
            control: kind,
        }
    );
}

#[test]
fn a_secondary_press_on_a_command_control_reports_the_alternate_gesture() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let control =
        scan_title(&c, id, |p| matches!(p, FurniturePart::WindowControl(_))).expect("a control");
    let kind = match c.frame_hit(id, control) {
        Some(FurniturePart::WindowControl(kind)) => kind,
        other => panic!("expected a control, found {other:?}"),
    };
    let bounds = c.window(id).expect("window").bounds();

    router.handle(moved(control.x, control.y), &mut c);
    assert_eq!(
        router.handle(press_secondary(), &mut c),
        InputResponse::WindowControlAlternate {
            window: id,
            control: kind,
        }
    );
    // The window is raised and focused as any secondary press does, but its
    // geometry and size state are untouched: no command ran.
    assert_eq!(router.focused(), Some(id));
    assert_eq!(c.window(id).expect("window").bounds(), bounds);
    assert_eq!(
        c.window(id).expect("window").size_state(),
        WindowSizeState::Restored
    );
    // A following primary click on the same control still means the command.
    router.handle(press_primary(), &mut c);
    assert_eq!(
        router.handle(release_primary(), &mut c),
        InputResponse::WindowControl {
            window: id,
            control: kind,
        }
    );
}

#[test]
fn a_secondary_press_elsewhere_on_the_frame_is_still_consumed() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let drag = scan_title(&c, id, |p| matches!(p, FurniturePart::TitleBar)).expect("a title band");

    router.handle(moved(drag.x, drag.y), &mut c);
    assert_eq!(
        router.handle(press_secondary(), &mut c),
        InputResponse::FurniturePressed { window: id }
    );
}

#[test]
fn the_keyboard_reaches_the_command_controls() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let control =
        scan_title(&c, id, |p| matches!(p, FurniturePart::WindowControl(_))).expect("a control");

    // A control press hands the frame furniture the keyboard.
    router.handle(moved(control.x, control.y), &mut c);
    router.handle(press_primary(), &mut c);
    router.handle(release_primary(), &mut c);

    // The arrow keys move focus between the controls and Enter activates the
    // focused one, so the group is fully usable without a pointer.
    assert_eq!(
        router.handle(key_pressed(Key::Named(NamedKey::Right)), &mut c),
        InputResponse::Ignored,
        "the arrow moves furniture focus and is consumed, not sent to the client"
    );
    let response = router.handle(key_pressed(Key::Named(NamedKey::Enter)), &mut c);
    assert!(matches!(
        response,
        InputResponse::WindowControl { window, .. } if window == id
    ));
}

#[test]
fn a_client_press_returns_the_keyboard_to_the_client() {
    let (mut c, id) = decorated_compositor();
    let mut router = InputRouter::new();
    let control =
        scan_title(&c, id, |p| matches!(p, FurniturePart::WindowControl(_))).expect("a control");
    let client_point = centre(c.window_client_rect(id).unwrap());

    // Take furniture keyboard focus via a control, then press the client.
    router.handle(moved(control.x, control.y), &mut c);
    router.handle(press_primary(), &mut c);
    router.handle(release_primary(), &mut c);
    router.handle(moved(client_point.x, client_point.y), &mut c);
    assert!(matches!(
        router.handle(press_primary(), &mut c),
        InputResponse::Activated { window, .. } if window == id
    ));

    // Keys now reach the client again — the furniture released the keyboard.
    assert!(matches!(
        router.handle(key_pressed(Key::Char('x')), &mut c),
        InputResponse::Key { window, .. } if window == id
    ));
}

#[test]
fn a_resizable_windows_client_matches_a_fixed_windows_client() {
    // The furniture band no longer widens for a resizable window: its client
    // is exactly the size a fixed-size window's would be for the same outer
    // bounds, so a resizable app's content is never shrunk to make room for a
    // grab border it does not visibly draw.
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let resizable = c.add_window(Point::new(20, 20), opaque(240, 150, RED));
    assert!(c.set_window_frame(resizable, WindowFrame::new(decorated())));

    let mut fixed_furniture = decorated();
    fixed_furniture.resizable = false;
    let fixed = c.add_window(Point::new(20, 20), opaque(240, 150, RED));
    assert!(c.set_window_frame(fixed, WindowFrame::new(fixed_furniture)));

    assert_eq!(
        c.window(resizable).unwrap().bounds(),
        c.window(fixed).unwrap().bounds(),
        "same outer geometry in, same outer geometry out"
    );
    assert_eq!(
        c.window_client_rect(resizable),
        c.window_client_rect(fixed),
        "a resizable window wastes no extra client space on a grab border"
    );
}

// ---- server-side window decorations (Stage D lifecycle) --------------

#[test]
fn put_to_back_sends_a_window_to_the_bottom_of_the_stack() {
    let mut c = new_compositor(mode(200, 200), BLUE).expect("compositor");
    let back = c.add_window(Point::new(0, 0), opaque(120, 120, RED));
    let front = c.add_window(Point::new(60, 60), opaque(120, 120, RED));
    // The overlap belongs to the most-recently-added (topmost) window.
    let overlap = Point::new(80, 80);
    assert_eq!(c.window_at(overlap), Some(front));

    // Lowering the front window puts it under the other one.
    assert!(c.lower(front));
    assert_eq!(c.window_at(overlap), Some(back));

    // Lowering the now-bottom window again is a no-op that still succeeds
    // (it stays at the back); an unknown id is refused.
    assert!(c.lower(front));
    assert_eq!(c.window_at(overlap), Some(back));
    assert!(!c.lower(WindowId(9_999)));
}

#[test]
fn maximize_and_restore_toggles_size_state_and_geometry() {
    let (mut c, id) = decorated_compositor();
    let work_area = c.screen_rect();
    let restored_bounds = c.window(id).unwrap().bounds();
    assert_eq!(
        c.window(id).unwrap().size_state(),
        WindowSizeState::Restored
    );

    // Maximize: the outer bounds fill the work area, the size state flips, and
    // the frame's furniture reports the maximized state (so the control now
    // offers Restore). The returned client is the inset content rectangle.
    let (state, client) = c.toggle_window_size(id, work_area).expect("maximize");
    assert_eq!(state, WindowSizeState::Maximized);
    assert_eq!(c.window(id).unwrap().bounds(), work_area);
    assert_eq!(
        c.window(id).unwrap().size_state(),
        WindowSizeState::Maximized
    );
    assert_eq!(
        c.window_frame(id).unwrap().furniture().size,
        WindowSizeState::Maximized
    );
    assert_eq!(c.window_client_rect(id), Some(client));
    assert!(client.width < work_area.width && client.height < work_area.height);

    // Restore: back to exactly the pre-maximize geometry and state.
    let (state, _) = c.toggle_window_size(id, work_area).expect("restore");
    assert_eq!(state, WindowSizeState::Restored);
    assert_eq!(c.window(id).unwrap().bounds(), restored_bounds);
    assert_eq!(
        c.window_frame(id).unwrap().furniture().size,
        WindowSizeState::Restored
    );
}

#[test]
fn size_toggle_is_refused_for_windows_that_cannot_maximize() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let work_area = c.screen_rect();

    // An unknown window.
    assert!(c.toggle_window_size(WindowId(9_999), work_area).is_none());

    // An undecorated window has no frame to size-toggle.
    let plain = c.add_window(Point::new(10, 10), opaque(40, 30, RED));
    assert!(c.toggle_window_size(plain, work_area).is_none());

    // A decorated but non-resizable window declines: maximize is disabled, so
    // its geometry never changes.
    let fixed = c.add_window(Point::new(10, 10), opaque(120, 90, RED));
    let furniture = WindowFurnitureState {
        activation: WindowActivationState::Active,
        size: WindowSizeState::Restored,
        movable: true,
        resizable: false,
    };
    assert!(c.set_window_frame(fixed, WindowFrame::new(furniture)));
    let before = c.window(fixed).unwrap().bounds();
    assert!(c.toggle_window_size(fixed, work_area).is_none());
    assert_eq!(c.window(fixed).unwrap().bounds(), before);
}

// ---- server-side window decorations (Stage E chrome strips) ----------
//
// `Window` used to retain one outer-window-sized decoration surface even
// though its client region is never sampled; it now keeps only the four
// furniture strips `Window::furniture_bands` describes. These tests pin the
// composited pixels exactly (so the split is provably invisible) and pin the
// retained-memory shape (so the split provably pays off).

/// Total pixels the four furniture bands cover for a decorated window: the
/// memory the strip-based chrome retains, since each strip is allocated at
/// exactly its band's size (a zero-extent band retains nothing).
fn retained_chrome_pixels(c: &Compositor, id: WindowId) -> u64 {
    c.window(id)
        .unwrap()
        .furniture_bands()
        .iter()
        .map(|band| u64::from(band.width) * u64::from(band.height))
        .sum()
}

#[test]
fn decorated_furniture_strips_render_pixel_exact_chrome() {
    // A titled, active, resizable decorated window: every furniture band and
    // the client are exercised in one composite, so the strip split is
    // checked against the exact pixels a single outer-sized surface would
    // have produced.
    let (mut c, id) = decorated_compositor();
    assert!(c.set_window_title(id, "Untitled"));
    c.composite();

    let bounds = c.window(id).unwrap().bounds();
    let client = c.window_client_rect(id).unwrap();
    let rim_color = c.theme().palette().frame.to_array();
    let surface = c.theme().palette().surface.to_array();
    // `decorated_compositor` clears the screen to the literal `BLUE` test
    // constant, independently of the active theme's own palette colours.
    let desktop = [0, 0, 255, 255];

    let left_x = u32::try_from(bounds.left()).unwrap();
    let right_x = u32::try_from(bounds.right() - 1).unwrap();
    let top_y = u32::try_from(bounds.top()).unwrap();
    let bottom_y = u32::try_from(bounds.bottom() - 1).unwrap();
    let mid_x = u32::try_from(centre(bounds).x).unwrap();
    let mid_y = u32::try_from(centre(client).y).unwrap();

    // Top strip: the rim colour along the outer top edge.
    assert_eq!(frame_pixel(&c, mid_x, top_y), rim_color);
    // Bottom strip: the rim colour along the outer bottom edge.
    assert_eq!(frame_pixel(&c, mid_x, bottom_y), rim_color);
    // Left and right strips: the rim colour at the outer edge, level with a
    // row that crosses the client's own vertical range — the case that now
    // samples the left strip and the right strip together.
    assert_eq!(frame_pixel(&c, left_x, mid_y), rim_color);
    assert_eq!(frame_pixel(&c, right_x, mid_y), rim_color);

    // That same row's client interior still shows the application content,
    // strictly between the two border strips.
    let content_x = u32::try_from(client.left() + 2).unwrap();
    assert_eq!(frame_pixel(&c, content_x, mid_y), [255, 0, 0, 255]);

    // The title-bar interior above the client (inside the top strip, off the
    // rim) shows the window body surface colour, proving the top strip
    // carries more than just the rim line.
    let body_y = u32::try_from(client.top() - 1).unwrap();
    assert_eq!(frame_pixel(&c, content_x, body_y), surface);

    // The rounded rim corners stay transparent: the extreme outer corner
    // (carried by the top strip) shows the desktop background straight
    // through, not the rim colour.
    assert_eq!(frame_pixel(&c, left_x, top_y), desktop);

    // The bottom-right corner draws no grip now the band is the plain frame
    // inset. Probing one corner radius in lands inside the client and clear of
    // the rounded mask, and finds the application's own content there.
    let radius = i32::try_from(
        c.scale()
            .scale_length(c.theme().metrics().window_corner_radius),
    )
    .unwrap_or(i32::MAX);
    let corner_x = u32::try_from(client.right() - radius).unwrap();
    let corner_y = u32::try_from(client.bottom() - radius).unwrap();
    assert_eq!(frame_pixel(&c, corner_x, corner_y), [255, 0, 0, 255]);
}

#[test]
fn an_undecorated_window_composites_unaffected_by_the_strip_split() {
    // A window with no frame never touches the chrome path at all; its
    // composited pixels are exactly its own content, unaffected by anything
    // furniture-related.
    let mut c = new_compositor(mode(60, 60), BLUE).expect("compositor");
    let id = c.add_window(Point::new(5, 5), opaque(30, 20, RED));
    c.composite();

    assert_eq!(frame_pixel(&c, 6, 6), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]);
    assert!(c.window(id).unwrap().frame().is_none());
}

#[test]
fn resizing_a_decorated_window_still_produces_correct_furniture() {
    let (mut c, id) = decorated_compositor();
    let rim_color = c.theme().palette().frame.to_array();
    // `decorated_compositor` clears the screen to the literal `BLUE` test
    // constant, independently of the active theme's own palette colours.
    let desktop = [0, 0, 255, 255];

    // Grow the client substantially, then re-render and re-check the same
    // furniture invariants at the new geometry: the strips are rebuilt at
    // the new outer size, not stretched from the old one.
    assert!(c.resize_window_client(id, 300, 200));
    c.composite();

    let bounds = c.window(id).unwrap().bounds();
    let client = c.window_client_rect(id).unwrap();
    assert_eq!(client.width, 300);
    assert_eq!(client.height, 200);

    let left_x = u32::try_from(bounds.left()).unwrap();
    let top_y = u32::try_from(bounds.top()).unwrap();
    let mid_x = u32::try_from(centre(bounds).x).unwrap();
    let mid_y = u32::try_from(centre(client).y).unwrap();

    assert_eq!(frame_pixel(&c, mid_x, top_y), rim_color);
    assert_eq!(frame_pixel(&c, left_x, mid_y), rim_color);
    assert_eq!(
        frame_pixel(&c, left_x, top_y),
        desktop,
        "corner still clips"
    );

    let content_x = u32::try_from(client.left() + 2).unwrap();
    assert_eq!(frame_pixel(&c, content_x, mid_y), [255, 0, 0, 255]);
}

#[test]
fn retained_chrome_scales_with_the_frame_band_not_the_window_area() {
    // Two decorated windows near the width of a 1080p panel, one short and
    // one nearly full height: a pre-split, outer-sized decoration surface
    // would have grown by the full extra window area. The strip-based
    // chrome only grows by the side borders' extra height — a small slice of
    // that once the window is wide relative to its border thickness.
    let mut short = new_compositor(mode(1920, 1080), BLUE).expect("compositor");
    let short_id = short.add_window(Point::new(0, 0), opaque(1880, 40, RED));
    assert!(short.set_window_frame(short_id, WindowFrame::new(decorated())));

    let mut tall = new_compositor(mode(1920, 1080), BLUE).expect("compositor");
    let tall_id = tall.add_window(Point::new(0, 0), opaque(1880, 1000, RED));
    assert!(tall.set_window_frame(tall_id, WindowFrame::new(decorated())));

    let short_outer = short.window(short_id).unwrap().bounds();
    let tall_outer = tall.window(tall_id).unwrap().bounds();
    let short_retained = retained_chrome_pixels(&short, short_id);
    let tall_retained = retained_chrome_pixels(&tall, tall_id);

    let outer_area = |b: Rect| u64::from(b.width) * u64::from(b.height);
    let outer_growth = outer_area(tall_outer) - outer_area(short_outer);
    let retained_growth = tall_retained - short_retained;

    assert!(
        retained_growth * 10 < outer_growth,
        "retained growth {retained_growth} should stay far below the outer-area \
         growth {outer_growth} a single outer-sized surface would have paid"
    );
    assert!(
        tall_retained * 5 < outer_area(tall_outer),
        "the tall window's retained chrome ({tall_retained}) should be a small \
         fraction of its outer area ({})",
        outer_area(tall_outer)
    );
}

/// A 1080p 32-bit output's worth of bytes: the backing the cursor cache's
/// budget is derived from in these tests, so the ceiling under test is the
/// real derivation rather than a number invented here.
const TEST_FB_BYTES: usize = 1920 * 1080 * 4;

/// The seat the cursor caches under test are charged to.
const TEST_SEAT: u64 = 1;

/// Discards audit records. These tests assert cache behaviour; the audit
/// path itself is covered where it is defined, in `lib/reclaim`.
struct SilentSink;

impl Sink for SilentSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

static TEST_SINK: SilentSink = SilentSink;

/// The gauge shared by every cursor test that does not care about
/// pressure. It is only ever reported `Normal`, so the tests may run in
/// parallel without one perturbing another; a test that *does* move the
/// band declares its own gauge instead.
static NORMAL_PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// A cursor cache at normal pressure, sized from a 1080p output.
fn test_cursor_cache() -> ReclaimCache<CursorKind, CursorImage, CursorEpoch> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    cursor_cache(TEST_SEAT, TEST_FB_BYTES, &NORMAL_PRESSURE, &TEST_SINK)
}

/// A window-furniture cache at normal pressure, sized from a 1080p output.
fn test_chrome_cache() -> ReclaimCache<WindowId, WindowChrome, ChromeEpoch> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    chrome_cache(TEST_SEAT, TEST_FB_BYTES, &NORMAL_PRESSURE, &TEST_SINK)
}

/// The frosted-backdrop cache the shipping desktop policy builds, at normal
/// pressure and sized from a 1080p output.
fn test_frost_cache() -> ReclaimCache<WindowId, FrostedBackdrop, FrostEpoch> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    frost_cache(TEST_SEAT, TEST_FB_BYTES, &NORMAL_PRESSURE, &TEST_SINK)
}

#[test]
fn a_re_shown_cursor_kind_is_rasterised_once_per_epoch() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(30, 30, RED));
    assert!(c.set_window_cursor(win, CursorKind::Text));
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());

    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    let after_first = ctrl.cache_stats().misses();

    // Moving onto the window and back re-shows the arrow: the second
    // showing must come from the cache, not a fresh rasterisation.
    router.handle(moved(20, 20), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(
        ctrl.cache_stats().misses(),
        after_first + 1,
        "only the newly shown kind may rasterise"
    );
    assert!(ctrl.cache_stats().hits() >= 1);
}

#[test]
fn a_scale_change_invalidates_every_cached_cursor() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());

    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(ctrl.cache_len(), 1);

    assert!(c.set_scale(Scale::from_percent(200).expect("scale")));
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(
        ctrl.cache_len(),
        1,
        "the new scale's image replaces the old one rather than joining it"
    );
    assert_eq!(ctrl.cache_stats().invalidations(), 1);
}

#[test]
fn mild_pressure_drops_the_cursor_cache_and_refuses_growth() {
    // A gauge private to this test: it moves the band, and the shared one
    // must stay at normal for the tests running beside it.
    static PRESSURE: ReportedPressure = ReportedPressure::unknown();
    PRESSURE.report(PressureBand::Normal);

    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(30, 30, RED));
    assert!(c.set_window_cursor(win, CursorKind::Text));
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(cursor_cache(
        TEST_SEAT,
        TEST_FB_BYTES,
        &PRESSURE,
        &TEST_SINK,
    ));

    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(ctrl.cache_len(), 1);
    assert!(ctrl.cache_bytes() > 0);

    // Mild pressure forces a disposable-UI cache to zero, ahead of any
    // clean-file or anonymous-page reclaim.
    PRESSURE.report(PressureBand::Mild);
    assert!(ctrl.trim() > 0, "mild pressure must release bytes");
    assert_eq!(ctrl.cache_len(), 0);
    assert_eq!(ctrl.cache_bytes(), 0);

    // A different shape is still drawn correctly; it is simply
    // rasterised on demand instead of retained.
    router.handle(moved(20, 20), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(ctrl.kind(), CursorKind::Text);
    assert!(c.cursor_bounds().is_some());
    assert_eq!(ctrl.cache_len(), 0, "no growth while pressure holds");
}

#[test]
fn the_cursor_cache_budget_follows_the_output_it_was_built_for() {
    // The budget is derived from the output's own frame size, so a tiny
    // panel is allowed a tiny cache and a large display a large one —
    // never one hand-picked ceiling for both. A 64x64 output's whole
    // frame is smaller than a single rasterised cursor, so nothing may
    // be retained, while the 1080p output of the other tests retains
    // normally.
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    let tiny_output_bytes = 64 * 64 * 4;
    let mut ctrl = CursorController::new(cursor_cache(
        TEST_SEAT,
        tiny_output_bytes,
        &NORMAL_PRESSURE,
        &TEST_SINK,
    ));
    NORMAL_PRESSURE.report(PressureBand::Normal);

    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c), "the cursor is still drawn");
    assert!(c.cursor_bounds().is_some());
    assert_eq!(
        ctrl.cache_len(),
        0,
        "an output too small to budget a cursor retains none"
    );
    assert!(ctrl.cache_stats().refusals() >= 1);
}

#[test]
fn teardown_releases_every_cached_cursor() {
    let mut c = new_compositor(mode(80, 80), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new(test_cursor_cache());
    router.handle(moved(70, 70), &mut c);
    assert!(ctrl.refresh(&router, &mut c));
    assert_eq!(ctrl.cache_len(), 1);

    ctrl.teardown();
    assert_eq!(ctrl.cache_len(), 0);
    assert_eq!(ctrl.cache_bytes(), 0);
    assert_eq!(ctrl.cache_stats().teardowns(), 1);
}

// ---- window furniture under the reclaim model -----------------------
//
// The four furniture strips live in a bounded, pressure-governed cache the
// compositor owns rather than in each window. These tests pin the two
// properties that make it worth doing — the desktop's furniture is bounded
// and given back under pressure — and the one that makes it safe: the cache
// is an accelerator, so the composited pixels are the same whether it is
// warm, empty, or refusing everything.

/// A decorated, titled window of `client` size placed at `(x, y)`.
fn titled_window(c: &mut Compositor, x: i32, y: i32, client: u32, title: &str) -> WindowId {
    let id = c.add_window(Point::new(x, y), opaque(client, client, RED));
    assert!(c.set_window_frame(id, WindowFrame::new(decorated())));
    assert!(c.set_window_title(id, title));
    id
}

/// Whether client-space `point` lies where a decorated window's own rim curves
/// through its client area: within the theme's window corner radius of both a
/// vertical and a horizontal client edge, at this compositor's scale.
///
/// The frame owns those pixels — the client is clipped out of them so the curve
/// the rim traces is what shows there — so a test asking what the *client*
/// draws asks outside them. An arc reaches no further than its radius from a
/// corner, so content bleeding anywhere else cannot hide behind this.
fn in_a_client_corner(c: &Compositor, client: Rect, point: Point) -> bool {
    let radius = c
        .scale()
        .scale_length(c.theme().metrics().window_corner_radius)
        .cast_signed();
    let dx = (point.x - client.left()).min(client.right() - 1 - point.x);
    let dy = (point.y - client.top()).min(client.bottom() - 1 - point.y);
    dx < radius && dy < radius
}

/// The bytes one such window's furniture costs the cache, measured rather
/// than assumed, so a budget expressed in whole entries stays correct when
/// the theme's band metrics change.
fn one_window_chrome_bytes() -> usize {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    titled_window(&mut c, 20, 20, 60, "measure");
    c.composite();
    assert_eq!(c.chrome_cache_len(), 1);
    c.chrome_cache_bytes()
}

#[test]
fn retained_furniture_never_exceeds_the_one_screenful_ceiling() {
    // Far more decorated windows than a screenful of furniture can hold:
    // the cache admits what fits and evicts the rest, so the desktop's
    // retained chrome is bounded by the output rather than by how many
    // windows the user happens to have open.
    let ceiling = 320 * 240 * 4;
    let mut c = Compositor::new(
        mode(320, 240),
        BLUE,
        chrome_cache(TEST_SEAT, ceiling, &NORMAL_PRESSURE, &TEST_SINK),
        test_frost_cache(),
        &NORMAL_PRESSURE,
    )
    .expect("compositor");
    NORMAL_PRESSURE.report(PressureBand::Normal);

    for index in 0..40 {
        let offset = (index % 8) * 8;
        titled_window(&mut c, offset, offset, 120, "window");
        c.composite();
        assert!(
            c.chrome_cache_bytes() <= ceiling,
            "retained furniture {} passed the one-screenful ceiling {ceiling} \
             after {} windows",
            c.chrome_cache_bytes(),
            index + 1
        );
    }
    assert!(
        c.chrome_cache_len() < 40,
        "a bounded cache cannot have retained every window's furniture"
    );
    assert!(c.chrome_cache_stats().evictions() > 0);
}

#[test]
fn a_scale_change_drops_every_window_s_furniture() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    for index in 0..3 {
        titled_window(&mut c, index * 8, index * 8, 40, "window");
    }
    c.composite();
    assert_eq!(c.chrome_cache_len(), 3);
    assert_eq!(c.chrome_cache_stats().misses(), 3);
    assert_eq!(c.chrome_cache_stats().invalidations(), 0);

    // A new density re-renders every frame, so the epoch moves and the
    // whole cache goes at once — one invalidation, not three.
    assert!(c.set_scale(Scale::from_percent(200).expect("scale")));
    c.composite();
    assert_eq!(c.chrome_cache_stats().invalidations(), 1);
    assert_eq!(c.chrome_cache_stats().misses(), 6);
    assert_eq!(c.chrome_cache_len(), 3);
}

#[test]
fn a_theme_change_drops_every_window_s_furniture() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    for index in 0..3 {
        titled_window(&mut c, index * 8, index * 8, 40, "window");
    }
    c.composite();
    assert_eq!(c.chrome_cache_len(), 3);
    assert_eq!(c.chrome_cache_stats().misses(), 3);

    assert!(c.set_theme(Theme::light()));
    c.composite();
    assert_eq!(c.chrome_cache_stats().invalidations(), 1);
    assert_eq!(c.chrome_cache_stats().misses(), 6);
}

#[test]
fn a_theme_swap_that_keeps_the_id_still_drops_the_furniture() {
    // Two distinct themes may share a `ThemeId` — a high-contrast variant
    // of the built-in dark theme keeps `ThemeId::DARK` — so the epoch
    // cannot be keyed on the id: stale furniture would be a wrong pixel,
    // not a missed hit.
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = titled_window(&mut c, 20, 20, 60, "contrast");
    c.composite();
    assert!(c.chrome_resident(id));

    let variant = with_contrast(&Theme::dark(), Contrast::High);
    assert_eq!(variant.id(), Theme::dark().id());
    assert!(c.set_theme(variant));
    assert!(
        !c.chrome_resident(id),
        "furniture painted under the previous palette must not be served"
    );
}

#[test]
fn a_title_change_invalidates_only_that_window_s_furniture() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let first = titled_window(&mut c, 10, 10, 40, "first");
    let second = titled_window(&mut c, 90, 10, 40, "second");
    let third = titled_window(&mut c, 170, 10, 40, "third");
    c.composite();
    assert_eq!(c.chrome_cache_len(), 3);

    assert!(c.set_window_title(second, "renamed"));
    assert_eq!(c.chrome_cache_len(), 2);
    assert!(c.chrome_resident(first));
    assert!(!c.chrome_resident(second));
    assert!(c.chrome_resident(third));

    // Only the renamed window is re-rendered on the next frame.
    c.composite();
    assert_eq!(c.chrome_cache_stats().misses(), 4);
    assert_eq!(c.chrome_cache_len(), 3);
}

#[test]
fn re_setting_the_title_a_window_already_wears_repaints_nothing() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = titled_window(&mut c, 10, 10, 40, "Documents");
    composite_checked(&mut c);
    assert!(c.chrome_resident(id));

    assert!(
        c.set_window_title(id, "Documents"),
        "the window is decorated"
    );
    assert!(!c.has_damage(), "the title bar already reads that");
    assert!(
        c.chrome_resident(id),
        "and the furniture rendered from it still stands"
    );
}

/// A point inside `rect`, one pixel in from its top-left corner — enough to
/// land a pointer sample on the thing without depending on its extent.
fn inside(rect: Rect) -> Point {
    Point::new(rect.left() + 1, rect.top() + 1)
}

/// The laid-out title bar of a decorated window in screen coordinates: the
/// same geometry the frame paints and hit-tests with.
fn title_layout(c: &Compositor, id: WindowId) -> tairix_controls::TitleBarLayout {
    let window = c.window(id).expect("window");
    let frame = c.window_frame(id).expect("decorated");
    let band = frame
        .layout(window.bounds(), c.scale(), c.theme())
        .title_bar;
    frame.title_bar().layout(band, c.scale(), c.theme())
}

/// The screen rect of one window-command control of a decorated window,
/// resolved through the same layout the frame paints and hit-tests with.
fn command_rect(c: &Compositor, id: WindowId, kind: WindowControlKind) -> Rect {
    title_layout(c, id)
        .controls
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, rect)| *rect)
        .expect("every command is laid out")
}

#[test]
fn a_pointer_sample_crossing_the_drag_region_repaints_nothing() {
    // Moving the pointer across a title bar changes no furniture pixel: the
    // drag region has no hover look. It must therefore neither mark damage nor
    // cost the window its rendered furniture — the alternative is a full chrome
    // re-render and a four-band recomposite per input sample.
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    // Wide enough to leave a real drag span between the two command clusters.
    let id = titled_window(&mut c, 10, 10, 180, "Documents");
    composite_checked(&mut c);
    assert!(c.chrome_resident(id));

    // Just past the leading cluster, at mid-height: the band's corners are
    // commands, and a sample on one of those is a hover, not idle motion.
    let layout = title_layout(&c, id);
    let drag = layout.drag;
    let origin = Point::new(drag.left(), i32::midpoint(drag.top(), drag.bottom()));
    assert!(
        origin.x + 8 < drag.right(),
        "the samples must stay inside the drag span"
    );
    for step in 0..8 {
        assert_eq!(
            c.frame_pointer(id, &moved(origin.x + step, origin.y)),
            None,
            "a drag-region sample produces no furniture event"
        );
        assert!(!c.has_damage(), "…and no repaint");
        assert!(c.chrome_resident(id), "…and keeps its rendered furniture");
    }
}

#[test]
fn entering_a_command_control_repaints_that_control_alone() {
    // A hover that reaches a command button is a real pixel change, so it does
    // cost a repaint — of that button, not of the band it sits in and never of
    // the client area.
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = titled_window(&mut c, 10, 10, 120, "Documents");
    composite_checked(&mut c);

    let close = command_rect(&c, id, WindowControlKind::Close);
    let over = inside(close);
    assert_eq!(c.frame_pointer(id, &moved(over.x, over.y)), None);
    assert!(
        !c.chrome_resident(id),
        "the furniture the hover invalidated must be dropped, not served stale"
    );

    let region = composite_checked(&mut c);
    assert_eq!(region.rects(), [close], "exactly the control that lit up");
    assert!(c.chrome_resident(id), "and re-rendered for the next frame");

    // The same sample again is idle motion: the look is already hover.
    assert_eq!(c.frame_pointer(id, &moved(over.x, over.y)), None);
    assert!(!c.has_damage(), "a repeated sample changes nothing");
    assert!(c.chrome_resident(id));
}

#[test]
fn a_focus_change_invalidates_only_that_window_s_furniture() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let first = titled_window(&mut c, 10, 10, 40, "first");
    let second = titled_window(&mut c, 90, 10, 40, "second");
    c.composite();
    assert_eq!(c.chrome_cache_len(), 2);

    assert!(c.set_active_frame(first, false));
    assert!(!c.chrome_resident(first));
    assert!(c.chrome_resident(second));
}

#[test]
fn re_asserting_the_activation_a_frame_already_shows_repaints_nothing() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = titled_window(&mut c, 10, 10, 40, "active");
    composite_checked(&mut c);
    assert!(c.chrome_resident(id));

    assert!(c.set_active_frame(id, true), "the window is decorated");
    assert!(!c.has_damage(), "it already wears the active frame");
    assert!(c.chrome_resident(id));

    assert!(c.set_active_frame(id, false));
    composite_checked(&mut c);
    assert!(c.set_active_frame(id, false), "the window is decorated");
    assert!(!c.has_damage(), "it already wears the inactive frame");
    assert!(c.chrome_resident(id));
}

#[test]
fn an_attention_request_survives_being_told_it_is_inactive_again() {
    // Deactivating an attention-requesting frame leaves it requesting, so the
    // second call really is a no-op — and the first must not quiet it either.
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = titled_window(&mut c, 10, 10, 40, "attention");
    let attention = WindowFurnitureState {
        activation: WindowActivationState::AttentionRequested,
        ..decorated()
    };
    assert!(c.set_window_frame(id, WindowFrame::new(attention)));
    assert!(c.set_window_title(id, "attention"));
    composite_checked(&mut c);

    assert!(c.set_active_frame(id, false), "the window is decorated");
    assert_eq!(
        c.window_frame(id).map(|f| f.furniture().activation),
        Some(WindowActivationState::AttentionRequested)
    );
    assert!(
        !c.has_damage(),
        "an attention request is already not active"
    );
    assert!(c.chrome_resident(id));
}

#[test]
fn a_resize_invalidates_only_that_window_s_furniture() {
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let first = titled_window(&mut c, 10, 10, 40, "first");
    let second = titled_window(&mut c, 150, 10, 40, "second");
    c.composite();
    assert_eq!(c.chrome_cache_len(), 2);

    let outer = c.window(first).expect("window").bounds();
    assert!(c.resize_window(
        first,
        Rect::new(
            outer.left(),
            outer.top(),
            outer.width + 20,
            outer.height + 20
        )
    ));
    assert!(!c.chrome_resident(first));
    assert!(c.chrome_resident(second));
}

#[test]
fn mild_pressure_drops_the_chrome_cache_and_refuses_growth() {
    // A gauge private to this test: it moves the band, and the shared one
    // must stay at normal for the tests running beside it.
    static PRESSURE: ReportedPressure = ReportedPressure::unknown();
    PRESSURE.report(PressureBand::Normal);

    let mut c = Compositor::new(
        mode(320, 240),
        BLUE,
        chrome_cache(TEST_SEAT, TEST_FB_BYTES, &PRESSURE, &TEST_SINK),
        frost_cache(TEST_SEAT, TEST_FB_BYTES, &PRESSURE, &TEST_SINK),
        &PRESSURE,
    )
    .expect("compositor");
    let id = titled_window(&mut c, 20, 20, 120, "pressed");
    c.composite();
    assert_eq!(c.chrome_cache_len(), 1);
    assert!(c.chrome_cache_bytes() > 0);
    let warm = c.frame().to_vec();

    // Mild pressure forces a disposable-UI cache to zero, ahead of any
    // clean-file or anonymous-page reclaim.
    PRESSURE.report(PressureBand::Mild);
    assert!(c.trim_chrome() > 0, "mild pressure must release bytes");
    assert_eq!(c.chrome_cache_len(), 0);
    assert_eq!(c.chrome_cache_bytes(), 0);

    // The frame is still drawn correctly; the furniture is simply
    // rendered on demand instead of retained.
    repaint_everything(&mut c);
    assert_eq!(c.frame(), &warm[..], "pressure must not change a pixel");
    assert_eq!(c.chrome_cache_len(), 0, "no growth while pressure holds");
    assert!(c.chrome_cache_stats().refusals() >= 1);
    assert!(!c.chrome_resident(id));
}

/// Force a full repaint without disturbing the scene: two background
/// changes damage the whole screen and land back on the colour that was
/// already there.
fn repaint_everything(c: &mut Compositor) {
    let background = c.background();
    let other = Color::rgb(0, 255, 0);
    assert_ne!(background, other);
    assert!(c.set_background(other));
    assert!(c.set_background(background));
    c.composite();
}

#[test]
fn the_composited_frame_is_identical_warm_empty_and_uncacheable() {
    // The proof that the cache is an accelerator and never a correctness
    // requirement: the same scene composites to the same bytes whether
    // its furniture is retained, has just been thrown away, or can never
    // be retained at all.
    let scene = |c: &mut Compositor| {
        titled_window(c, 10, 10, 60, "alpha");
        titled_window(c, 120, 40, 80, "beta");
        let hidden = titled_window(c, 40, 120, 50, "gamma");
        assert!(c.set_visible(hidden, false));
    };

    let mut warm = new_compositor(mode(320, 240), BLUE).expect("compositor");
    scene(&mut warm);
    warm.composite();
    repaint_everything(&mut warm);
    assert!(warm.chrome_cache_stats().hits() > 0, "the cache is warm");

    let mut emptied = new_compositor(mode(320, 240), BLUE).expect("compositor");
    scene(&mut emptied);
    emptied.composite();
    emptied.teardown_chrome();
    assert_eq!(emptied.chrome_cache_len(), 0);
    repaint_everything(&mut emptied);

    let mut uncacheable = Compositor::new(
        mode(320, 240),
        BLUE,
        chrome_cache(TEST_SEAT, 0, &NORMAL_PRESSURE, &TEST_SINK),
        test_frost_cache(),
        &NORMAL_PRESSURE,
    )
    .expect("compositor");
    NORMAL_PRESSURE.report(PressureBand::Normal);
    scene(&mut uncacheable);
    uncacheable.composite();
    assert_eq!(
        uncacheable.chrome_cache_len(),
        0,
        "a zero budget must retain nothing"
    );
    assert!(uncacheable.chrome_cache_stats().refusals() >= 2);

    assert_eq!(warm.frame(), emptied.frame());
    assert_eq!(warm.frame(), uncacheable.frame());
}

#[test]
fn tearing_the_chrome_cache_down_overwrites_the_retained_strips() {
    // Furniture carries the window's title, so releasing it is a wipe,
    // not a drop. The wipe the cache performs on release is this one:
    // observing it here is the only way to see bytes whose allocation is
    // freed the instant afterwards.
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let id = titled_window(&mut c, 20, 20, 120, "Secret Document");
    c.composite();
    assert_eq!(c.chrome_cache_len(), 1);
    assert!(c.chrome_cache_bytes() > 0);

    let mut chrome = c
        .window(id)
        .expect("window")
        .render_chrome(c.scale(), c.theme())
        .expect("chrome renders");
    assert!(chrome.payload_bytes() > 0);
    let bands = c.window(id).expect("window").furniture_bands();
    let drawn = |chrome: &WindowChrome| {
        (0..bands[0].height).any(|y| chrome.top_row(y).iter().any(|p| *p != Pixel::TRANSPARENT))
    };
    assert!(drawn(&chrome), "the title band starts with painted pixels");

    chrome.wipe();
    assert!(!drawn(&chrome), "every title-band byte must be overwritten");
    for y in 0..bands[1].height {
        assert!(chrome
            .bottom_row(y)
            .iter()
            .all(|p| *p == Pixel::TRANSPARENT));
    }
    for y in 0..bands[2].height {
        assert!(chrome.left_row(y).iter().all(|p| *p == Pixel::TRANSPARENT));
        assert!(chrome.right_row(y).iter().all(|p| *p == Pixel::TRANSPARENT));
    }

    c.teardown_chrome();
    assert_eq!(c.chrome_cache_len(), 0);
    assert_eq!(c.chrome_cache_bytes(), 0);
    assert_eq!(c.chrome_cache_stats().teardowns(), 1);
}

#[test]
fn a_hidden_window_s_furniture_is_evicted_before_a_visible_one_s() {
    // Eviction takes the least recently *composited* entry, and a hidden
    // window is not composited — so the furniture of a minimised window
    // is what a full cache gives back, never that of the window the user
    // is looking at.
    let entry = one_window_chrome_bytes();
    // A ceiling that holds two windows' furniture and forces exactly one
    // eviction when a third arrives.
    let ceiling = entry * 14 / 5;
    let mut c = Compositor::new(
        mode(320, 240),
        BLUE,
        chrome_cache(TEST_SEAT, ceiling, &NORMAL_PRESSURE, &TEST_SINK),
        test_frost_cache(),
        &NORMAL_PRESSURE,
    )
    .expect("compositor");
    NORMAL_PRESSURE.report(PressureBand::Normal);

    let minimised = titled_window(&mut c, 10, 10, 60, "minimised");
    let visible = titled_window(&mut c, 150, 10, 60, "visible");
    c.composite();
    assert!(c.chrome_resident(minimised));
    assert!(c.chrome_resident(visible));

    // Minimise the first and compose again: the visible one is touched,
    // the hidden one is not, so it becomes the oldest entry.
    assert!(c.set_visible(minimised, false));
    c.composite();
    assert!(
        c.chrome_resident(minimised),
        "hiding retains, it does not evict"
    );

    let newcomer = titled_window(&mut c, 10, 120, 60, "newcomer");
    c.composite();
    assert!(
        !c.chrome_resident(minimised),
        "the minimised window's furniture is what a full cache gives back"
    );
    assert!(
        c.chrome_resident(visible),
        "the visible window's furniture must survive"
    );
    assert!(c.chrome_resident(newcomer));
}

// ---- retained backdrops ----------------------------------------------

/// One scene composed twice: once reusing retained frosts, once blurring
/// afresh every frame.
///
/// Every operation is applied to both compositors and every frame is compared
/// byte for byte, so a frost the reuse path kept when something beneath it
/// changed shows up as a differing pixel rather than as a plausible-looking
/// screenshot. The back buffer is compared as well as the scan-out frame,
/// because a frost reads the back buffer: a difference there would go
/// unnoticed for a frame and surface later.
struct BothWays {
    reusing: Compositor,
    blurring: Compositor,
}

impl BothWays {
    fn new(mode: DisplayMode) -> Self {
        let reusing = new_compositor(mode, BLUE).expect("compositor");
        let mut blurring = new_compositor(mode, BLUE).expect("compositor");
        blurring.set_frost_reuse(false);
        Self { reusing, blurring }
    }

    /// Apply `act` to both compositors, asserting they agree on what it
    /// returned — window ids are handed out in order, so the two stacks stay
    /// identical — and hand that back.
    fn both<T>(&mut self, act: impl Fn(&mut Compositor) -> T) -> T
    where
        T: core::fmt::Debug + PartialEq,
    {
        let reusing = act(&mut self.reusing);
        let blurring = act(&mut self.blurring);
        assert_eq!(
            reusing, blurring,
            "the two compositors took different paths"
        );
        reusing
    }

    /// Composite both and require the results to be identical.
    fn settle(&mut self, step: &str) {
        let reused = self.reusing.composite();
        let blurred = self.blurring.composite();
        assert_eq!(
            self.reusing.frame(),
            self.blurring.frame(),
            "scan-out differs after {step} (reused {reused:?}, blurred {blurred:?})"
        );
        assert_eq!(
            self.reusing.back_buffer().pixels(),
            self.blurring.back_buffer().pixels(),
            "back buffer differs after {step}"
        );
    }
}

#[test]
fn every_change_around_a_frosted_window_composes_the_frame_a_fresh_blur_would() {
    let mut both = BothWays::new(mode(40, 24));
    let under = both.both(|c| c.add_window(Point::ORIGIN, opaque(40, 24, GREEN)));
    let glass = both.both(|c| c.add_window(Point::new(6, 4), clear(20, 14)));
    let over = both.both(|c| c.add_window(Point::new(30, 2), opaque(8, 8, RED)));
    both.both(|c| c.set_backdrop_blur(glass, 3));
    both.settle("the first frost");

    // The window's own content: the case the whole cache exists for.
    both.both(|c| present_content(c, glass, paint_dot));
    both.settle("the frosted window's own content");

    // The cursor, above every window.
    both.both(|c| {
        c.set_cursor(solid_cursor(4, RED), Point::new(12, 9));
        true
    });
    both.settle("a cursor over the frost");
    both.both(|c| c.move_cursor(Point::new(14, 10)));
    both.settle("a cursor moved over the frost");

    // The screen reveal, applied only as a pixel is encoded.
    both.both(|c| c.set_reveal(128));
    both.settle("a partial reveal");
    both.both(|c| c.set_reveal(u8::MAX));
    both.settle("a full reveal");

    // A window above it: nothing the frost reads, including while it is
    // dragged right across the frosted rectangle. It is left overlapping, so
    // every step below runs with a window above the frost.
    both.both(|c| c.move_window(over, Point::new(31, 2)));
    both.settle("the window above moved");
    both.both(|c| c.move_window(over, Point::new(20, 6)));
    both.settle("the window above dragged onto the frost");
    both.both(|c| c.move_window(over, Point::new(14, 8)));
    both.settle("the window above dragged across the frost");

    // Everything below it, which the frost does read.
    both.both(|c| present_content(c, under, paint_dot));
    both.settle("the window below presented");
    both.both(|c| c.move_window(under, Point::new(1, 0)));
    both.settle("the window below moved");
    both.both(|c| c.set_opacity(under, 128));
    both.settle("the window below faded");
    both.both(|c| c.set_visible(under, false));
    both.settle("the window below hidden");
    both.both(|c| c.set_visible(under, true));
    both.settle("the window below shown");
    both.both(|c| c.set_background(RED));
    both.settle("the root fill recoloured");
    both.both(|c| c.repaint_desktop(|surface| surface.fill(GREEN)));
    both.settle("the desktop layer repainted");

    // The frosted window's own geometry, shape, radius, and density.
    both.both(|c| c.move_window(glass, Point::new(7, 5)));
    both.settle("the frost moved");
    both.both(|c| c.resize_window_client(glass, 22, 12));
    both.settle("the frost resized");
    both.both(|c| c.set_corners(glass, Corners::Rounded { radius: 5 }));
    both.settle("the frost rounded");
    both.both(|c| c.set_backdrop_blur(glass, 2));
    both.settle("the radius changed");
    both.both(|c| c.set_scale(Scale::from_percent(200).expect("valid scale")));
    both.settle("the density changed");
    both.both(|c| c.set_scale(Scale::ONE));
    both.settle("the density restored");

    // Restacking, which changes what is beneath the frost and what the frost
    // is beneath.
    both.both(|c| c.raise(glass));
    both.settle("the frost raised");
    both.both(|c| c.lower(glass));
    both.settle("the frost lowered");
    both.both(|c| c.raise(over));
    both.settle("the window above raised");

    // A second frost overlapping the first, so each reads what the other wrote.
    let second = both.both(|c| c.add_window(Point::new(18, 8), clear(16, 12)));
    both.both(|c| c.set_backdrop_blur(second, 4));
    both.settle("a second overlapping frost");
    both.both(|c| present_content(c, second, paint_dot));
    both.settle("the second frost's own content");
    both.both(|c| present_content(c, under, paint_dot));
    both.settle("under both frosts");
    both.both(|c| c.move_window(second, Point::new(20, 9)));
    both.settle("the second frost moved");

    // A frost hanging off the screen edge, so its rectangle is clipped.
    both.both(|c| c.move_window(glass, Point::new(-6, -4)));
    both.settle("the frost partly off screen");
    both.both(|c| present_content(c, glass, paint_dot));
    both.settle("the clipped frost's own content");

    // The theme, the mode, and finally taking the windows away.
    both.both(|c| c.set_theme(Theme::light()));
    both.settle("a theme switch");
    both.both(|c| c.set_mode(mode(32, 20)));
    both.settle("a mode change");
    both.both(|c| c.remove(second));
    both.settle("the second frost removed");
    both.both(|c| c.remove(glass));
    both.settle("the last frost removed");
}

#[test]
fn a_frost_pushed_further_off_screen_is_not_the_one_it_clipped_to_before() {
    // A window wider than the screen clips to the same on-screen rectangle at
    // both of these positions, but its rounded shape is read from its own
    // top-left, so the two frosts weight those pixels differently.
    let mut both = BothWays::new(mode(20, 16));
    both.both(|c| c.add_window(Point::ORIGIN, opaque(20, 16, GREEN)));
    // An edge under the corner: a blur of a flat colour is that colour, so
    // only a textured backdrop can tell two frostings apart at all.
    both.both(|c| c.add_window(Point::ORIGIN, opaque(9, 9, RED)));
    let glass = both.both(|c| c.add_window(Point::ORIGIN, clear(30, 12)));
    both.both(|c| c.set_corners(glass, Corners::Rounded { radius: 6 }));
    both.both(|c| c.set_backdrop_blur(glass, 3));
    both.settle("a frost wider than the screen");
    both.both(|c| c.move_window(glass, Point::new(-8, 0)));
    both.settle("the same clipped rectangle, a different part of the shape");
}

#[test]
fn a_window_dragged_across_a_frosted_one_never_costs_a_re_blur() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    let over = c.add_window(Point::new(28, 2), opaque(8, 8, RED));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert!(c.frost_resident(glass));
    let served = c.frost_cache_stats().hits();

    for x in [24, 20, 16, 12, 8] {
        assert!(c.move_window(over, Point::new(x, 6)));
        assert!(
            c.frost_resident(glass),
            "a window above cannot change what the frost below reads"
        );
        composite_checked(&mut c);
        assert_eq!(
            c.frame_stats().blur_px,
            0,
            "no step of the drag re-frosts the window it crosses"
        );
    }
    assert_eq!(
        c.frost_cache_stats().hits(),
        served + 5,
        "every frame of the drag served the retained frost"
    );
}

#[test]
fn dragging_a_frosted_window_blurs_only_the_border_the_move_uncovers() {
    // The interaction this cache was least good at: the frosted window itself
    // being dragged. Its rectangle moves, so the frost taken for the old one no
    // longer describes it — but the layers *beneath* it did not move, so every
    // pixel far enough inside both rectangles that neither the blur's
    // replication nor the shape's corners can reach it is still exactly right.
    // Only the border is blurred again.
    const SIDE: (u32, u32) = (200, 140);
    const RADIUS: u16 = 4;
    let mut c = new_compositor(mode(320, 240), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(320, 240, GREEN));
    let glass = c.add_window(Point::new(40, 40), clear(SIDE.0, SIDE.1));
    assert!(c.set_backdrop_blur(glass, RADIUS));
    composite_checked(&mut c);
    let whole = u64::from(SIDE.0 * SIDE.1);
    assert_eq!(
        c.frame_stats().blur_px,
        whole,
        "the first frame has nothing to keep"
    );

    // A pointer sample's worth of movement, several times over, so the steady
    // state of a drag is what is measured and not just its first step.
    for step in 1..=5 {
        assert!(c.move_window(glass, Point::new(40 + step * 3, 40 + step * 3)));
        assert!(
            c.frost_resident(glass),
            "moving a window changes nothing beneath it, so its frost survives"
        );
        composite_checked(&mut c);
        let blurred = c.frame_stats().blur_px;
        assert!(blurred > 0, "the border it uncovered must be blurred");
        assert!(
            blurred * 5 < whole,
            "step {step} blurred {blurred} of {whole} pixels; the border of a \
             three-pixel move is a small fraction of the window"
        );
    }

    // A jump far enough to leave no shared core at all falls back to blurring
    // the whole rectangle rather than producing a seam.
    assert!(c.move_window(glass, Point::new(300, 220)));
    composite_checked(&mut c);
    assert_eq!(
        c.frame_stats().blur_px,
        20 * 20,
        "a window jumped clear of itself keeps nothing, and only its \
         on-screen part is frosted"
    );
}

#[test]
fn every_change_around_a_translucent_window_composes_the_frame_a_fresh_one_would() {
    // The same sweep as the frosted one above, over a window that is merely
    // translucent. It retains its backdrop through the same cache — a blur of
    // radius zero leaves the composed layers exactly as they were — so every
    // way the picture beneath it can change has to drop it, and a frame that
    // kept one it should not have differs from a frame that composed the stack
    // afresh.
    let mut both = BothWays::new(mode(40, 24));
    let under = both.both(|c| c.add_window(Point::ORIGIN, opaque(40, 24, GREEN)));
    let glass = both.both(|c| c.add_window(Point::new(6, 4), opaque(20, 14, RED)));
    let over = both.both(|c| c.add_window(Point::new(30, 2), opaque(8, 8, BLUE)));
    both.both(|c| c.set_opacity(glass, 160));
    both.settle("the first translucent backdrop");

    both.both(|c| present_content(c, glass, paint_dot));
    both.settle("the translucent window's own content");
    both.both(|c| {
        c.set_cursor(solid_cursor(4, RED), Point::new(12, 9));
        true
    });
    both.settle("a cursor over it");
    both.both(|c| c.move_cursor(Point::new(14, 10)));
    both.settle("a cursor moved over it");
    both.both(|c| c.set_reveal(128));
    both.settle("a partial reveal");
    both.both(|c| c.set_reveal(u8::MAX));
    both.settle("a full reveal");

    // Above it: nothing its backdrop reads.
    both.both(|c| c.move_window(over, Point::new(20, 6)));
    both.settle("the window above dragged onto it");
    both.both(|c| c.move_window(over, Point::new(14, 8)));
    both.settle("the window above dragged across it");

    // Beneath it: everything its backdrop reads.
    both.both(|c| present_content(c, under, paint_dot));
    both.settle("the window below presented");
    both.both(|c| c.move_window(under, Point::new(1, 0)));
    both.settle("the window below moved");
    both.both(|c| c.set_visible(under, false));
    both.settle("the window below hidden");
    both.both(|c| c.set_visible(under, true));
    both.settle("the window below shown");
    both.both(|c| c.set_background(GREEN));
    both.settle("the root fill recoloured");
    both.both(|c| c.repaint_desktop(|surface| surface.fill(RED)));
    both.settle("the desktop layer repainted");

    // Its own geometry and shape, each of which decides how much survives.
    both.both(|c| c.move_window(glass, Point::new(7, 5)));
    both.settle("moved a pointer sample");
    both.both(|c| c.move_window(glass, Point::new(26, 14)));
    both.settle("moved clear of where it was");
    both.both(|c| c.resize_window_client(glass, 22, 12));
    both.settle("resized");
    both.both(|c| c.set_corners(glass, Corners::Rounded { radius: 5 }));
    both.settle("rounded");
    both.both(|c| c.set_opacity(glass, 90));
    both.settle("faded further");
    both.both(|c| c.set_scale(Scale::from_percent(200).expect("valid scale")));
    both.settle("the density changed");
    both.both(|c| c.set_scale(Scale::ONE));
    both.settle("the density restored");

    // Restacking, and a second translucent window overlapping it, so each
    // reads what the other wrote.
    both.both(|c| c.raise(glass));
    both.settle("raised");
    both.both(|c| c.lower(glass));
    both.settle("lowered");
    let second = both.both(|c| c.add_window(Point::new(18, 8), opaque(16, 12, GREEN)));
    both.both(|c| c.set_opacity(second, 128));
    both.settle("a second overlapping translucent window");
    both.both(|c| c.move_window(second, Point::new(20, 9)));
    both.settle("the second one moved");
    both.both(|c| present_content(c, under, paint_dot));
    both.settle("under both of them");

    // Off the screen edge, where the retained rectangle is clipped, then a
    // blur added on top of the opacity and finally taken away again.
    both.both(|c| c.move_window(glass, Point::new(-6, -4)));
    both.settle("partly off screen");
    both.both(|c| present_content(c, glass, paint_dot));
    both.settle("the clipped window's own content");
    both.both(|c| c.set_backdrop_blur(glass, 3));
    both.settle("blurred as well as translucent");
    both.both(|c| c.set_backdrop_blur(glass, 0));
    both.settle("the blur taken away");
    both.both(|c| c.set_opacity(glass, u8::MAX));
    both.settle("made opaque, so it retains nothing");
    both.both(|c| c.remove(second));
    both.settle("the second one removed");
    both.both(|c| c.remove(glass));
    both.settle("the last one removed");
}

/// Apply `act` to two compositors, requiring both to take it, so the pair
/// stays in the same state and their frame counters stay comparable.
fn apply_to_both(a: &mut Compositor, b: &mut Compositor, act: impl Fn(&mut Compositor) -> bool) {
    assert!(act(a) && act(b), "both stacks take the act");
}

#[test]
fn dragging_a_translucent_window_keeps_the_backdrop_it_already_composed() {
    // The complaint this closes: a translucent window was the *slowest* thing
    // to drag, because every pointer sample recomposed the whole stack beneath
    // it. Moving it disturbs nothing below, so all of the backdrop the two
    // positions share is still exactly right and only the sliver the move
    // uncovers has to be composed.
    // Two compositors given identical scenes and identical moves, one keeping
    // its backdrops and one composing every frame from the root fill up, so
    // the counters below are the same frame's work measured two ways rather
    // than two different frames compared.
    let mut kept = new_compositor(mode(320, 240), BLUE).expect("compositor");
    let mut fresh = new_compositor(mode(320, 240), BLUE).expect("compositor");
    fresh.set_frost_reuse(false);
    let whole = u64::from(200 * 140_u32);
    apply_to_both(&mut kept, &mut fresh, |c| {
        c.add_window(Point::ORIGIN, opaque(320, 240, GREEN));
        // Translucent, so the layer beneath the dragged window has to be
        // blended rather than copied by the opaque-run path — which is what
        // makes it something a retained backdrop can spare.
        let mid = c.add_window(Point::new(20, 20), opaque(260, 190, RED));
        c.set_opacity(mid, 200)
    });
    let glass = kept.add_window(Point::new(40, 40), opaque(200, 140, BLUE));
    assert_eq!(
        fresh.add_window(Point::new(40, 40), opaque(200, 140, BLUE)),
        glass,
        "ids are handed out in order, so the two stacks stay identical"
    );
    apply_to_both(&mut kept, &mut fresh, |c| c.set_opacity(glass, 160));
    composite_checked(&mut kept);
    composite_checked(&mut fresh);
    assert!(kept.frost_resident(glass), "its backdrop is retained");

    for step in 1..=5 {
        apply_to_both(&mut kept, &mut fresh, |c| {
            c.move_window(glass, Point::new(40 + step * 3, 40 + step * 3))
        });
        composite_checked(&mut kept);
        composite_checked(&mut fresh);
        let (with, without) = (kept.frame_stats(), fresh.frame_stats());
        assert_eq!(
            (with.blur_px, without.blur_px),
            (0, 0),
            "step {step}: nothing in this scene is blurred"
        );
        // Keeping the backdrop also keeps the damaged rectangle down to what
        // the move actually disturbed: a window that must recompose its
        // backdrop is promoted to its whole bounds first, so the stack beneath
        // it is resolved over more of the screen as well as more deeply.
        assert!(
            with.damaged_px < without.damaged_px,
            "step {step} damaged {} pixels keeping the backdrop and {} without",
            with.damaged_px,
            without.damaged_px
        );
        // Its own pixels still blend over the whole rectangle; what the kept
        // backdrop spares is resolving the layers under it at all — neither
        // blended nor copied.
        let resolved = |s: crate::FrameStats| s.blended_px + s.opaque_px;
        assert!(
            resolved(with) < whole * 5 / 4,
            "step {step} resolved {} layer contributions over a {whole}-pixel \
             window; a kept backdrop leaves the window itself and the sliver \
             the move uncovered",
            resolved(with)
        );
        assert!(
            resolved(without) > resolved(with) * 2,
            "step {step} resolved {} contributions with the backdrop kept \
             against {} without it; keeping it must spare the stack beneath",
            resolved(with),
            resolved(without)
        );
    }

    // Both took the same route to the same screen: the saving is work not
    // done, never a different picture.
    assert_eq!(kept.frame(), fresh.frame(), "the two screens are identical");
}

#[test]
fn a_reused_frost_spares_composing_the_layers_it_covers() {
    // A frost is copied over whatever is beneath it, so composing that stack
    // first is work the copy throws away. The window's own pixels still blend
    // over the frost — one contribution per pixel — and nothing else does.
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    let under = c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    // Translucent as well as frosted, which is the window this is about: an
    // opaque one would have its pixels copied rather than blended.
    assert!(c.set_opacity(glass, 200));
    composite_checked(&mut c);
    assert!(c.frost_resident(glass));

    // The frosted window repaints all of itself: the frost is reused whole.
    assert_eq!(present_content(&mut c, glass, repaint_all(RED)), Some(true));
    composite_checked(&mut c);
    let stats = c.frame_stats();
    assert_eq!(stats.blur_px, 0, "a retained frost is copied, not blurred");
    assert_eq!(
        (stats.damaged_px, stats.blended_px, stats.opaque_px),
        (20 * 14, 20 * 14, 0),
        "one blend per pixel — the window over its frost — and the window \
         beneath it, which the frost hides, neither blended nor copied"
    );

    // The same damage with nothing retained pays for that stack: the blur has
    // to read it.
    assert_eq!(
        present_content(&mut c, under, repaint_all(GREEN)),
        Some(true)
    );
    composite_checked(&mut c);
    assert!(
        c.frame_stats().opaque_px >= 20 * 14,
        "a recomputed frost must resolve the layers it blurs"
    );
}

#[test]
fn each_frame_asks_about_a_frost_once_and_records_what_it_got() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    let under = c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::ORIGIN, clear(20, 14));
    let away = c.add_window(Point::new(30, 18), opaque(8, 5, RED));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    let counts = |c: &Compositor| (c.frost_cache_stats().hits(), c.frost_cache_stats().misses());
    assert_eq!(
        counts(&c),
        (0, 1),
        "the first frame found nothing retained, and retaining is not a lookup"
    );

    assert_eq!(present_content(&mut c, glass, paint_dot), Some(true));
    composite_checked(&mut c);
    assert_eq!(counts(&c), (1, 1), "the frost was copied, not blurred");

    assert_eq!(present_content(&mut c, under, paint_dot), Some(true));
    composite_checked(&mut c);
    assert_eq!(
        counts(&c),
        (1, 2),
        "a recompute is one miss, not one for the plan and one for retaining it"
    );

    assert_eq!(present_content(&mut c, away, paint_dot), Some(true));
    composite_checked(&mut c);
    assert_eq!(
        counts(&c),
        (1, 2),
        "a frame that never reaches the frost asks nothing about it"
    );
}

#[test]
fn a_frosted_window_s_own_repaint_costs_no_blur_and_no_widening() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert!(c.frame_stats().blur_px > 0, "the first frame must frost");
    assert!(c.frost_resident(glass));

    // A one-pixel content present inside the frost: no blur at all, and the
    // damage stays the pixel rather than growing to the window.
    assert_eq!(present_content(&mut c, glass, paint_dot), Some(true));
    let repainted = composite_checked(&mut c);
    let stats = c.frame_stats();
    assert_eq!(stats.blur_px, 0, "a retained frost is copied, not blurred");
    assert_eq!(repainted.rects(), &[Rect::new(7, 8, 1, 1)]);
    assert_eq!(stats.damaged_px, 1);
}

#[test]
fn a_change_beneath_a_frosted_window_re_frosts_the_whole_of_it() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    let under = c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    // The frost covers the pixel `paint_dot` changes, so the change really is
    // one its blur reads: a frost samples only inside its own rectangle.
    let glass = c.add_window(Point::ORIGIN, clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert!(c.frost_resident(glass));

    assert_eq!(present_content(&mut c, under, paint_dot), Some(true));
    assert!(
        !c.frost_resident(glass),
        "a present below the frost must drop it"
    );
    composite_checked(&mut c);
    assert_eq!(
        c.frame_stats().blur_px,
        20 * 14,
        "the whole window is frosted again, not the presented pixel"
    );
    assert!(c.frost_resident(glass), "and retained for the next frame");
}

#[test]
fn a_present_above_a_frosted_window_leaves_its_frost_alone() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    let over = c.add_window(Point::new(10, 6), opaque(8, 8, RED));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert!(c.frost_resident(glass));

    assert_eq!(present_content(&mut c, over, paint_dot), Some(true));
    assert!(
        c.frost_resident(glass),
        "a window stacked above contributes nothing to the frost beneath it"
    );
    composite_checked(&mut c);
    assert_eq!(c.frame_stats().blur_px, 0);
}

#[test]
fn raising_the_topmost_frosted_window_keeps_its_backdrop() {
    // An open menu has its parent and itself re-raised before every composite.
    // Both are already where they belong, so a terminal behind its own menu
    // must not pay a whole-window re-blur per wake.
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert!(c.frost_resident(glass));

    assert!(c.raise(glass));
    assert!(
        c.frost_resident(glass),
        "a raise that restacks nothing cannot have changed what the frost sees"
    );
    assert!(!c.has_damage());
    composite_checked(&mut c);
    let stats = c.frame_stats();
    // Unguarded this frame cost (280, 280): every pixel of the window
    // recomposited and its whole backdrop blurred again.
    assert_eq!((stats.damaged_px, stats.blur_px), (0, 0));
}

#[test]
fn raising_a_covered_frosted_window_re_frosts_it() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    c.add_window(Point::new(10, 6), opaque(8, 8, RED));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert!(c.frost_resident(glass));

    // Raising it puts a window that was above it below: the backdrop it
    // blurred is a different one now.
    assert!(c.raise(glass));
    assert!(!c.frost_resident(glass));
    composite_checked(&mut c);
    assert_eq!(c.frame_stats().blur_px, 20 * 14);
}

#[test]
fn re_frosting_one_window_drops_the_frost_stacked_above_it() {
    // The lower frost's *visible* pixels change across the whole of it when it
    // is recomputed, because a blur spreads the change far past the rectangle
    // that caused it — so the frost above reads different bytes even where the
    // damage never reached it.
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    let under = c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let lower = c.add_window(Point::new(0, 2), clear(24, 20));
    let upper = c.add_window(Point::new(20, 2), clear(18, 20));
    assert!(c.set_backdrop_blur(lower, 3));
    assert!(c.set_backdrop_blur(upper, 3));
    composite_checked(&mut c);
    assert!(c.frost_resident(lower) && c.frost_resident(upper));

    // A pixel inside the lower frost's rectangle and well clear of the upper
    // one's, so only the spreading blur can reach the window above.
    assert_eq!(present_content(&mut c, under, paint_dot), Some(true));
    assert!(!c.frost_resident(lower));
    let repainted = composite_checked(&mut c);
    assert_eq!(
        repainted.rects(),
        &[Rect::new(0, 2, 38, 20)],
        "both frosts recompose as one rectangle"
    );
    assert!(c.frost_resident(lower) && c.frost_resident(upper));
}

#[test]
fn a_removed_window_takes_its_frost_with_it() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert_eq!(c.frost_cache_len(), 1);
    assert!(c.frost_cache_bytes() > 0);

    assert!(c.remove(glass));
    assert_eq!(c.frost_cache_len(), 0);
    assert_eq!(c.frost_cache_bytes(), 0);
}

#[test]
fn a_window_that_stops_frosting_retains_nothing() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert_eq!(c.frost_cache_len(), 1);

    assert!(c.set_backdrop_blur(glass, 0));
    assert!(!c.frost_resident(glass));
    composite_checked(&mut c);
    assert_eq!(
        c.frost_cache_len(),
        0,
        "an unfrosted window has no backdrop to retain"
    );
    assert_eq!(c.frame_stats().blur_px, 0);
}

#[test]
fn retained_frosts_never_exceed_the_one_screenful_ceiling() {
    // Far more frosted windows than a screenful of frost can hold: the cache
    // admits what fits and evicts the rest, so a machine's retained frost is
    // bounded by its output rather than by how many frosted windows are open.
    let ceiling = 64 * 64 * 4;
    let mut c = Compositor::new(
        mode(64, 64),
        BLUE,
        test_chrome_cache(),
        frost_cache(TEST_SEAT, ceiling, &NORMAL_PRESSURE, &TEST_SINK),
        &NORMAL_PRESSURE,
    )
    .expect("compositor");
    NORMAL_PRESSURE.report(PressureBand::Normal);
    c.add_window(Point::ORIGIN, opaque(64, 64, GREEN));
    for index in 0..12 {
        let offset = (index % 4) * 4;
        let glass = c.add_window(Point::new(offset, offset), clear(40, 40));
        assert!(c.set_backdrop_blur(glass, 2));
    }

    composite_checked(&mut c);
    assert!(
        c.frost_cache_bytes() <= ceiling,
        "retained frost {} passed the one-screenful ceiling {ceiling}",
        c.frost_cache_bytes()
    );
    assert!(
        c.frost_cache_len() < 12,
        "a bounded cache cannot have retained every window's frost"
    );
    assert!(c.frost_cache_stats().evictions() > 0);
}

#[test]
fn pressure_gives_the_frost_back_and_the_frame_is_unchanged() {
    // A gauge private to this test: it moves the band, and the shared one must
    // stay at normal for the tests running beside it.
    static PRESSURE: ReportedPressure = ReportedPressure::unknown();
    PRESSURE.report(PressureBand::Normal);

    let mut c = Compositor::new(
        mode(40, 24),
        BLUE,
        chrome_cache(TEST_SEAT, TEST_FB_BYTES, &PRESSURE, &TEST_SINK),
        frost_cache(TEST_SEAT, TEST_FB_BYTES, &PRESSURE, &TEST_SINK),
        &PRESSURE,
    )
    .expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    let warm = c.frame().to_vec();
    assert!(c.frost_cache_bytes() > 0);

    PRESSURE.report(PressureBand::Mild);
    assert!(c.trim_frost() > 0, "mild pressure must release bytes");
    assert_eq!(c.frost_cache_bytes(), 0);

    // Nothing was lost: the same scene composes the same frame, blurred again.
    repaint_everything(&mut c);
    assert_eq!(c.frame(), warm.as_slice());
    assert!(c.frame_stats().blur_px > 0);
}

#[test]
fn tearing_the_seat_down_releases_every_frost() {
    let mut c = new_compositor(mode(40, 24), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(40, 24, GREEN));
    let glass = c.add_window(Point::new(6, 4), clear(20, 14));
    assert!(c.set_backdrop_blur(glass, 3));
    composite_checked(&mut c);
    assert_eq!(c.frost_cache_len(), 1);

    c.teardown_frost();
    assert_eq!(c.frost_cache_len(), 0);
    assert_eq!(c.frost_cache_bytes(), 0);
}

// ---- releasable window content (Stage F) -----------------------------

/// A gauge private to the content-release ladder tests: they move the band,
/// and the shared [`NORMAL_PRESSURE`] must stay at normal for the tests
/// running beside them.
static CONTENT_PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// A compositor whose content-release ladder is driven by
/// [`CONTENT_PRESSURE`], started at the normal band.
fn releasable_compositor(mode: DisplayMode, background: Color) -> Compositor {
    CONTENT_PRESSURE.report(PressureBand::Normal);
    NORMAL_PRESSURE.report(PressureBand::Normal);
    Compositor::new(
        mode,
        background,
        test_chrome_cache(),
        frost_cache(TEST_SEAT, TEST_FB_BYTES, &CONTENT_PRESSURE, &TEST_SINK),
        &CONTENT_PRESSURE,
    )
    .expect("compositor")
}

/// A decorated window whose pixels an app presents — the only kind the
/// release ladder ever takes.
fn app_window(c: &mut Compositor, x: i32, y: i32, client: u32, title: &str) -> WindowId {
    let id = titled_window(c, x, y, client, title);
    assert!(c.set_app_presented(id, true));
    id
}

/// Fill the whole of the window named by `id` with `color`, exactly as a
/// client presenting a full-window frame does.
fn present_full(c: &mut Compositor, id: WindowId, color: Color) {
    let filled = present_content(c, id, |surface| {
        let (w, h) = (surface.width(), surface.height());
        for y in 0..h {
            for x in 0..w {
                surface.set(x, y, color.premultiply());
            }
        }
        ((), Rect::new(0, 0, w, h))
    });
    assert!(filled.is_some(), "the present must reach the window");
}

#[test]
fn releasing_content_overwrites_the_pixels_before_dropping_them() {
    // A window's content is whatever the user was looking at, so the
    // release is a wipe and not a drop. Taking the spent buffer out is the
    // only way to witness bytes whose allocation is freed immediately
    // afterwards.
    let mut window = crate::Window::new(WindowId(1), Point::new(0, 0), opaque(8, 4, RED));
    assert!(window.has_content());
    assert!(window
        .content()
        .expect("content")
        .pixels()
        .iter()
        .all(|p| *p == RED.premultiply()));

    let spent = window.take_content_wiped().expect("the released buffer");
    assert!(
        spent.pixels().iter().all(|p| *p == Pixel::TRANSPARENT),
        "every content byte must be overwritten before the heap is reusable"
    );
    assert!(!window.has_content());
    assert!(window.take_content_wiped().is_none());

    // The window keeps everything but the pixels.
    assert_eq!(window.client_size(), (8, 4));
    assert_eq!(window.bounds(), Rect::new(0, 0, 8, 4));
}

#[test]
fn releasing_content_drops_the_retained_bytes_to_zero() {
    let mut c = releasable_compositor(mode(320, 240), BLUE);
    let id = app_window(&mut c, 20, 20, 60, "held");
    assert!(c.set_visible(id, false));
    c.composite();
    let held = c.content_bytes();
    assert_eq!(held, 60 * 60 * size_of::<Pixel>());

    CONTENT_PRESSURE.report(PressureBand::Mild);
    assert_eq!(c.release_content_under_pressure(None), held);
    assert_eq!(c.content_bytes(), 0);
    assert!(!c.window(id).expect("window").has_content());
    CONTENT_PRESSURE.report(PressureBand::Normal);
}

#[test]
fn a_released_window_composites_as_transparent_and_leaves_the_desktop_identical() {
    // The desktop shows through where the pixels were; every other pixel
    // on screen — background, furniture, the window beside it — is
    // untouched.
    let mut c = releasable_compositor(mode(200, 160), BLUE);
    let kept = app_window(&mut c, 10, 10, 40, "kept");
    present_full(&mut c, kept, GREEN);
    let dropped = app_window(&mut c, 120, 10, 40, "dropped");
    present_full(&mut c, dropped, RED);
    c.composite();

    let client = c.window_client_rect(dropped).expect("client rect");
    let before = c.frame().to_vec();

    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(Some(kept)) > 0);
    c.composite();
    CONTENT_PRESSURE.report(PressureBand::Normal);

    // The released window's client area is now the desktop background — except
    // at the corners its own rim curves through, which are frame, not client.
    let desktop = [BLUE.r, BLUE.g, BLUE.b, 255];
    let mut corner_drawn = false;
    for y in client.top()..client.bottom() {
        for x in client.left()..client.right() {
            let px = frame_pixel(&c, x.cast_unsigned(), y.cast_unsigned());
            if in_a_client_corner(&c, client, Point::new(x, y)) {
                corner_drawn |= px != desktop;
                continue;
            }
            assert_eq!(
                px, desktop,
                "the released client area must show the desktop at ({x}, {y})"
            );
        }
    }
    assert!(
        corner_drawn,
        "a released window still draws the curve its rim traces"
    );
    // Everything outside it is byte-for-byte what it was.
    let after = c.frame().to_vec();
    let stride = c.mode().stride_bytes;
    for y in 0..c.mode().height_px {
        for x in 0..c.mode().width_px {
            if client.contains(Point::new(x.cast_signed(), y.cast_signed())) {
                continue;
            }
            let off = (y * stride + x * 4) as usize;
            assert_eq!(
                before[off..off + 4],
                after[off..off + 4],
                "pixel ({x}, {y}) outside the released window must not move"
            );
        }
    }
}

#[test]
fn a_released_window_still_hit_tests_shows_furniture_focuses_and_resizes() {
    let mut c = releasable_compositor(mode(240, 200), BLUE);
    let id = app_window(&mut c, 20, 20, 60, "live");
    present_full(&mut c, id, RED);
    c.composite();
    let bounds = c.window(id).expect("window").bounds();
    let title_band = c.window(id).expect("window").furniture_bands()[0];

    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(None) > 0);
    CONTENT_PRESSURE.report(PressureBand::Normal);
    c.composite();

    // Still hit-tests over its whole outer rectangle.
    assert_eq!(
        c.window_at(Point::new(bounds.left(), bounds.top())),
        Some(id)
    );
    assert_eq!(
        c.window_at(Point::new(bounds.right() - 1, bounds.bottom() - 1)),
        Some(id)
    );
    // Still draws its furniture: the title band is painted, not desktop.
    let painted = (title_band.left()..title_band.right()).any(|x| {
        frame_pixel(&c, x.cast_unsigned(), title_band.top().cast_unsigned())
            != [BLUE.r, BLUE.g, BLUE.b, 255]
    });
    assert!(painted, "a released window still draws its furniture");
    // Still takes focus, and the activation flip still repaints furniture.
    assert!(c.set_active_frame(id, false));
    assert!(c.set_active_frame(id, true));
    // Still resizes: the retained client size follows, with no buffer to
    // grow.
    assert!(c.resize_window_client(id, 80, 50));
    assert_eq!(c.window(id).expect("window").client_size(), (80, 50));
    assert!(!c.window(id).expect("window").has_content());
    c.composite();
}

#[test]
fn a_full_window_present_after_release_restores_pixel_identical_content() {
    let mut c = releasable_compositor(mode(200, 160), BLUE);
    let id = app_window(&mut c, 20, 20, 50, "restored");
    present_full(&mut c, id, GREEN);
    c.composite();
    let before = c.frame().to_vec();

    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(None) > 0);
    CONTENT_PRESSURE.report(PressureBand::Normal);
    c.composite();
    assert_ne!(before, c.frame(), "the release must be visible");

    // The redraw the release asked for arrives as a full-window present.
    present_full(&mut c, id, GREEN);
    c.composite();
    assert_eq!(
        before,
        c.frame(),
        "a full-window present must restore the exact frame"
    );
}

#[test]
fn the_release_ladder_follows_the_pressure_band() {
    let mut c = releasable_compositor(mode(320, 240), BLUE);
    let focused = app_window(&mut c, 10, 10, 40, "focused");
    let unfocused = app_window(&mut c, 100, 10, 40, "unfocused");
    let hidden = app_window(&mut c, 200, 10, 40, "hidden");
    assert!(c.set_visible(hidden, false));
    c.composite();
    let _ = c.pending_redraws();

    let content = |c: &Compositor, id: WindowId| c.window(id).expect("window").has_content();

    // Normal: memory is plentiful and every release costs a repaint.
    CONTENT_PRESSURE.report(PressureBand::Normal);
    assert_eq!(c.release_content_under_pressure(Some(focused)), 0);
    assert!(content(&c, focused) && content(&c, unfocused) && content(&c, hidden));

    // Mild: only what nobody is looking at.
    CONTENT_PRESSURE.report(PressureBand::Mild);
    assert!(c.release_content_under_pressure(Some(focused)) > 0);
    assert!(content(&c, focused), "the focused window is never released");
    assert!(content(&c, unfocused), "a visible window survives mild");
    assert!(!content(&c, hidden), "a hidden window goes first");
    assert_eq!(c.release_content_under_pressure(Some(focused)), 0);

    // Critical: visible but unfocused goes too; the focused one never does.
    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(Some(focused)) > 0);
    assert!(
        content(&c, focused),
        "there would be nothing to show in the focused window's place"
    );
    assert!(!content(&c, unfocused));

    // With nothing focused, critical takes every window.
    present_full(&mut c, focused, RED);
    assert!(c.release_content_under_pressure(None) > 0);
    assert!(!content(&c, focused));
    CONTENT_PRESSURE.report(PressureBand::Normal);
}

#[test]
fn each_release_queues_exactly_one_redraw_request() {
    let mut c = releasable_compositor(mode(320, 240), BLUE);
    let visible = app_window(&mut c, 10, 10, 40, "visible");
    let hidden = app_window(&mut c, 120, 10, 40, "hidden");
    assert!(c.set_visible(hidden, false));
    c.composite();
    // Hiding a window that still holds its pixels asks for nothing.
    assert!(c.pending_redraws().is_empty());

    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(None) > 0);
    let mut queued = c.pending_redraws();
    queued.sort_unstable_by_key(|id| id.0);
    assert_eq!(queued, alloc::vec![visible, hidden]);
    assert!(
        c.pending_redraws().is_empty(),
        "draining must leave the queue empty"
    );

    // A second release with nothing left to give back asks for nothing.
    assert_eq!(c.release_content_under_pressure(None), 0);
    assert!(c.pending_redraws().is_empty());

    // Showing a window whose pixels are gone asks again, once.
    assert!(c.set_visible(hidden, true));
    assert_eq!(c.pending_redraws(), alloc::vec![hidden]);
    CONTENT_PRESSURE.report(PressureBand::Normal);
}

#[test]
fn an_app_that_ignores_the_redraw_request_leaves_its_window_blank_and_the_desktop_runs_on() {
    // The event is advisory: a client that never answers simply shows the
    // desktop through its client area. Nothing panics, nothing spins, and
    // every other window keeps compositing.
    let mut c = releasable_compositor(mode(240, 200), BLUE);
    let answering = app_window(&mut c, 10, 10, 40, "answers");
    present_full(&mut c, answering, GREEN);
    let silent = app_window(&mut c, 120, 10, 40, "silent");
    present_full(&mut c, silent, RED);
    c.composite();
    let _ = c.pending_redraws();

    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(None) > 0);
    CONTENT_PRESSURE.report(PressureBand::Normal);
    // Both apps were asked; only one of them answers.
    let mut asked = c.pending_redraws();
    asked.sort_unstable_by_key(|id| id.0);
    assert_eq!(asked, alloc::vec![answering, silent]);
    present_full(&mut c, answering, GREEN);

    for _ in 0..3 {
        c.composite();
    }
    let silent_client = c.window_client_rect(silent).expect("client rect");
    for y in silent_client.top()..silent_client.bottom() {
        for x in silent_client.left()..silent_client.right() {
            if in_a_client_corner(&c, silent_client, Point::new(x, y)) {
                continue;
            }
            assert_eq!(
                frame_pixel(&c, x.cast_unsigned(), y.cast_unsigned()),
                [BLUE.r, BLUE.g, BLUE.b, 255],
                "the silent window stays blank at ({x}, {y})"
            );
        }
    }
    let answered_client = c.window_client_rect(answering).expect("client rect");
    assert_eq!(
        frame_pixel(
            &c,
            answered_client.left().cast_unsigned(),
            answered_client.top().cast_unsigned()
        ),
        [GREEN.r, GREEN.g, GREEN.b, 255]
    );
    // The blank window is still a window: it hit-tests and holds its size.
    assert_eq!(
        c.window_at(Point::new(silent_client.left(), silent_client.top())),
        Some(silent)
    );
    // Nothing was queued again by merely compositing a blank window.
    assert!(c.pending_redraws().is_empty());
}

#[test]
fn tearing_content_down_wipes_every_window_and_asks_nobody_to_redraw() {
    // The seat is going away, so there is nobody left to present: the
    // pixels are overwritten and no request is raised.
    let mut c = releasable_compositor(mode(240, 200), BLUE);
    let a = app_window(&mut c, 10, 10, 40, "a");
    let b = app_window(&mut c, 120, 10, 40, "b");
    present_full(&mut c, a, RED);
    present_full(&mut c, b, GREEN);
    c.composite();
    assert!(c.content_bytes() > 0);

    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(None) > 0);
    c.teardown_content();
    CONTENT_PRESSURE.report(PressureBand::Normal);
    assert_eq!(c.content_bytes(), 0);
    assert!(!c.window(a).expect("window").has_content());
    assert!(!c.window(b).expect("window").has_content());
    assert!(
        c.pending_redraws().is_empty(),
        "a torn-down seat has nobody to present"
    );
}

#[test]
fn a_window_the_embedder_paints_itself_is_never_released() {
    // The taskbar, a session dialog, the lock screen: nobody would answer
    // a redraw request for them, so releasing their pixels would blank
    // them permanently. An un-declared window keeps every pixel.
    let mut c = releasable_compositor(mode(240, 200), BLUE);
    let session_painted = titled_window(&mut c, 10, 10, 40, "bar");
    let app = app_window(&mut c, 120, 10, 40, "app");
    assert!(!c
        .window(session_painted)
        .expect("window")
        .is_app_presented());
    c.composite();

    CONTENT_PRESSURE.report(PressureBand::Critical);
    assert!(c.release_content_under_pressure(None) > 0);
    CONTENT_PRESSURE.report(PressureBand::Normal);
    assert!(
        c.window(session_painted).expect("window").has_content(),
        "a window nobody can redraw must keep its pixels"
    );
    assert!(!c.window(app).expect("window").has_content());
    assert_eq!(c.pending_redraws(), alloc::vec![app]);

    // Hiding it does not change that: even invisible, no client would
    // present it again.
    assert!(c.set_visible(session_painted, false));
    CONTENT_PRESSURE.report(PressureBand::Mild);
    assert_eq!(c.release_content_under_pressure(None), 0);
    CONTENT_PRESSURE.report(PressureBand::Normal);
    assert!(c.window(session_painted).expect("window").has_content());
    assert!(c.pending_redraws().is_empty());
}

// --- The desktop layer ----------------------------------------------------

#[test]
fn the_desktop_layer_draws_over_the_background_and_under_every_window() {
    let mut c = new_compositor(mode(20, 20), BLUE).expect("compositor");
    c.set_desktop(opaque(20, 20, GREEN));
    let win = c.add_window(Point::new(0, 0), opaque(4, 4, RED));
    c.composite();

    // Under the window the window wins; everywhere else the desktop layer
    // covers the background entirely.
    assert_eq!(frame_pixel(&c, 1, 1), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 10, 10), [0, 255, 0, 255]);

    // Raising or hiding a window cannot put anything beneath the layer: the
    // layer has no place in the z-order at all.
    assert!(c.set_visible(win, false));
    c.composite();
    assert_eq!(frame_pixel(&c, 1, 1), [0, 255, 0, 255]);
}

#[test]
fn a_desktop_layer_smaller_than_the_screen_leaves_the_background_showing() {
    let mut c = new_compositor(mode(20, 20), BLUE).expect("compositor");
    c.set_desktop(opaque(8, 8, GREEN));
    assert_eq!(c.desktop_bounds(), Some(Rect::new(0, 0, 8, 8)));
    c.composite();

    assert_eq!(frame_pixel(&c, 4, 4), [0, 255, 0, 255]);
    assert_eq!(frame_pixel(&c, 12, 4), [0, 0, 255, 255], "past its width");
    assert_eq!(frame_pixel(&c, 4, 12), [0, 0, 255, 255], "past its height");
}

#[test]
fn setting_and_clearing_the_desktop_layer_damages_exactly_what_it_covered() {
    let mut c = new_compositor(mode(20, 20), BLUE).expect("compositor");
    c.composite();
    assert!(!c.has_damage());

    c.set_desktop(opaque(8, 8, GREEN));
    assert!(c.has_damage(), "installing a layer repaints its footprint");
    c.composite();
    assert_eq!(frame_pixel(&c, 4, 4), [0, 255, 0, 255]);

    // Replacing it damages both the old footprint and the new one, so a
    // shrinking layer cannot leave its old pixels behind.
    c.set_desktop(opaque(4, 4, RED));
    c.composite();
    assert_eq!(frame_pixel(&c, 2, 2), [255, 0, 0, 255]);
    assert_eq!(
        frame_pixel(&c, 6, 6),
        [0, 0, 255, 255],
        "the old layer is gone"
    );

    assert!(c.clear_desktop(), "a layer was installed");
    c.composite();
    assert_eq!(frame_pixel(&c, 2, 2), [0, 0, 255, 255]);
    assert!(!c.clear_desktop(), "clearing twice changes nothing");
    assert!(!c.has_damage());
}

#[test]
fn repainting_the_desktop_layer_reuses_its_buffer_and_damages_its_footprint() {
    let mut c = new_compositor(mode(20, 20), BLUE).expect("compositor");
    c.composite();
    assert!(!c.has_damage());

    // With no layer installed the first repaint allocates one at exactly the
    // screen's extent, whatever the painter chooses to draw into it.
    assert!(c.repaint_desktop(|surface| surface.fill(GREEN)));
    assert_eq!(c.desktop_bounds(), Some(Rect::new(0, 0, 20, 20)));
    assert!(c.has_damage());
    c.composite();
    assert_eq!(frame_pixel(&c, 10, 10), [0, 255, 0, 255]);

    // A second repaint paints into that very buffer: the painter sees the
    // pixels the previous one left, which is what lets a wallpapered desktop
    // touch only the tiles that changed.
    assert!(c.repaint_desktop(|surface| {
        assert_eq!(surface.get(10, 10), Some(GREEN.premultiply()));
        surface.fill_rect(0, 0, 4, 4, RED);
    }));
    c.composite();
    assert_eq!(frame_pixel(&c, 2, 2), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 10, 10), [0, 255, 0, 255], "kept its pixels");
}

#[test]
fn repainting_the_desktop_layer_re_allocates_when_the_screen_size_changed() {
    let mut c = new_compositor(mode(20, 20), BLUE).expect("compositor");
    // A layer installed at some other extent (a mode change, or an owner that
    // installed a partial layer) is replaced by a screen-sized one rather
    // than painted into at the wrong size.
    c.set_desktop(opaque(8, 8, GREEN));
    assert!(c.repaint_desktop(|surface| {
        assert_eq!((surface.width(), surface.height()), (20, 20));
        surface.fill(RED);
    }));
    assert_eq!(c.desktop_bounds(), Some(Rect::new(0, 0, 20, 20)));
    c.composite();
    assert_eq!(frame_pixel(&c, 18, 18), [255, 0, 0, 255]);
}

#[test]
fn the_accelerated_scene_carries_the_desktop_layer_beneath_the_windows() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    c.set_desktop(opaque(16, 16, GREEN));
    c.add_window(Point::new(2, 2), opaque(4, 4, RED));
    let mut display = MockAccel::new(mode(16, 16), generous_caps());

    c.present_accelerated(&mut display)
        .expect("accelerated present");

    // Back to front: background, the desktop layer, then the window — the
    // same order the software path blends them in.
    assert_eq!(display.layers.len(), 3, "background + desktop + window");
    let desktop = &display.layers[1];
    assert_eq!(
        (desktop.width, desktop.height, desktop.dst_x, desktop.dst_y),
        (16, 16, 0, 0)
    );
    assert_eq!(layer_pixel(desktop, 8, 8), [0, 255, 0, 255]);
    let win = &display.layers[2];
    assert_eq!((win.width, win.height, win.dst_x, win.dst_y), (4, 4, 2, 2));
}

#[test]
fn a_new_mode_is_adopted_whole_and_keeps_the_served_windows() {
    // A desktop resumed onto a different monitor keeps its apps.
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    let win = c.add_window(Point::new(2, 2), opaque(4, 4, RED));
    c.composite();

    assert!(c.set_mode(mode(32, 24)));

    assert_eq!(c.mode(), mode(32, 24));
    assert_eq!(c.screen_rect(), Rect::new(0, 0, 32, 24));
    assert_eq!(c.frame().len(), 32 * 4 * 24);
    assert!(c.window(win).is_some(), "the served window survives");
    // The whole new screen is damaged, so the first frame after a re-mode is
    // complete rather than a patch of the old one.
    let damage = c.composite();
    assert_eq!(damage.bounds(), Rect::new(0, 0, 32, 24));
    assert_eq!(frame_pixel(&c, 4, 4), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 30, 22), [0, 0, 255, 255]);
}

#[test]
fn a_window_outside_the_smaller_new_screen_is_clipped_not_lost() {
    let mut c = new_compositor(mode(32, 32), BLUE).expect("compositor");
    let win = c.add_window(Point::new(20, 20), opaque(8, 8, RED));

    assert!(c.set_mode(mode(16, 16)));

    assert!(c.window(win).is_some());
    c.composite();
    assert_eq!(frame_pixel(&c, 15, 15), [0, 0, 255, 255]);
}

#[test]
fn re_adopting_the_same_mode_costs_nothing_and_changes_nothing() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    c.composite();

    assert!(c.set_mode(mode(16, 16)));

    // No damage was raised, so an unchanged mode does not force a full
    // repaint of a screen that already holds the right pixels.
    assert!(c.composite().bounds().is_empty());
}

#[test]
fn a_mode_that_cannot_be_drawn_is_refused_and_leaves_the_compositor_intact() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    c.add_window(Point::new(2, 2), opaque(4, 4, RED));
    c.composite();

    // A stride too small for one scanline, and an extent with no pixels:
    // both leave the old mode in force rather than half-adopting one the
    // compositor would scan out as garbage.
    let short_stride = DisplayMode {
        stride_bytes: 8,
        ..mode(16, 16)
    };
    assert!(!c.set_mode(short_stride));
    assert!(!c.set_mode(mode(0, 16)));

    assert_eq!(c.mode(), mode(16, 16));
    assert_eq!(c.frame().len(), 16 * 4 * 16);
    assert_eq!(frame_pixel(&c, 4, 4), [255, 0, 0, 255]);
}

// ---- screen reveal ---------------------------------------------------

/// A 16×12 scene taking every path a composed pixel has to the scan-out
/// frame: the root fill, a desktop layer, an opaque window, a frosted one
/// (whose rectangle is composed in two segments, the second continuing over
/// the first's blurred result), a translucent one, and the cursor on top.
fn revealable_scene() -> Compositor {
    let mut c = new_compositor(mode(16, 12), BLUE).expect("compositor");
    c.set_desktop(opaque(16, 12, GREEN));
    c.add_window(Point::new(1, 1), opaque(6, 6, RED));
    let glass = c.add_window(Point::new(4, 2), clear(8, 8));
    assert!(c.set_backdrop_blur(glass, 2));
    let sheer = c.add_window(Point::new(9, 5), opaque(5, 5, RED));
    assert!(c.set_opacity(sheer, 128));
    c.set_cursor(solid_cursor(4, GREEN), Point::new(11, 7));
    c.composite();
    c
}

/// Every scan-out pixel of `c` in row-major order.
fn frame_pixels(c: &Compositor) -> alloc::vec::Vec<[u8; 4]> {
    let info = c.mode();
    (0..info.height_px)
        .flat_map(|y| (0..info.width_px).map(move |x| (x, y)))
        .map(|(x, y)| frame_pixel(c, x, y))
        .collect()
}

#[test]
fn a_fully_revealed_screen_is_the_frame_the_compositor_always_produced() {
    let untouched = revealable_scene();
    let mut revealed = revealable_scene();

    // The strength already in force: no pixel changes and no frame is owed,
    // so a desktop that never fades pays nothing for the reveal at all.
    assert!(!revealed.set_reveal(u8::MAX));
    assert!(!revealed.has_damage());
    assert_eq!(revealed.frame(), untouched.frame());
}

#[test]
fn a_completed_reveal_restores_every_byte_of_the_frame() {
    let untouched = revealable_scene();
    let mut fading = revealable_scene();

    assert!(fading.set_reveal(0));
    fading.composite();
    assert!(fading.set_reveal(96));
    fading.composite();
    assert!(fading.set_reveal(u8::MAX));
    fading.composite();

    assert_eq!(
        fading.frame(),
        untouched.frame(),
        "the dimming never touched the composed colour it was applied to"
    );
}

#[test]
fn a_half_reveal_scales_every_composed_pixel_towards_black() {
    let lit = frame_pixels(&revealable_scene());
    let mut c = revealable_scene();

    assert!(c.set_reveal(128));
    c.composite();

    for (i, (dim, lit)) in frame_pixels(&c).iter().zip(&lit).enumerate() {
        let expected = [
            div255(u32::from(lit[0]) * 128),
            div255(u32::from(lit[1]) * 128),
            div255(u32::from(lit[2]) * 128),
            lit[3],
        ];
        assert_eq!(*dim, expected, "pixel {i}");
    }
    // Applied on the way out, so the composed colour a later frame blends
    // against — a frosted backdrop, a continuing segment — is undimmed.
    assert_eq!(
        c.back_buffer().get(0, 0),
        revealable_scene().back_buffer().get(0, 0)
    );
}

#[test]
fn a_reveal_of_zero_presents_a_black_screen() {
    let mut c = revealable_scene();

    assert!(c.set_reveal(0));
    c.composite();

    for (i, pixel) in frame_pixels(&c).iter().enumerate() {
        assert_eq!(
            *pixel,
            [0, 0, 0, 255],
            "pixel {i} is black and still opaque"
        );
    }
}

#[test]
fn the_premultiplied_invariant_holds_at_every_reveal_strength() {
    let mut c = revealable_scene();
    let mut previous = frame_pixels(&c);

    for strength in [192u8, 128, 64, 1, 0] {
        assert!(c.set_reveal(strength));
        c.composite();
        let now = frame_pixels(&c);
        for (i, (pixel, was)) in now.iter().zip(&previous).enumerate() {
            let [red, green, blue, alpha] = *pixel;
            assert_eq!(
                alpha, was[3],
                "pixel {i}: alpha is not the reveal's to scale"
            );
            assert!(
                red <= alpha && green <= alpha && blue <= alpha,
                "pixel {i} left premultiplied range at {strength}"
            );
            assert!(
                red <= was[0] && green <= was[1] && blue <= was[2],
                "pixel {i} brightened as the screen darkened at {strength}"
            );
        }
        previous = now;
    }
}

#[test]
fn changing_the_reveal_repaints_the_whole_screen_and_repeating_it_repaints_nothing() {
    let mut c = revealable_scene();
    assert!(!c.has_damage());

    // Every pixel's presented value changed, so every pixel is owed.
    assert!(c.set_reveal(64));
    assert_eq!(composite_checked(&mut c).bounds(), c.screen_rect());

    assert!(!c.set_reveal(64));
    assert!(!c.has_damage());
}

#[test]
fn the_hardware_layer_path_declines_while_a_reveal_is_in_flight() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    c.add_window(Point::new(2, 2), opaque(4, 4, RED));
    let mut display = MockAccel::new(mode(16, 16), generous_caps());

    assert!(c.set_reveal(128));
    c.present_accelerated(&mut display).expect("present");

    assert!(
        display.layers.is_empty(),
        "a layer the engine scans out directly would skip the dimming"
    );
    assert_eq!(display.software_frame.len(), 16 * 16 * 4);
    assert_eq!(
        frame_pixel(&c, 4, 4),
        [div255(255 * 128), 0, 0, 255],
        "the software fallback carried the reveal"
    );

    assert!(c.set_reveal(u8::MAX));
    c.present_accelerated(&mut display).expect("present");
    assert_eq!(
        display.layers.len(),
        2,
        "background + window once the fade is over"
    );
}

#[test]
fn a_mode_change_keeps_a_reveal_in_flight() {
    let mut c = new_compositor(mode(16, 16), BLUE).expect("compositor");
    assert!(c.set_reveal(0));

    assert!(c.set_mode(mode(24, 20)));

    assert_eq!(
        c.reveal(),
        0,
        "a session that re-modes mid-fade keeps fading"
    );
    c.composite();
    assert_eq!(frame_pixel(&c, 20, 18), [0, 0, 0, 255]);
}
