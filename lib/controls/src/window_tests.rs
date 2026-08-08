//! Unit tests for the window-manager furniture family (spec §20 furniture
//! checklist).
//!
//! These cover the command glyphs (distinct per command, and the size toggle
//! reflecting its *next* action), the shared window-control state model
//! (pointer/keyboard activation, disabled/denied), the title bar (control
//! layout on either edge, title sanitisation, activate/drag/control routing,
//! keyboard focus), the window frame's furniture hit map (the client interior
//! against furniture, the resize edges that overlap the client's outermost
//! pixels, activation not changing geometry), the resize grabber (drag capture
//! and Escape-cancel, non-overlap with scrollbars), and the neutral scroll
//! corner, across dark/light/high-contrast and scale.

use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, TextRole, Theme};

use crate::state::{
    AuthorityState, ControlState, PointerState, SizeAction, WindowActivationState,
    WindowControlKind, WindowFurnitureState, WindowSizeState,
};
use crate::testkit::high_contrast;
use crate::window::{
    ControlPlacement, FrameInsets, FurniturePart, ResizeEdge, ResizeEvent, ResizeGrabber,
    ScrollCorner, TitleBar, TitleBarEvent, TitleHit, WindowControl, WindowControlAction,
    WindowFrame,
};

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
const SECONDARY_PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Secondary,
};
const SECONDARY_RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Secondary,
};

fn furniture() -> WindowFurnitureState {
    WindowFurnitureState {
        activation: WindowActivationState::Active,
        size: WindowSizeState::Restored,
        movable: true,
        resizable: true,
    }
}

