//! Headless unit tests for the compositor core.

extern crate alloc;

use rustos_abi::driver::display::{Display, DisplayFormat, DisplayMode};
use rustos_abi::DriverError;

use crate::color::{Color, Pixel};
use crate::corner::Corners;
use crate::damage::DamageRegion;
use crate::geometry::{Point, Rect};
use crate::surface::Surface;
use crate::Compositor;

use rustos_theme::{ThemeId, ThemeRegistry};

fn mode(w: u32, h: u32) -> DisplayMode {
    DisplayMode {
        width_px: w,
        height_px: h,
        stride_bytes: w * 4,
        format: DisplayFormat::Rgba8888,
    }
}

fn opaque(w: u32, h: u32, color: Color) -> Surface {
    Surface::filled(w, h, color.premultiply()).expect("surface allocates")
}

/// Read the RGBA scan-out bytes of frame pixel `(x, y)`.
fn frame_pixel(comp: &Compositor, x: u32, y: u32) -> [u8; 4] {
    let info = comp.mode();
    let off = (y * info.stride_bytes + x * 4) as usize;
    let frame = comp.frame();
    [frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]
}

/// A display seam that records the last presented frame, or always
/// fails when `fail` is set.
struct MockDisplay {
    mode: DisplayMode,
    last: alloc::vec::Vec<u8>,
    fail: bool,
}

impl MockDisplay {
    fn new(mode: DisplayMode) -> Self {
        Self {
            mode,
            last: alloc::vec::Vec::new(),
            fail: false,
        }
    }
}

impl Display for MockDisplay {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }

    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        if self.fail {
            return Err(DriverError::DeviceFault);
        }
        self.last = frame.to_vec();
        Ok(())
    }
}

const BLUE: Color = Color::rgb(0, 0, 255);
const RED: Color = Color::rgb(255, 0, 0);

// ---- colour / blending ----------------------------------------------

#[test]
fn premultiply_opaque_is_identity() {
    assert_eq!(
        RED.premultiply(),
        Pixel {
            r: 255,
            g: 0,
            b: 0,
            a: 255
        }
    );
}

#[test]
fn premultiply_transparent_clears_colour() {
    assert_eq!(
        Color::rgba(255, 255, 255, 0).premultiply(),
        Pixel::TRANSPARENT
    );
}

#[test]
fn over_opaque_source_replaces_destination() {
    let src = RED.premultiply();
    let dst = BLUE.premultiply();
    assert_eq!(src.over(dst), src);
}

#[test]
fn over_transparent_source_keeps_destination() {
    let dst = BLUE.premultiply();
    assert_eq!(Pixel::TRANSPARENT.over(dst), dst);
}

#[test]
fn over_half_alpha_blends_premultiplied() {
    let src = Color::rgba(255, 0, 0, 128).premultiply();
    let dst = BLUE.premultiply();
    assert_eq!(
        src.over(dst),
        Pixel {
            r: 128,
            g: 0,
            b: 127,
            a: 255
        }
    );
}

#[test]
fn scale_alpha_extremes() {
    let p = RED.premultiply();
    assert_eq!(p.scale_alpha(255), p);
    assert_eq!(p.scale_alpha(0), Pixel::TRANSPARENT);
}

#[test]
fn scale_alpha_half() {
    assert_eq!(
        RED.premultiply().scale_alpha(128),
        Pixel {
            r: 128,
            g: 0,
            b: 0,
            a: 128
        }
    );
}

#[test]
fn unpremultiply_round_trips_opaque() {
    let c = Color::rgb(17, 200, 240);
    assert_eq!(c.premultiply().unpremultiply(), c);
}

#[test]
fn unpremultiply_transparent_is_transparent() {
    assert_eq!(Pixel::TRANSPARENT.unpremultiply(), Color::TRANSPARENT);
}

// The `Point`/`Rect` primitives are unit-tested in their own crate
// (`lib/geometry`); the compositor tests below exercise how the window
// manager *uses* them (damage, z-order, hit-testing).

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

