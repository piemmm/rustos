//! Unit tests for the Pressure section: one card per flagged cause, its
//! relief actions' postures and verdicts, and the jump to the culprit task.

use tairix_geometry::{Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, AuthorityState, ControlDisposition, ControlRole, ControlState, PressureKind,
    PressureState, RecoveryState,
};

use tairix_controls::testkit::high_contrast;

use super::{PressureAction, PressureCause, PressureControl};
use crate::view::frame::DETAIL_PANE_WIDTH;
use crate::view::system_data::{Reading, Unmeasured};
use crate::view::test_support::{
    bounds, card_body_centre, card_slot, centre, click, font, has_ink, model, moved, PRESS, RELEASE,
};
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
            pressure: PressureState::None,
            activity: ActivityState::Idle,
            recovery: RecoveryState::None,
            group: None,
            ..TaskSummary::default()
        });
    }
    m.pressure.push(PressureCause {
        resource: alloc::string::String::from("CPU"),
        kind: PressureKind::Cpu,
        culprit: alloc::string::String::from("culprit"),
        cause: alloc::string::String::from("busy loop"),
        activity: ActivityState::Working,
        task_index: None,
        amount: Reading::measured("92%"),
        since: Reading::measured("4m"),
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
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let info = sb.list_info(&layout, Scale::ONE, theme);
    let item = info.item_rect(u32::try_from(index).unwrap_or(0));
    let rects = sb.pressure.entries[index]
        .card
        .footer_rects(item, Scale::ONE, theme);
    centre(rects[action])
}

/// The label/value pairs the detail pane's fact list would draw for `cause`.
fn facts_of(
    cause: &PressureCause,
) -> alloc::vec::Vec<(alloc::string::String, alloc::string::String)> {
    super::detail_facts(cause)
        .facts()
        .iter()
        .map(|fact| {
            (
                alloc::string::String::from(fact.label()),
                alloc::string::String::from(fact.value()),
            )
        })
        .collect()
}

#[test]
fn pressure_anatomy_seats_a_detail_pane_beside_the_cards() {
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Pressure);
    let anatomy = sb.active().anatomy();

    assert_eq!(
        anatomy.detail_width, DETAIL_PANE_WIDTH,
        "the cause detail claims the same column every detail pane does"
    );
    assert_eq!(anatomy.sidebar_width, 0);
    assert_eq!(anatomy.header_height, 0);
    assert_eq!(anatomy.impact_width, 0);
    assert_eq!(
        anatomy.rail_width, 0,
        "a cause's relief lives in its own card footer, not an anchored rail"
    );
    assert_eq!(anatomy.footer_height, 0);
    assert_eq!(
        anatomy.primary_row_commands, 0,
        "cards carry their own footer commands, so no row strip is reserved"
    );
}

#[test]
fn the_detail_pane_states_the_selected_causes_four_facts() {
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Pressure);
    let cause = sb.pressure.selected_item().expect("a cause is selected");
    let facts = facts_of(cause);

    // Each fact is the reading the model carried, never a number recovered
    // from the card's prose.
    assert_eq!(
        facts,
        alloc::vec![
            (
                alloc::string::String::from("Resource"),
                cause.resource.clone()
            ),
            (alloc::string::String::from("Pressure"), "92%".into()),
            (alloc::string::String::from("In band"), "4m".into()),
            (alloc::string::String::from("Relief"), "Pause".into()),
        ]
    );
    assert_eq!(
        cause.amount,
        Reading::measured("92%"),
        "the Pressure fact is the model's own amount reading"
    );
    assert_eq!(
        cause.since,
        Reading::measured("4m"),
        "the In band fact is the model's own band-age reading"
    );
}

#[test]
fn an_unmeasured_detail_fact_states_its_reason() {
    let mut m = pressure_model(alloc::vec![relief_action(
        PressureControl::Pause,
        ActionVerdict::Ready,
        true,
    )]);
    m.pressure[0].amount = Reading::Absent(Unmeasured::NotPermitted);
    m.pressure[0].since = Reading::Absent(Unmeasured::Unavailable);
    let mut sb = Switchboard::new(&m);
    sb.select_section(Section::Pressure);
    let facts = facts_of(sb.pressure.selected_item().expect("a cause is selected"));

    let pressure = &facts[1].1;
    let band = &facts[2].1;
    assert!(
        pressure.contains(Unmeasured::NotPermitted.reason()),
        "an unread amount states why it was refused, not a plausible figure: {pressure}"
    );
    assert!(
        band.contains(Unmeasured::Unavailable.reason()),
        "a band age nobody observed states that, not a made-up duration: {band}"
    );
    assert_ne!(
        pressure, band,
        "'not permitted' and 'unavailable' are different statements"
    );
}

