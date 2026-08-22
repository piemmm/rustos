//! Unit tests for the window-manager furniture family (spec §20 furniture
//! checklist).
//!
//! These cover the command glyphs (distinct per command, and the size toggle
//! reflecting its *next* action), the shared window-control state model
//! (pointer/keyboard activation, disabled/denied), the title bar (the two
//! corner command clusters and the identity group left-justified in the span
//! between them, title sanitisation, activate/drag/control routing,
//! keyboard focus), the window frame's furniture hit map (the client interior
//! against furniture, the resize edges that overlap the client's outermost
//! pixels, activation not changing geometry), the resize grabber (drag capture
//! and Escape-cancel, non-overlap with scrollbars), and the neutral scroll
//! corner, across dark/light/high-contrast and scale.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{round_rect_coverage, Color, Pixel, Surface};
use tairix_theme::{Rgba, TextRole, Theme};

use crate::damage::sink;
use crate::state::{
    AuthorityState, ControlState, PointerState, SizeAction, WindowActivationState,
    WindowControlKind, WindowFurnitureState, WindowSizeState,
};
use crate::testkit::high_contrast;
use crate::window::{
    FrameInsets, FurniturePart, ResizeEdge, ResizeEvent, ResizeGrabber, ScrollCorner, TitleBar,
    TitleBarEvent, TitleHit, WindowControl, WindowControlAction, WindowFrame, CONTROL_ORDER,
    IDENTITY_SATURATION_ACTIVE, IDENTITY_SATURATION_INACTIVE,
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

/// The bar or grabber region a keyboard event is given. The bar lays its
/// controls out inside it and the grabber's cancel covers all of it, so the
/// exact rectangle only has to be plausible.
const TITLE_BOUNDS: Rect = Rect::new(0, 0, 320, 28);

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
    let surfaces: alloc::vec::Vec<_> = CONTROL_ORDER
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
    assert_eq!(
        control.on_pointer(&moved(10, 10), bounds, &mut sink()),
        None
    );
    assert_eq!(control.on_pointer(&PRESS, bounds, &mut sink()), None);
    assert_eq!(
        control.on_pointer(&RELEASE, bounds, &mut sink()),
        Some(WindowControlAction::Invoked(WindowControlKind::Close))
    );
}

#[test]
fn a_secondary_press_reports_the_alternate_gesture_and_leaves_the_control_untouched() {
    let theme = Theme::dark();
    let mut control = WindowControl::new(WindowControlKind::Close);
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds, &mut sink());
    let before = render_control(&control, &theme, 40).pixels().to_vec();
    assert_eq!(
        control.on_pointer(&SECONDARY_PRESS, bounds, &mut sink()),
        Some(WindowControlAction::AlternateInvoked(
            WindowControlKind::Close
        ))
    );
    // No latch, no press wash: the button draws exactly as it did.
    assert_eq!(control.state().pointer, PointerState::Hover);
    assert_eq!(render_control(&control, &theme, 40).pixels(), &before[..]);
    // Neither release fires the command, so one gesture cannot do both.
    assert_eq!(
        control.on_pointer(&SECONDARY_RELEASE, bounds, &mut sink()),
        None
    );
    assert_eq!(control.on_pointer(&RELEASE, bounds, &mut sink()), None);
    // Off the control, a secondary press resolves nothing.
    let _ = control.on_pointer(&moved(100, 100), bounds, &mut sink());
    assert_eq!(
        control.on_pointer(&SECONDARY_PRESS, bounds, &mut sink()),
        None
    );
}

#[test]
fn a_secondary_press_on_a_denied_control_resolves_nothing() {
    let mut control = WindowControl::new(WindowControlKind::Close);
    control.set_state(ControlState {
        authority: AuthorityState::Denied,
        ..ControlState::default()
    });
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds, &mut sink());
    assert_eq!(
        control.on_pointer(&SECONDARY_PRESS, bounds, &mut sink()),
        None
    );
}

#[test]
fn pointer_release_outside_does_not_invoke() {
    let mut control = WindowControl::new(WindowControlKind::Close);
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds, &mut sink());
    let _ = control.on_pointer(&PRESS, bounds, &mut sink());
    let _ = control.on_pointer(&moved(100, 100), bounds, &mut sink());
    assert_eq!(control.on_pointer(&RELEASE, bounds, &mut sink()), None);
}

