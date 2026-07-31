//! Unit tests for the Switchboard reference composition (spec §17, §20).
//!
//! These prove the composition is assembled from the shared controls and
//! behaves correctly: the window chrome and scrollbar junction stay separate
//! from the client, the header resource band shifts everything below it down
//! by exactly its measured height and never fabricates input, the tab strip
//! switches sections (by pointer and keyboard), a host can open the panel on
//! any section and lands in exactly the state the keyboard would have reached,
//! a refreshed model re-derives the controls while leaving the user's section,
//! scroll offset, and focus alone and never lets a stale gesture reach a
//! replaced row, the mouse wheel and keyboard scroll the active section,
//! denied actions fail closed and render distinctly from disabled ones, a
//! force action carries a confirmation posture, and the layout scales.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::{Contrast, Theme};

use crate::meter::{Meter, MeterValue};
use crate::state::{
    ActivityState, ControlDisposition, ControlRole, PressureKind, PressureState, ProgressValue,
    RecoveryState,
};
use crate::switchboard::{
    JobControl, JobSummary, RecoveryControl, RecoveryItem, ResourceSummary, Section,
    ServiceSummary, Switchboard, SwitchboardAction, SwitchboardModel, SystemAction, TaskSummary,
};
use crate::window::FurniturePart;

fn font() -> BitmapFont {
    BitmapFont::inconsolata()
}

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

/// A populated model with enough items to overflow a modest viewport.
///
/// The three resources deliberately span the meter's honest range: CPU is
/// measured with a history sparkline, Memory is measured with a plain fill
/// (no history), and Disk is left honestly unmeasured — exactly the "quiet
/// meter" default a host with no wired query must fall back to.
fn model() -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for i in 0..50 {
        m.tasks.push(TaskSummary {
            name: alloc::format!("task {i}"),
            detail: alloc::format!("{i}%"),
            pressure: if i % 3 == 0 {
                PressureState::Under(PressureKind::Cpu)
            } else {
                PressureState::None
            },
            activity: ActivityState::Progress(ProgressValue::new(500)),
            recovery: RecoveryState::None,
            action: alloc::string::String::from("End"),
            action_allowed: true,
        });
    }
    for i in 0..8 {
        m.jobs.push(JobSummary {
            name: alloc::format!("job {i}"),
            detail: alloc::string::String::from("copying"),
            activity: ActivityState::Progress(ProgressValue::new(300)),
            can_pause: true,
            can_cancel: true,
        });
    }
    for i in 0..6 {
        m.recovery.push(RecoveryItem {
            name: alloc::format!("hung {i}"),
            detail: alloc::string::String::from("not responding"),
            recovery: RecoveryState::Hung,
            can_restart: true,
            can_force: true,
        });
    }
    m.resources.push(
        ResourceSummary::new(
            "CPU",
            "62%",
            PressureKind::Cpu,
            ActivityState::Progress(ProgressValue::new(620)),
        )
        .with_meter(
            MeterValue::Measured(ProgressValue::new(620)),
            PressureState::Under(PressureKind::Cpu),
            [100, 300, 500, 620],
        ),
    );
    m.resources.push(
        ResourceSummary::new(
            "Memory",
            "8.6 GB / 16 GB",
            PressureKind::Memory,
            ActivityState::Progress(ProgressValue::new(538)),
        )
        .with_meter(
            MeterValue::Measured(ProgressValue::new(538)),
            PressureState::None,
            [],
        ),
    );
    m.resources.push(ResourceSummary::new(
        "Disk",
        "72%",
        PressureKind::Disk,
        ActivityState::Progress(ProgressValue::new(720)),
    ));
    for i in 0..10 {
        m.services.push(ServiceSummary {
            name: alloc::format!("svc {i}"),
            detail: alloc::string::String::from("running"),
            recovery: RecoveryState::None,
            action: alloc::string::String::from("Restart"),
            action_allowed: true,
        });
    }
    m.system_actions.push(SystemAction {
        label: alloc::string::String::from("Lock"),
        role: ControlRole::System,
        allowed: true,
    });
    m.system_actions.push(SystemAction {
        label: alloc::string::String::from("Shut Down"),
        role: ControlRole::Destructive,
        allowed: true,
    });
    m
}

fn bounds() -> Rect {
    Rect::new(0, 0, 600, 400)
}

