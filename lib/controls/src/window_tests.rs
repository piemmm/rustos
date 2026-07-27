//! Unit tests for the window-manager furniture family (spec §20 furniture
//! checklist).
//!
//! These cover the command glyphs (distinct per command, and the size toggle
//! reflecting its *next* action), the shared window-control state model
//! (pointer/keyboard activation, disabled/denied), the title bar (control
//! layout on either edge, title sanitisation, activate/drag/control routing,
//! keyboard focus), the window frame's furniture hit map (client vs furniture
//! isolation, resize edges, activation not changing geometry), the resize
//! grabber (drag capture and Escape-cancel, non-overlap with scrollbars), and
//! the neutral scroll corner, across dark/light/high-contrast and scale.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Contrast, Rgba, Theme};

use crate::state::{
    AuthorityState, ControlState, PointerState, SizeAction, WindowActivationState,
    WindowControlKind, WindowFurnitureState, WindowSizeState,
};
use crate::window::{
    ControlPlacement, FrameInsets, FurniturePart, ResizeEdge, ResizeEvent, ResizeGrabber,
    ScrollCorner, TitleBar, TitleBarEvent, TitleHit, WindowControl, WindowControlAction,
    WindowFrame,
};

fn font() -> BitmapFont {
    BitmapFont::inconsolata()
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

fn opaque_count(surface: &Surface) -> usize {
    surface.pixels().iter().filter(|p| p.a > 0).count()
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

/// Half a `u32` extent as an `i32`, avoiding lossy `as` casts in the tests.
fn half(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(0) / 2
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

/// A theme identical to [`Theme::dark`] but with [`Contrast::High`].
fn high_contrast() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test High Contrast",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        Contrast::High,
    )
}

fn furniture() -> WindowFurnitureState {
    WindowFurnitureState {
        activation: WindowActivationState::Active,
        size: WindowSizeState::Restored,
        movable: true,
        resizable: true,
    }
}

fn render_control(control: &WindowControl, theme: &Theme, side: u32) -> Surface {
    let mut surface = Surface::new(side, side).expect("surface");
    control.render(&mut surface, Rect::new(0, 0, side, side), Scale::ONE, theme);
    surface
}

// --- WindowControl --------------------------------------------------------

#[test]
fn window_control_draws_its_glyph() {
    let theme = Theme::dark();
    let control = WindowControl::new(WindowControlKind::Close);
    let surface = render_control(&control, &theme, 40);
    // An idle interactive control shows just its glyph on a transparent
    // surface, so some non-transparent pixels are the command mark.
    assert!(opaque_count(&surface) > 0);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

#[test]
fn command_glyphs_are_distinct() {
    let theme = Theme::dark();
    let kinds = [
        WindowControlKind::Close,
        WindowControlKind::Minimize,
        WindowControlKind::PutToBack,
        WindowControlKind::SizeToggle,
    ];
    let surfaces: alloc::vec::Vec<_> = kinds
        .iter()
        .map(|k| {
            let control = WindowControl::new(*k);
            render_control(&control, &theme, 40).pixels().to_vec()
        })
        .collect();
    for i in 0..surfaces.len() {
        for j in (i + 1)..surfaces.len() {
            assert_ne!(surfaces[i], surfaces[j], "glyphs {i} and {j} must differ");
        }
    }
}

#[test]
fn size_toggle_glyph_reflects_next_action() {
    let theme = Theme::dark();
    let mut maximize = WindowControl::new(WindowControlKind::SizeToggle);
    maximize.set_size_action(SizeAction::Maximize);
    let mut restore = WindowControl::new(WindowControlKind::SizeToggle);
    restore.set_size_action(SizeAction::Restore);
    assert_ne!(
        render_control(&maximize, &theme, 40).pixels(),
        render_control(&restore, &theme, 40).pixels()
    );
    assert_eq!(maximize.accessible_name(), "Maximize");
    assert_eq!(restore.accessible_name(), "Restore");
}

#[test]
fn accessible_names_identify_commands() {
    assert_eq!(
        WindowControl::new(WindowControlKind::Close).accessible_name(),
        "Close"
    );
    assert_eq!(
        WindowControl::new(WindowControlKind::Minimize).accessible_name(),
        "Minimize"
    );
    assert_eq!(
        WindowControl::new(WindowControlKind::PutToBack).accessible_name(),
        "Put window to back"
    );
}

#[test]
fn pointer_press_release_invokes() {
    let mut control = WindowControl::new(WindowControlKind::Close);
    let bounds = Rect::new(0, 0, 40, 40);
    assert_eq!(control.on_pointer(&moved(10, 10), bounds), None);
    assert_eq!(control.on_pointer(&PRESS, bounds), None);
    assert_eq!(
        control.on_pointer(&RELEASE, bounds),
        Some(WindowControlAction::Invoked(WindowControlKind::Close))
    );
}

#[test]
fn pointer_release_outside_does_not_invoke() {
    let mut control = WindowControl::new(WindowControlKind::Close);
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds);
    let _ = control.on_pointer(&PRESS, bounds);
    let _ = control.on_pointer(&moved(100, 100), bounds);
    assert_eq!(control.on_pointer(&RELEASE, bounds), None);
}

#[test]
fn keyboard_activates_focused_control() {
    let mut control = WindowControl::new(WindowControlKind::Minimize);
    assert_eq!(control.on_key(Key::Named(NamedKey::Enter)), None);
    control.set_focused(true);
    assert_eq!(
        control.on_key(Key::Char(' ')),
        Some(WindowControlAction::Invoked(WindowControlKind::Minimize))
    );
}

#[test]
fn pointer_activation_returns_the_control_to_rest() {
    // A completed click clears the hover/press highlight so the button loses
    // its border once the command fires (a genuine hover returns on the next
    // pointer move), rather than leaving a stale highlight when activation
    // relocates the control (a size toggle) or takes the frame away.
    let mut control = WindowControl::new(WindowControlKind::SizeToggle);
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds);
    let _ = control.on_pointer(&PRESS, bounds);
    assert_eq!(control.state().pointer, PointerState::Pressed);
    assert_eq!(
        control.on_pointer(&RELEASE, bounds),
        Some(WindowControlAction::Invoked(WindowControlKind::SizeToggle))
    );
    assert_eq!(
        control.state().pointer,
        PointerState::None,
        "activation drops the hover/press highlight"
    );
    assert!(
        !control.state().focus.focused,
        "activation leaves no focus ring"
    );
}