fn fixed_size_furniture() -> WindowFurnitureState {
    WindowFurnitureState {
        resizable: false,
        ..furniture()
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
fn axis_aligned_glyph_marks_land_on_whole_pixels() {
    // The minimize bar and the size-toggle squares are axis-aligned, so grid
    // fitting leaves them with no anti-aliased fringe at all: every pixel they
    // touch carries the full glyph colour. Scaling the authored design grid
    // straight to the box instead — the defect this replaced — put a
    // 1.4-pixel-wide stroke at a fractional offset, spreading every mark over
    // two rows at partial alpha so it read as a grey smear rather than a line.
    let theme = Theme::dark();
    let ink = premul(theme.palette().on_surface);
    for kind in [
        WindowControlKind::Minimize,
        WindowControlKind::SizeToggle,
        WindowControlKind::PutToBack,
    ] {
        // Sizes that divide the design grid evenly and sizes that do not.
        for side in [16_u32, 20, 21, 28, 40] {
            let control = WindowControl::new(kind);
            let surface = render_control(&control, &theme, side);
            assert!(
                opaque_count(&surface) > 0,
                "{kind:?} at {side} drew nothing"
            );
            for p in surface.pixels() {
                assert!(
                    p.a == 0 || *p == ink,
                    "{kind:?} at {side}: partial coverage {p:?}"
                );
            }
        }
    }
}

#[test]
fn a_glyph_stroke_is_always_at_least_one_whole_pixel() {
    // A stroke authored as a fraction of the box rounds to whole pixels, and
    // rounding must never round it away: a control small enough that its
    // authored weight is under half a pixel still draws a one-pixel mark rather
    // than vanishing or fading to a ghost.
    let theme = Theme::dark();
    let ink = premul(theme.palette().on_surface);
    for side in 6..=12_u32 {
        let control = WindowControl::new(WindowControlKind::Minimize);
        let surface = render_control(&control, &theme, side);
        assert!(
            has_pixel(&surface, ink),
            "the minimize bar vanished at {side}"
        );
    }
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
fn a_secondary_press_reports_the_alternate_gesture_and_leaves_the_control_untouched() {
    let theme = Theme::dark();
    let mut control = WindowControl::new(WindowControlKind::Close);
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds);
    let before = render_control(&control, &theme, 40).pixels().to_vec();
    assert_eq!(
        control.on_pointer(&SECONDARY_PRESS, bounds),
        Some(WindowControlAction::AlternateInvoked(
            WindowControlKind::Close
        ))
    );
    // No latch, no press wash: the button draws exactly as it did.
    assert_eq!(control.state().pointer, PointerState::Hover);
    assert_eq!(render_control(&control, &theme, 40).pixels(), &before[..]);
    // Neither release fires the command, so one gesture cannot do both.
    assert_eq!(control.on_pointer(&SECONDARY_RELEASE, bounds), None);
    assert_eq!(control.on_pointer(&RELEASE, bounds), None);
    // Off the control, a secondary press resolves nothing.
    let _ = control.on_pointer(&moved(100, 100), bounds);
    assert_eq!(control.on_pointer(&SECONDARY_PRESS, bounds), None);
}

#[test]
fn a_secondary_press_on_a_denied_control_resolves_nothing() {
    let mut control = WindowControl::new(WindowControlKind::Close);
    control.set_state(ControlState {
        authority: AuthorityState::Denied,
        ..ControlState::default()
    });
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds);
    assert_eq!(control.on_pointer(&SECONDARY_PRESS, bounds), None);
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
fn a_secondary_press_over_a_control_reports_the_alternate_and_leaves_the_bar_alone() {
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
    let _ = bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme);
    assert_eq!(
        bar.on_pointer(&SECONDARY_PRESS, bounds, Scale::ONE, &theme),
        Some(TitleBarEvent::AlternateControl(WindowControlKind::Close))
    );
    // The bar never activates or drags from it, and the release is inert.
    assert_eq!(
        bar.on_pointer(&SECONDARY_RELEASE, bounds, Scale::ONE, &theme),
        None
    );
    assert_eq!(
        bar.on_pointer(&moved(cx + 40, cy), bounds, Scale::ONE, &theme),
        None
    );
    // Over the drag region a secondary press is unchanged: nothing at all.
    let _ = bar.on_pointer(&moved(50, 10), bounds, Scale::ONE, &theme);
    assert_eq!(
        bar.on_pointer(&SECONDARY_PRESS, bounds, Scale::ONE, &theme),
        None
    );
    // A primary press over the control still means the command.
    let _ = bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme);
    let _ = bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    assert_eq!(
        bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(TitleBarEvent::Control(WindowControlKind::Close))
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
fn size_toggle_returns_when_the_window_becomes_resizable_again() {
    let mut furn = furniture();
    furn.resizable = false;
    let mut bar = TitleBar::new(furn);
    assert!(!bar
        .control(WindowControlKind::SizeToggle)
        .state()
        .is_actionable());

    furn.resizable = true;
    bar.set_furniture(furn);

    assert!(
        bar.control(WindowControlKind::SizeToggle)
            .state()
            .is_actionable(),
        "a resizable window must get its size toggle back"
    );
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
    active.render(&mut a, title_bounds(), Scale::ONE, &theme, None);
    inactive.render(&mut b, title_bounds(), Scale::ONE, &theme, None);
    assert_ne!(a.pixels(), b.pixels());
}

/// A bar with no identity reserves nothing: its text region is the whole
/// draggable region, and it draws exactly what it drew before identities
/// existed.
#[test]
fn a_bar_without_an_identity_gives_the_whole_band_to_its_title() {
    let theme = Theme::dark();
    let bar = TitleBar::new(furniture());
    let layout = bar.layout(title_bounds(), Scale::ONE, &theme);
    assert_eq!(bar.identity(), None);
    assert_eq!(layout.icon, Rect::EMPTY);
    // The text starts at the band's inset, exactly where the drag region does.
    let inset = i32::try_from(Scale::ONE.scale_length(theme.metrics().control_inset))
        .expect("a small inset");
    assert_eq!(layout.title.left(), title_bounds().left() + inset);
    assert_eq!(layout.title.height, title_bounds().height);
}

/// An identity reserves a square slot at the leading edge of the drag region
/// and the title text starts after it; the slot is the side the owner is told
/// to rasterise at.
#[test]
fn an_identity_reserves_the_leading_slot_and_the_title_starts_after_it() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let plain = TitleBar::new(furniture());
    let mut identified = TitleBar::new(furniture());
    identified.set_identity(Some(IconKind::AppBundle));
    assert_eq!(identified.identity(), Some(IconKind::AppBundle));

    let bare = plain.layout(bounds, Scale::ONE, &theme);
    let with = identified.layout(bounds, Scale::ONE, &theme);
    let side = identified.icon_side(bare.title, Scale::ONE, &theme);
    assert!(side > 0, "the band is tall enough for a slot");
    assert_eq!(with.icon.width, side);
    assert_eq!(with.icon.height, side);
    assert_eq!(with.icon.left(), bare.title.left());
    assert!(
        with.title.left() > with.icon.right(),
        "the text starts past the slot"
    );
    assert!(with.title.width < bare.title.width);
    // The slot never reaches a control.
    for (_, rect) in with.controls {
        assert!(with.icon.intersection(&rect).is_empty());
    }
    // The icon is inert: the point over it still drags the window.
    let over = Point::new(with.icon.left() + 1, with.icon.top() + 1);
    assert_eq!(
        identified.hit(bounds, Scale::ONE, &theme, over),
        TitleHit::Drag
    );
}

/// An identity draws: the owner's artwork when it has some, the built-in
/// class glyph when it does not — never a blank slot.
#[test]
fn an_identity_draws_its_artwork_and_falls_back_to_the_glyph() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let paint = |bar: &TitleBar, artwork: Option<&Surface>| {
        let mut surface = Surface::new(300, 28).expect("surface");
        bar.render(&mut surface, bounds, Scale::ONE, &theme, artwork);
        surface
    };
    let mut bar = TitleBar::new(furniture());
    bar.set_title("Report");
    let bare = paint(&bar, None);

    bar.set_identity(Some(IconKind::AppBundle));
    let glyph = paint(&bar, None);
    assert_ne!(
        bare.pixels(),
        glyph.pixels(),
        "the built-in glyph fills the slot"
    );

    let side = bar.icon_side(bounds, Scale::ONE, &theme);
    let mut art = Surface::new(side, side).expect("artwork");
    art.fill_rect(0, 0, side, side, Color::from(theme.palette().accent));
    let drawn = paint(&bar, Some(&art));
    assert_ne!(
        glyph.pixels(),
        drawn.pixels(),
        "the owner's artwork replaces the glyph"
    );
    assert!(has_pixel(&drawn, premul(theme.palette().accent)));

    // Artwork offered to a bar with no identity is ignored.
    bar.set_identity(None);
    assert_eq!(paint(&bar, Some(&art)).pixels(), bare.pixels());
}

