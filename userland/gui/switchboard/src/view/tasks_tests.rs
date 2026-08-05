//! Unit tests for the Tasks section: its rows' actions, and the group
//! popup that files a task into an activity.

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_theme::Theme;

use tairix_controls::{ActivityState, ControlDisposition, PressureState, RecoveryState};

use super::TaskSummary;
use crate::view::test_support::{bounds, centre, click, font, model};
use crate::view::{Section, Switchboard, SwitchboardAction, SwitchboardModel};

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
    // Tall enough to hold the whole popup below its anchor: the flip-upward
    // path is a different case, covered by its own test.
    let b = Rect::new(0, 0, 600, 560);
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