#[test]
fn the_relief_fact_names_a_refused_command_with_its_refusal() {
    // A permitted relief is named plainly.
    assert_eq!(
        super::recommended_relief(
            &pressure_model(alloc::vec![relief_action(
                PressureControl::Pause,
                ActionVerdict::Ready,
                true,
            )])
            .pressure[0]
        ),
        "Relieve"
    );

    // A relief the state forbids is still named, with why it cannot be taken.
    let disabled = super::recommended_relief(
        &pressure_model(alloc::vec![relief_action(
            PressureControl::Pause,
            ActionVerdict::DisabledByState,
            true,
        )])
        .pressure[0],
    );
    assert!(disabled.starts_with("Relieve"), "{disabled}");
    assert!(
        disabled.contains("not available in this state"),
        "{disabled}"
    );

    // A relief this session may not use says so, and says it differently.
    let denied = super::recommended_relief(
        &pressure_model(alloc::vec![relief_action(
            PressureControl::Pause,
            ActionVerdict::DeniedByAuthority,
            true,
        )])
        .pressure[0],
    );
    assert!(denied.contains("not permitted"), "{denied}");
    assert_ne!(
        disabled, denied,
        "a state refusal and an authority refusal are different statements"
    );

    // A cause with nothing recommended says there is no relief rather than
    // volunteering one of the other commands.
    let none = super::recommended_relief(
        &pressure_model(alloc::vec![relief_action(
            PressureControl::Pause,
            ActionVerdict::Ready,
            false,
        )])
        .pressure[0],
    );
    assert_eq!(none, super::NO_RELIEF);
}

#[test]
fn the_pressure_selection_survives_a_refresh_and_drops_when_the_cause_eases() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Pressure);
    let mut surface = Surface::new(b.width, b.height).expect("surface");

    // Move the cursor onto the Memory cause: the card the reader is on is the
    // cause the detail describes.
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    sb.on_key(Key::Named(NamedKey::Down));
    assert_eq!(sb.pressure.selected, Some(PressureKind::Memory));

    // A refresh that reorders the causes keeps the reader on Memory, wherever
    // it has moved to.
    let mut moved_model = model();
    moved_model.pressure.reverse();
    sb.set_model(&moved_model);
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.pressure.selected, Some(PressureKind::Memory));
    assert_eq!(
        sb.pressure.selected_item().map(|item| item.kind),
        Some(PressureKind::Memory)
    );

    // A refresh in which memory has eased drops the selection to what is left,
    // rather than describing a cause that is no longer flagged.
    let mut eased = model();
    eased
        .pressure
        .retain(|cause| cause.kind != PressureKind::Memory);
    sb.set_model(&eased);
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_ne!(sb.pressure.selected, Some(PressureKind::Memory));
    assert_eq!(sb.pressure.selected, Some(PressureKind::Cpu));

    // With nothing flagged at all there is no selection to describe.
    sb.set_model(&SwitchboardModel::new("Switchboard"));
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.pressure.selected, None);
    assert!(sb.pressure.selected_item().is_none());
}

#[test]
fn the_keyboard_refuses_a_disabled_or_denied_relief_command() {
    for verdict in [
        ActionVerdict::DisabledByState,
        ActionVerdict::DeniedByAuthority,
    ] {
        let mut sb = Switchboard::new(&pressure_model(alloc::vec![relief_action(
            PressureControl::Pause,
            verdict,
            false,
        )]));
        sb.select_section(Section::Pressure);
        assert_eq!(
            sb.on_key(Key::Named(NamedKey::Enter)),
            None,
            "{verdict:?} must refuse the keyboard exactly as it refuses the pointer"
        );
    }
}

#[test]
fn both_themes_and_the_heavier_contrast_path_render() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut sb = Switchboard::new(&model());
        sb.select_section(Section::Pressure);
        let b = bounds();
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        assert!(
            has_ink(&surface, b),
            "the Pressure screen must paint under every theme"
        );
    }
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
        let mut sb = Switchboard::new(&m);
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
        let mut sb = Switchboard::new(&m);
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
    let sb = Switchboard::new(&model());
    let footer = sb.pressure.entries[0].card.footer();
    assert_eq!(footer[0].role(), ControlRole::Recommended);
    assert_eq!(footer[1].role(), ControlRole::Neutral);
}

#[test]
fn ready_relief_action_activates() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
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
    let mut sb = Switchboard::new(&pressure_model(alloc::vec![relief_action(
        PressureControl::Pause,
        ActionVerdict::DisabledByState,
        false,
    )]));
    sb.select_section(Section::Pressure);
    assert_eq!(
        sb.pressure.entries[0].card.footer()[0]
            .state()
            .disposition(),
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
    let mut sb = Switchboard::new(&pressure_model(alloc::vec![relief_action(
        PressureControl::Pause,
        ActionVerdict::DeniedByAuthority,
        false,
    )]));
    sb.select_section(Section::Pressure);
    assert_eq!(
        sb.pressure.entries[0].card.footer()[0]
            .state()
            .disposition(),
        ControlDisposition::DeniedByAuthority
    );
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a denied relief action must not activate"
    );
}