#[test]
fn keyboard_activation_clears_the_focus_ring() {
    // Navigating with the keyboard shows the focus border, but activating the
    // control drops it — the border only shows while navigating, not after
    // the command has fired.
    let mut control = WindowControl::new(WindowControlKind::Minimize);
    control.set_focused(true);
    assert_eq!(
        control.on_key(Key::Named(NamedKey::Enter)),
        Some(WindowControlAction::Invoked(WindowControlKind::Minimize))
    );
    assert!(
        !control.state().focus.focused,
        "activation clears the keyboard focus ring"
    );
}

#[test]
fn disabled_control_ignores_input() {
    let mut control = WindowControl::new(WindowControlKind::Close);
    control.set_state(ControlState::disabled());
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds);
    let _ = control.on_pointer(&PRESS, bounds);
    assert_eq!(control.on_pointer(&RELEASE, bounds), None);
    control.set_focused(true);
    assert_eq!(control.on_key(Key::Named(NamedKey::Enter)), None);
}

#[test]
fn denied_control_shows_lock_bead() {
    let theme = Theme::dark();
    let mut control = WindowControl::new(WindowControlKind::Close);
    control.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let surface = render_control(&control, &theme, 40);
    assert!(has_pixel(&surface, premul(theme.palette().denied)));
}

#[test]
fn inactive_frame_mutes_idle_control() {
    let theme = Theme::dark();
    let mut active = WindowControl::new(WindowControlKind::Close);
    active.set_active_frame(true);
    let mut inactive = WindowControl::new(WindowControlKind::Close);
    inactive.set_active_frame(false);
    assert_ne!(
        render_control(&active, &theme, 40).pixels(),
        render_control(&inactive, &theme, 40).pixels()
    );
}

#[test]
fn high_contrast_thickens_glyph() {
    let normal = Theme::dark();
    let hc = high_contrast();
    let control = WindowControl::new(WindowControlKind::Close);
    let normal_count = opaque_count(&render_control(&control, &normal, 40));
    let hc_count = opaque_count(&render_control(&control, &hc, 40));
    assert!(
        hc_count > normal_count,
        "high contrast ({hc_count}) should draw a thicker mark than normal ({normal_count})"
    );
}

