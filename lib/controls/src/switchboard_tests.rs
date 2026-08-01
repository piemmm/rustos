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
    ActionVerdict, ActivityControl, ActivityMember, ActivityRow, ActivitySummary, JobControl,
    JobSummary, PressureAction, PressureCause, PressureControl, RecoveryControl, RecoveryItem,
    ResourceSummary, Section, ServiceSummary, Switchboard, SwitchboardAction, SwitchboardModel,
    SystemAction, TaskSummary,
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
            group: None,
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
    m.pressure = (0..8).map(model_pressure_cause).collect();
    m.activities = (0..6).map(model_activity).collect();
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

/// The CPU pressure cause at `index` of the populated model: Working, blamed
/// on task `index`, offering a recommended Pause, Lower priority, and Show
/// tasks — all Ready.
fn model_pressure_cause(index: usize) -> PressureCause {
    PressureCause {
        resource: alloc::string::String::from("CPU"),
        kind: PressureKind::Cpu,
        culprit: alloc::format!("culprit {index}"),
        cause: alloc::string::String::from("busy loop"),
        activity: ActivityState::Working,
        task_index: Some(index),
        actions: alloc::vec![
            PressureAction {
                label: alloc::string::String::from("Pause"),
                control: PressureControl::Pause,
                verdict: ActionVerdict::Ready,
                recommended: true,
            },
            PressureAction {
                label: alloc::string::String::from("Lower priority"),
                control: PressureControl::LowerPriority,
                verdict: ActionVerdict::Ready,
                recommended: false,
            },
            PressureAction {
                label: alloc::string::String::from("Show tasks"),
                control: PressureControl::ShowTasks,
                verdict: ActionVerdict::Ready,
                recommended: false,
            },
        ],
    }
}

/// The activity at `index` of the populated model: stable id `100 + index`,
/// controllable, accepting members, paused on the odd indices, with one
/// working and one idle member.
fn model_activity(index: u64) -> ActivitySummary {
    ActivitySummary {
        id: 100 + index,
        name: alloc::format!("activity {index}"),
        detail: alloc::string::String::from("2 tasks"),
        activity: ActivityState::Working,
        paused: index % 2 == 1,
        can_control: true,
        can_accept_member: true,
        members: alloc::vec![
            ActivityMember {
                name: alloc::format!("member {index}.0"),
                detail: alloc::string::String::from("running"),
                activity: ActivityState::Working,
            },
            ActivityMember {
                name: alloc::format!("member {index}.1"),
                detail: alloc::string::String::from("idle"),
                activity: ActivityState::Idle,
            },
        ],
    }
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
    // The second tab (Jobs) occupies the second sixth of the strip.
    let tab_w = layout.tabs.width / u32::try_from(Section::ALL.len()).unwrap_or(1);
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
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 2, Scale::ONE, &theme);
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
        group: None,
    });
    let mut sb = Switchboard::new(m);
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 2, Scale::ONE, &theme);
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
        group: None,
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
    let tab_w = layout.tabs.width / u32::try_from(Section::ALL.len()).unwrap_or(1);
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
    for _ in 0..Section::Recovery.index() {
        assert_eq!(by_key.on_key(Key::Named(NamedKey::Right)), None);
    }
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
            group: None,
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
        let (_, buttons) = Switchboard::split_row(info.item_rect(slot), 2, Scale::ONE, &theme);
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
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 2, Scale::ONE, &theme);
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

// --- The six-section tab strip -------------------------------------------

#[test]
fn six_sections_in_tab_order() {
    assert_eq!(
        Section::ALL.map(Section::title),
        [
            "Tasks",
            "Jobs",
            "Pressure",
            "Activities",
            "Recovery",
            "Overview"
        ]
    );
    for (i, section) in Section::ALL.iter().enumerate() {
        assert_eq!(section.index(), i);
        assert_eq!(Section::from_index(i), Some(*section));
    }
    assert_eq!(Section::from_index(Section::ALL.len()), None);
}

