//! Unit tests for the Pressure section: one card per flagged cause, its
//! relief actions' postures and verdicts, and the jump to the culprit task.

use tairix_geometry::{Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, ControlDisposition, ControlRole, PressureKind, PressureState, RecoveryState,
};

use super::{PressureAction, PressureCause, PressureControl};
use crate::view::test_support::{bounds, centre, click, font, model, moved, PRESS, RELEASE};
use crate::view::{
    ActionVerdict, Section, Switchboard, SwitchboardAction, SwitchboardModel, TaskSummary,
};

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