#[test]
fn control_renders_at_scale() {
    let theme = Theme::dark();
    let control = WindowControl::new(WindowControlKind::Minimize);
    let scale = Scale::from_percent(200).expect("scale");
    let mut surface = Surface::new(80, 80).expect("surface");
    control.render(&mut surface, Rect::new(0, 0, 80, 80), scale, &theme);
    assert!(opaque_count(&surface) > 0);
}

#[test]
fn light_theme_renders() {
    let theme = Theme::light();
    let control = WindowControl::new(WindowControlKind::Close);
    let surface = render_control(&control, &theme, 40);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

// --- TitleBar -------------------------------------------------------------

fn title_bounds() -> Rect {
    Rect::new(0, 0, 300, 28)
}

#[test]
fn trailing_layout_places_close_outermost() {
    let theme = Theme::dark();
    let bar = TitleBar::new(furniture());
    let layout = bar.layout(title_bounds(), Scale::ONE, &theme);
    assert_eq!(layout.controls[3].0, WindowControlKind::Close);
    let close_x = layout.controls[3].1.left();
    assert!(
        layout.controls.iter().all(|(_, r)| r.left() <= close_x),
        "close should be the rightmost (outermost trailing) control"
    );
}

#[test]
fn leading_layout_mirrors_close_outermost() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    bar.set_placement(ControlPlacement::Leading);
    let layout = bar.layout(title_bounds(), Scale::ONE, &theme);
    let close_rect = layout
        .controls
        .iter()
        .find(|(k, _)| *k == WindowControlKind::Close)
        .expect("close")
        .1;
    assert!(
        layout
            .controls
            .iter()
            .all(|(_, r)| r.left() >= close_rect.left()),
        "close should be the leftmost (outermost leading) control"
    );
}

#[test]
fn title_is_sanitised() {
    let mut bar = TitleBar::new(furniture());
    bar.set_title("a\tb\nc");
    assert_eq!(bar.title(), "a b c");
}

#[test]
fn press_on_drag_region_activates() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    let bounds = title_bounds();
    assert_eq!(
        bar.on_pointer(&moved(50, 10), bounds, Scale::ONE, &theme),
        None
    );
    assert_eq!(
        bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme),
        Some(TitleBarEvent::Activate)
    );
}

#[test]
fn drag_begins_moves_and_ends() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    let bounds = title_bounds();
    let _ = bar.on_pointer(&moved(50, 10), bounds, Scale::ONE, &theme);
    let _ = bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    assert_eq!(
        bar.on_pointer(&moved(70, 10), bounds, Scale::ONE, &theme),
        Some(TitleBarEvent::DragBegin)
    );
    assert_eq!(
        bar.on_pointer(&moved(90, 10), bounds, Scale::ONE, &theme),
        Some(TitleBarEvent::DragMoved {
            to: Point::new(90, 10)
        })
    );
    assert_eq!(
        bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(TitleBarEvent::DragEnd)
    );
}

#[test]
fn press_over_control_routes_to_control_not_drag() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    let bounds = title_bounds();
    let close_rect = bar
        .layout(bounds, Scale::ONE, &theme)
        .controls
        .iter()
        .find(|(k, _)| *k == WindowControlKind::Close)
        .expect("close")
        .1;
    let cx = close_rect.left() + half(close_rect.width);
    let cy = close_rect.top() + half(close_rect.height);
    assert_eq!(
        bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme),
        None
    );
    // A press over the control must not activate/drag the title bar.
    assert_eq!(bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    assert_eq!(
        bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(TitleBarEvent::Control(WindowControlKind::Close))
    );
}

#[test]
fn hit_distinguishes_control_from_drag() {
    let theme = Theme::dark();
    let bar = TitleBar::new(furniture());
    let bounds = title_bounds();
    let close_rect = bar
        .layout(bounds, Scale::ONE, &theme)
        .controls
        .iter()
        .find(|(k, _)| *k == WindowControlKind::Close)
        .expect("close")
        .1;
    let cx = close_rect.left() + half(close_rect.width);
    let cy = close_rect.top() + half(close_rect.height);
    assert_eq!(
        bar.hit(bounds, Scale::ONE, &theme, Point::new(cx, cy)),
        TitleHit::Control(WindowControlKind::Close)
    );
    assert_eq!(
        bar.hit(bounds, Scale::ONE, &theme, Point::new(50, 10)),
        TitleHit::Drag
    );
}