#[test]
fn offsets_persist_for_the_new_sections() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(model());
    let b = bounds();
    sb.select_section(Section::Pressure);
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 2 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(sb.scroll_offset(), 2);
    sb.select_section(Section::Activities);
    assert_eq!(sb.scroll_offset(), 0);
    sb.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        b,
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(sb.scroll_offset(), 3);
    sb.select_section(Section::Pressure);
    assert_eq!(sb.scroll_offset(), 2);
    sb.select_section(Section::Activities);
    assert_eq!(sb.scroll_offset(), 3);
}

// --- The Pressure section -------------------------------------------------

/// One relief action for a hand-built pressure cause.
fn relief_action(
    control: PressureControl,
    verdict: ActionVerdict,
    recommended: bool,
) -> PressureAction {
    PressureAction {
        label: alloc::string::String::from("Relieve"),
        control,
        verdict,
        recommended,
    }
}

/// A model with three tasks and one CPU pressure cause carrying `actions`,
/// for the Pressure tests that need a precise footer.
fn pressure_model(actions: alloc::vec::Vec<PressureAction>) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for i in 0..3 {
        m.tasks.push(TaskSummary {
            name: alloc::format!("task {i}"),
            detail: alloc::string::String::new(),
            pressure: PressureState::None,
            activity: ActivityState::Idle,
            recovery: RecoveryState::None,
            action: alloc::string::String::from("End"),
            action_allowed: true,
            group: None,
        });
    }
    m.pressure.push(PressureCause {
        resource: alloc::string::String::from("CPU"),
        kind: PressureKind::Cpu,
        culprit: alloc::string::String::from("culprit"),
        cause: alloc::string::String::from("busy loop"),
        activity: ActivityState::Working,
        task_index: None,
        actions,
    });
    m
}

/// The centre of the pressure card footer button at `action` for the cause
/// at `index`, in window coordinates.
fn pressure_footer_centre(
    sb: &Switchboard,
    b: Rect,
    theme: &Theme,
    index: usize,
    action: usize,
) -> (i32, i32) {
    let layout = sb.compute_layout(b, Scale::ONE, theme, font());
    let info = sb.list_info(&layout, Scale::ONE, theme);
    let item = info.item_rect(u32::try_from(index).unwrap_or(0));
    let rects = sb.pressure[index]
        .card
        .footer_rects(item, Scale::ONE, theme);
    centre(rects[action])
}

#[test]
fn pressure_card_rail_differs_across_kinds() {
    let theme = Theme::dark();
    let b = bounds();
    let mut painted = alloc::vec::Vec::new();
    for kind in [PressureKind::Cpu, PressureKind::Memory, PressureKind::Disk] {
        let mut m = pressure_model(alloc::vec::Vec::new());
        m.pressure[0].kind = kind;
        m.pressure[0].activity = ActivityState::Idle;
        let mut sb = Switchboard::new(m);
        sb.select_section(Section::Pressure);
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        painted.push((kind, surface.pixels().to_vec()));
    }
    for (i, (kind, pixels)) in painted.iter().enumerate() {
        for (other_kind, other_pixels) in painted.iter().skip(i + 1) {
            assert_ne!(
                pixels, other_pixels,
                "{kind:?} and {other_kind:?} drew the same semantic rail"
            );
        }
    }
}

#[test]
fn pressure_card_heat_seam_marks_working() {
    let theme = Theme::dark();
    let b = bounds();
    let mut painted = alloc::vec::Vec::new();
    for activity in [ActivityState::Idle, ActivityState::Working] {
        let mut m = pressure_model(alloc::vec::Vec::new());
        m.pressure[0].activity = activity;
        let mut sb = Switchboard::new(m);
        sb.select_section(Section::Pressure);
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        painted.push(surface.pixels().to_vec());
    }
    assert_ne!(
        painted[0], painted[1],
        "a Working cause must show its heat seam"
    );
}

#[test]
fn recommended_relief_action_carries_action_warmth() {
    let sb = Switchboard::new(model());
    let footer = sb.pressure[0].card.footer();
    assert_eq!(footer[0].role(), ControlRole::Recommended);
    assert_eq!(footer[1].role(), ControlRole::Neutral);
}