// ---- surface ---------------------------------------------------------

#[test]
fn new_surface_is_transparent() {
    let s = Surface::new(3, 2).expect("allocates");
    assert_eq!(s.width(), 3);
    assert_eq!(s.height(), 2);
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn surface_get_set_bounds() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.set(1, 1, RED.premultiply());
    assert_eq!(s.get(1, 1), Some(RED.premultiply()));
    assert_eq!(s.get(2, 0), None);
    s.set(9, 9, RED.premultiply()); // out of bounds: ignored
}

#[test]
fn fill_rect_is_clipped() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.fill_rect(2, 2, 10, 10, RED.premultiply().unpremultiply());
    assert_eq!(s.get(3, 3), Some(RED.premultiply()));
    assert_eq!(s.get(0, 0), Some(Pixel::TRANSPARENT));
}

// ---- damage ----------------------------------------------------------

#[test]
fn damage_ignores_empty_rects() {
    let mut d = DamageRegion::new();
    d.add(Rect::EMPTY);
    assert!(d.is_empty());
}

#[test]
fn damage_tracks_bounds_and_membership() {
    let mut d = DamageRegion::new();
    d.add(Rect::new(0, 0, 2, 2));
    d.add(Rect::new(4, 4, 2, 2));
    assert_eq!(d.bounds(), Rect::new(0, 0, 6, 6));
    assert!(d.covers(Point::new(1, 1)));
    assert!(!d.covers(Point::new(3, 3)));
    assert_eq!(d.rects().len(), 2);
}

#[test]
fn damage_clear_empties() {
    let mut d = DamageRegion::new();
    d.add(Rect::new(0, 0, 1, 1));
    d.clear();
    assert!(d.is_empty());
    assert_eq!(d.bounds(), Rect::EMPTY);
}

// ---- compositor ------------------------------------------------------

#[test]
fn new_rejects_zero_size() {
    assert!(Compositor::new(mode(0, 4), BLUE).is_none());
    assert!(Compositor::new(mode(4, 0), BLUE).is_none());
}

#[test]
fn new_rejects_short_stride() {
    let bad = DisplayMode {
        stride_bytes: 4,
        ..mode(4, 4)
    };
    assert!(Compositor::new(bad, BLUE).is_none());
}

#[test]
fn background_fills_screen() {
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]);
    assert_eq!(frame_pixel(&c, 1, 1), [0, 0, 255, 255]);
}

#[test]
fn bgra_channel_order_is_honoured() {
    let m = DisplayMode {
        format: DisplayFormat::Bgra8888,
        ..mode(2, 2)
    };
    let mut c = Compositor::new(m, BLUE).expect("compositor");
    c.composite();
    // Blue in BGRA is byte order B,G,R,A.
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]);
}

#[test]
fn opaque_window_overwrites_background() {
    let mut c = Compositor::new(mode(4, 4), BLUE).expect("compositor");
    c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]); // background
    assert_eq!(frame_pixel(&c, 1, 1), [255, 0, 0, 255]); // window
    assert_eq!(frame_pixel(&c, 2, 2), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 3, 3), [0, 0, 255, 255]); // background again
}

#[test]
fn top_window_wins_z_order() {
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(2, 2, RED));
    c.add_window(Point::ORIGIN, opaque(2, 2, Color::rgb(0, 255, 0)));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 255, 0, 255]); // green on top
}

#[test]
fn raise_changes_z_order() {
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
    let bottom = c.add_window(Point::ORIGIN, opaque(2, 2, RED));
    c.add_window(Point::ORIGIN, opaque(2, 2, Color::rgb(0, 255, 0)));
    assert!(c.raise(bottom));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]); // red raised to top
}

#[test]
fn semi_transparent_window_blends_with_background() {
    let mut c = Compositor::new(mode(1, 1), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(1, 2), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, surface);
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]); // opaque row
    assert_eq!(frame_pixel(&c, 0, 1), [128, 0, 127, 255]); // blended row
}