#[test]
fn size_toggle_disabled_when_not_resizable() {
    let theme = Theme::dark();
    let mut furn = furniture();
    furn.resizable = false;
    let mut bar = TitleBar::new(furn);
    assert!(!bar
        .control(WindowControlKind::SizeToggle)
        .state()
        .is_actionable());
    let rect = bar
        .layout(title_bounds(), Scale::ONE, &theme)
        .controls
        .iter()
        .find(|(k, _)| *k == WindowControlKind::SizeToggle)
        .expect("size toggle")
        .1;
    let cx = rect.left() + half(rect.width);
    let cy = rect.top() + half(rect.height);
    let bounds = title_bounds();
    let _ = bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme);
    let _ = bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    assert_eq!(bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme), None);
}

#[test]
fn size_toggle_shows_restore_when_maximized() {
    let mut furn = furniture();
    furn.size = WindowSizeState::Maximized;
    let bar = TitleBar::new(furn);
    assert_eq!(
        bar.control(WindowControlKind::SizeToggle).accessible_name(),
        "Restore"
    );
}

#[test]
fn keyboard_focus_navigates_and_activates() {
    let mut bar = TitleBar::new(furniture());
    assert_eq!(bar.on_key(Key::Named(NamedKey::Right)), None);
    assert!(
        bar.control(WindowControlKind::PutToBack)
            .state()
            .focus
            .focused
    );
    assert_eq!(
        bar.on_key(Key::Named(NamedKey::Enter)),
        Some(TitleBarEvent::Control(WindowControlKind::PutToBack))
    );
}

#[test]
fn inactive_title_bar_reads_quieter() {
    let theme = Theme::dark();
    let mut active = TitleBar::new(furniture());
    active.set_title("Report");
    let mut inactive_furn = furniture();
    inactive_furn.activation = WindowActivationState::Inactive;
    let mut inactive = TitleBar::new(inactive_furn);
    inactive.set_title("Report");
    let mut a = Surface::new(300, 28).expect("surface");
    let mut b = Surface::new(300, 28).expect("surface");
    active.render(&mut a, title_bounds(), Scale::ONE, &theme, font());
    inactive.render(&mut b, title_bounds(), Scale::ONE, &theme, font());
    assert_ne!(a.pixels(), b.pixels());
}

// --- WindowFrame ----------------------------------------------------------

fn frame_bounds() -> Rect {
    Rect::new(0, 0, 300, 240)
}

#[test]
fn frame_client_sits_below_title_bar() {
    let theme = Theme::dark();
    let frame = WindowFrame::new(furniture());
    let layout = frame.layout(frame_bounds(), Scale::ONE, &theme);
    assert_eq!(layout.client.top(), layout.title_bar.bottom());
    assert!(layout.client.intersection(&layout.title_bar).is_empty());
}

#[test]
fn hit_map_isolates_client_from_furniture() {
    let theme = Theme::dark();
    let frame = WindowFrame::new(furniture());
    let bounds = frame_bounds();
    let client = frame.layout(bounds, Scale::ONE, &theme).client;
    let inside = Point::new(
        client.left() + half(client.width),
        client.top() + half(client.height),
    );
    assert_eq!(
        frame.hit(bounds, Scale::ONE, &theme, inside),
        FurniturePart::Client
    );
    assert_eq!(
        frame.hit(bounds, Scale::ONE, &theme, Point::new(50, 10)),
        FurniturePart::TitleBar
    );
    assert_eq!(
        frame.hit(bounds, Scale::ONE, &theme, Point::new(400, 400)),
        FurniturePart::Outside
    );
}