fn centre(rect: Rect) -> (i32, i32) {
    (
        rect.left() + i32::try_from(rect.width).unwrap_or(0) / 2,
        rect.top() + i32::try_from(rect.height).unwrap_or(0) / 2,
    )
}

fn has_ink(surface: &Surface, rect: Rect) -> bool {
    (rect.left()..rect.right()).any(|x| {
        (rect.top()..rect.bottom()).any(|y| {
            let (xu, yu) = (u32::try_from(x).unwrap_or(0), u32::try_from(y).unwrap_or(0));
            surface.get(xu, yu).is_some_and(|p| p.a > 0)
        })
    })
}

/// Feed a full click (move, press, release) at `(x, y)` and collect the
/// actions produced.
fn click(
    sb: &mut Switchboard,
    b: Rect,
    scale: Scale,
    theme: &Theme,
    x: i32,
    y: i32,
) -> alloc::vec::Vec<SwitchboardAction> {
    let mut out = alloc::vec::Vec::new();
    for event in [moved(x, y), PRESS, RELEASE] {
        if let Some(action) = sb.on_pointer(&event, b, scale, theme, font()) {
            out.push(action);
        }
    }
    out
}

#[test]
fn new_starts_on_tasks_at_offset_zero() {
    let sb = Switchboard::new(model());
    assert_eq!(sb.section(), Section::Tasks);
    assert_eq!(sb.scroll_offset(), 0);
}

#[test]
fn render_paints_content() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn scroll_track_and_resize_corner_do_not_overlap() {
    let theme = Theme::dark();
    let sb = Switchboard::new(model());
    let layout = sb.compute_layout(bounds(), Scale::ONE, &theme, font());
    // The corner sits below the scroll track, so they never share a pixel.
    assert!(layout.scroll.bottom() <= layout.corner.top());
    // The content area stops where the scrollbar gutter begins.
    assert_eq!(layout.content.right(), layout.scroll.left());
    // Everything stays inside the frame's client viewport.
    let client = layout.frame.client;
    assert!(layout.tabs.right() <= client.right());
    assert!(layout.corner.bottom() <= client.bottom());
}

#[test]
fn client_content_is_isolated_from_furniture() {
    let theme = Theme::dark();
    let sb = Switchboard::new(model());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let (cx, cy) = centre(layout.content);
    // A point in the content area is the client viewport, never furniture.
    assert_eq!(
        sb.furniture_at(b, Scale::ONE, &theme, Point::new(cx, cy)),
        FurniturePart::Client
    );
    // A point in the title bar is furniture, never the client.
    let (tx, ty) = centre(layout.frame.title_bar);
    assert_ne!(
        sb.furniture_at(b, Scale::ONE, &theme, Point::new(tx, ty)),
        FurniturePart::Client
    );
}

#[test]
fn tab_click_switches_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    // The second tab (Jobs) occupies the second quarter of the strip.
    let tab_w = layout.tabs.width / 4;
    let x = layout.tabs.left()
        + i32::try_from(tab_w).unwrap_or(0)
        + i32::try_from(tab_w).unwrap_or(0) / 2;
    let y = centre(layout.tabs).1;
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Jobs
    }));
    assert_eq!(sb.section(), Section::Jobs);
}

#[test]
fn wheel_scrolls_the_active_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    let action = sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    match action {
        Some(SwitchboardAction::Scrolled { offset }) => assert_eq!(offset, 3),
        other => panic!("expected a scroll, got {other:?}"),
    }
    assert_eq!(sb.scroll_offset(), 3);
}

#[test]
fn keyboard_scrolls_the_focused_scrollbar() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(600, 400).expect("surface");
    // Render once so the scroll model matches the layout.
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    // Cycle focus Content -> Scrollbar (one Tab).
    assert_eq!(sb.on_key(Key::Named(NamedKey::Tab)), None);
    let action = sb.on_key(Key::Named(NamedKey::Down));
    match action {
        Some(SwitchboardAction::Scrolled { offset }) => assert!(offset >= 1),
        other => panic!("expected a keyboard scroll, got {other:?}"),
    }
}

