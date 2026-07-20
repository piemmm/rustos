//! Headless unit tests for the compositor core.

extern crate alloc;

use tairix_abi::driver::display::{
    AccelCaps, AccelLayer, AcceleratedDisplay, Display, DisplayFormat, DisplayMode,
};
use tairix_abi::DriverError;

use crate::color::Color;
use crate::corner::Corners;
use crate::damage::DamageRegion;
use crate::geometry::{Point, Rect};
use crate::surface::Surface;
use crate::{Compositor, WindowId};

use tairix_theme::{ThemeId, ThemeRegistry};

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

#[test]
fn damage_coalesces_repeated_rectangles() {
    // The window-open path marks the same window rectangle dirty several
    // times (open, focus, surface update) and the taskbar its bar
    // rectangle several times per present. Recomposition is per
    // rectangle, so without coalescing that region would be composited
    // once per duplicate — the defect this guards. The same rectangle
    // added many times must collapse to exactly one.
    let mut d = DamageRegion::new();
    let window = Rect::new(80, 80, 900, 620);
    for _ in 0..6 {
        d.add(window);
    }
    assert_eq!(d.rects(), &[window]);
    assert_eq!(d.bounds(), window);
}

#[test]
fn damage_coalesces_overlapping_into_their_union() {
    // Two overlapping updates (a window's old and new position after a
    // move) merge into one rectangle so the overlap is not composited
    // twice.
    let mut d = DamageRegion::new();
    d.add(Rect::new(0, 0, 4, 4));
    d.add(Rect::new(2, 2, 4, 4));
    assert_eq!(d.rects(), &[Rect::new(0, 0, 6, 6)]);
}

#[test]
fn damage_bridging_rectangle_merges_a_disjoint_pair() {
    // A later rectangle overlapping two previously-disjoint rectangles
    // draws all three into a single union, never leaving a stale
    // duplicate behind.
    let mut d = DamageRegion::new();
    d.add(Rect::new(0, 0, 2, 2));
    d.add(Rect::new(6, 0, 2, 2));
    assert_eq!(d.rects().len(), 2);
    d.add(Rect::new(0, 0, 8, 2));
    assert_eq!(d.rects(), &[Rect::new(0, 0, 8, 2)]);
}

#[test]
fn duplicate_damage_composites_a_window_once_and_correctly() {
    // Marking a window's rectangle dirty repeatedly must not change the
    // composited result (and, with coalescing, composites it once): the
    // frame is identical to a single clean composite.
    let mut once = Compositor::new(mode(4, 4), BLUE).expect("compositor");
    once.add_window(Point::new(1, 1), opaque(2, 2, RED));
    once.composite();

    let mut many = Compositor::new(mode(4, 4), BLUE).expect("compositor");
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
fn set_background_repaints_the_whole_screen() {
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
    c.composite();

    assert!(c.set_background(RED));
    assert_eq!(c.background(), RED);
    assert!(c.has_damage(), "a changed background dirties the screen");
    assert_eq!(c.composite(), Rect::new(0, 0, 2, 2));
    assert_eq!(frame_pixel(&c, 0, 0), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 1, 1), [255, 0, 0, 255]);
}

#[test]
fn set_background_same_colour_is_a_no_op() {
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
    c.composite();

    assert!(!c.set_background(BLUE));
    assert!(!c.has_damage(), "an unchanged background dirties nothing");
}

#[test]
fn set_background_forces_opaque() {
    let mut c = Compositor::new(mode(2, 2), BLUE).expect("compositor");
    c.composite();

    // A translucent spelling of the current colour is the same opaque
    // background, so nothing changes; a translucent new colour lands opaque.
    assert!(!c.set_background(Color { a: 9, ..BLUE }));
    assert!(c.set_background(Color { a: 0, ..RED }));
    assert_eq!(c.background(), RED);
}

#[test]
fn set_background_keeps_windows_on_top() {
    let mut c = Compositor::new(mode(4, 4), BLUE).expect("compositor");
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
    // screen clears to. One shared definition drives the WM.
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
    // through the single compositor rounded-corner path: the
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
fn key_is_delivered_to_the_focused_window() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
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
fn key_without_focus_is_ignored() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    c.add_window(Point::ORIGIN, opaque(10, 10, RED));
    let mut router = InputRouter::new();

    assert_eq!(router.focused(), None);
    assert_eq!(
        router.handle(key_pressed(Key::Char('a')), &mut c),
        InputResponse::Ignored
    );
}

#[test]
fn key_to_a_vanished_focus_is_ignored_and_drops_focus() {
    let mut c = Compositor::new(mode(40, 40), BLUE).expect("compositor");
    let win = c.add_window(Point::ORIGIN, opaque(10, 10, RED));
    let mut router = InputRouter::new();
    assert!(router.focus(win, &c));

    assert!(c.remove(win), "the focused window is removed");
    assert_eq!(
        router.handle(key_pressed(Key::Char('a')), &mut c),
        InputResponse::Ignored
    );
    assert_eq!(
        router.focused(),
        None,
        "a key to a vanished window drops stale focus"
    );
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
fn solid_cursor(size: u32, color: Color) -> tairix_cursor::CursorImage {
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
use tairix_cursor::{CursorRegistry, CursorSetId, CursorTheme};
use tairix_geometry::Scale;
use tairix_theme::CursorKind;

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
    let mut c = Compositor::new(mode(8, 8), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(8, 8), BLUE).expect("compositor");
    let win = c.add_window(Point::new(1, 1), opaque(2, 2, RED));
    assert!(c.set_visible(win, false));
    let mut display = MockAccel::new(mode(8, 8), generous_caps());

    c.present_accelerated(&mut display).expect("present");
    assert_eq!(display.layers.len(), 1, "only the background remains");
}

#[test]
fn accelerated_present_falls_back_when_over_layer_budget() {
    let mut c = Compositor::new(mode(8, 8), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(8, 8), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(200, 200), BLUE).expect("compositor");
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
fn furniture_press_is_not_delivered_to_the_client() {
    let mut c = Compositor::new(mode(200, 200), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(200, 200), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(200, 200), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(200, 200), BLUE).expect("compositor");
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
    let mut c = Compositor::new(mode(200, 200), BLUE).expect("compositor");
    let _ = with_vertical_viewport(&mut c);
    c.composite();
    // The client fills 0..86 with red; the reserved 14px gutter shows the
    // desktop background instead (the client cannot paint into the furniture).
    assert_eq!(frame_pixel(&c, 10, 10), [255, 0, 0, 255]);
    assert_eq!(frame_pixel(&c, 90, 10), [0, 0, 255, 255]);
}

#[test]
fn clearing_a_root_viewport_removes_the_furniture() {
    let mut c = Compositor::new(mode(200, 200), BLUE).expect("compositor");
    let id = with_vertical_viewport(&mut c);
    assert!(c.root_viewport(id).is_some());

    // Clearing drops the viewport, so no furniture is composed and the client
    // reclaims the gutter.
    assert!(c.clear_root_viewport(id));
    assert!(c.root_viewport(id).is_none());

    // A clear against an unknown id is refused.
    assert!(!c.clear_root_viewport(WindowId(9_999)));
}