#[test]
fn ready_relief_action_activates() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Pressure);
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Pressure {
        index: 0,
        control: PressureControl::Pause
    }));
}

#[test]
fn disabled_relief_action_renders_muted_and_fails_closed() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(pressure_model(alloc::vec![relief_action(
        PressureControl::Pause,
        ActionVerdict::DisabledByState,
        false,
    )]));
    sb.select_section(Section::Pressure);
    assert_eq!(
        sb.pressure[0].card.footer()[0].state().disposition(),
        ControlDisposition::DisabledByState
    );
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a disabled relief action must not activate"
    );
}

#[test]
fn denied_relief_action_renders_authority_and_fails_closed() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(pressure_model(alloc::vec![relief_action(
        PressureControl::Pause,
        ActionVerdict::DeniedByAuthority,
        false,
    )]));
    sb.select_section(Section::Pressure);
    assert_eq!(
        sb.pressure[0].card.footer()[0].state().disposition(),
        ControlDisposition::DeniedByAuthority
    );
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a denied relief action must not activate"
    );
}

#[test]
fn show_tasks_lands_on_the_culprit_task() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Pressure);
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 1, 2);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Tasks
    }));
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, SwitchboardAction::Pressure { .. })),
        "an internally resolved relief action must not also emit Pressure"
    );
    assert_eq!(sb.section(), Section::Tasks);
    assert_eq!(
        sb.content_focus, 1,
        "the culprit's task row takes the focus"
    );
}

#[test]
fn show_tasks_clamps_a_missing_or_stale_task_index() {
    let theme = Theme::dark();
    let b = bounds();
    // No task index: the focus falls to the first task.
    let mut sb = Switchboard::new(pressure_model(alloc::vec![relief_action(
        PressureControl::ShowTasks,
        ActionVerdict::Ready,
        false,
    )]));
    sb.select_section(Section::Pressure);
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Tasks
    }));
    assert_eq!(sb.content_focus, 0);

    // A stale index past the list end clamps to the last task.
    let mut m = pressure_model(alloc::vec![relief_action(
        PressureControl::ShowTasks,
        ActionVerdict::Ready,
        false,
    )]);
    m.pressure[0].task_index = Some(999);
    let mut sb = Switchboard::new(m);
    sb.select_section(Section::Pressure);
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Tasks
    }));
    assert_eq!(
        sb.content_focus, 2,
        "a stale task index clamps into the shown list"
    );
}

#[test]
fn empty_pressure_section_has_nothing_to_activate() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(SwitchboardModel::new("Switchboard"));
    sb.select_section(Section::Pressure);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
}

#[test]
fn keyboard_reaches_every_pressure_footer() {
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Pressure);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::Pause
        })
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::LowerPriority
        })
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::SectionChanged {
            section: Section::Tasks
        })
    );
    assert_eq!(sb.section(), Section::Tasks);
    assert_eq!(sb.content_focus, 0, "cause 0 names task 0");
}

// --- The Activities section -----------------------------------------------

/// The centre of the activity header button at `button` for the header in
/// flattened row `slot`, in window coordinates.
fn activity_button_centre(
    sb: &Switchboard,
    b: Rect,
    theme: &Theme,
    slot: u32,
    button: usize,
) -> (i32, i32) {
    let layout = sb.compute_layout(b, Scale::ONE, theme, font());
    let info = sb.list_info(&layout, Scale::ONE, theme);
    let (_, buttons) = Switchboard::split_row(info.item_rect(slot), 4, Scale::ONE, theme);
    centre(buttons[button])
}

/// The premultiplied channel values of the `width`-wide strip at the leading
/// edge of `rect`.
fn leading_strip(surface: &Surface, rect: Rect, width: u32) -> alloc::vec::Vec<(u8, u8, u8, u8)> {
    let mut out = alloc::vec::Vec::new();
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.left() + i32::try_from(width).unwrap_or(0) {
            let (xu, yu) = (u32::try_from(x).unwrap_or(0), u32::try_from(y).unwrap_or(0));
            if let Some(p) = surface.get(xu, yu) {
                out.push((p.r, p.g, p.b, p.a));
            }
        }
    }
    out
}

