//! Unit tests for the scrollbar renderer (spec §11.28–§11.30, §20 checklist).
//!
//! These cover the one orientation-parameterized behaviour on both axes: thumb
//! math, the preserved drag anchor and mid-drag re-clamp, end-button line steps,
//! track paging, press-and-hold repeat, orientation-aware keys, the wheel, the
//! §13 denied/disabled fail-closed treatment, dark/light and high-contrast
//! rendering, the focus ring, the render-equivalence repaint gate, and the
//! fail-closed degenerate/non-scrollable cases.

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::Theme;

use crate::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use crate::scrollbar::{ScrollAction, ScrollBar, ScrollPart};
use crate::state::{AuthorityState, ControlState};
use crate::testkit::high_contrast;

const VW: u32 = 16;
const VH: u32 = 300;

fn model() -> ScrollModel {
    ScrollModel::new(ScrollRange::new(1000, 300, 0), 10, 100)
}

fn vbar() -> ScrollBar {
    ScrollBar::new(ScrollOrientation::Vertical, model())
}

fn hbar() -> ScrollBar {
    ScrollBar::new(ScrollOrientation::Horizontal, model())
}

fn vbounds() -> Rect {
    Rect::new(0, 0, VW, VH)
}

fn theme() -> Theme {
    Theme::dark()
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

fn premul(rgba: tairix_theme::Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// The requested offset from an action, if any.
fn off(action: Option<ScrollAction>) -> Option<u64> {
    action.map(|ScrollAction::ScrollTo { offset }| offset)
}

fn render(bar: &ScrollBar, bounds: Rect, theme: &Theme) -> Surface {
    let mut surface = Surface::new(bounds.width, bounds.height).expect("surface");
    bar.render(&mut surface, bounds, Scale::ONE, theme);
    surface
}

#[test]
fn both_orientations_are_one_behaviour() {
    // The same model wheeled the same number of ticks moves identically on
    // either axis — the bars are one component parameterised by orientation.
    let mut v = vbar();
    let mut h = hbar();
    assert_eq!(off(v.wheel(0, 3)), Some(30));
    assert_eq!(off(h.wheel(3, 0)), Some(30));
    assert_eq!(v.model().offset(), h.model().offset());
}

#[test]
fn wheel_moves_one_line_per_tick_and_clamps() {
    let mut bar = vbar();
    assert_eq!(off(bar.wheel(0, 3)), Some(30));
    // A cross-axis tick does nothing to a vertical bar.
    assert_eq!(bar.wheel(5, 0), None);
    // Scrolling back past the start clamps at zero, then stops changing.
    assert_eq!(off(bar.wheel(0, -100)), Some(0));
    assert_eq!(bar.wheel(0, -1), None);
}

#[test]
fn keys_step_line_page_and_bounds_when_focused() {
    let mut bar = vbar();
    bar.set_focused(true);
    assert_eq!(off(bar.on_key(Key::Named(NamedKey::Down))), Some(10));
    assert_eq!(off(bar.on_key(Key::Named(NamedKey::PageDown))), Some(110));
    assert_eq!(off(bar.on_key(Key::Named(NamedKey::End))), Some(700));
    assert_eq!(off(bar.on_key(Key::Named(NamedKey::PageUp))), Some(600));
    assert_eq!(off(bar.on_key(Key::Named(NamedKey::Home))), Some(0));
    // At the start, another line back does nothing (fail-closed bounds).
    assert_eq!(bar.on_key(Key::Named(NamedKey::Up)), None);
}

#[test]
fn horizontal_bar_uses_left_right_not_up_down() {
    let mut bar = hbar();
    bar.set_focused(true);
    assert_eq!(bar.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(off(bar.on_key(Key::Named(NamedKey::Right))), Some(10));
    assert_eq!(off(bar.on_key(Key::Named(NamedKey::Left))), Some(0));
}

#[test]
fn unfocused_bar_ignores_keys() {
    let mut bar = vbar();
    assert_eq!(bar.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(bar.model().offset(), 0);
}

#[test]
fn end_button_press_steps_one_line() {
    let theme = theme();
    let mut bar = vbar();
    bar.set_model(bar.model().scroll_to(500));
    // Decrement button at the top.
    assert_eq!(
        off(bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme)),
        Some(490)
    );
    bar.on_pointer(&RELEASE, vbounds(), Scale::ONE, &theme);
    // Increment button at the bottom.
    bar.on_pointer(&moved(2, 290), vbounds(), Scale::ONE, &theme);
    assert_eq!(
        off(bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme)),
        Some(500)
    );
}

#[test]
fn track_press_pages_toward_the_pointer() {
    let theme = theme();
    let mut bar = vbar();
    // At offset 0 the thumb sits at the track start; a press well below it in
    // the after-thumb region pages forward.
    bar.on_pointer(&moved(2, 200), vbounds(), Scale::ONE, &theme);
    assert_eq!(
        off(bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme)),
        Some(100)
    );
}

#[test]
fn thumb_drag_preserves_anchor_and_does_not_jump_on_grab() {
    let theme = theme();
    let mut bar = vbar();
    // Press on the thumb (near the track start at offset 0): no jump.
    bar.on_pointer(&moved(2, 20), vbounds(), Scale::ONE, &theme);
    assert_eq!(bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme), None);
    assert_eq!(bar.model().offset(), 0);
    // Dragging down moves the offset forward, clamped within the range.
    let a = off(bar.on_pointer(&moved(2, 150), vbounds(), Scale::ONE, &theme));
    let moved_to = a.expect("drag moved");
    assert!(moved_to > 0 && moved_to <= 700);
    bar.on_pointer(&RELEASE, vbounds(), Scale::ONE, &theme);
}