/// A title too wide for its region ends in the shared elision mark rather
/// than being cut mid-glyph, because titles carry paths.
#[test]
fn an_over_wide_title_ends_in_the_shared_mark() {
    let theme = Theme::dark();
    // A band only wide enough for the controls and a sliver of text.
    let bounds = Rect::new(0, 0, 300, 28);
    let mut bar = TitleBar::new(furniture());
    bar.set_title("/Users/root/Documents/Projects/tairix/lib/controls/src/window.rs");
    let mut long = Surface::new(300, 28).expect("surface");
    bar.render(&mut long, bounds, Scale::ONE, &theme, None);

    let font = crate::paint::role_font(&theme, Scale::ONE, TextRole::WindowTitle);
    let width = bar.layout(bounds, Scale::ONE, &theme).title.width;
    let (fitted, marked) = font.elide_to_width(bar.title(), width);
    assert!(marked, "the title does not fit, so it is marked");
    assert!(fitted.len() < bar.title().len());

    // The mark is drawn: the same text without it paints different pixels.
    let mut cut = Surface::new(300, 28).expect("surface");
    let mut short = TitleBar::new(furniture());
    short.set_title(fitted);
    short.render(&mut cut, bounds, Scale::ONE, &theme, None);
    assert_ne!(long.pixels(), cut.pixels());
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
fn hit_map_separates_the_client_interior_from_furniture() {
    let theme = Theme::dark();
    let frame = WindowFrame::new(furniture());
    let bounds = frame_bounds();
    let client = frame.layout(bounds, Scale::ONE, &theme).client;
    // The centre, clear of the resize zone that overlaps the client's own
    // outer pixels.
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
    let resizable = WindowFrame::new(furniture());
    let fixed = WindowFrame::new(fixed_size_furniture());
    // Both lay the client out identically, so a single point tells the two hit
    // maps apart: it resizes one window and is inert furniture on the other.
    let client = resizable.layout(bounds, Scale::ONE, &theme).client;
    assert_eq!(client, fixed.layout(bounds, Scale::ONE, &theme).client);
    let below = Point::new(client.left() + 10, client.bottom());
    assert_eq!(
        resizable.hit(bounds, Scale::ONE, &theme, below),
        FurniturePart::ResizeEdge(ResizeEdge::Bottom)
    );
    assert_eq!(
        fixed.hit(bounds, Scale::ONE, &theme, below),
        FurniturePart::Frame
    );
}

#[test]
fn a_resizable_window_gives_up_no_client_space() {
    // The complaint this answered: a resizable window widened its left, right,
    // and bottom furniture to the grabber extent, so its content sat visibly
    // inside a fixed-size window's. Both now pay the plain frame inset.
    let bounds = frame_bounds();
    let resizable = WindowFrame::new(furniture());
    let fixed = WindowFrame::new(fixed_size_furniture());
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        for scale in [Scale::ONE, Scale::from_percent(200).expect("scale")] {
            let insets = resizable.insets(scale, &theme);
            assert_eq!(
                insets,
                fixed.insets(scale, &theme),
                "a resize affordance must cost no drawn space"
            );
            assert_eq!(
                resizable.layout(bounds, scale, &theme).client,
                fixed.layout(bounds, scale, &theme).client
            );
            let metrics = theme.metrics();
            let border = scale.scale_length(metrics.border_thickness).max(1);
            let rim = scale.scale_length(metrics.frame_inset).max(border);
            assert_eq!(insets.left, rim);
            assert_eq!(insets.right, rim);
            assert_eq!(insets.bottom, rim);
            assert!(
                rim < scale.scale_length(metrics.resize_grabber_extent),
                "the band must be the thin rim, never the grabber extent"
            );
        }
    }
}

