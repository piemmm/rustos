//! Unit tests for the toolbar / toolstrip (spec §11.11, §20 checklist).
//!
//! These cover the strip background, group layout with a divider between
//! groups, the active-tool lower accent seam, hit-testing, pointer activation
//! of an icon tool and a split tool's disclosure, keyboard focus movement and
//! activation, the active-tool flag, theme switching, and scale.

use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{ButtonContent, IconButton, SplitButton};
use crate::state::ControlRole;
use crate::toolbar::{ToolActivation, Toolbar, ToolbarAction};
use tairix_icon::IconKind;

const W: u32 = 220;
const H: u32 = 28;
const GAP: u32 = 8;
const CH: u32 = 28;

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

fn region_has(surface: &Surface, xr: (u32, u32), yr: (u32, u32), want: Pixel) -> bool {
    (xr.0..xr.1)
        .flat_map(|x| (yr.0..yr.1).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
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
    toolbar.render(&mut surface, Rect::new(0, 0, W, H), Scale::ONE, theme);
    surface
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
    assert!(has_pixel(&grouped, premul(theme.palette().border)));
    assert!(!has_pixel(&one_group, premul(theme.palette().border)));
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
    toolbar.on_pointer(&moved(x, 14), bounds, Scale::ONE, &theme);
    toolbar.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    assert_eq!(
        toolbar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(ToolbarAction {
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
    toolbar.on_pointer(&moved(disclosure_x, 14), bounds, Scale::ONE, &theme);
    toolbar.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    assert_eq!(
        toolbar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(ToolbarAction {
            index: 0,
            part: ToolActivation::Disclosure
        })
    );
}

// --- Keyboard -----------------------------------------------------------

#[test]
fn right_moves_focus_and_enter_activates_the_focused_tool() {
    let mut toolbar = grouped_toolbar();
    toolbar.on_key(Key::Named(NamedKey::Right));
    assert_eq!(toolbar.focused(), Some(0));
    toolbar.on_key(Key::Named(NamedKey::Right));
    assert_eq!(toolbar.focused(), Some(1));
    assert_eq!(
        toolbar.on_key(Key::Named(NamedKey::Enter)),
        Some(ToolbarAction {
            index: 1,
            part: ToolActivation::Primary
        })
    );
}

#[test]
fn left_wraps_and_home_end_jump() {
    let mut toolbar = grouped_toolbar();
    toolbar.on_key(Key::Named(NamedKey::Left));
    assert_eq!(toolbar.focused(), Some(2));
    toolbar.on_key(Key::Named(NamedKey::Home));
    assert_eq!(toolbar.focused(), Some(0));
    toolbar.on_key(Key::Named(NamedKey::End));
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
    toolbar.render(&mut surface, Rect::new(0, 0, W * 2, H * 2), scale, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

#[test]
fn empty_toolbar_reports_no_tools() {
    let mut toolbar = Toolbar::new();
    assert!(toolbar.is_empty());
    assert_eq!(toolbar.on_key(Key::Named(NamedKey::Enter)), None);
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
    a.on_pointer(&moved(x + 40, y + 40), bounds, Scale::ONE, &theme);
    b.on_pointer(&moved(x + 90, y + 12), bounds, Scale::ONE, &theme);

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
        let a = routed.on_pointer(&event, bounds, Scale::ONE, &theme);
        let b = fanned.fan_pointer(&event, bounds, Scale::ONE, &theme);
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
        toolbar.on_pointer(&event, bounds, Scale::ONE, &theme)
    };

    feed(&mut toolbar, moved(icon_centre_x(0), xi(H / 2)));
    feed(&mut toolbar, PRESS);
    feed(&mut toolbar, moved(icon_centre_x(2), xi(H / 2)));
    assert_eq!(feed(&mut toolbar, RELEASE), None);

    let mut rested = grouped_toolbar();
    rested.on_pointer(
        &moved(icon_centre_x(2), xi(H / 2)),
        bounds,
        Scale::ONE,
        &theme,
    );
    assert_eq!(toolbar, rested, "the cancelled press must leave no mark");
}
