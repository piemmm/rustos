//! Unit tests for the toolbar / toolstrip (spec §11.11, §20 checklist).
//!
//! These cover the strip background, group layout with a divider between
//! groups, the active-tool lower accent seam, hit-testing, pointer activation
//! of an icon tool and a split tool's disclosure, keyboard focus movement and
//! activation, the active-tool flag, theme switching, and scale — and the
//! overflow behaviour of a strip too narrow for its tools: whole tools only
//! and nothing seated or hit-tested outside the bounds, a reserved slot at
//! each end whose chevron is drawn only where there is something that way, a
//! press stepping one tool and a held press repeating, the wheel, keyboard
//! focus scrolling a tool into view, and the offset compared by the repaint
//! gate while the press latch is not.

use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::NoArtwork;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Pixel, Surface};
use tairix_theme::Theme;

use crate::button::{ButtonContent, IconButton, SplitButton};
use crate::damage::sink;
use crate::state::ControlRole;
use crate::testkit::{has_pixel, premul, region_has};
use crate::toolbar::{ToolActivation, Toolbar, ToolbarAction, ToolbarOutcome};
use tairix_icon::IconKind;

const W: u32 = 220;
const H: u32 = 28;
const GAP: u32 = 8;
const CH: u32 = 28;

/// The tallest run of consecutive rows in any one column carrying `want`.
///
/// A divider is a *line*, so this is what asks whether one was drawn. A single
/// pixel that happens to land on the same colour — a plate's blend rounding
/// there under the surface's ordered dither — is not a divider, and a test
/// that scanned for the bare colour would have called it one.
fn tallest_column_run(surface: &Surface, want: Pixel) -> u32 {
    let mut tallest = 0;
    for x in 0..surface.width() {
        let mut run = 0;
        for y in 0..surface.height() {
            run = if surface.get(x, y) == Some(want) {
                run + 1
            } else {
                0
            };
            tallest = tallest.max(run);
        }
    }
    tallest
}

fn icon() -> IconButton {
    IconButton::new(IconKind::Bell, ControlRole::Neutral)
}

/// Two icon tools in group 0 and one in group 1.
fn grouped_toolbar() -> Toolbar {
    Toolbar::new()
        .with_icon(icon(), 0)
        .with_icon(icon(), 0)
        .with_icon(icon(), 1)
}

fn render(toolbar: &Toolbar, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    toolbar.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        &mut NoArtwork,
    );
    surface
}