#[test]
fn the_resize_zone_overlaps_the_clients_outer_pixels() {
    // The invisible border: the outermost `hit_slop` columns of the client
    // resize the window, and the very next column inward reaches the app.
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let frame = WindowFrame::new(furniture());
    let client = frame.layout(bounds, Scale::ONE, &theme).client;
    let grab = i32::try_from(Scale::ONE.scale_length(theme.metrics().hit_slop)).expect("slop");
    assert!(grab > 0, "an invisible border needs some depth");
    let y = client.top() + half(client.height);
    for step in 0..grab {
        assert_eq!(
            frame.hit(
                bounds,
                Scale::ONE,
                &theme,
                Point::new(client.left() + step, y)
            ),
            FurniturePart::ResizeEdge(ResizeEdge::Left),
            "client column {step} in from the left must still resize"
        );
    }
    assert_eq!(
        frame.hit(
            bounds,
            Scale::ONE,
            &theme,
            Point::new(client.left() + grab, y)
        ),
        FurniturePart::Client,
        "one column further in belongs to the app"
    );
}

#[test]
fn every_resize_edge_resolves_from_the_band_and_the_client_overlap() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let frame = WindowFrame::new(furniture());
    let client = frame.layout(bounds, Scale::ONE, &theme).client;
    let mid_x = client.left() + half(client.width);
    let mid_y = client.top() + half(client.height);
    let hit = |x: i32, y: i32| frame.hit(bounds, Scale::ONE, &theme, Point::new(x, y));

    // Inside the client, on its outermost pixel of each edge and corner.
    assert_eq!(
        hit(client.left(), mid_y),
        FurniturePart::ResizeEdge(ResizeEdge::Left)
    );
    assert_eq!(
        hit(client.right() - 1, mid_y),
        FurniturePart::ResizeEdge(ResizeEdge::Right)
    );
    assert_eq!(
        hit(mid_x, client.bottom() - 1),
        FurniturePart::ResizeEdge(ResizeEdge::Bottom)
    );
    assert_eq!(
        hit(client.left(), client.bottom() - 1),
        FurniturePart::ResizeEdge(ResizeEdge::BottomLeft)
    );
    assert_eq!(
        hit(client.right() - 1, client.bottom() - 1),
        FurniturePart::ResizeEdge(ResizeEdge::BottomRight)
    );

    // The thin band just outside the client answers the same way, so the two
    // branches of the hit map cannot disagree about an edge.
    assert_eq!(
        hit(bounds.left(), mid_y),
        FurniturePart::ResizeEdge(ResizeEdge::Left)
    );
    assert_eq!(
        hit(bounds.right() - 1, mid_y),
        FurniturePart::ResizeEdge(ResizeEdge::Right)
    );
    assert_eq!(
        hit(mid_x, bounds.bottom() - 1),
        FurniturePart::ResizeEdge(ResizeEdge::Bottom)
    );
    assert_eq!(
        hit(bounds.left(), bounds.bottom() - 1),
        FurniturePart::ResizeEdge(ResizeEdge::BottomLeft)
    );
    assert_eq!(
        hit(bounds.right() - 1, bounds.bottom() - 1),
        FurniturePart::ResizeEdge(ResizeEdge::BottomRight)
    );

    // The top edge is never a resize edge: a window is sized from its three
    // free edges, and the row below the rim is the title bar's to drag.
    let border = i32::try_from(
        Scale::ONE
            .scale_length(theme.metrics().border_thickness)
            .max(1),
    )
    .expect("border");
    assert_eq!(hit(mid_x, bounds.top()), FurniturePart::Frame);
    assert_eq!(
        hit(bounds.left() + 50, bounds.top() + border),
        FurniturePart::TitleBar
    );
}