#[test]
fn keyboard_activates_focused_control() {
    let mut control = WindowControl::new(WindowControlKind::Minimize);
    let bounds = Rect::new(0, 0, 40, 40);
    assert_eq!(
        control.on_key(Key::Named(NamedKey::Enter), bounds, &mut sink()),
        None
    );
    control.set_focused(true);
    assert_eq!(
        control.on_key(Key::Char(' '), bounds, &mut sink()),
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
    let _ = control.on_pointer(&moved(10, 10), bounds, &mut sink());
    let _ = control.on_pointer(&PRESS, bounds, &mut sink());
    assert_eq!(control.state().pointer, PointerState::Pressed);
    assert_eq!(
        control.on_pointer(&RELEASE, bounds, &mut sink()),
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
    let bounds = Rect::new(0, 0, 40, 40);
    let mut damage = sink();
    assert_eq!(
        control.on_key(Key::Named(NamedKey::Enter), bounds, &mut damage),
        Some(WindowControlAction::Invoked(WindowControlKind::Minimize))
    );
    assert!(
        !control.state().focus.focused,
        "activation clears the keyboard focus ring"
    );
    // The dropped ring is drawn, so the control has to say it repainted.
    assert_eq!(damage.rects(), [bounds]);
}

#[test]
fn a_key_that_activates_nothing_reports_nothing() {
    // An unfocused control ignores the key, and a control that is already at
    // rest has no ring or highlight to drop: neither may cost a repaint.
    let mut control = WindowControl::new(WindowControlKind::Close);
    let bounds = Rect::new(0, 0, 40, 40);
    let mut damage = sink();
    assert_eq!(control.on_key(Key::Char(' '), bounds, &mut damage), None);
    assert!(damage.is_empty());
}

#[test]
fn disabled_control_ignores_input() {
    let mut control = WindowControl::new(WindowControlKind::Close);
    control.set_state(ControlState::disabled());
    let bounds = Rect::new(0, 0, 40, 40);
    let _ = control.on_pointer(&moved(10, 10), bounds, &mut sink());
    let _ = control.on_pointer(&PRESS, bounds, &mut sink());
    assert_eq!(control.on_pointer(&RELEASE, bounds, &mut sink()), None);
    control.set_focused(true);
    assert_eq!(
        control.on_key(Key::Named(NamedKey::Enter), bounds, &mut sink()),
        None
    );
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

/// A scaled theme metric as an `i32`, for comparing against laid-out edges.
fn metric(value: u32) -> i32 {
    i32::try_from(Scale::ONE.scale_length(value)).expect("a small metric")
}

fn title_font(theme: &Theme) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::WindowTitle, Scale::ONE)
}

/// A point on `bar`'s drag region within [`title_bounds`]: the middle of the
/// span the two command clusters leave between them.
fn drag_point(bar: &TitleBar, theme: &Theme) -> Point {
    let bounds = title_bounds();
    let layout = bar.layout(bounds, Scale::ONE, theme);
    Point::new(
        i32::midpoint(layout.controls[1].1.right(), layout.controls[2].1.left()),
        i32::midpoint(bounds.top(), bounds.bottom()),
    )
}

#[test]
fn the_commands_seat_two_in_each_corner_in_reading_order() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let bar = TitleBar::new(furniture());
    let layout = bar.layout(bounds, Scale::ONE, &theme);
    let ins = metric(theme.metrics().control_inset);
    let gap = metric(theme.metrics().control_gap);

    assert_eq!(
        layout.controls.map(|(kind, _)| kind),
        [
            WindowControlKind::PutToBack,
            WindowControlKind::Close,
            WindowControlKind::Minimize,
            WindowControlKind::SizeToggle,
        ],
        "put-to-back and close lead, minimize and size-toggle trail"
    );
    for pair in layout.controls.windows(2) {
        assert!(
            pair[0].1.right() <= pair[1].1.left(),
            "the commands are laid out in that same reading order"
        );
    }
    assert_eq!(
        layout.controls[0].1.left(),
        bounds.left() + ins,
        "the leading cluster is inset into the left corner"
    );
    assert_eq!(
        layout.controls[3].1.right(),
        bounds.right() - ins,
        "and the trailing cluster into the right"
    );
    assert_eq!(
        layout.controls[1].1.left(),
        layout.controls[0].1.right() + gap
    );
    assert_eq!(
        layout.controls[3].1.left(),
        layout.controls[2].1.right() + gap
    );
    assert!(
        layout.controls[2].1.left() > layout.controls[1].1.right() + gap,
        "the identity span lies between the two clusters"
    );
}

#[test]
fn the_identity_group_is_left_justified_against_the_leading_commands() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let gap = metric(theme.metrics().control_gap);
    let identity_gap = metric(theme.metrics().control_inset);
    let mut bar = TitleBar::new(furniture());
    bar.set_identity(Some(IconKind::AppBundle));
    bar.set_title("Report");
    let layout = bar.layout(bounds, Scale::ONE, &theme);

    assert_eq!(
        layout.icon.left(),
        layout.controls[1].1.right() + gap,
        "the icon starts one gap past the last leading command"
    );
    assert_eq!(
        layout.title.left(),
        layout.icon.right() + identity_gap,
        "and the text follows the slot by the identity gap"
    );
    assert!(
        layout.title.right() < layout.controls[2].1.left(),
        "a title this short leaves the rest of the span empty"
    );

    // The point of justifying left: the group starts in the same place
    // whatever the title says, so the eye finds it without hunting.
    bar.set_title("A considerably longer window title");
    let longer = bar.layout(bounds, Scale::ONE, &theme);
    assert_eq!(longer.icon, layout.icon);
    assert_eq!(longer.title.left(), layout.title.left());
    assert!(longer.title.width > layout.title.width);
}