#[test]
fn keyboard_cycles_focus_and_selects_a_section() {
    let mut sb = Switchboard::new(model());
    // Content -> Scrollbar -> TitleBar -> Tabs.
    for _ in 0..3 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Tab)), None);
    }
    // Move the current tab to Jobs and select it.
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    let action = sb.on_key(Key::Named(NamedKey::Enter));
    assert_eq!(
        action,
        Some(SwitchboardAction::SectionChanged {
            section: Section::Jobs
        })
    );
}

#[test]
fn allowed_task_action_activates() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 1, Scale::ONE, &theme);
    let (x, y) = centre(buttons[0]);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Task { index: 0 }));
}

#[test]
fn denied_task_action_fails_closed() {
    let theme = Theme::dark();
    let mut m = SwitchboardModel::new("Switchboard");
    m.tasks.push(TaskSummary {
        name: alloc::string::String::from("locked task"),
        detail: alloc::string::String::from(""),
        pressure: PressureState::None,
        activity: ActivityState::Idle,
        recovery: RecoveryState::None,
        action: alloc::string::String::from("End"),
        action_allowed: false,
    });
    let mut sb = Switchboard::new(m);
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 1, Scale::ONE, &theme);
    let (x, y) = centre(buttons[0]);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.is_empty(), "a denied action must not activate");
}

#[test]
fn denied_action_renders_distinct_from_disabled() {
    let mut m = SwitchboardModel::new("Switchboard");
    m.tasks.push(TaskSummary {
        name: alloc::string::String::from("locked task"),
        detail: alloc::string::String::from(""),
        pressure: PressureState::None,
        activity: ActivityState::Idle,
        recovery: RecoveryState::None,
        action: alloc::string::String::from("End"),
        action_allowed: false,
    });
    let sb = Switchboard::new(m);
    // A refused action is DeniedByAuthority, never a plain disabled control.
    assert_eq!(
        sb.tasks[0].action.state().disposition(),
        ControlDisposition::DeniedByAuthority
    );
}

#[test]
fn force_action_carries_confirmation_posture() {
    let mut m = SwitchboardModel::new("Switchboard");
    m.recovery.push(RecoveryItem {
        name: alloc::string::String::from("hung"),
        detail: alloc::string::String::from(""),
        recovery: RecoveryState::Hung,
        can_restart: true,
        can_force: true,
    });
    let sb = Switchboard::new(m);
    assert_eq!(
        sb.recovery[0].force.state().disposition(),
        ControlDisposition::NeedsConfirmation
    );
    assert_eq!(sb.recovery[0].force.role(), ControlRole::Destructive);
}

#[test]
fn keyboard_activates_a_job_footer() {
    let mut sb = Switchboard::new(model());
    // Switch to Jobs with content focus on the first card.
    assert_eq!(
        sb.select_section(Section::Jobs),
        Some(SwitchboardAction::SectionChanged {
            section: Section::Jobs
        })
    );
    let action = sb.on_key(Key::Named(NamedKey::Enter));
    assert_eq!(
        action,
        Some(SwitchboardAction::Job {
            index: 0,
            control: JobControl::Pause
        })
    );
}

#[test]
fn recovery_row_force_activates_by_pointer() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Recovery);
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 2, Scale::ONE, &theme);
    let (x, y) = centre(buttons[1]);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Recovery {
        index: 0,
        control: RecoveryControl::Force
    }));
}

#[test]
fn offsets_are_independent_per_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    // Scroll Tasks down.
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 4 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(sb.scroll_offset(), 4);
    // Switching to Jobs shows its own (zero) offset.
    sb.select_section(Section::Jobs);
    assert_eq!(sb.scroll_offset(), 0);
    // Switching back restores the Tasks offset.
    sb.select_section(Section::Tasks);
    assert_eq!(sb.scroll_offset(), 4);
}

#[test]
fn section_switch_reclamps_offset_to_new_content() {
    let theme = Theme::dark();
    let mut m = model();
    m.services.clear(); // Overview has no scrollable rows.
    let mut sb = Switchboard::new(m);
    let b = bounds();
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 6 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    sb.select_section(Section::Overview);
    // Sync against the (empty) Overview list clamps the offset to zero.
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.scroll_offset(), 0);
}

#[test]
fn layout_scales_with_the_ui_scale() {
    let theme = Theme::dark();
    let sb = Switchboard::new(model());
    let one = sb.compute_layout(bounds(), Scale::ONE, &theme, font());
    let two = sb.compute_layout(
        bounds(),
        Scale::from_percent(200).expect("scale"),
        &theme,
        font(),
    );
    assert!(two.tabs.height > one.tabs.height);
}

