//! Unit tests for the Switchboard reference composition (spec §17, §20).
//!
//! These prove the composition is assembled from the shared controls and
//! behaves correctly: the window chrome and scrollbar junction stay separate
//! from the client, the tab strip switches sections (by pointer and keyboard),
//! the mouse wheel and keyboard scroll the active section, denied actions fail
//! closed and render distinctly from disabled ones, a force action carries a
//! confirmation posture, and the layout scales.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

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
    for (name, kind) in [
        ("CPU", PressureKind::Cpu),
        ("Memory", PressureKind::Memory),
        ("Disk", PressureKind::Disk),
    ] {
        m.resources.push(ResourceSummary {
            name: alloc::string::String::from(name),
            reading: alloc::string::String::from("62%"),
            kind,
            activity: ActivityState::Progress(ProgressValue::new(620)),
        });
    }
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
        if let Some(action) = sb.on_pointer(&event, b, scale, theme) {
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
    let layout = sb.compute_layout(bounds(), Scale::ONE, &theme);
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
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
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
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
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
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
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
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
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
        sb.select_section_index(Section::Jobs.index()),
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
    sb.select_section_index(Section::Recovery.index());
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
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
    );
    assert_eq!(sb.scroll_offset(), 4);
    // Switching to Jobs shows its own (zero) offset.
    sb.select_section_index(Section::Jobs.index());
    assert_eq!(sb.scroll_offset(), 0);
    // Switching back restores the Tasks offset.
    sb.select_section_index(Section::Tasks.index());
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
    );
    sb.select_section_index(Section::Overview.index());
    // Sync against the (empty) Overview list clamps the offset to zero.
    let mut surface = Surface::new(600, 400).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.scroll_offset(), 0);
}

#[test]
fn layout_scales_with_the_ui_scale() {
    let theme = Theme::dark();
    let sb = Switchboard::new(model());
    let one = sb.compute_layout(bounds(), Scale::ONE, &theme);
    let two = sb.compute_layout(bounds(), Scale::from_percent(200).expect("scale"), &theme);
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