#[test]
fn activities_flatten_headers_and_indent_members() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Activities);
    assert_eq!(sb.activity_row_at(0), Some(ActivityRow::Header(0)));
    assert_eq!(sb.activity_row_at(1), Some(ActivityRow::Member(0, 0)));
    assert_eq!(sb.activity_row_at(2), Some(ActivityRow::Member(0, 1)));
    assert_eq!(sb.activity_row_at(3), Some(ActivityRow::Header(1)));
    assert_eq!(sb.activity_row_at(17), Some(ActivityRow::Member(5, 1)));
    assert_eq!(sb.activity_row_at(18), None);

    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let indent = Scale::ONE.scale_length(theme.metrics().control_height);
    let header = info.item_rect(0);
    let member = info.item_rect(1);
    // The header row owns its leading edge; a member row leaves the same
    // strip to the background, which is what makes the hierarchy visible.
    assert_ne!(
        leading_strip(&surface, header, indent),
        leading_strip(&surface, member, indent),
        "a member row must be indented off its leading edge"
    );
    let inset = Rect::new(
        member.left() + i32::try_from(indent).unwrap_or(0),
        member.top(),
        member.width.saturating_sub(indent.saturating_mul(2)),
        member.height,
    );
    assert!(
        has_ink(&surface, inset),
        "a member row paints when indented"
    );
}

#[test]
fn activity_switch_and_close_activate_by_pointer() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Activities);
    let (x, y) = activity_button_centre(&sb, b, &theme, 0, 0);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Activity {
        index: 0,
        control: ActivityControl::Switch
    }));
    let (x, y) = activity_button_centre(&sb, b, &theme, 0, 3);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Activity {
        index: 0,
        control: ActivityControl::Close
    }));
}

#[test]
fn pause_resume_emission_follows_the_paused_flag() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Activities);
    // Activity 0 runs, so its header offers Pause.
    let (x, y) = activity_button_centre(&sb, b, &theme, 0, 1);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Activity {
        index: 0,
        control: ActivityControl::Pause
    }));
    // Activity 1 is paused; its header (flattened row 3) offers Resume.
    let (x, y) = activity_button_centre(&sb, b, &theme, 3, 1);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Activity {
        index: 1,
        control: ActivityControl::Resume
    }));
}

#[test]
fn activity_close_carries_confirmation_posture() {
    let sb = Switchboard::new(model());
    assert_eq!(sb.activities[0].close.role(), ControlRole::Destructive);
    assert_eq!(
        sb.activities[0].close.state().disposition(),
        ControlDisposition::NeedsConfirmation
    );
}

#[test]
fn uncontrollable_activity_fails_closed() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.activities[0].can_control = false;
    let mut sb = Switchboard::new(m);
    sb.select_section(Section::Activities);
    assert_eq!(
        sb.activities[0].pause_resume.state().disposition(),
        ControlDisposition::DeniedByAuthority
    );
    assert_eq!(
        sb.activities[0].close.state().disposition(),
        ControlDisposition::DeniedByAuthority
    );
    for button in [1, 3] {
        let (x, y) = activity_button_centre(&sb, b, &theme, 0, button);
        assert!(
            click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
            "a denied activity control must not activate"
        );
    }
    // Switching needs no control authority, so it stays available.
    let (x, y) = activity_button_centre(&sb, b, &theme, 0, 0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).contains(&SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Switch
        })
    );
}

#[test]
fn member_rows_are_display_only() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Activities);
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let (x, y) = centre(info.item_rect(1));
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a member row is display-only"
    );
}

#[test]
fn keyboard_reaches_every_activity_header_button() {
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Activities);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Switch
        })
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Pause
        })
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        None,
        "Rename begins an edit instead of emitting"
    );
    assert!(sb.rename.is_some());
    assert_eq!(sb.on_key(Key::Named(NamedKey::Escape)), None);
    assert!(sb.rename.is_none());
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Close
        })
    );

    // A member row (flattened row 1) has no buttons to focus or activate.
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
}

// --- The Group popup --------------------------------------------------------