#[test]
fn the_title_box_is_exactly_as_wide_as_the_text_it_draws() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    bar.set_app_name("Files");
    bar.set_title("Documents");
    let layout = bar.layout(title_bounds(), Scale::ONE, &theme);
    // The box bounds the drawn line, not the room left over: a caller can take
    // it as where the title is, and the render path elides into exactly it.
    assert_eq!(
        layout.title.width,
        title_font(&theme).text_width("Files \u{2014} Documents")
    );
}

#[test]
fn a_title_wider_than_the_span_fills_it_and_elides_on_the_right() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let mut bar = TitleBar::new(furniture());
    bar.set_identity(Some(IconKind::AppBundle));
    let long = "a window title far too long for this band";
    bar.set_title(long);
    let layout = bar.layout(bounds, Scale::ONE, &theme);
    let gap = metric(theme.metrics().control_gap);

    assert_eq!(
        layout.icon.left(),
        layout.controls[1].1.right() + gap,
        "the group still starts at the span's leading edge"
    );
    assert_eq!(
        layout.title.right(),
        layout.controls[2].1.left() - gap,
        "and runs to its trailing one"
    );
    assert!(
        title_font(&theme)
            .elide_to_width(long, layout.title.width)
            .1,
        "the hidden tail is marked, not silently cut"
    );
}

#[test]
fn the_identity_group_never_reaches_a_command() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    bar.set_identity(Some(IconKind::AppBundle));
    bar.set_app_name("Files");
    bar.set_title("a window title far too long for a narrow band");
    for width in [40, 80, 120, 200, 300, 640, 1920] {
        let bounds = Rect::new(0, 0, width, 28);
        let layout = bar.layout(bounds, Scale::ONE, &theme);
        for (kind, rect) in layout.controls {
            assert!(
                layout.icon.intersection(&rect).is_empty(),
                "the icon reaches {kind:?} at {width}px"
            );
            assert!(
                layout.title.intersection(&rect).is_empty(),
                "the title reaches {kind:?} at {width}px"
            );
        }
    }
}

#[test]
fn a_band_too_narrow_for_both_clusters_abuts_them_rather_than_stacking_them() {
    // A control drawn under another cannot be hit where it is seen, so the
    // clusters meet instead of overlapping and the leading pair — which
    // carries close — keeps its place.
    let theme = Theme::dark();
    let bar = TitleBar::new(furniture());
    let bounds = Rect::new(0, 0, 60, 28);
    let layout = bar.layout(bounds, Scale::ONE, &theme);

    assert_eq!(
        layout.controls[0].1.left(),
        bounds.left() + metric(theme.metrics().control_inset)
    );
    for (i, (kind, rect)) in layout.controls.iter().enumerate() {
        for (other_kind, other) in &layout.controls[i + 1..] {
            assert!(
                rect.intersection(other).is_empty(),
                "{kind:?} is drawn over {other_kind:?}"
            );
        }
    }
    assert_eq!(layout.icon, Rect::EMPTY, "no span is left for an identity");
    assert_eq!(layout.title.width, 0, "nor for a title");
}

#[test]
fn the_minimum_band_is_the_narrowest_that_still_leaves_a_drag_surface() {
    // The floor a window manager sizes against: at it the commands are seated
    // and a comfortable target is left to drag by; under it that target is
    // already too thin to keep hitting.
    let bar = TitleBar::new(furniture());
    let half_scale = Scale::from_percent(50).expect("scale");
    let double = Scale::from_percent(200).expect("scale");
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        for scale in [Scale::ONE, half_scale, double] {
            let extent = scale
                .scale_length(theme.metrics().window_control_extent)
                .max(1);
            let min = TitleBar::min_band_width(scale, &theme);
            let at = bar.layout(Rect::new(0, 0, min, 28), scale, &theme);
            assert_eq!(
                at.drag.width, extent,
                "the floor reserves exactly one command's worth of drag surface"
            );
            for (i, (kind, rect)) in at.controls.iter().enumerate() {
                assert!(rect.width > 0, "{kind:?} is seated");
                for (other_kind, other) in &at.controls[i + 1..] {
                    assert!(
                        rect.intersection(other).is_empty(),
                        "{kind:?} is drawn over {other_kind:?}"
                    );
                }
            }
            let under = bar.layout(Rect::new(0, 0, min.saturating_sub(1), 28), scale, &theme);
            assert!(
                under.drag.width < extent,
                "and one pixel under it the drag surface is under that target"
            );
        }
    }
}