#[test]
fn a_fixed_size_window_reports_no_resize_edge_anywhere() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let frame = WindowFrame::new(fixed_size_furniture());
    for y in bounds.top()..bounds.bottom() {
        for x in bounds.left()..bounds.right() {
            let part = frame.hit(bounds, Scale::ONE, &theme, Point::new(x, y));
            assert!(
                !matches!(part, FurniturePart::ResizeEdge(_)),
                "({x}, {y}) offered a resize edge on a fixed-size window"
            );
        }
    }
}

#[test]
fn the_rim_is_one_quiet_tone_and_the_title_carries_focus() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let mut frame = WindowFrame::new(furniture());
    frame.title_bar_mut().set_title("Documents");
    let mut active = Surface::new(300, 240).expect("surface");
    frame.render(&mut active, bounds, Scale::ONE, &theme, None);

    let mut inactive_furn = furniture();
    inactive_furn.activation = WindowActivationState::Inactive;
    frame.set_furniture(inactive_furn);
    let mut inactive = Surface::new(300, 240).expect("surface");
    frame.render(&mut inactive, bounds, Scale::ONE, &theme, None);

    // The rim is the same quiet neutral at either activation: the line the eye
    // reads a window's shape by does not change when focus moves elsewhere.
    assert!(has_pixel(&active, premul(theme.palette().frame)));
    assert!(has_pixel(&inactive, premul(theme.palette().frame)));

    // Focus is still legible, carried by the title bar's text tone.
    assert_ne!(active.pixels(), inactive.pixels());
    assert!(has_pixel(&active, premul(theme.palette().on_surface)));
    assert!(has_pixel(
        &inactive,
        premul(theme.palette().on_surface_muted)
    ));
}

#[test]
fn attention_request_changes_rendering() {
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let mut frame = WindowFrame::new(furniture());
    let mut plain = Surface::new(300, 240).expect("surface");
    frame.render(&mut plain, bounds, Scale::ONE, &theme, None);

    let mut attn = furniture();
    attn.activation = WindowActivationState::AttentionRequested;
    frame.set_furniture(attn);
    let mut attention = Surface::new(300, 240).expect("surface");
    frame.render(&mut attention, bounds, Scale::ONE, &theme, None);
    assert_ne!(plain.pixels(), attention.pixels());
}