/// Open the Group popup on task row 0 by clicking its Group button.
fn open_group_popup_on_first_task(sb: &mut Switchboard, b: Rect, theme: &Theme) {
    let layout = sb.compute_layout(b, Scale::ONE, theme, font());
    let info = sb.list_info(&layout, Scale::ONE, theme);
    let (_, buttons) = Switchboard::split_row(info.item_rect(0), 2, Scale::ONE, theme);
    let (x, y) = centre(buttons[1]);
    assert!(
        click(sb, b, Scale::ONE, theme, x, y).is_empty(),
        "opening the popup emits nothing"
    );
    assert!(sb.group_popup.is_some(), "the Group popup must open");
}

/// A window point that hits row `index` of the open Group popup.
fn popup_row_point(sb: &Switchboard, b: Rect, theme: &Theme, index: usize) -> (i32, i32) {
    let layout = sb.compute_layout(b, Scale::ONE, theme, font());
    let popup = sb.group_popup.as_ref().expect("an open Group popup");
    let anchor = sb.group_anchor_rect(popup.task, &layout, Scale::ONE, theme);
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, theme, font());
    let x = rect.left() + i32::try_from(rect.width).unwrap_or(0) / 2;
    for y in rect.top()..rect.bottom() {
        if popup.menu.row_at(rect, Scale::ONE, theme, Point::new(x, y)) == Some(index) {
            return (x, y);
        }
    }
    panic!("popup row {index} is not hit-testable");
}

#[test]
fn group_button_opens_the_popup_on_its_task() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let popup = sb.group_popup.as_ref().expect("popup");
    assert_eq!(popup.task, 0);
    // One row per activity, then "New activity"; an ungrouped task gets no
    // "Remove from activity" row.
    assert_eq!(popup.menu.items().len(), 7);
    assert_eq!(popup.menu.items()[0].label(), "activity 0");
    assert_eq!(popup.menu.items()[6].label(), "New activity");
}

#[test]
fn group_popup_anchors_below_its_button_inside_the_window() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let popup = sb.group_popup.as_ref().expect("popup");
    let anchor = sb.group_anchor_rect(popup.task, &layout, Scale::ONE, &theme);
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, &theme, font());
    assert_eq!(
        rect.top(),
        anchor.bottom(),
        "the popup opens below its anchor"
    );
    assert!(rect.left() >= b.left());
    assert!(rect.right() <= b.right());
    assert!(rect.bottom() <= b.bottom());
}

#[test]
fn group_popup_lists_activities_with_disable_reasons() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.tasks[0].group = Some(0);
    m.activities[1].can_accept_member = false;
    m.can_create_activity = false;
    let mut sb = Switchboard::new(m);
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let items = sb.group_popup.as_ref().expect("popup").menu.items();
    assert_eq!(items.len(), 8);
    assert_eq!(
        items[0].state().disposition(),
        ControlDisposition::DisabledByState
    );
    assert_eq!(items[0].reason(), Some("Current activity"));
    assert_eq!(
        items[1].state().disposition(),
        ControlDisposition::DisabledByState
    );
    assert_eq!(items[1].reason(), Some("Activity is full"));
    assert_eq!(
        items[2].state().disposition(),
        ControlDisposition::Interactive
    );
    assert_eq!(items[6].label(), "New activity");
    assert_eq!(
        items[6].state().disposition(),
        ControlDisposition::DisabledByState
    );
    assert_eq!(items[6].reason(), Some("Activity limit reached"));
    assert_eq!(items[7].label(), "Remove from activity");
    assert_eq!(
        items[7].state().disposition(),
        ControlDisposition::Interactive
    );
}

#[test]
fn group_popup_groups_to_an_existing_activity() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let (x, y) = popup_row_point(&sb, b, &theme, 2);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::TaskGrouped {
        task: 0,
        activity: Some(2)
    }));
    assert!(sb.group_popup.is_none(), "activation closes the popup");
}

#[test]
fn group_popup_new_activity_groups_to_none() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let (x, y) = popup_row_point(&sb, b, &theme, 6);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::TaskGrouped {
        task: 0,
        activity: None
    }));
    assert!(sb.group_popup.is_none());
}