#[test]
fn light_theme_renders() {
    let theme = Theme::light();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

#[test]
fn high_contrast_theme_renders() {
    let theme = high_contrast();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, bounds(), Scale::ONE, &theme, font());
    assert!(surface.pixels().iter().any(|p| p.a > 0));
}

// --- The header resource band ------------------------------------------

#[test]
fn band_height_shifts_tabs_and_content_down() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let b = bounds();
    let with_resources = Switchboard::new(model());
    let without_resources = Switchboard::new(SwitchboardModel::new("Switchboard"));

    let with_layout = with_resources.compute_layout(b, scale, &theme, font());
    let without_layout = without_resources.compute_layout(b, scale, &theme, font());

    let expected_height = Meter::measured_height(scale, &theme, font());
    assert_eq!(with_layout.band.height, expected_height);
    assert_eq!(with_layout.band.top(), with_layout.frame.client.top());
    assert_eq!(with_layout.tabs.top(), with_layout.band.bottom());

    let shift = i32::try_from(expected_height).unwrap_or(0);
    assert_eq!(with_layout.tabs.top(), without_layout.tabs.top() + shift);
    assert_eq!(
        with_layout.content.top(),
        without_layout.content.top() + shift
    );
    assert_eq!(
        with_layout.scroll.top(),
        without_layout.scroll.top() + shift
    );
}

#[test]
fn empty_resources_yield_no_band_and_unchanged_layout() {
    let theme = Theme::dark();
    let sb = Switchboard::new(SwitchboardModel::new("Switchboard"));
    let layout = sb.compute_layout(bounds(), Scale::ONE, &theme, font());
    assert_eq!(layout.band.height, 0);
    assert_eq!(layout.tabs.top(), layout.frame.client.top());
}

#[test]
fn measured_and_unmeasured_resources_render_differently() {
    let theme = Theme::dark();
    let mut measured_model = SwitchboardModel::new("Switchboard");
    measured_model.resources.push(
        ResourceSummary::new(
            "CPU",
            "62%",
            PressureKind::Cpu,
            ActivityState::Progress(ProgressValue::new(620)),
        )
        .with_meter(
            MeterValue::Measured(ProgressValue::new(620)),
            PressureState::None,
            [],
        ),
    );
    let mut unmeasured_model = SwitchboardModel::new("Switchboard");
    unmeasured_model.resources.push(ResourceSummary::new(
        "CPU",
        "62%",
        PressureKind::Cpu,
        ActivityState::Progress(ProgressValue::new(620)),
    ));

    let mut measured_sb = Switchboard::new(measured_model);
    let mut unmeasured_sb = Switchboard::new(unmeasured_model);
    let b = bounds();
    let mut measured_surface = Surface::new(b.width, b.height).expect("surface");
    let mut unmeasured_surface = Surface::new(b.width, b.height).expect("surface");
    measured_sb.render(&mut measured_surface, b, Scale::ONE, &theme, font());
    unmeasured_sb.render(&mut unmeasured_surface, b, Scale::ONE, &theme, font());
    assert_ne!(
        measured_surface.pixels(),
        unmeasured_surface.pixels(),
        "a measured meter's fill must paint differently from the honest \
         unmeasured groove"
    );
}

#[test]
fn band_meter_rects_tile_evenly_for_various_counts() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let band = Rect::new(10, 20, 600, 40);
    for count in [1usize, 2, 4, 7] {
        let mut prev_right = band.left();
        for i in 0..count {
            let rect = Switchboard::band_meter_rect(band, i, count, scale, &theme);
            assert_eq!(rect.top(), band.top());
            assert_eq!(rect.height, band.height);
            assert!(
                rect.left() >= prev_right,
                "meter {i} of {count} overlaps its predecessor"
            );
            assert!(
                rect.right() <= band.right(),
                "meter {i} of {count} escapes the band"
            );
            prev_right = rect.right();
        }
    }
}

#[test]
fn each_resource_paints_at_its_own_band_slot() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let count = 3;
    for i in 0..count {
        let rect = Switchboard::band_meter_rect(layout.band, i, count, Scale::ONE, &theme);
        assert!(has_ink(&surface, rect), "meter slot {i} painted nothing");
    }
}