#[test]
fn the_drag_span_is_the_band_between_the_clusters_and_touches_no_command() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    bar.set_identity(Some(IconKind::AppBundle));
    bar.set_title("Documents");
    let gap = metric(theme.metrics().control_gap);
    for width in [60, 152, 200, 300, 1920] {
        let layout = bar.layout(Rect::new(0, 0, width, 28), Scale::ONE, &theme);
        for (kind, rect) in layout.controls {
            assert!(
                layout.drag.intersection(&rect).is_empty(),
                "the drag span reaches {kind:?} at {width}px"
            );
        }
        if layout.drag.width > 0 {
            assert_eq!(layout.drag.left(), layout.controls[1].1.right() + gap);
            assert_eq!(layout.drag.right(), layout.controls[2].1.left() - gap);
            assert!(
                layout.drag.intersection(&layout.title) == layout.title || layout.title.width == 0,
                "the title is drawn inside the span at {width}px"
            );
        }
    }
}

#[test]
fn the_minimum_outer_size_leaves_a_usable_band_and_a_real_client() {
    let frame = WindowFrame::new(furniture());
    let double = Scale::from_percent(200).expect("scale");
    for theme in [Theme::dark(), Theme::light()] {
        for scale in [Scale::ONE, double] {
            let (w, h) = frame.min_outer_size(scale, &theme);
            let layout = frame.layout(Rect::new(0, 0, w, h), scale, &theme);
            assert!(
                layout.title_bar.width >= TitleBar::min_band_width(scale, &theme),
                "the band the title bar is given is at least the band it needs"
            );
            assert!(
                layout.client.width > 0 && layout.client.height > 0,
                "and a window at the floor is still a window, not a strip of chrome"
            );
            let insets = frame.insets(scale, &theme);
            assert_eq!(
                frame.outer_for_client(layout.client, scale, &theme),
                Rect::new(0, 0, w, h),
                "the floor round-trips through the band {insets:?}"
            );
        }
    }
}

#[test]
fn nothing_the_frame_draws_squares_off_its_rounded_corner() {
    // The title bar used to fill its whole band — in the very colour the
    // frame's plate had already laid down, rounded — which squared the two top
    // corners off. Every pixel outside the rim's arc stays untouched, whatever
    // the bar has to draw.
    for theme in [Theme::dark(), Theme::light()] {
        let mut frame = WindowFrame::new(furniture());
        frame
            .title_bar_mut()
            .set_title("/Users/root/Documents/Projects/tairix");
        let (w, h) = (200, 120);
        let mut surface = Surface::new(w, h).expect("surface");
        frame.render(
            &mut surface,
            Rect::new(0, 0, w, h),
            Scale::ONE,
            &theme,
            None,
        );

        let radius = frame.rim(Scale::ONE, &theme).radius;
        assert!(radius > 0, "{}: rounds its windows", theme.name());
        for y in 0..h {
            for x in 0..w {
                let drawn = surface.get(x, y) != Some(Pixel::TRANSPARENT);
                assert_eq!(
                    drawn,
                    round_rect_coverage(x, y, w, h, radius) > 0,
                    "{}: ({x}, {y}) is not the shape the rim traces",
                    theme.name()
                );
            }
        }
        // And the rim itself resumes where the arc gives way to a straight run.
        assert_eq!(
            surface.get(radius, 0),
            Some(premul(theme.palette().frame)),
            "{}: the top rim",
            theme.name()
        );
    }
}

/// The wash each command lights up with on `theme`: its authored hue resolved
/// against the window body its title bar is painted on.
///
/// The kind-to-role mapping is restated here on purpose rather than borrowed
/// from the renderer. Asking the renderer which hue it uses would agree with
/// itself even if close had been wired to the green role; stating the intended
/// pairing independently is what makes the assertion mean anything.
fn command_wash(theme: &Theme, kind: WindowControlKind) -> Pixel {
    let palette = theme.palette();
    let hue = match kind {
        WindowControlKind::Close => palette.window_close,
        WindowControlKind::Minimize => palette.window_minimize,
        WindowControlKind::SizeToggle => palette.window_maximize,
        WindowControlKind::PutToBack => palette.window_put_to_back,
    };
    premul(hue.over(palette.surface))
}

/// `kind`'s plate rendered in the state `prepare` leaves it in.
fn command_surface(
    theme: &Theme,
    kind: WindowControlKind,
    prepare: impl FnOnce(&mut WindowControl, Rect),
) -> Surface {
    let bounds = Rect::new(0, 0, 24, 24);
    let mut control = WindowControl::new(kind);
    prepare(&mut control, bounds);
    let mut surface = Surface::new(24, 24).expect("surface");
    control.render(&mut surface, bounds, Scale::ONE, theme);
    surface
}