#[test]
fn group_popup_removes_a_grouped_task() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.tasks[0].group = Some(0);
    let mut sb = Switchboard::new(m);
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let (x, y) = popup_row_point(&sb, b, &theme, 7);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::TaskUngrouped { task: 0 }));
    assert!(sb.group_popup.is_none());
}

#[test]
fn group_popup_refuses_a_disabled_row() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.tasks[0].group = Some(0);
    let mut sb = Switchboard::new(m);
    open_group_popup_on_first_task(&mut sb, b, &theme);
    // Row 0 is the task's current activity, disabled with its reason.
    let (x, y) = popup_row_point(&sb, b, &theme, 0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a disabled popup row must not activate"
    );
    assert!(
        sb.group_popup.is_some(),
        "a refused activation leaves the popup open"
    );
}

#[test]
fn group_popup_escape_dismisses_without_emitting() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Escape)), None);
    assert!(sb.group_popup.is_none());
}

#[test]
fn group_popup_outside_press_dismisses_without_emitting() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
    let popup = sb.group_popup.as_ref().expect("popup");
    let anchor = sb.group_anchor_rect(popup.task, &layout, Scale::ONE, &theme);
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, &theme, font());
    let (x, y) = centre(layout.band);
    assert!(
        !rect.contains(Point::new(x, y)),
        "the probe point must sit outside the popup"
    );
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "an outside press dismisses without emitting"
    );
    assert!(sb.group_popup.is_none());
}

#[test]
fn group_popup_drops_on_refresh_and_section_change() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    sb.set_model(model());
    assert!(
        sb.group_popup.is_none(),
        "a refresh supersedes the menu the popup was built from"
    );

    open_group_popup_on_first_task(&mut sb, b, &theme);
    sb.select_section(Section::Jobs);
    assert!(
        sb.group_popup.is_none(),
        "a section change invalidates the popup's anchor"
    );
}

#[test]
fn keyboard_group_flow_reaches_the_popup_and_activates() {
    let mut sb = Switchboard::new(model());
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        None,
        "opening the popup emits nothing"
    );
    assert_eq!(sb.group_popup.as_ref().map(|p| p.task), Some(0));
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::TaskGrouped {
            task: 0,
            activity: Some(0)
        })
    );
    assert!(sb.group_popup.is_none());
}

// --- Inline rename ----------------------------------------------------------

/// Begin an inline rename of the first activity's header by pointer.
fn begin_first_rename(sb: &mut Switchboard, b: Rect, theme: &Theme) {
    sb.select_section(Section::Activities);
    let (x, y) = activity_button_centre(sb, b, theme, 0, 2);
    assert!(
        click(sb, b, Scale::ONE, theme, x, y).is_empty(),
        "beginning a rename emits nothing"
    );
    assert!(sb.rename.is_some(), "the rename must begin");
}

#[test]
fn rename_commits_by_enter_and_reports_the_name() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(
        sb.rename.as_ref().map(|e| e.field.text()),
        Some("activity 0"),
        "the field pre-fills with the current name"
    );
    assert_eq!(sb.on_key(Key::Char('!')), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::ActivityRenamed { index: 0 })
    );
    assert_eq!(sb.submitted_activity_name(), Some("activity 0!"));
    assert_eq!(sb.activities[0].name, "activity 0!");
    assert!(sb.rename.is_none());
}

#[test]
fn rename_escape_cancels_without_emitting() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(sb.on_key(Key::Char('!')), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Escape)), None);
    assert!(sb.rename.is_none());
    assert_eq!(sb.submitted_activity_name(), None);
    assert_eq!(
        sb.activities[0].name, "activity 0",
        "a cancel changes nothing"
    );
}

#[test]
fn rename_survives_a_refresh_that_moves_its_activity() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(sb.on_key(Key::Char('!')), None);

    // The refresh reorders the list: id 100 moves from index 0 to index 5.
    let mut m = model();
    m.activities.rotate_left(1);
    sb.set_model(m);

    let edit = sb.rename.as_ref().expect("the edit survives its activity");
    assert_eq!(edit.index, 5, "the edit re-locates its activity by id");
    assert_eq!(edit.field.text(), "activity 0!", "the typed text survives");
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::ActivityRenamed { index: 5 })
    );
    assert_eq!(sb.submitted_activity_name(), Some("activity 0!"));
    assert_eq!(sb.activities[5].name, "activity 0!");
}