#[test]
fn press_inside_band_produces_no_action() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let (x, y) = centre(layout.band);
    let before_section = sb.section();
    let before_offset = sb.scroll_offset();

    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);

    assert!(
        actions.is_empty(),
        "the header band is an instrument, not a control"
    );
    assert_eq!(sb.section(), before_section);
    assert_eq!(sb.scroll_offset(), before_offset);
}

#[test]
fn press_just_below_band_still_reaches_the_tabs() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let tab_w = layout.tabs.width / 4;
    let x = layout.tabs.left()
        + i32::try_from(tab_w).unwrap_or(0)
        + i32::try_from(tab_w).unwrap_or(0) / 2;
    let y = layout.tabs.top();
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Jobs
    }));
}

#[test]
fn window_too_short_for_the_band_still_renders_in_bounds() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    // Shorter than the title bar and band together would ordinarily need.
    let b = Rect::new(0, 0, 600, 24);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    // Must not panic: every region clips to the client instead.
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    assert!(layout.band.bottom() <= layout.frame.client.bottom());
    assert!(layout.tabs.bottom() <= layout.frame.client.bottom());
    assert!(layout.content.bottom() <= layout.frame.client.bottom());
    assert!(layout.scroll.bottom() <= layout.frame.client.bottom());
}

// --- Opening on a chosen section ---------------------------------------

#[test]
fn select_section_shows_that_section_and_marks_its_tab() {
    let theme = Theme::dark();
    let b = bounds();
    let mut painted = alloc::vec::Vec::new();
    for section in Section::ALL {
        let mut sb = Switchboard::new(model());
        let changed = sb.select_section(section);
        if section == Section::Tasks {
            assert_eq!(changed, None, "Tasks is what a fresh Switchboard shows");
        } else {
            assert_eq!(changed, Some(SwitchboardAction::SectionChanged { section }));
        }
        assert_eq!(sb.section(), section);
        assert_eq!(sb.tabs.selected(), Some(section.index()));
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        painted.push((section, surface.pixels().to_vec()));
    }
    for (i, (section, pixels)) in painted.iter().enumerate() {
        for (other_section, other_pixels) in painted.iter().skip(i + 1) {
            assert_ne!(
                pixels, other_pixels,
                "{section:?} and {other_section:?} drew the same surface"
            );
        }
    }
}

#[test]
fn select_section_reranges_the_scroll_for_the_new_section() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    // Scroll deep into the long (50-item) Tasks list.
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 20 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    let deep = sb.scroll_offset();
    assert!(deep > 0, "the long list must actually scroll");

    // The short (6-item) Recovery list opens at its own offset, never the
    // long list's, and the scrollbar is re-ranged to its content.
    assert_eq!(
        sb.select_section(Section::Recovery),
        Some(SwitchboardAction::SectionChanged {
            section: Section::Recovery
        })
    );
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 6);
    assert_eq!(sb.scroll_offset(), 0);
    assert!(range.offset() <= range.max_offset());

    // Going back restores the long list's own, still-valid offset.
    assert_eq!(
        sb.select_section(Section::Tasks),
        Some(SwitchboardAction::SectionChanged {
            section: Section::Tasks
        })
    );
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 50);
    assert_eq!(sb.scroll_offset(), deep);
    assert!(range.offset() <= range.max_offset());
}

#[test]
fn selecting_the_shown_section_changes_nothing() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 4 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    // Move the keyboard off the first item too, so a stray reset would show.
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    let before = sb.clone();
    let offset = sb.scroll_offset();

    assert_eq!(sb.select_section(Section::Tasks), None);

    assert_eq!(
        sb, before,
        "re-selecting the shown section must change nothing"
    );
    assert_eq!(sb.scroll_offset(), offset);
}

#[test]
fn pointer_after_selection_reaches_the_new_sections_content() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    sb.select_section(Section::Recovery);
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    // A recovery row splits into two actions; its leading one sits where the
    // Tasks section has no button at all, so only the new section can answer.
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 2, Scale::ONE, &theme);
    let (x, y) = centre(buttons[0]);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Recovery {
        index: 0,
        control: RecoveryControl::Restart
    }));
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, SwitchboardAction::Task { .. })),
        "the superseded section must not still be receiving input"
    );
}