#[test]
fn mid_drag_range_shrink_stays_valid() {
    let theme = theme();
    let mut bar = vbar();
    bar.on_pointer(&moved(2, 20), vbounds(), Scale::ONE, &theme);
    bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme);
    // The content shrinks mid-drag; the bar recomputes from the new range.
    bar.set_model(ScrollModel::new(ScrollRange::new(400, 300, 0), 10, 100));
    let dragged = off(bar.on_pointer(&moved(2, 290), vbounds(), Scale::ONE, &theme));
    let value = dragged.unwrap_or_else(|| bar.model().offset());
    assert!(
        value <= 100,
        "offset {value} must stay within the new max 100"
    );
}

#[test]
fn denied_bar_keeps_position_and_ignores_input() {
    let theme = theme();
    let mut bar = vbar();
    bar.set_model(bar.model().scroll_to(200));
    bar.set_focused(true);
    bar.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    assert_eq!(bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme), None);
    assert_eq!(bar.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(bar.wheel(0, 3), None);
    assert_eq!(bar.model().offset(), 200);
    // It renders the denied colour, distinct from a disabled look.
    let surface = render(&bar, vbounds(), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().denied)));
}

#[test]
fn disabled_bar_ignores_input() {
    let theme = theme();
    let mut bar = vbar();
    bar.set_focused(true);
    bar.set_state(ControlState::disabled());
    assert_eq!(bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme), None);
    assert_eq!(bar.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(bar.wheel(0, 3), None);
}

#[test]
fn part_at_classifies_every_region() {
    let theme = theme();
    let mut bar = vbar();
    let s = Scale::ONE;
    assert_eq!(
        bar.part_at(vbounds(), Point::new(2, 2), s, &theme),
        ScrollPart::Decrement
    );
    assert_eq!(
        bar.part_at(vbounds(), Point::new(2, 290), s, &theme),
        ScrollPart::Increment
    );
    assert_eq!(
        bar.part_at(vbounds(), Point::new(2, 20), s, &theme),
        ScrollPart::Thumb
    );
    assert_eq!(
        bar.part_at(vbounds(), Point::new(2, 200), s, &theme),
        ScrollPart::TrackAfter
    );
    // A point off the bar entirely.
    assert_eq!(
        bar.part_at(vbounds(), Point::new(100, 5), s, &theme),
        ScrollPart::Outside
    );
    // With the thumb scrolled to the end, a point above it is the before region.
    bar.set_model(bar.model().to_end());
    assert_eq!(
        bar.part_at(vbounds(), Point::new(2, 100), s, &theme),
        ScrollPart::TrackBefore
    );
}