/// A point in the body of the pressure card for the cause at `index`.
fn pressure_body_centre(sb: &Switchboard, b: Rect, theme: &Theme, index: usize) -> (i32, i32) {
    let item = card_slot(sb, b, theme, index);
    let footer = sb.pressure.entries[index]
        .card
        .footer_rects(item, Scale::ONE, theme);
    card_body_centre(item, &footer)
}

#[test]
fn a_press_on_a_cause_card_body_selects_that_cause() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Pressure);
    assert_eq!(
        sb.pressure.selected,
        Some(PressureKind::Cpu),
        "the first cause is the one open to begin with"
    );

    let (x, y) = pressure_body_centre(&sb, b, &theme, 1);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);

    // Pressing a card opens its detail: the cause it is about becomes the
    // selected one, and the pane describes that cause.
    assert_eq!(sb.pressure.selected, Some(PressureKind::Memory));
    let cause = sb.pressure.selected_item().expect("a cause is selected");
    assert_eq!(cause.kind, PressureKind::Memory);
    assert_eq!(
        facts_of(cause)[0],
        (
            alloc::string::String::from("Resource"),
            cause.resource.clone()
        ),
        "the detail pane states the pressed cause's own resource"
    );
    assert!(
        actions.is_empty(),
        "a body press opens the detail; it is not a command: {actions:?}"
    );
}

#[test]
fn a_press_on_a_cause_card_footer_selects_it_and_resolves_the_command() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Pressure);
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 1, 0);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(
        actions.contains(&SwitchboardAction::Pressure {
            index: 1,
            control: PressureControl::Pause
        }),
        "a footer press still resolves its own command: {actions:?}"
    );
    assert_eq!(
        sb.pressure.selected,
        Some(PressureKind::Memory),
        "the card whose command fired is also the cause now open"
    );
}

#[test]
fn a_press_on_a_disabled_or_denied_cause_card_selects_nothing() {
    let theme = Theme::dark();
    let b = bounds();
    for state in [
        ControlState::disabled(),
        ControlState::idle().with_authority(AuthorityState::Denied),
    ] {
        let mut sb = Switchboard::new(&model());
        sb.select_section(Section::Pressure);
        sb.pressure.entries[1].card.set_state(state);
        let (x, y) = pressure_body_centre(&sb, b, &theme, 1);
        let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
        assert_eq!(
            sb.pressure.selected,
            Some(PressureKind::Cpu),
            "a card that is not actionable must not become the open cause"
        );
        assert!(actions.is_empty(), "{actions:?}");
    }
}

#[test]
fn show_tasks_lands_on_the_culprit_task() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
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
        sb.active().content_focus(),
        sb.tasks.focus_index_for_row(1),
        "the culprit's task row takes the focus"
    );
}

#[test]
fn show_tasks_clamps_a_missing_or_stale_task_index() {
    let theme = Theme::dark();
    let b = bounds();
    // No task index: the focus falls to the first task.
    let mut sb = Switchboard::new(&pressure_model(alloc::vec![relief_action(
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
    assert_eq!(sb.active().content_focus(), sb.tasks.focus_index_for_row(0));

    // A stale index past the list end clamps to the last task.
    let mut m = pressure_model(alloc::vec![relief_action(
        PressureControl::ShowTasks,
        ActionVerdict::Ready,
        false,
    )]);
    m.pressure[0].task_index = Some(999);
    let mut sb = Switchboard::new(&m);
    sb.select_section(Section::Pressure);
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::SectionChanged {
        section: Section::Tasks
    }));
    assert_eq!(
        sb.active().content_focus(),
        sb.tasks.focus_index_for_row(2),
        "a stale task index clamps into the shown list"
    );
}

#[test]
fn empty_pressure_section_has_nothing_to_activate() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&SwitchboardModel::new("Switchboard"));
    sb.select_section(Section::Pressure);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
}

#[test]
fn keyboard_reaches_every_pressure_footer() {
    let mut sb = Switchboard::new(&model());
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
    assert_eq!(
        sb.active().content_focus(),
        sb.tasks.focus_index_for_row(0),
        "cause 0 names task 0"
    );
}

#[test]
fn set_model_cannot_complete_a_press_begun_on_a_replaced_pressure_card() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Pressure);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let (x, y) = pressure_footer_centre(&sb, b, &theme, 0, 0);

    assert_eq!(
        sb.on_pointer(&moved(x, y), b, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(sb.on_pointer(&PRESS, b, Scale::ONE, &theme, font()), None);
    sb.set_model(&model());

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