#[test]
fn rename_drops_when_its_activity_vanishes() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    begin_first_rename(&mut sb, b, &theme);

    let mut m = model();
    m.activities.remove(0);
    sb.set_model(m);

    assert!(
        sb.rename.is_none(),
        "an edit never re-attaches to a different activity"
    );
    assert_eq!(sb.submitted_activity_name(), None);
}

#[test]
fn submitted_name_clears_on_the_next_refresh() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::ActivityRenamed { index: 0 })
    );
    assert!(sb.submitted_activity_name().is_some());
    sb.set_model(model());
    assert_eq!(
        sb.submitted_activity_name(),
        None,
        "a committed name is read before the next sample"
    );
}

// --- Keyboard action focus -------------------------------------------------

#[test]
fn keyboard_reaches_the_recovery_force_action() {
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Recovery);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Restart
        })
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Force
        }),
        "Force must be keyboard-reachable"
    );
}

#[test]
fn keyboard_reaches_the_task_group_button() {
    let mut sb = Switchboard::new(model());
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Task { index: 0 })
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert_eq!(
        sb.group_popup.as_ref().map(|p| p.task),
        Some(0),
        "the popup opens on the focused task"
    );
}

#[test]
fn keyboard_reaches_the_job_cancel_footer() {
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Jobs);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Job {
            index: 0,
            control: JobControl::Cancel
        })
    );
}

#[test]
fn action_focus_clamps_and_resets_with_the_row_focus() {
    let mut sb = Switchboard::new(model());
    assert_eq!(sb.on_key(Key::Named(NamedKey::Left)), None);
    assert_eq!(sb.row_action, 0, "Left at the first button stays put");
    for _ in 0..5 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    }
    assert_eq!(sb.row_action, 1, "Right clamps at the last button");
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(
        sb.row_action, 0,
        "moving the row focus resets the action focus"
    );
}

// --- The new sections under themes and refresh identity ---------------------

#[test]
fn light_and_high_contrast_render_the_new_sections() {
    let b = bounds();
    for theme in [Theme::light(), high_contrast()] {
        for section in [Section::Pressure, Section::Activities] {
            let mut sb = Switchboard::new(model());
            sb.select_section(section);
            let mut surface = Surface::new(b.width, b.height).expect("surface");
            sb.render(&mut surface, b, Scale::ONE, &theme, font());
            let layout = sb.compute_layout(b, Scale::ONE, &theme, font());
            assert!(
                has_ink(&surface, layout.content),
                "{section:?} painted nothing"
            );
        }
    }
}

#[test]
fn set_model_cannot_complete_a_press_begun_on_a_replaced_pressure_card() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Pressure);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);

    assert_eq!(
        sb.on_pointer(&moved(x, y), b, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(sb.on_pointer(&PRESS, b, Scale::ONE, &theme, font()), None);
    sb.set_model(model());

    assert_eq!(
        sb.on_pointer(&RELEASE, b, Scale::ONE, &theme, font()),
        None,
        "a press must not complete against the card that replaced its target"
    );
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).contains(&SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::Pause
        }),
        "a fresh gesture on the new card must still work"
    );
}

#[test]
fn set_model_cannot_complete_a_press_begun_on_a_replaced_activity_row() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(model());
    sb.select_section(Section::Activities);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let (x, y) = activity_button_centre(&sb, b, &theme, 0, 0);

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
        click(&mut sb, b, Scale::ONE, &theme, x, y).contains(&SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Switch
        }),
        "a fresh gesture on the new row must still work"
    );
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

/// Paint `sb` at the standard bounds and hand back the surface a host would
/// present.
fn painted(sb: &mut Switchboard, theme: &Theme) -> Surface {
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, theme, font());
    surface
}