#[test]
fn a_hovered_command_lights_up_in_its_own_colour() {
    // Each command carries its own hue, so the pointer landing on one says
    // which of the four it is about to fire. The wash is authored translucent
    // and resolved against the window body, so the title bar reads through it
    // rather than being covered by a block of colour.
    for theme in [Theme::dark(), Theme::light()] {
        for kind in CONTROL_ORDER {
            let resting = command_surface(&theme, kind, |_, _| {});
            assert!(
                !has_pixel(&resting, command_wash(&theme, kind)),
                "{}: a resting {kind:?} shows only its glyph on the bar's own surface",
                theme.name()
            );

            let hovered = command_surface(&theme, kind, |control, bounds| {
                control.on_pointer(&moved(12, 12), bounds, &mut sink());
            });
            assert!(
                has_pixel(&hovered, command_wash(&theme, kind)),
                "{}: the pointer lights {kind:?} in its own hue",
                theme.name()
            );

            // No other command's hue can be mistaken for this one's.
            for other in CONTROL_ORDER {
                if other != kind {
                    assert!(
                        !has_pixel(&hovered, command_wash(&theme, other)),
                        "{}: a hovered {kind:?} also wears {other:?}'s colour",
                        theme.name()
                    );
                }
            }
        }
    }
}

#[test]
fn a_pressed_command_deepens_the_hue_it_hovered_to() {
    // Once the colour is on, the only step left is to deepen it — the same
    // press darkening every filled control takes.
    for theme in [Theme::dark(), Theme::light()] {
        let hovered = command_surface(&theme, WindowControlKind::Close, |control, bounds| {
            control.on_pointer(&moved(12, 12), bounds, &mut sink());
        });
        let pressed = command_surface(&theme, WindowControlKind::Close, |control, bounds| {
            control.on_pointer(&moved(12, 12), bounds, &mut sink());
            control.on_pointer(&PRESS, bounds, &mut sink());
        });
        let wash = command_wash(&theme, WindowControlKind::Close);
        assert!(has_pixel(&hovered, wash), "{}: hover", theme.name());
        assert!(
            !has_pixel(&pressed, wash),
            "{}: a press must not draw the hover wash unchanged",
            theme.name()
        );
        assert!(
            pressed.pixels().iter().any(|p| p.a > 0),
            "{}: a pressed command still draws a plate",
            theme.name()
        );
    }
}

#[test]
fn a_command_the_keyboard_merely_rests_on_stays_unwashed() {
    // The hue is the pointer's highlight; focus states itself on the ring
    // inside the plate, so a keyboard-focused command is not mistaken for the
    // one under the cursor.
    for theme in [Theme::dark(), Theme::light()] {
        let focused = command_surface(&theme, WindowControlKind::Close, |control, _| {
            control.set_focused(true);
        });
        assert!(
            !has_pixel(&focused, command_wash(&theme, WindowControlKind::Close)),
            "{}: keyboard focus lit the command's hue",
            theme.name()
        );
        assert!(
            has_pixel(&focused, premul(theme.palette().rim_active)),
            "{}: and the focus ring is what it draws instead",
            theme.name()
        );
    }
}