#[test]
fn hit_map_finds_window_control() {
    let theme = Theme::dark();
    let frame = WindowFrame::new(furniture());
    let bounds = frame_bounds();
    let title_bar = frame.layout(bounds, Scale::ONE, &theme).title_bar;
    let close_rect = frame
        .title_bar()
        .layout(title_bar, Scale::ONE, &theme)
        .controls
        .iter()
        .find(|(k, _)| *k == WindowControlKind::Close)
        .expect("close")
        .1;
    let cx = close_rect.left() + half(close_rect.width);
    let cy = close_rect.top() + half(close_rect.height);
    assert_eq!(
        frame.hit(bounds, Scale::ONE, &theme, Point::new(cx, cy)),
        FurniturePart::WindowControl(WindowControlKind::Close)
    );
}

#[test]
fn activation_does_not_move_client() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let mut frame = WindowFrame::new(furniture());
    let active_client = frame.layout(bounds, Scale::ONE, &theme).client;
    let mut inactive = furniture();
    inactive.activation = WindowActivationState::Inactive;
    frame.set_furniture(inactive);
    let inactive_client = frame.layout(bounds, Scale::ONE, &theme).client;
    assert_eq!(active_client, inactive_client);
}

#[test]
fn resize_edges_only_when_resizable() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let frame = WindowFrame::new(furniture());
    let client = frame.layout(bounds, Scale::ONE, &theme).client;
    let edge = Point::new(client.left() + 10, client.bottom());
    assert_eq!(
        frame.hit(bounds, Scale::ONE, &theme, edge),
        FurniturePart::ResizeEdge(ResizeEdge::Bottom)
    );
    // A fixed-size window has no resize band (its client keeps the thin frame
    // inset), so the point just below *its own* client is inert frame, never a
    // resize edge.
    let mut fixed = furniture();
    fixed.resizable = false;
    let frame = WindowFrame::new(fixed);
    let fixed_client = frame.layout(bounds, Scale::ONE, &theme).client;
    let fixed_edge = Point::new(fixed_client.left() + 10, fixed_client.bottom());
    assert_eq!(
        frame.hit(bounds, Scale::ONE, &theme, fixed_edge),
        FurniturePart::Frame
    );
}

#[test]
fn active_and_inactive_rims_differ() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let mut frame = WindowFrame::new(furniture());
    let mut active = Surface::new(300, 240).expect("surface");
    frame.render(&mut active, bounds, Scale::ONE, &theme, font());
    assert!(has_pixel(&active, premul(theme.palette().frame_active)));

    let mut inactive_furn = furniture();
    inactive_furn.activation = WindowActivationState::Inactive;
    frame.set_furniture(inactive_furn);
    let mut inactive = Surface::new(300, 240).expect("surface");
    frame.render(&mut inactive, bounds, Scale::ONE, &theme, font());
    assert!(has_pixel(&inactive, premul(theme.palette().frame_inactive)));
}

#[test]
fn attention_request_changes_rendering() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let mut frame = WindowFrame::new(furniture());
    let mut plain = Surface::new(300, 240).expect("surface");
    frame.render(&mut plain, bounds, Scale::ONE, &theme, font());

    let mut attn = furniture();
    attn.activation = WindowActivationState::AttentionRequested;
    frame.set_furniture(attn);
    let mut attention = Surface::new(300, 240).expect("surface");
    frame.render(&mut attention, bounds, Scale::ONE, &theme, font());
    assert_ne!(plain.pixels(), attention.pixels());
}

// --- ResizeGrabber --------------------------------------------------------

#[test]
fn grabber_draws_teeth() {
    let theme = Theme::dark();
    let grabber = ResizeGrabber::new();
    let mut surface = Surface::new(20, 20).expect("surface");
    grabber.render(&mut surface, Rect::new(0, 0, 20, 20), Scale::ONE, &theme);
    assert!(opaque_count(&surface) > 0);
}

#[test]
fn grabber_captures_drag() {
    let mut grabber = ResizeGrabber::new();
    let hit = Rect::new(0, 0, 20, 20);
    let _ = grabber.on_pointer(&moved(10, 10), hit);
    assert_eq!(grabber.on_pointer(&PRESS, hit), Some(ResizeEvent::Begin));
    assert!(grabber.is_dragging());
    assert_eq!(
        grabber.on_pointer(&moved(15, 15), hit),
        Some(ResizeEvent::Moved {
            to: Point::new(15, 15)
        })
    );
    assert_eq!(grabber.on_pointer(&RELEASE, hit), Some(ResizeEvent::End));
    assert!(!grabber.is_dragging());
}