/// A Switchboard whose layout has been settled by one render, so a following
/// pointer event resolves against the geometry the next render will use.
fn settled(theme: &Theme) -> Switchboard {
    let mut sb = Switchboard::new(model());
    let _ = painted(&mut sb, theme);
    sb
}

/// A point over the header resource band — an instrument that takes no
/// pointer input, so a sample there crosses no control.
fn band_point(sb: &Switchboard, theme: &Theme) -> (i32, i32) {
    centre(sb.compute_layout(bounds(), Scale::ONE, theme, font()).band)
}

/// A point over the first row of the active section's content.
fn content_point(sb: &Switchboard, theme: &Theme) -> (i32, i32) {
    let content = sb
        .compute_layout(bounds(), Scale::ONE, theme, font())
        .content;
    (content.left() + 4, content.top() + 4)
}

fn feed(sb: &mut Switchboard, theme: &Theme, event: &InputEvent) -> Option<SwitchboardAction> {
    sb.on_pointer(event, bounds(), Scale::ONE, theme, font())
}

#[test]
fn pointer_move_that_crosses_no_control_leaves_the_composition_equal() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let (x, y) = band_point(&sb, &theme);
    feed(&mut sb, &theme, &moved(x, y));

    let before = sb.clone();
    feed(&mut sb, &theme, &moved(x + 5, y + 1));

    assert_ne!(
        *sb.pointer, *before.pointer,
        "the sample must genuinely land on a new coordinate, or this proves \
         nothing"
    );
    assert_eq!(
        sb, before,
        "a sample at a new coordinate over the inert band draws the same \
         pixels, so it must not defeat a host's repaint gate"
    );
}

#[test]
fn pointer_position_alone_never_changes_the_pixels() {
    let theme = Theme::dark();
    let mut moved_pointer = settled(&theme);
    let mut resting = moved_pointer.clone();
    *moved_pointer.pointer = Point::new(517, 313);

    assert_ne!(
        *moved_pointer.pointer, *resting.pointer,
        "the two must genuinely differ in the excluded field"
    );
    assert_eq!(
        moved_pointer, resting,
        "the raw pointer coordinate is excluded from equality"
    );
    let a = painted(&mut moved_pointer, &theme);
    let b = painted(&mut resting, &theme);
    assert_eq!(
        a.pixels(),
        b.pixels(),
        "that exclusion is only sound because no render path reads it"
    );
}

#[test]
fn pointer_move_onto_a_row_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let (bx, by) = band_point(&sb, &theme);
    feed(&mut sb, &theme, &moved(bx, by));

    let before = sb.clone();
    let (x, y) = content_point(&sb, &theme);
    feed(&mut sb, &theme, &moved(x, y));

    assert_ne!(
        sb, before,
        "a hover highlight is visible, so it must force a repaint"
    );
}

#[test]
fn press_and_release_each_change_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let (x, y) = content_point(&sb, &theme);
    feed(&mut sb, &theme, &moved(x, y));

    let hovered = sb.clone();
    feed(&mut sb, &theme, &PRESS);
    assert_ne!(sb, hovered, "a press is visible on the pressed row");

    let pressed = sb.clone();
    feed(&mut sb, &theme, &RELEASE);
    assert_ne!(sb, pressed, "the release drops the pressed treatment");
}

#[test]
fn selection_change_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    sb.select_section(Section::Jobs);

    assert_ne!(
        sb, before,
        "the marked tab and the shown section are both visible"
    );
}

#[test]
fn focus_change_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    sb.on_key(Key::Named(NamedKey::Tab));

    assert_ne!(sb, before, "the focus ring moves to another region");
}

#[test]
fn scrolling_the_content_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    feed(
        &mut sb,
        &theme,
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
    );

    assert_ne!(sb.scroll_offset(), before.scroll_offset());
    assert_ne!(sb, before, "different rows are on screen");
}

#[test]
fn model_refresh_changes_the_composition() {
    let theme = Theme::dark();
    let mut sb = settled(&theme);
    let before = sb.clone();

    let mut refreshed = model();
    refreshed.tasks[0].detail = alloc::string::String::from("99%");
    sb.set_model(refreshed);

    assert_ne!(sb, before, "a re-derived row shows the new reading");
}