#[test]
fn set_opacity_makes_window_translucent() {
    let mut c = Compositor::new(mode(1, 1), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.set_opacity(id, 128));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [128, 0, 127, 255]);
}

#[test]
fn rounded_window_shows_background_at_corner() {
    let mut c = Compositor::new(mode(20, 20), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(20, 20, RED));
    assert!(c.set_corners(id, Corners::Rounded { radius: 8 }));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]); // corner clipped to bg
    assert_eq!(frame_pixel(&c, 10, 10), [255, 0, 0, 255]); // centre opaque
}

#[test]
fn hidden_window_is_not_composited() {
    let mut c = Compositor::new(mode(1, 1), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.set_visible(id, false));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]);
}

#[test]
fn removed_window_disappears() {
    let mut c = Compositor::new(mode(1, 1), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    assert!(c.remove(id));
    assert_eq!(c.window_count(), 0);
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]);
}

#[test]
fn move_window_repaints_old_and_new() {
    let mut c = Compositor::new(mode(4, 1), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
    let ghost = c.add_window(Point::ORIGIN, opaque(1, 1, RED));
    c.remove(ghost);
    assert!(!c.move_window(ghost, Point::new(1, 1)));
    assert!(!c.set_opacity(ghost, 0));
    assert!(!c.raise(ghost));
}

#[test]
fn back_buffer_holds_premultiplied_pixels() {
    let mut c = Compositor::new(mode(1, 1), BLUE).expect("compositor");
    c.composite();
    assert_eq!(c.back_buffer().get(0, 0), Some(BLUE.premultiply()));
}

// ---- present seam ----------------------------------------------------

#[test]
fn present_composites_then_writes_frame() {
    let m = mode(2, 2);
    let mut c = Compositor::new(m, BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(2, 2, RED));
    let mut display = MockDisplay::new(m);
    assert!(c.present(&mut display).is_ok());
    assert_eq!(display.last, c.frame());
    assert!(!c.has_damage());
}

#[test]
fn present_propagates_driver_error() {
    let m = mode(2, 2);
    let mut c = Compositor::new(m, BLUE).expect("compositor");
    let mut display = MockDisplay::new(m);
    display.fail = true;
    assert_eq!(c.present(&mut display), Err(DriverError::DeviceFault));
}

// ---- shared theme integration (lib/theme) ---------------------------

#[test]
fn active_theme_drives_compositor_background() {
    // The compositor sources its root background from the active theme,
    // and a runtime theme switch (dark -> light) changes the colour the
    // screen clears to. One shared definition drives the WM (§10).
    let mut themes = ThemeRegistry::with_builtins();
    let dark_bg = themes.active().palette().desktop;
    let mut c = Compositor::new(mode(2, 2), dark_bg.into()).expect("compositor");
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), dark_bg.to_array());

    themes
        .set_active(ThemeId::LIGHT)
        .expect("light is built in");
    let light_bg = themes.active().palette().desktop;
    assert_ne!(light_bg, dark_bg);
    let mut c = Compositor::new(mode(2, 2), light_bg.into()).expect("compositor");
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), light_bg.to_array());
}

#[test]
fn theme_corner_radius_shapes_windows() {
    // A window takes its corner radius from the active theme's metrics
    // through the single compositor rounded-corner path (§2.2): the
    // rounded corner still reveals the background behind it.
    let themes = ThemeRegistry::with_builtins();
    let radius = themes.active().metrics().window_corner_radius;
    let corners = Corners::from_radius(radius);
    assert_eq!(corners, Corners::Rounded { radius });
    assert_eq!(Corners::from_radius(0), Corners::Square);

    let mut c = Compositor::new(mode(20, 20), BLUE).expect("compositor");
    let id = c.add_window(Point::ORIGIN, opaque(20, 20, RED));
    assert!(c.set_corners(id, corners));
    c.composite();
    assert_eq!(frame_pixel(&c, 0, 0), [0, 0, 255, 255]); // corner clipped to bg
    assert_eq!(frame_pixel(&c, 10, 10), [255, 0, 0, 255]); // centre opaque
}