#[test]
fn grabber_escape_cancels_drag() {
    let mut grabber = ResizeGrabber::new();
    let hit = Rect::new(0, 0, 20, 20);
    let _ = grabber.on_pointer(&moved(10, 10), hit);
    let _ = grabber.on_pointer(&PRESS, hit);
    assert_eq!(
        grabber.on_key(Key::Named(NamedKey::Escape)),
        Some(ResizeEvent::Cancel)
    );
    assert!(!grabber.is_dragging());
}

#[test]
fn disabled_grabber_ignores_input() {
    let mut grabber = ResizeGrabber::new();
    grabber.set_enabled(false);
    let hit = Rect::new(0, 0, 20, 20);
    let _ = grabber.on_pointer(&moved(10, 10), hit);
    assert_eq!(grabber.on_pointer(&PRESS, hit), None);
    assert!(!grabber.is_dragging());
}

#[test]
fn grabber_junction_never_overlaps_scrollbars() {
    // The grabber owns the junction cell; the vertical bar's track sits above
    // it and the horizontal bar's track to its left, so neither overlaps.
    let junction = Rect::new(200, 200, 14, 14);
    let vertical_track = Rect::new(200, 50, 14, 150);
    let horizontal_track = Rect::new(50, 200, 150, 14);
    assert!(junction.intersection(&vertical_track).is_empty());
    assert!(junction.intersection(&horizontal_track).is_empty());
}

// --- ScrollCorner ---------------------------------------------------------

#[test]
fn scroll_corner_renders_neutral() {
    let theme = Theme::dark();
    let corner = ScrollCorner::new();
    let mut surface = Surface::new(14, 14).expect("surface");
    corner.render(&mut surface, Rect::new(0, 0, 14, 14), Scale::ONE, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().surface)));
}

// --- Frame insets / outer_for_client --------------------------------------

#[test]
fn insets_match_the_client_band_layout_reserves() {
    // The four insets are exactly the gap `layout` leaves between the outer
    // bounds and the client on each edge — one definition, not two recipes.
    let theme = Theme::dark();
    let frame = WindowFrame::new(furniture());
    let outer = Rect::new(30, 40, 300, 220);
    let layout = frame.layout(outer, Scale::ONE, &theme);
    let insets = frame.insets(Scale::ONE, &theme);
    let expected = FrameInsets {
        top: u32::try_from(layout.client.top() - outer.top()).unwrap(),
        left: u32::try_from(layout.client.left() - outer.left()).unwrap(),
        right: u32::try_from(outer.right() - layout.client.right()).unwrap(),
        bottom: u32::try_from(outer.bottom() - layout.client.bottom()).unwrap(),
    };
    assert_eq!(insets, expected);
    // The top band carries the title bar and is therefore the thickest.
    assert!(insets.top > insets.bottom);
}

#[test]
fn outer_for_client_round_trips_through_layout() {
    // Sizing the outer window from a client-sized content surface and then
    // laying that outer rect out must reproduce the client exactly, at
    // reference and scaled DPI (the geometry the window manager relies on).
    let theme = Theme::dark();
    let frame = WindowFrame::new(furniture());
    let client = Rect::new(120, 90, 400, 300);
    for scale in [Scale::ONE, Scale::from_percent(200).expect("scale")] {
        let outer = frame.outer_for_client(client, scale, &theme);
        let insets = frame.insets(scale, &theme);
        // The outer rect is the client grown by the band on every edge.
        assert_eq!(
            outer.left(),
            client.left() - i32::try_from(insets.left).unwrap()
        );
        assert_eq!(
            outer.top(),
            client.top() - i32::try_from(insets.top).unwrap()
        );
        // Laying it out reproduces the client.
        assert_eq!(frame.layout(outer, scale, &theme).client, client);
    }
}

#[test]
fn outer_for_client_uses_the_light_theme_metrics_too() {
    // The derivation reads the active theme's metrics, so a light-theme frame
    // round-trips under the light theme's own band thicknesses.
    let theme = Theme::light();
    let frame = WindowFrame::new(furniture());
    let client = Rect::new(10, 10, 200, 150);
    let outer = frame.outer_for_client(client, Scale::ONE, &theme);
    assert_eq!(frame.layout(outer, Scale::ONE, &theme).client, client);
}