/// The strip a keyboard test's focus report covers.
fn bar() -> Rect {
    Rect::new(0, 0, W, H)
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

/// A `u32` coordinate as an `i32` (test coordinates always fit).
fn xi(v: u32) -> i32 {
    i32::try_from(v).expect("coordinate fits in i32")
}

/// The surface-x centre of icon tool `i` (all icons in one adjacency run).
fn icon_centre_x(i: u32) -> i32 {
    xi(GAP + i * (CH + GAP) + CH / 2)
}

// --- Layout and background ---------------------------------------------

#[test]
fn strip_paints_the_raised_background() {
    let theme = Theme::dark();
    let surface = render(&grouped_toolbar(), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

#[test]
fn a_group_boundary_draws_a_divider() {
    let theme = Theme::dark();
    let grouped = render(&grouped_toolbar(), &theme);
    let one_group = render(
        &Toolbar::new().with_icon(icon(), 0).with_icon(icon(), 0),
        &theme,
    );
    let border = premul(theme.palette().border);
    let drawn = tallest_column_run(&grouped, border);
    assert!(drawn > 1, "a group boundary drew no divider line");
    assert!(
        tallest_column_run(&one_group, border) < drawn,
        "a single group drew a divider of its own"
    );
}

#[test]
fn tool_at_maps_points_to_tools() {
    let toolbar = grouped_toolbar();
    let bounds = Rect::new(0, 0, W, H);
    let theme = Theme::dark();
    assert_eq!(
        toolbar.tool_at(bounds, Scale::ONE, &theme, Point::new(icon_centre_x(0), 14)),
        Some(0)
    );
    assert_eq!(
        toolbar.tool_at(bounds, Scale::ONE, &theme, Point::new(icon_centre_x(1), 14)),
        Some(1)
    );
    // The gap between two tools is not over any tool.
    assert_eq!(
        toolbar.tool_at(
            bounds,
            Scale::ONE,
            &theme,
            Point::new(xi(CH + GAP + GAP / 2), 14)
        ),
        None
    );
}

#[test]
fn tool_rect_is_the_forward_mirror_of_tool_at() {
    let toolbar = grouped_toolbar();
    let bounds = Rect::new(0, 0, W, H);
    let theme = Theme::dark();
    for index in 0..3 {
        let rect = toolbar
            .tool_rect(index, bounds, Scale::ONE, &theme)
            .expect("in-range tool has a rect");
        let centre = Point::new(
            rect.left() + i32::try_from(rect.width).unwrap_or(0) / 2,
            rect.top() + 1,
        );
        assert_eq!(
            toolbar.tool_at(bounds, Scale::ONE, &theme, centre),
            Some(index),
            "the rect's centre hit-tests back to the same tool",
        );
    }
    // Out of range fails closed.
    assert_eq!(toolbar.tool_rect(3, bounds, Scale::ONE, &theme), None);
}

// --- Active tool --------------------------------------------------------

#[test]
fn set_active_marks_one_tool() {
    let mut toolbar = grouped_toolbar();
    toolbar.set_active(1);
    assert!(toolbar.is_active(1));
    assert!(!toolbar.is_active(0));
}

#[test]
fn active_tool_draws_a_lower_accent_seam() {
    let theme = Theme::dark();
    let mut toolbar = grouped_toolbar();
    toolbar.set_active(1);
    let surface = render(&toolbar, &theme);
    // Tool 1 occupies x in [GAP + CH + GAP, ...]; its lower edge carries the seam.
    let x0 = GAP + CH + GAP;
    assert!(region_has(
        &surface,
        (x0 + 2, x0 + CH - 2),
        (H - 2, H),
        premul(theme.palette().accent),
    ));
}

// --- Pointer ------------------------------------------------------------

#[test]
fn clicking_an_icon_tool_activates_its_primary() {
    let mut toolbar = grouped_toolbar();
    let bounds = Rect::new(0, 0, W, H);
    let theme = Theme::dark();
    let x = icon_centre_x(0);
    toolbar.on_pointer(&moved(x, 14), bounds, Scale::ONE, &theme, &mut sink());
    toolbar.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        toolbar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        ToolbarOutcome::Activated(ToolbarAction {
            index: 0,
            part: ToolActivation::Primary
        })
    );
}

#[test]
fn clicking_a_split_tool_disclosure_reports_disclosure() {
    let split = SplitButton::new(ButtonContent::Icon(IconKind::Bell), ControlRole::Neutral);
    let mut toolbar = Toolbar::new().with_split(split, 0);
    let bounds = Rect::new(0, 0, W, H);
    let theme = Theme::dark();
    // The split tool spans [GAP, GAP + 2*CH); its disclosure is the right half.
    let disclosure_x = xi(GAP + CH + CH / 2);
    toolbar.on_pointer(
        &moved(disclosure_x, 14),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    toolbar.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        toolbar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        ToolbarOutcome::Activated(ToolbarAction {
            index: 0,
            part: ToolActivation::Disclosure
        })
    );
}

// --- Keyboard -----------------------------------------------------------

#[test]
fn right_moves_focus_and_enter_activates_the_focused_tool() {
    let theme = Theme::dark();
    let mut toolbar = grouped_toolbar();
    toolbar.on_key(
        Key::Named(NamedKey::Right),
        bar(),
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar.focused(), Some(0));
    toolbar.on_key(
        Key::Named(NamedKey::Right),
        bar(),
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar.focused(), Some(1));
    assert_eq!(
        toolbar.on_key(
            Key::Named(NamedKey::Enter),
            bar(),
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        ToolbarOutcome::Activated(ToolbarAction {
            index: 1,
            part: ToolActivation::Primary
        })
    );
}

#[test]
fn left_wraps_and_home_end_jump() {
    let theme = Theme::dark();
    let mut toolbar = grouped_toolbar();
    toolbar.on_key(
        Key::Named(NamedKey::Left),
        bar(),
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar.focused(), Some(2));
    toolbar.on_key(
        Key::Named(NamedKey::Home),
        bar(),
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar.focused(), Some(0));
    toolbar.on_key(
        Key::Named(NamedKey::End),
        bar(),
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar.focused(), Some(2));
}

// --- Theme switching and scale -----------------------------------------

#[test]
fn theme_switch_changes_the_strip() {
    let toolbar = grouped_toolbar();
    let dark = render(&toolbar, &Theme::dark());
    let light = render(&toolbar, &Theme::light());
    assert_ne!(dark.get(2, H / 2), light.get(2, H / 2));
}

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let toolbar = grouped_toolbar();
    let mut surface = Surface::new(W * 2, H * 2).expect("surface");
    toolbar.render(
        &mut surface,
        Rect::new(0, 0, W * 2, H * 2),
        scale,
        &theme,
        &mut NoArtwork,
    );
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

#[test]
fn empty_toolbar_reports_no_tools() {
    let theme = Theme::dark();
    let mut toolbar = Toolbar::new();
    assert!(toolbar.is_empty());
    assert_eq!(
        toolbar.on_key(
            Key::Named(NamedKey::Enter),
            bar(),
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        ToolbarOutcome::Idle
    );
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

#[test]
fn pointer_position_alone_never_changes_a_toolbar_render() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    // Two samples clear of the bar, so only the recorded coordinate differs;
    // what a hover *does* to a tool lives in that tool and is still compared.
    let mut a = grouped_toolbar();
    let mut b = a.clone();
    let x = i32::try_from(W).expect("width");
    let y = i32::try_from(H).expect("height");
    a.on_pointer(
        &moved(x + 40, y + 40),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    b.on_pointer(
        &moved(x + 90, y + 12),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );

    assert_eq!(
        a, b,
        "a coordinate clear of the bar is not a drawn property"
    );
    assert_eq!(
        render(&a, &theme).pixels(),
        render(&b, &theme).pixels(),
        "…and the two must therefore paint identically"
    );
}

// --- Pointer routing ---------------------------------------------------

/// A pointer path that crosses every boundary and drags a press off its tool.
fn script() -> Vec<InputEvent> {
    alloc::vec![
        moved(icon_centre_x(0), xi(H / 2)),
        moved(icon_centre_x(0) + 1, xi(H / 2)),
        moved(icon_centre_x(1), xi(H / 2)),
        PRESS,
        moved(icon_centre_x(2), xi(H / 2)),
        RELEASE,
        moved(icon_centre_x(2), xi(H / 2)),
        PRESS,
        RELEASE,
        moved(xi(W) - 1, xi(H / 2)),
        moved(icon_centre_x(0), xi(H / 2)),
    ]
}

/// Routing is an optimisation, not a behaviour change: the routed strip must
/// end every step of a scripted path in the state fanning to all tools would
/// have left it in, and report the same activations.
#[test]
fn routing_leaves_the_same_state_as_fanning_to_every_tool() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let mut routed = grouped_toolbar();
    let mut fanned = grouped_toolbar();
    let resting = grouped_toolbar();
    let mut hovered_at_some_point = false;

    for event in script() {
        let a = routed.on_pointer(&event, bounds, Scale::ONE, &theme, &mut sink());
        let b = fanned.fan_pointer(&event, bounds, Scale::ONE, &theme, &mut sink());
        assert_eq!(a, b, "activation differs after {event:?}");
        assert_eq!(routed, fanned, "state differs after {event:?}");
        hovered_at_some_point |= routed != resting;
    }
    assert!(
        hovered_at_some_point,
        "the script must actually move the strip off its resting state"
    );
}

/// A press dragged off its tool keeps reaching that tool, so its latch
/// resolves against where the pointer really is and the release cancels
/// instead of activating a tool the pointer left.
#[test]
fn a_press_dragged_off_its_tool_cancels() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let mut toolbar = grouped_toolbar();
    let feed = |toolbar: &mut Toolbar, event: InputEvent| {
        toolbar.on_pointer(&event, bounds, Scale::ONE, &theme, &mut sink())
    };

    feed(&mut toolbar, moved(icon_centre_x(0), xi(H / 2)));
    feed(&mut toolbar, PRESS);
    feed(&mut toolbar, moved(icon_centre_x(2), xi(H / 2)));
    assert_eq!(feed(&mut toolbar, RELEASE), ToolbarOutcome::Redraw);

    let mut rested = grouped_toolbar();
    rested.on_pointer(
        &moved(icon_centre_x(2), xi(H / 2)),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar, rested, "the cancelled press must leave no mark");
}

// --- Overflow: a strip too narrow for its tools -------------------------

/// Six icon tools in two groups, so a narrow strip has plenty to hide.
fn long_toolbar() -> Toolbar {
    let mut toolbar = Toolbar::new();
    for i in 0..6 {
        toolbar = toolbar.with_icon(icon(), u16::from(i >= 3));
    }
    toolbar
}

/// The narrowest strip `long_toolbar` still shows a tool in, plus room for
/// two more, so the scroll has somewhere to go on both sides.
fn narrow() -> Rect {
    Rect::new(0, 0, 2 * CH + GAP + 3 * (CH + GAP), H)
}

/// Every seated tool, in index order.
fn seated(toolbar: &Toolbar, bounds: Rect, theme: &Theme) -> Vec<(usize, Rect)> {
    (0..toolbar.len())
        .filter_map(|i| {
            toolbar
                .tool_rect(i, bounds, Scale::ONE, theme)
                .map(|rect| (i, rect))
        })
        .collect()
}

#[test]
fn a_wide_strip_seats_every_tool_and_reserves_nothing() {
    let theme = Theme::dark();
    let toolbar = long_toolbar();
    let wide = Rect::new(0, 0, toolbar.natural_width(Scale::ONE, &theme), H);
    assert_eq!(
        seated(&toolbar, wide, &theme).len(),
        toolbar.len(),
        "the natural width must seat every tool"
    );
    let model = toolbar.scroll_model(wide, Scale::ONE, &theme);
    assert!(!model.range().is_scrollable(), "nothing to scroll");
    // The leading slot a scrollable strip reserves holds a tool here, which is
    // what says no room was set aside.
    let first = toolbar
        .tool_rect(0, wide, Scale::ONE, &theme)
        .expect("tool 0 is seated");
    assert_eq!(first.left(), xi(GAP));
}

#[test]
fn a_narrow_strip_seats_whole_tools_inside_its_bounds() {
    let theme = Theme::dark();
    let toolbar = long_toolbar();
    let bounds = narrow();
    let shown = seated(&toolbar, bounds, &theme);
    assert!(!shown.is_empty(), "a narrow strip still shows tools");
    assert!(
        shown.len() < toolbar.len(),
        "…and cannot show them all, or the case is not the one under test"
    );
    for (i, rect) in &shown {
        assert!(
            rect.left() >= bounds.left() && rect.right() <= bounds.right(),
            "tool {i} at {rect:?} lies outside {bounds:?}"
        );
        assert_eq!(rect.width, CH, "a part-tool was seated");
    }
}

#[test]
fn nothing_outside_the_strip_hit_tests_to_a_tool() {
    let theme = Theme::dark();
    let toolbar = long_toolbar();
    let bounds = narrow();
    // One pixel past the trailing edge, and where an unseated tool would have
    // been laid out had the strip run off its own end.
    for x in [bounds.right(), bounds.right() + xi(CH)] {
        assert_eq!(
            toolbar.tool_at(bounds, Scale::ONE, &theme, Point::new(x, xi(H / 2))),
            None,
            "a point at x={x} resolved to a tool"
        );
    }
    // The reserved affordance slots are not tools either.
    for x in [xi(CH / 2), bounds.right() - xi(CH / 2)] {
        assert_eq!(
            toolbar.tool_at(bounds, Scale::ONE, &theme, Point::new(x, xi(H / 2))),
            None,
            "the affordance slot at x={x} resolved to a tool"
        );
    }
}

/// Where a chevron is drawn, so a test can ask whether one was.
fn chevron_drawn(toolbar: &Toolbar, bounds: Rect, theme: &Theme, leading: bool) -> bool {
    let mut surface = Surface::new(bounds.width, bounds.height).expect("surface");
    toolbar.render(&mut surface, bounds, Scale::ONE, theme, &mut NoArtwork);
    let slot = if leading {
        (0, CH)
    } else {
        (bounds.width - CH, bounds.width)
    };
    region_has(
        &surface,
        slot,
        (0, bounds.height),
        premul(theme.palette().on_surface_muted),
    )
}

#[test]
fn a_chevron_is_drawn_only_where_there_is_something_that_way() {
    let theme = Theme::dark();
    let mut toolbar = long_toolbar();
    let bounds = narrow();
    // At the start: nothing behind, tools ahead.
    assert!(!chevron_drawn(&toolbar, bounds, &theme, true));
    assert!(chevron_drawn(&toolbar, bounds, &theme, false));

    // Scrolled to the end: tools behind, nothing ahead.
    let model = toolbar.scroll_model(bounds, Scale::ONE, &theme);
    toolbar.wheel(
        i32::try_from(model.range().max_offset()).expect("a small offset"),
        0,
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert!(chevron_drawn(&toolbar, bounds, &theme, true));
    assert!(!chevron_drawn(&toolbar, bounds, &theme, false));

    // A strip wide enough for every tool offers neither.
    let wide = Rect::new(0, 0, toolbar.natural_width(Scale::ONE, &theme), H);
    assert!(!chevron_drawn(&toolbar, wide, &theme, true));
    assert!(!chevron_drawn(&toolbar, wide, &theme, false));
}

#[test]
fn pressing_the_trailing_affordance_steps_one_tool_and_holding_repeats() {
    let theme = Theme::dark();
    let mut toolbar = long_toolbar();
    let bounds = narrow();
    let at = Point::new(bounds.right() - xi(CH / 2), xi(H / 2));
    let feed = |toolbar: &mut Toolbar, event: InputEvent| {
        toolbar.on_pointer(&event, bounds, Scale::ONE, &theme, &mut sink())
    };

    feed(&mut toolbar, moved(at.x, at.y));
    assert_eq!(feed(&mut toolbar, PRESS), ToolbarOutcome::Redraw);
    assert_eq!(
        toolbar.scroll_model(bounds, Scale::ONE, &theme).offset(),
        1,
        "a press steps exactly one tool"
    );
    assert!(toolbar.is_repeating(), "the press latched for the timer");

    assert!(toolbar.repeat(bounds, Scale::ONE, &theme, &mut sink()));
    assert_eq!(
        toolbar.scroll_model(bounds, Scale::ONE, &theme).offset(),
        2,
        "a held press steps again on the owner's wake"
    );

    // Released, the latch is gone and a further wake steps nothing.
    feed(&mut toolbar, RELEASE);
    assert!(!toolbar.is_repeating());
    assert!(!toolbar.repeat(bounds, Scale::ONE, &theme, &mut sink()));

    // And the repeat stops contributing at the end rather than running on.
    let end = toolbar.scroll_model(bounds, Scale::ONE, &theme).to_end();
    toolbar.wheel(
        i32::try_from(end.offset()).expect("a small offset"),
        0,
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    feed(&mut toolbar, PRESS);
    assert!(!toolbar.repeat(bounds, Scale::ONE, &theme, &mut sink()));
}

#[test]
fn the_wheel_scrolls_the_strip_and_a_full_strip_ignores_it() {
    let theme = Theme::dark();
    let mut toolbar = long_toolbar();
    let bounds = narrow();
    assert!(toolbar.wheel(1, 0, bounds, Scale::ONE, &theme, &mut sink()));
    assert_eq!(toolbar.scroll_model(bounds, Scale::ONE, &theme).offset(), 1);
    // A vertical wheel reaches a strip with no sideways axis to offer.
    assert!(toolbar.wheel(0, 1, bounds, Scale::ONE, &theme, &mut sink()));
    assert_eq!(toolbar.scroll_model(bounds, Scale::ONE, &theme).offset(), 2);
    // Back past the start clamps rather than wrapping.
    assert!(toolbar.wheel(-9, 0, bounds, Scale::ONE, &theme, &mut sink()));
    assert_eq!(toolbar.scroll_model(bounds, Scale::ONE, &theme).offset(), 0);
    assert!(!toolbar.wheel(-1, 0, bounds, Scale::ONE, &theme, &mut sink()));

    let wide = Rect::new(0, 0, toolbar.natural_width(Scale::ONE, &theme), H);
    assert!(!toolbar.wheel(1, 0, wide, Scale::ONE, &theme, &mut sink()));
}

#[test]
fn keyboard_focus_scrolls_the_focused_tool_into_view() {
    let theme = Theme::dark();
    let mut toolbar = long_toolbar();
    let bounds = narrow();
    let last = toolbar.len() - 1;

    toolbar.on_key(
        Key::Named(NamedKey::End),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar.focused(), Some(last));
    assert!(
        toolbar
            .tool_rect(last, bounds, Scale::ONE, &theme)
            .is_some(),
        "the focus ring must not sit on a tool nothing drew"
    );

    toolbar.on_key(
        Key::Named(NamedKey::Home),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(toolbar.focused(), Some(0));
    assert!(toolbar.tool_rect(0, bounds, Scale::ONE, &theme).is_some());
    assert_eq!(toolbar.scroll_model(bounds, Scale::ONE, &theme).offset(), 0);
}

#[test]
fn the_offset_compares_but_the_affordance_press_latch_does_not() {
    let theme = Theme::dark();
    let bounds = narrow();
    let mut scrolled = long_toolbar();
    scrolled.wheel(1, 0, bounds, Scale::ONE, &theme, &mut sink());
    assert_ne!(
        scrolled,
        long_toolbar(),
        "a moved offset draws different tools"
    );

    // A latched affordance draws the same pixels, so it must not force a
    // host's repaint gate open.
    let mut pressed = long_toolbar();
    let at = Point::new(bounds.right() - xi(CH / 2), xi(H / 2));
    pressed.on_pointer(&moved(at.x, at.y), bounds, Scale::ONE, &theme, &mut sink());
    pressed.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert!(pressed.is_repeating());
    assert_eq!(pressed, scrolled, "the press latch is not a drawn property");
}

#[test]
fn a_strip_with_no_room_for_a_tool_shows_and_offers_nothing() {
    let theme = Theme::dark();
    let toolbar = long_toolbar();
    // Two reserved slots and nothing between them.
    let bounds = Rect::new(0, 0, 2 * CH, H);
    assert!(seated(&toolbar, bounds, &theme).is_empty());
    assert!(!chevron_drawn(&toolbar, bounds, &theme, true));
    assert!(!chevron_drawn(&toolbar, bounds, &theme, false));
    assert!(!toolbar
        .scroll_model(bounds, Scale::ONE, &theme)
        .range()
        .is_scrollable());
    // And the declared minimum is what it takes to show one.
    let least = toolbar.min_width(Scale::ONE, &theme);
    let floor = Rect::new(0, 0, least, H);
    assert_eq!(seated(&toolbar, floor, &theme).len(), 1);
}

#[test]
fn a_wheel_scroll_moves_the_hover_to_the_tool_now_under_the_pointer() {
    let theme = Theme::dark();
    let mut toolbar = long_toolbar();
    let bounds = narrow();
    // Rest the pointer on the first seated tool, then scroll a different tool
    // under it.
    let first = toolbar
        .tool_rect(0, bounds, Scale::ONE, &theme)
        .expect("the first tool is seated");
    let at = first.center();
    toolbar.on_pointer(&moved(at.x, at.y), bounds, Scale::ONE, &theme, &mut sink());
    let hovering = toolbar.clone();
    assert!(toolbar.wheel(1, 0, bounds, Scale::ONE, &theme, &mut sink()));

    // Tool 0 is no longer seated, so nothing of it may still read as hovered:
    // a strip that left it lit would show a highlight on a tool the pointer
    // is not over.
    assert!(toolbar.tool_rect(0, bounds, Scale::ONE, &theme).is_none());
    let mut rested = long_toolbar();
    rested.wheel(1, 0, bounds, Scale::ONE, &theme, &mut sink());
    rested.on_pointer(&moved(at.x, at.y), bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        toolbar, rested,
        "a scroll must leave the strip as a move onto the same point would"
    );
    assert_ne!(toolbar, hovering, "…and not as it was before the scroll");
}