#[test]
fn press_hold_repeat_steps_the_held_part_and_stops_at_a_bound() {
    let theme = theme();
    let mut bar = vbar();
    bar.set_model(bar.model().scroll_to(25));
    bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme); // decrement, held
    assert_eq!(bar.held(), Some(ScrollPart::Decrement));
    assert_eq!(bar.model().offset(), 15);
    assert_eq!(off(bar.repeat()), Some(5));
    // The next repeat reaches the start; a further one contributes nothing.
    assert_eq!(off(bar.repeat()), Some(0));
    assert_eq!(bar.repeat(), None);
    // Releasing clears the held part.
    bar.on_pointer(&RELEASE, vbounds(), Scale::ONE, &theme);
    assert_eq!(bar.held(), None);
    assert_eq!(bar.repeat(), None);
}

#[test]
fn render_draws_the_channel_and_an_idle_thumb() {
    let theme = theme();
    let bar = vbar();
    let surface = render(&bar, vbounds(), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(has_pixel(&surface, premul(theme.palette().scroll_thumb)));
}

#[test]
fn an_awake_bar_brightens_the_thumb() {
    let theme = theme();
    let mut bar = vbar();
    // Hovering the bar makes it awake; the thumb takes the reactive rim.
    bar.on_pointer(&moved(2, 200), vbounds(), Scale::ONE, &theme);
    let surface = render(&bar, vbounds(), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().rim_active)));
}

#[test]
fn a_focused_bar_draws_a_focus_ring() {
    let theme = theme();
    let mut bar = vbar();
    bar.set_focused(true);
    let surface = render(&bar, vbounds(), &theme);
    // The reactive rim appears on the outermost edge (the focus outline).
    assert_eq!(surface.get(0, 0), Some(premul(theme.palette().rim_active)));
}