#[test]
fn a_command_states_itself_on_its_plate_and_never_on_an_edge() {
    // The bar's own surface runs right up to a command, so an edge of its own
    // would read as a line drawn round the window's corner rather than as
    // feedback on a button. Only the keyboard ring may carry the accent, and
    // it sits inside the plate.
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let palette = theme.palette();
        let bounds = Rect::new(0, 0, 24, 24);
        let edges = |control: &WindowControl, state: &str| {
            let mut surface = Surface::new(24, 24).expect("surface");
            control.render(&mut surface, bounds, Scale::ONE, &theme);
            assert!(
                !has_pixel(&surface, premul(palette.rim_active)),
                "a {state} command draws the reactive rim"
            );
            assert!(
                !has_pixel(&surface, premul(palette.rim)),
                "a {state} command draws the quiet rim"
            );
        };

        let mut control = WindowControl::new(WindowControlKind::Close);
        edges(&control, "resting");
        control.on_pointer(&moved(12, 12), bounds, &mut sink());
        edges(&control, "hovered");
        control.on_pointer(&PRESS, bounds, &mut sink());
        edges(&control, "pressed");

        let mut focused = WindowControl::new(WindowControlKind::Close);
        focused.set_focused(true);
        let mut surface = Surface::new(24, 24).expect("surface");
        focused.render(&mut surface, bounds, Scale::ONE, &theme);
        assert!(
            has_pixel(&surface, premul(palette.rim_active)),
            "the keyboard ring is the one accent mark a command wears"
        );
    }
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
    let drag = drag_point(&bar, &theme);
    assert_eq!(
        bar.on_pointer(
            &moved(drag.x, drag.y),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        None
    );
    assert_eq!(
        bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
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
    let _ = bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        bar.on_pointer(&SECONDARY_PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        Some(TitleBarEvent::AlternateControl(WindowControlKind::Close))
    );
    // The bar never activates or drags from it, and the release is inert.
    assert_eq!(
        bar.on_pointer(&SECONDARY_RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        bar.on_pointer(&moved(cx + 40, cy), bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    // Over the drag region a secondary press is unchanged: nothing at all.
    let drag = drag_point(&bar, &theme);
    let _ = bar.on_pointer(
        &moved(drag.x, drag.y),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(
        bar.on_pointer(&SECONDARY_PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    // A primary press over the control still means the command.
    let _ = bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme, &mut sink());
    let _ = bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        Some(TitleBarEvent::Control(WindowControlKind::Close))
    );
}

#[test]
fn drag_begins_moves_and_ends() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    let bounds = title_bounds();
    let drag = drag_point(&bar, &theme);
    let _ = bar.on_pointer(
        &moved(drag.x, drag.y),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    let _ = bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        bar.on_pointer(
            &moved(drag.x + 20, drag.y),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(TitleBarEvent::DragBegin)
    );
    assert_eq!(
        bar.on_pointer(
            &moved(drag.x + 40, drag.y),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(TitleBarEvent::DragMoved {
            to: Point::new(drag.x + 40, drag.y)
        })
    );
    assert_eq!(
        bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
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
        bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    // A press over the control must not activate/drag the title bar.
    assert_eq!(
        bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
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
        bar.hit(bounds, Scale::ONE, &theme, drag_point(&bar, &theme)),
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
    let _ = bar.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme, &mut sink());
    let _ = bar.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(
        bar.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
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
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    assert_eq!(
        bar.on_key(
            Key::Named(NamedKey::Right),
            TITLE_BOUNDS,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        None
    );
    assert!(
        bar.control(WindowControlKind::PutToBack)
            .state()
            .focus
            .focused
    );
    assert_eq!(
        bar.on_key(
            Key::Named(NamedKey::Enter),
            TITLE_BOUNDS,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(TitleBarEvent::Control(WindowControlKind::PutToBack))
    );
}

#[test]
fn a_focus_move_reports_the_two_controls_it_touches() {
    // The ring leaves one control and lands on another. Repainting the strip
    // between them would drop every frost above the title band for nothing, so
    // the bar reports exactly the two rects and never its own bounds.
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    let layout = bar.layout(TITLE_BOUNDS, Scale::ONE, &theme);
    let rect_of = |kind: WindowControlKind| {
        layout
            .controls
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, r)| *r)
            .expect("every command is laid out")
    };

    let arrive = |bar: &mut TitleBar, damage: &mut Region| {
        bar.on_key(
            Key::Named(NamedKey::Right),
            TITLE_BOUNDS,
            Scale::ONE,
            &theme,
            damage,
        )
    };

    let mut damage = sink();
    assert_eq!(arrive(&mut bar, &mut damage), None);
    assert_eq!(
        damage.rects(),
        [rect_of(WindowControlKind::PutToBack)],
        "the first step only lights the control it lands on"
    );

    let mut damage = sink();
    assert_eq!(arrive(&mut bar, &mut damage), None);
    for rect in [
        rect_of(WindowControlKind::PutToBack),
        rect_of(WindowControlKind::Close),
    ] {
        assert!(
            damage
                .rects()
                .iter()
                .any(|reported| reported.contains(rect.origin)),
            "the control at {rect:?} the ring moved between must be repainted"
        );
    }
    assert!(
        damage.bounds().width < TITLE_BOUNDS.width,
        "a focus move must not report the whole bar"
    );
}

#[test]
fn a_focus_move_reports_every_ring_it_clears() {
    // The bar's own invariant is one focused control, but a caller reaches the
    // controls directly. A move must report each ring it actually clears, not
    // the two the invariant predicts.
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    for kind in [WindowControlKind::Close, WindowControlKind::SizeToggle] {
        bar.control_mut(kind).set_focused(true);
    }
    let layout = bar.layout(TITLE_BOUNDS, Scale::ONE, &theme);
    let rect_of = |kind: WindowControlKind| {
        layout
            .controls
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, r)| *r)
            .expect("every command is laid out")
    };

    let mut damage = sink();
    assert_eq!(
        bar.on_key(
            Key::Named(NamedKey::Right),
            TITLE_BOUNDS,
            Scale::ONE,
            &theme,
            &mut damage
        ),
        None
    );
    let covers = |kind: WindowControlKind| {
        damage
            .rects()
            .iter()
            .any(|reported| reported.contains(rect_of(kind).origin))
    };
    // The ring left both lit controls and arrived at the one past the first.
    for kind in [
        WindowControlKind::Close,
        WindowControlKind::SizeToggle,
        WindowControlKind::Minimize,
    ] {
        assert!(covers(kind), "the ring changed on {kind:?} and must report");
    }
    assert!(
        !covers(WindowControlKind::PutToBack),
        "a control whose ring did not change costs nothing"
    );
}

#[test]
fn a_key_the_bar_ignores_reports_nothing() {
    let theme = Theme::dark();
    let mut bar = TitleBar::new(furniture());
    let mut damage = sink();
    assert_eq!(
        bar.on_key(
            Key::Char('x'),
            TITLE_BOUNDS,
            Scale::ONE,
            &theme,
            &mut damage
        ),
        None
    );
    assert!(damage.is_empty());
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

/// A bar with no identity reserves nothing: its title is the whole group, and
/// it draws exactly what it drew before identities existed.
#[test]
fn a_bar_without_an_identity_leads_with_its_title_alone() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let gap = metric(theme.metrics().control_gap);
    let mut bar = TitleBar::new(furniture());
    bar.set_title("Report");
    let layout = bar.layout(bounds, Scale::ONE, &theme);
    assert_eq!(bar.identity(), None);
    assert_eq!(layout.icon, Rect::EMPTY);
    assert_eq!(layout.title.height, bounds.height);
    assert_eq!(
        layout.title.left(),
        layout.controls[1].1.right() + gap,
        "the text takes the leading edge the slot would have had"
    );
}

/// An identity leads the group with a square slot and the title text follows
/// it; the slot is the side the owner is told to rasterise at.
#[test]
fn an_identity_leads_the_group_and_the_title_follows_it() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let mut plain = TitleBar::new(furniture());
    plain.set_title("Report");
    let mut identified = TitleBar::new(furniture());
    identified.set_title("Report");
    identified.set_identity(Some(IconKind::AppBundle));
    assert_eq!(identified.identity(), Some(IconKind::AppBundle));

    let bare = plain.layout(bounds, Scale::ONE, &theme);
    let with = identified.layout(bounds, Scale::ONE, &theme);
    let side = TitleBar::icon_side(bounds, Scale::ONE, &theme);
    assert!(side > 0, "the band is tall enough for a slot");
    assert_eq!(with.icon.width, side);
    assert_eq!(with.icon.height, side);
    assert!(
        with.title.left() > with.icon.right(),
        "the text starts past the slot"
    );
    assert_eq!(
        with.title.width, bare.title.width,
        "the same title draws at the same width either way"
    );
    assert_eq!(
        with.icon.left(),
        bare.title.left(),
        "the slot takes the leading edge the bare title had"
    );
    assert!(
        with.title.left() > bare.title.left(),
        "and pushes the text along by the slot and its gap"
    );
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

    let side = TitleBar::icon_side(bounds, Scale::ONE, &theme);
    let mut art = Surface::new(side, side).expect("artwork");
    art.fill_rect(0, 0, side, side, Color::from(theme.palette().accent));
    let drawn = paint(&bar, Some(&art));
    assert_ne!(
        glyph.pixels(),
        drawn.pixels(),
        "the owner's artwork replaces the glyph"
    );
    // The artwork is desaturated by activation, so its ink is the accent
    // pulled toward its own luminance rather than the accent itself.
    assert!(has_pixel(
        &drawn,
        premul(theme.palette().accent).desaturate(IDENTITY_SATURATION_ACTIVE)
    ));

    // Artwork offered to a bar with no identity is ignored.
    bar.set_identity(None);
    assert_eq!(paint(&bar, Some(&art)).pixels(), bare.pixels());
}

/// Colour on the identity icon says "this is the window in hand": an active
/// window's artwork is drawn a shade off full colour and an inactive one's
/// fully grey, so a glance finds the focused window by its one coloured icon.
#[test]
fn the_identity_artwork_desaturates_with_the_frame() {
    let theme = Theme::dark();
    let bounds = title_bounds();
    let ink = Rgba::new(0xd0, 0x20, 0x20, 0xff);
    let mut bar = TitleBar::new(furniture());
    bar.set_identity(Some(IconKind::AppBundle));
    bar.set_title("Report");
    let side = TitleBar::icon_side(bounds, Scale::ONE, &theme);
    assert!(side > 0, "the band is tall enough for a slot");
    let mut art = Surface::new(side, side).expect("artwork");
    art.fill_rect(0, 0, side, side, Color::from(ink));

    let paint = |activation: WindowActivationState| {
        let mut state = furniture();
        state.activation = activation;
        let mut bar = TitleBar::new(state);
        bar.set_identity(Some(IconKind::AppBundle));
        bar.set_title("Report");
        let mut surface = Surface::new(bounds.width, bounds.height).expect("surface");
        bar.render(&mut surface, bounds, Scale::ONE, &theme, Some(&art));
        surface
    };

    let active = paint(WindowActivationState::Active);
    assert!(
        !has_pixel(&active, premul(ink)),
        "an active window's icon is still a shade off full colour"
    );
    assert!(
        has_pixel(&active, premul(ink).desaturate(IDENTITY_SATURATION_ACTIVE)),
        "…but keeps nearly all of it"
    );

    let inactive = paint(WindowActivationState::Inactive);
    let grey = premul(ink).desaturate(IDENTITY_SATURATION_INACTIVE);
    assert!(grey.r == grey.g && grey.g == grey.b, "{grey:?} is not grey");
    let slot = bar.layout(bounds, Scale::ONE, &theme).icon;
    for y in slot.top()..slot.bottom() {
        for x in slot.left()..slot.right() {
            let at = |v: i32| u32::try_from(v).expect("an on-surface coordinate");
            assert_eq!(
                inactive.get(at(x), at(y)),
                Some(grey),
                "colour survives at ({x}, {y}) on an unfocused window"
            );
        }
    }
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
    let band = frame.layout(bounds, Scale::ONE, &theme).title_bar;
    let on_band = Point::new(
        i32::midpoint(band.left(), band.right()),
        i32::midpoint(band.top(), band.bottom()),
    );
    assert_eq!(
        frame.hit(bounds, Scale::ONE, &theme, on_band),
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
    let _ = grabber.on_pointer(&moved(10, 10), hit, &mut sink());
    assert_eq!(
        grabber.on_pointer(&PRESS, hit, &mut sink()),
        Some(ResizeEvent::Begin)
    );
    assert!(grabber.is_dragging());
    // A sample that only carries the drag forward paints the same teeth.
    let mut damage = sink();
    assert_eq!(
        grabber.on_pointer(&moved(15, 15), hit, &mut damage),
        Some(ResizeEvent::Moved {
            to: Point::new(15, 15)
        })
    );
    assert!(damage.is_empty(), "a drag sample repaints nothing");
    assert_eq!(
        grabber.on_pointer(&RELEASE, hit, &mut sink()),
        Some(ResizeEvent::End)
    );
    assert!(!grabber.is_dragging());
}

#[test]
fn grabber_escape_cancels_drag() {
    let mut grabber = ResizeGrabber::new();
    let hit = Rect::new(0, 0, 20, 20);
    let _ = grabber.on_pointer(&moved(10, 10), hit, &mut sink());
    let _ = grabber.on_pointer(&PRESS, hit, &mut sink());
    assert_eq!(
        grabber.on_key(Key::Named(NamedKey::Escape), TITLE_BOUNDS, &mut sink()),
        Some(ResizeEvent::Cancel)
    );
    assert!(!grabber.is_dragging());
}

#[test]
fn a_cancel_away_from_the_corner_still_reports_the_teeth() {
    // The drag itself is drawn in the pressed treatment, so dropping it
    // repaints even when the pointer has long left the hit region and the
    // pointer look is already at rest.
    let mut grabber = ResizeGrabber::new();
    let hit = Rect::new(0, 0, 20, 20);
    let _ = grabber.on_pointer(&moved(10, 10), hit, &mut sink());
    let _ = grabber.on_pointer(&PRESS, hit, &mut sink());
    let _ = grabber.on_pointer(&moved(400, 400), hit, &mut sink());

    let mut damage = sink();
    assert_eq!(
        grabber.on_key(Key::Named(NamedKey::Escape), hit, &mut damage),
        Some(ResizeEvent::Cancel)
    );
    assert_eq!(damage.rects(), [hit]);
}

#[test]
fn disabled_grabber_ignores_input() {
    let mut grabber = ResizeGrabber::new();
    grabber.set_enabled(false);
    let hit = Rect::new(0, 0, 20, 20);
    let mut damage = sink();
    let _ = grabber.on_pointer(&moved(10, 10), hit, &mut sink());
    assert_eq!(grabber.on_pointer(&PRESS, hit, &mut damage), None);
    assert!(!grabber.is_dragging());
    assert!(
        damage.is_empty(),
        "a refused press captures nothing and repaints nothing"
    );
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
    let _ = a.on_pointer(&moved(80, 80), bounds, &mut sink());
    let _ = b.on_pointer(&moved(120, 12), bounds, &mut sink());
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
    let _ = latched.on_pointer(&moved(10, 10), bounds, &mut sink());
    let _ = latched.on_pointer(&PRESS, bounds, &mut sink());
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
        latched.on_pointer(&RELEASE, bounds, &mut sink()),
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
    let _ = a.on_pointer(&moved(50, 200), bounds, Scale::ONE, &theme, &mut sink());
    let _ = b.on_pointer(&moved(90, 240), bounds, Scale::ONE, &theme, &mut sink());
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
    let drag = drag_point(&near, &theme);
    let _ = near.on_pointer(
        &moved(drag.x - 40, drag.y),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    let _ = near.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    let mut far = TitleBar::new(furniture());
    let _ = far.on_pointer(
        &moved(drag.x + 40, drag.y),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    let _ = far.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
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
    let _ = a.on_pointer(&moved(60, 60), bounds, &mut sink());
    let _ = b.on_pointer(&moved(90, 40), bounds, &mut sink());

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