#[test]
fn direct_selection_and_the_keyboard_path_agree() {
    let mut by_key = Switchboard::new(model());
    let mut direct = Switchboard::new(model());
    // Put both on the tab strip (Content -> Scrollbar -> TitleBar -> Tabs) so
    // the only difference is how the section is chosen.
    for _ in 0..3 {
        assert_eq!(by_key.on_key(Key::Named(NamedKey::Tab)), None);
        assert_eq!(direct.on_key(Key::Named(NamedKey::Tab)), None);
    }
    // One walks the strip to Recovery and commits it...
    assert_eq!(by_key.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(by_key.on_key(Key::Named(NamedKey::Right)), None);
    let by_key_action = by_key.on_key(Key::Named(NamedKey::Enter));
    // ...the other asks for it directly.
    let direct_action = direct.select_section(Section::Recovery);

    assert_eq!(
        by_key_action,
        Some(SwitchboardAction::SectionChanged {
            section: Section::Recovery
        })
    );
    assert_eq!(by_key_action, direct_action);
    assert_eq!(by_key, direct, "one transition must leave one state");
}

// --- Refreshing the model in place ---------------------------------------

/// A model of `tasks` tasks and `resources` resources and nothing else, for a
/// refresh that shortens, empties, or re-populates a section.
///
/// The first task's action is refused while the rest are permitted. The base
/// [`model`] permits every one of its tasks, so a refused first row is how a
/// test tells a refreshed row apart from the one it replaced: only the new row
/// can answer a click with silence.
fn refreshed_model(tasks: usize, resources: usize) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for i in 0..tasks {
        m.tasks.push(TaskSummary {
            name: alloc::format!("fresh {i}"),
            detail: alloc::format!("{i} MB"),
            pressure: PressureState::None,
            activity: ActivityState::Idle,
            recovery: RecoveryState::None,
            action: alloc::string::String::from("End"),
            action_allowed: i > 0,
        });
    }
    for i in 0..resources {
        m.resources.push(
            ResourceSummary::new(
                alloc::format!("R{i}"),
                "10%",
                PressureKind::Cpu,
                ActivityState::Progress(ProgressValue::new(100)),
            )
            .with_meter(
                MeterValue::Measured(ProgressValue::new(100)),
                PressureState::None,
                [],
            ),
        );
    }
    m
}

#[test]
fn set_model_keeps_the_section_the_offset_and_the_focus() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.select_section(Section::Jobs);
    sb.render(&mut surface, b, Scale::ONE, &theme, font());

    // Walk the keyboard down the job list, scroll it away from the top, then
    // step the focus region on, so all three are off their defaults.
    for _ in 0..3 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    }
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    let offset = sb.scroll_offset();
    assert!(offset > 0, "the job list must actually scroll");
    assert_eq!(sb.on_key(Key::Named(NamedKey::Tab)), None);
    let focus = sb.focus;
    let content_focus = sb.content_focus;

    sb.set_model(model());

    assert_eq!(sb.section(), Section::Jobs);
    assert_eq!(sb.tabs.selected(), Some(Section::Jobs.index()));
    assert_eq!(
        sb.scroll_offset(),
        offset,
        "a sample must not scroll the user back to the top"
    );
    assert_eq!(sb.focus, focus);
    assert_eq!(sb.content_focus, content_focus);
}

#[test]
fn set_model_clamps_an_offset_past_the_end_of_a_shorter_list() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 40 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert!(
        sb.scroll_offset() > 5,
        "the 50-item list must scroll well past a 5-item one"
    );

    // Five tasks have nowhere near that far to scroll: the refresh re-ranges
    // there and then, rather than leaving a dangling offset for the next frame.
    sb.set_model(refreshed_model(5, 3));

    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 5);
    assert!(range.offset() <= range.max_offset());
    assert!(
        sb.scroll_offset() < 5,
        "the offset must land inside the new list"
    );

    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let range = sb.scroll.model().range();
    assert_eq!(range.content_extent(), 5);
    assert!(range.offset() <= range.max_offset());
}