#[test]
fn high_contrast_rims_the_thumb() {
    let theme = high_contrast();
    let bar = vbar();
    let surface = render(&bar, vbounds(), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

#[test]
fn dark_and_light_thumbs_differ() {
    let dark = Theme::dark();
    let light = Theme::light();
    let bar = vbar();
    assert!(has_pixel(
        &render(&bar, vbounds(), &dark),
        premul(dark.palette().scroll_thumb)
    ));
    assert!(has_pixel(
        &render(&bar, vbounds(), &light),
        premul(light.palette().scroll_thumb)
    ));
    assert_ne!(
        premul(dark.palette().scroll_thumb),
        premul(light.palette().scroll_thumb)
    );
}

#[test]
fn degenerate_bounds_never_panic_and_never_move() {
    let theme = theme();
    let mut bar = vbar();
    let zero = Rect::new(0, 0, 0, 0);
    // Rendering a zero surface is a no-op; input yields nothing.
    let mut surface = Surface::new(1, 1).expect("surface");
    bar.render(&mut surface, zero, Scale::ONE, &theme);
    assert_eq!(bar.on_pointer(&PRESS, zero, Scale::ONE, &theme), None);
    assert_eq!(
        bar.part_at(zero, Point::ORIGIN, Scale::ONE, &theme),
        ScrollPart::Outside
    );
    assert_eq!(bar.model().offset(), 0);
}

#[test]
fn a_non_scrollable_bar_has_a_non_draggable_full_thumb() {
    let theme = theme();
    let mut bar = ScrollBar::new(
        ScrollOrientation::Vertical,
        ScrollModel::new(ScrollRange::new(200, 300, 0), 10, 100),
    );
    let geometry = bar
        .geometry(vbounds(), Scale::ONE, &theme)
        .expect("geometry");
    assert!(!geometry.draggable());
    // A press on the "thumb" starts no drag and the wheel cannot move it.
    bar.on_pointer(&moved(2, 100), vbounds(), Scale::ONE, &theme);
    assert_eq!(bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme), None);
    assert_eq!(bar.wheel(0, 3), None);
    assert_eq!(bar.model().offset(), 0);
}

#[test]
fn scale_floors_the_thumb_at_the_scaled_minimum() {
    let theme = theme();
    // A tiny viewport fraction: the proportional thumb is below the minimum, so
    // the minimum (a theme metric) floors it — and scaling the metric up
    // lengthens the thumb.
    let bar = ScrollBar::new(
        ScrollOrientation::Vertical,
        ScrollModel::new(ScrollRange::new(100_000, 300, 0), 10, 100),
    );
    let one = bar
        .geometry(vbounds(), Scale::ONE, &theme)
        .expect("1x")
        .thumb()
        .length;
    let two = bar
        .geometry(vbounds(), Scale::from_percent(200).expect("2x"), &theme)
        .expect("2x")
        .thumb()
        .length;
    assert!(
        two > one,
        "scaled min thumb {two} must exceed unscaled {one}"
    );
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

#[test]
fn the_drag_anchor_alone_never_changes_a_scrollbar_render() {
    let theme = theme();
    // Two bars grab the thumb at different points, so each keeps a different
    // grab offset within it, then both release and come to rest on the same
    // point. Everything a reader can see — the offset, the hover, the held
    // part — now matches; only the anchor differs, and it is consulted solely
    // while a drag is in flight.
    let grab = |y: i32| {
        let mut bar = vbar();
        bar.on_pointer(&moved(2, y), vbounds(), Scale::ONE, &theme);
        bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme);
        bar.on_pointer(&RELEASE, vbounds(), Scale::ONE, &theme);
        bar.on_pointer(&moved(2, 40), vbounds(), Scale::ONE, &theme);
        bar
    };
    let near = grab(20);
    let far = grab(30);

    assert_eq!(
        near.model().offset(),
        far.model().offset(),
        "a grab inside the thumb must not move the content"
    );
    // Both presses really do capture a different grab offset: while a drag is
    // live the anchor decides where the same pointer lands.
    let drag_from = |y: i32| {
        let mut bar = vbar();
        bar.on_pointer(&moved(2, y), vbounds(), Scale::ONE, &theme);
        bar.on_pointer(&PRESS, vbounds(), Scale::ONE, &theme);
        off(bar.on_pointer(&moved(2, 150), vbounds(), Scale::ONE, &theme))
    };
    assert_ne!(
        drag_from(20),
        drag_from(30),
        "both presses must land inside the thumb, or this proves nothing"
    );
    assert_eq!(near, far, "the drag anchor is not a drawn property");
    assert_eq!(
        render(&near, vbounds(), &theme).pixels(),
        render(&far, vbounds(), &theme).pixels(),
        "…and the two must therefore paint identically"
    );
}

#[test]
fn a_move_within_one_part_leaves_a_scrollbar_equal() {
    let theme = theme();
    // A host feeds every pointer sample to every control it holds, so a bar
    // must not report a change for a sample that lands on a new coordinate
    // without changing which part is beneath it — off the bar entirely, or
    // further along the same region. Reporting one would repaint an unchanged
    // surface on every mouse move.
    let settle = |x: i32, y: i32| {
        let mut bar = vbar();
        bar.on_pointer(&moved(x, y), vbounds(), Scale::ONE, &theme);
        bar
    };
    for (from, to, region) in [
        ((200, 400), (205, 401), "clear of the bar"),
        ((2, 2), (3, 4), "the decrement button"),
        ((2, 150), (3, 170), "the track after the thumb"),
    ] {
        let before = settle(from.0, from.1);
        let after = settle(to.0, to.1);
        assert_eq!(
            before, after,
            "two samples within {region} draw the same pixels"
        );
        assert_eq!(
            render(&before, vbounds(), &theme).pixels(),
            render(&after, vbounds(), &theme).pixels(),
            "…and must therefore paint identically within {region}"
        );
    }
}

#[test]
fn the_part_under_the_pointer_is_a_drawn_property() {
    let theme = theme();
    // The end button beneath the pointer brightens, so *which* part the
    // pointer sits over does compare — excluding it would let the gate pass a
    // bar that paints a lit chevron off as one that does not.
    let mut on_end = vbar();
    on_end.on_pointer(&moved(2, 2), vbounds(), Scale::ONE, &theme);
    let mut on_track = vbar();
    on_track.on_pointer(&moved(2, 150), vbounds(), Scale::ONE, &theme);

    assert_ne!(
        on_end, on_track,
        "a lit decrement chevron is a different composition"
    );
    assert_ne!(
        render(&on_end, vbounds(), &theme).pixels(),
        render(&on_track, vbounds(), &theme).pixels(),
        "…and the two must therefore paint differently"
    );
}