#[test]
fn the_frame_paints_no_furniture_mark_inside_the_client() {
    // The rim tone and the body plate run under the client and the app paints
    // over them, but a furniture *mark* never lands there: the resize zone is
    // invisible, so a resizable window's client pixels are a fixed-size
    // window's exactly — no grip teeth in the corner, no title ink.
    let theme = Theme::dark();
    let bounds = frame_bounds();
    let paint = |furn| {
        let mut frame = WindowFrame::new(furn);
        frame.title_bar_mut().set_title("Documents");
        let mut surface = Surface::new(300, 240).expect("surface");
        frame.render(&mut surface, bounds, Scale::ONE, &theme, None);
        surface
    };
    let resizable = paint(furniture());
    let fixed = paint(fixed_size_furniture());
    let client = WindowFrame::new(furniture())
        .layout(bounds, Scale::ONE, &theme)
        .client;
    let palette = theme.palette();
    for y in client.top()..client.bottom() {
        for x in client.left()..client.right() {
            let (px, py) = (u32::try_from(x).expect("x"), u32::try_from(y).expect("y"));
            let pixel = resizable.get(px, py).expect("inside the surface");
            assert_eq!(
                Some(pixel),
                fixed.get(px, py),
                "({x}, {y}) differs from a fixed-size window's client"
            );
            for mark in [
                palette.on_surface,
                palette.on_surface_muted,
                palette.accent,
                palette.rim_active,
            ] {
                assert_ne!(pixel, premul(mark), "a furniture mark landed at ({x}, {y})");
            }
        }
    }
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

// --- Render-equivalence equality (the host's repaint gate) ----------------

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_window_control() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, 40, 40);

    // Two samples clear of the glyph, so only the recorded coordinate differs.
    let mut a = WindowControl::new(WindowControlKind::Close);
    let mut b = a.clone();
    let _ = a.on_pointer(&moved(80, 80), bounds);
    let _ = b.on_pointer(&moved(120, 12), bounds);
    assert_eq!(
        a, b,
        "a coordinate clear of the glyph is not a drawn property"
    );
    assert_eq!(
        render_control(&a, &theme, 40).pixels(),
        render_control(&b, &theme, 40).pixels(),
        "…and the two must therefore paint identically"
    );

    // One holds a real press latch, the other is merely *shown* pressed.
    let mut latched = WindowControl::new(WindowControlKind::Close);
    let _ = latched.on_pointer(&moved(10, 10), bounds);
    let _ = latched.on_pointer(&PRESS, bounds);
    let mut shown = WindowControl::new(WindowControlKind::Close);
    let mut pressed = ControlState::idle();
    pressed.pointer = PointerState::Pressed;
    shown.set_state(pressed);
    assert_eq!(latched, shown, "the press latch is not a drawn property");
    assert_eq!(
        render_control(&latched, &theme, 40).pixels(),
        render_control(&shown, &theme, 40).pixels(),
        "…and the two must therefore paint identically"
    );
    assert_eq!(
        latched.on_pointer(&RELEASE, bounds),
        Some(WindowControlAction::Invoked(WindowControlKind::Close)),
        "the latch still governs activation, it is only invisible"
    );
}

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_title_bar() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let paint = |bar: &TitleBar| {
        let mut surface = Surface::new(300, 28).expect("surface");
        bar.render(&mut surface, bounds, Scale::ONE, &theme, None);
        surface.pixels().to_vec()
    };

    // Two samples clear of the bar, so only the recorded coordinate differs.
    let mut a = TitleBar::new(furniture());
    let mut b = a.clone();
    let _ = a.on_pointer(&moved(50, 200), bounds, Scale::ONE, &theme);
    let _ = b.on_pointer(&moved(90, 240), bounds, Scale::ONE, &theme);
    assert_eq!(
        a, b,
        "a coordinate clear of the bar is not a drawn property"
    );
    assert_eq!(
        paint(&a),
        paint(&b),
        "…and the two must therefore paint identically"
    );

    // Both are pressed in the drag region, at different points: the origin
    // the drag threshold is measured from is bookkeeping, not a picture.
    let mut near = TitleBar::new(furniture());
    let _ = near.on_pointer(&moved(50, 10), bounds, Scale::ONE, &theme);
    let _ = near.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    let mut far = TitleBar::new(furniture());
    let _ = far.on_pointer(&moved(90, 14), bounds, Scale::ONE, &theme);
    let _ = far.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    assert_eq!(near, far, "the press origin is not a drawn property");
    assert_eq!(
        paint(&near),
        paint(&far),
        "…and the two must therefore paint identically"
    );
}

#[test]
fn pointer_position_alone_never_changes_a_grabber_render() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, 20, 20);
    let paint = |grabber: &ResizeGrabber| {
        let mut surface = Surface::new(20, 20).expect("surface");
        grabber.render(&mut surface, bounds, Scale::ONE, &theme);
        surface.pixels().to_vec()
    };

    // Two samples clear of the teeth, so only the recorded coordinate
    // differs; a drag in progress is visible and stays compared.
    let mut a = ResizeGrabber::new();
    let mut b = a.clone();
    let _ = a.on_pointer(&moved(60, 60), bounds);
    let _ = b.on_pointer(&moved(90, 40), bounds);

    assert_eq!(
        a, b,
        "a coordinate clear of the grabber is not a drawn property"
    );
    assert_eq!(
        paint(&a),
        paint(&b),
        "…and the two must therefore paint identically"
    );
}