#[test]
fn set_model_to_an_empty_model_stays_valid_and_renderable() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    for _ in 0..4 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    }
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 20 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert!(sb.scroll_offset() > 0);

    sb.set_model(SwitchboardModel::new("Switchboard"));

    assert_eq!(sb.scroll_offset(), 0, "an empty list has nowhere to scroll");
    assert_eq!(sb.content_focus, 0);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        None,
        "an emptied section has nothing to activate"
    );
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    assert_eq!(layout.band.height, 0, "no resources means no band");
    assert!(layout.content.bottom() <= layout.frame.client.bottom());
}

#[test]
fn pointer_after_set_model_addresses_the_new_rows() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());

    // Three tasks replace fifty, and the first of the three is refused.
    sb.set_model(refreshed_model(3, 3));
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let action_button = |slot: u32| {
        let (_, buttons) = Switchboard::split_row(info.item_rect(slot), 1, Scale::ONE, &theme);
        centre(buttons[0])
    };

    let (x, y) = action_button(0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "the refused new row must answer, not the permitted row it replaced"
    );

    let (x, y) = action_button(2);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).contains(&SwitchboardAction::Task { index: 2 })
    );

    let (x, y) = action_button(3);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a row the refresh removed must never be actionable"
    );
}

#[test]
fn set_model_cannot_complete_a_press_begun_on_the_row_it_replaced() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 1, Scale::ONE, &theme);
    let (x, y) = centre(buttons[0]);

    // Arm the first task's action, refresh under the held pointer, let go.
    // The replacement row sits at the same place and is equally permitted, so
    // only the dropped arm can keep it from firing.
    assert_eq!(
        sb.on_pointer(&moved(x, y), b, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(sb.on_pointer(&PRESS, b, Scale::ONE, &theme, font()), None);
    sb.set_model(model());

    assert_eq!(
        sb.on_pointer(&RELEASE, b, Scale::ONE, &theme, font()),
        None,
        "a press must not complete against the row that replaced its target"
    );
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).contains(&SwitchboardAction::Task { index: 0 }),
        "a fresh gesture on the new row must still work"
    );
}

#[test]
fn band_re_renders_from_the_refreshed_resources() {
    let theme = Theme::dark();
    let b = bounds();
    // The task list is identical across every refresh below, so the band is
    // the only thing that can change the painted surface.
    let mut sb = Switchboard::new(refreshed_model(3, 3));
    let mut before = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut before, b, Scale::ONE, &theme, font());
    let band = sb.compute_layout(b, Scale::ONE, &theme, font()).band;
    assert!(band.height > 0);
    assert!(has_ink(&before, band));

    sb.set_model(refreshed_model(3, 1));
    let mut after = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut after, b, Scale::ONE, &theme, font());
    assert_ne!(
        before.pixels(),
        after.pixels(),
        "one meter must not tile the band like three"
    );
    assert!(has_ink(&after, band));

    sb.set_model(refreshed_model(3, 0));
    sb.render(&mut after, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    assert_eq!(
        layout.band.height, 0,
        "the band collapses with its last meter"
    );
    assert_eq!(layout.tabs.top(), layout.frame.client.top());
}

#[test]
fn new_then_set_model_draws_what_building_with_that_model_draws() {
    let theme = Theme::dark();
    let b = bounds();
    // Neither has been interacted with, so there is no preserved state to
    // account for: any difference would be a second derivation.
    let mut refreshed = Switchboard::new(model());
    refreshed.set_model(refreshed_model(4, 2));
    let mut built = Switchboard::new(refreshed_model(4, 2));

    let mut refreshed_surface = Surface::new(b.width, b.height).expect("surface");
    let mut built_surface = Surface::new(b.width, b.height).expect("surface");
    refreshed.render(&mut refreshed_surface, b, Scale::ONE, &theme, font());
    built.render(&mut built_surface, b, Scale::ONE, &theme, font());

    assert_eq!(
        refreshed_surface.pixels(),
        built_surface.pixels(),
        "one derivation must draw one surface"
    );
    assert_eq!(refreshed, built, "one derivation must leave one state");
}

#[test]
fn overview_resource_cards_still_render_from_the_extended_model() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Overview);
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let pc = sb
        .panel
        .content_rect(layout.content, Scale::ONE, &theme)
        .expect("panel content");
    let card_h = Switchboard::card_item_height(Scale::ONE, &theme);
    let block = Rect::new(pc.left(), pc.top(), pc.width, card_h.saturating_mul(3));
    assert!(
        has_ink(&surface, block),
        "the resource card block must still paint"
    );
}
