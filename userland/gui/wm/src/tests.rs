//! Headless unit tests for the compositor core.

extern crate alloc;

use rustos_abi::driver::display::{Display, DisplayFormat, DisplayMode};
use rustos_abi::DriverError;

use crate::color::Color;
use crate::corner::Corners;
use crate::damage::DamageRegion;
use crate::geometry::{Point, Rect};
use crate::surface::Surface;
use crate::{Compositor, WindowId};

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

// ---- input routing ---------------------------------------------------

use crate::input::{InputEvent, InputResponse, InputRouter, PointerButton};

fn press_primary() -> InputEvent {
    InputEvent::PointerPressed {
        button: PointerButton::Primary,
    }
}

fn release_primary() -> InputEvent {
    InputEvent::PointerReleased {
        button: PointerButton::Primary,
    }
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

#[test]
fn hit_test_picks_top_most_visible_window() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    let bottom = c.add_window(Point::new(0, 0), opaque(30, 30, RED));
    let top = c.add_window(Point::new(20, 0), opaque(20, 30, RED));
    let mut router = InputRouter::new();

    // Press on the bottom window where the top does not cover it.
    let r = router.handle(moved(5, 5), &mut c);
    assert_eq!(r, InputResponse::Ignored);
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
fn press_on_desktop_clears_focus() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
fn non_primary_buttons_do_not_change_focus() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    c.add_window(Point::new(0, 0), opaque(10, 10, RED));
    let mut router = InputRouter::new();

    router.handle(moved(5, 5), &mut c);
    let r = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut c,
    );
    assert_eq!(r, InputResponse::Ignored);
    assert_eq!(router.focused(), None);
}

#[test]
fn move_grab_drags_focused_window() {
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
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
    assert_eq!(router.handle(moved(60, 60), &mut c), InputResponse::Ignored);
    assert_eq!(
        c.window(win).map(super::window::Window::origin),
        Some(Point::new(30, 18))
    );
}

#[test]
fn begin_move_fails_closed_without_focus() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    c.add_window(Point::new(0, 0), opaque(10, 10, RED));
    let mut router = InputRouter::new();

    assert!(!router.begin_move(&c));
    assert!(!router.is_moving());
}

#[test]
fn drag_ends_if_grabbed_window_removed() {
    let mut c = Compositor::new(mode(60, 60), BLUE).expect("compositor");
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
fn pointer_position_tracks_motion() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    assert_eq!(router.pointer(), Point::ORIGIN);
    router.handle(moved(7, 9), &mut c);
    assert_eq!(router.pointer(), Point::new(7, 9));
}

/// A solid opaque `size`×`size` cursor image in `color`, hotspot at the
/// top-left, built through the shared cursor library.
fn solid_cursor(size: u32, color: Color) -> rustos_cursor::CursorImage {
    use rustos_cursor::{Shape, VectorCursor, Vertex};
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
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    c.set_cursor(solid_cursor(8, RED), Point::new(10, 10));
    assert_eq!(c.cursor_bounds(), Some(Rect::new(10, 10, 8, 8)));
    c.composite();
    // Under the cursor: red. Away from it: the blue desktop.
    assert_eq!(frame_pixel(&c, 12, 12), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 30, 30), [0, 0, 255, 255]);
}

#[test]
fn cursor_overlay_draws_above_windows() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    let green = Color::rgb(0, 255, 0);
    c.add_window(Point::new(0, 0), opaque(40, 40, green));
    c.set_cursor(solid_cursor(8, RED), Point::new(4, 4));
    c.composite();
    assert_eq!(frame_pixel(&c, 5, 5), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 30, 30), [0, 255, 0, 255]);
}

#[test]
fn moving_the_cursor_restores_pixels_behind_it() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    assert!(!c.move_cursor(Point::new(5, 5)));
    assert!(!c.hide_cursor());
    assert_eq!(c.cursor_bounds(), None);
}

#[test]
fn replacing_the_cursor_image_marks_both_footprints_dirty() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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

// ---- cursor selection from interaction state -------------------------

use crate::select::{desired_cursor, CursorController};
use rustos_cursor::{CursorRegistry, CursorSetId, CursorTheme};
use rustos_geometry::Scale;
use rustos_theme::CursorKind;

#[test]
fn window_cursor_hint_round_trips_and_unknown_id_fails_closed() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(60, 60), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(30, 30, RED));
    assert!(c.set_window_cursor(win, CursorKind::Text));
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new();

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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
    let win = c.add_window(Point::new(10, 10), opaque(30, 30, RED));
    assert!(c.set_window_cursor(win, CursorKind::Text));
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new();

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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
    let router = InputRouter::new();
    let mut ctrl = CursorController::new();

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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new();

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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
    let mut router = InputRouter::new();
    let mut ctrl = CursorController::new();

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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
    assert_eq!(c.scale(), Scale::ONE);

    let bigger = Scale::from_percent(200).expect("valid scale");
    assert!(c.set_scale(bigger), "a new scale changes the output");
    assert_eq!(c.scale(), bigger);

    // Setting the scale already in effect is a no-op the embedder can skip.
    assert!(!c.set_scale(bigger));
}

#[test]
fn setting_the_output_scale_marks_the_whole_screen_dirty() {
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(80, 80), BLUE).expect("compositor");
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
