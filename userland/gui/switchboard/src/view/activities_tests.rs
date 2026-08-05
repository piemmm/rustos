//! Unit tests for the Activities section: its header rows' set actions,
//! display-only member rows, and inline rename.

use tairix_geometry::{Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::testkit::high_contrast;
use tairix_controls::{ControlDisposition, ControlRole, FactList};

use super::{ActivityControl, ActivityRow};
use crate::view::frame::DETAIL_PANE_WIDTH;
use crate::view::system_data::{reading_text, Reading, Unmeasured};
use crate::view::test_support::{
    bounds, centre, click, font, has_ink, model, moved, PRESS, RELEASE,
};
use crate::view::{Section, Switchboard, SwitchboardAction, SwitchboardModel};

/// The centre of the activity header button at `button` for the header in
/// flattened row `slot`, in window coordinates.
fn activity_button_centre(
    sb: &Switchboard,
    b: Rect,
    theme: &Theme,
    slot: u32,
    button: usize,
) -> (i32, i32) {
    let layout = sb.compute_layout(b, Scale::ONE, theme);
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
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Activities);
    assert_eq!(sb.activities.row_at(0), Some(ActivityRow::Header(0)));
    assert_eq!(sb.activities.row_at(1), Some(ActivityRow::Member(0, 0)));
    assert_eq!(sb.activities.row_at(2), Some(ActivityRow::Member(0, 1)));
    assert_eq!(sb.activities.row_at(3), Some(ActivityRow::Header(1)));
    assert_eq!(sb.activities.row_at(17), Some(ActivityRow::Member(5, 1)));
    assert_eq!(sb.activities.row_at(18), None);

    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
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

/// The label/value pairs a fact list would draw.
fn pairs(facts: &FactList) -> alloc::vec::Vec<(alloc::string::String, alloc::string::String)> {
    facts
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
fn activities_anatomy_seats_a_detail_pane_beside_the_rows() {
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Activities);
    let anatomy = sb.active().anatomy();

    assert_eq!(
        anatomy.detail_width, DETAIL_PANE_WIDTH,
        "the group detail claims the same column every detail pane does"
    );
    assert_eq!(anatomy.sidebar_width, 0);
    assert_eq!(anatomy.header_height, 0);
    assert_eq!(anatomy.impact_width, 0);
    assert_eq!(
        anatomy.rail_width, 0,
        "an activity's commands live in its own header row, not an anchored rail"
    );
    assert_eq!(anatomy.footer_height, 0);
    assert_eq!(
        anatomy.primary_row_commands, 4,
        "a header row's four commands are the width the primary column may not fall below"
    );
}

#[test]
fn the_detail_pane_states_the_groups_combined_readings() {
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Activities);
    let item = sb
        .activities
        .selected_item()
        .expect("an activity is selected");

    // Each total is the model's own combined reading, in the fixed order a
    // reader learns once.
    assert_eq!(
        pairs(&super::combined_facts(item)),
        alloc::vec![
            (alloc::string::String::from("CPU"), "12%".into()),
            (alloc::string::String::from("Memory"), "96.0 MiB".into()),
            (alloc::string::String::from("Disk"), "0 B/s".into()),
            (
                alloc::string::String::from("Network"),
                reading_text(&Reading::Absent(Unmeasured::NoInterface))
            ),
        ]
    );
    assert_eq!(
        item.network,
        Reading::Absent(Unmeasured::NoInterface),
        "there is no per-task network accounting to total, and the pane says so"
    );
}

#[test]
fn a_combined_reading_nobody_could_total_states_its_reason() {
    let mut m = model();
    m.activities[0].cpu = Reading::Absent(Unmeasured::Unavailable);
    m.activities[0].memory = Reading::Absent(Unmeasured::NotPermitted);
    let mut sb = Switchboard::new(&m);
    sb.select_section(Section::Activities);
    let facts = pairs(&super::combined_facts(
        sb.activities
            .selected_item()
            .expect("an activity is selected"),
    ));

    assert!(
        facts[0].1.contains(Unmeasured::Unavailable.reason()),
        "a total no member reading supports says so: {}",
        facts[0].1
    );
    assert!(
        facts[1].1.contains(Unmeasured::NotPermitted.reason()),
        "a refused total says it was refused: {}",
        facts[1].1
    );
    assert_ne!(
        facts[0].1, facts[1].1,
        "'unavailable' and 'not permitted' are different statements"
    );
}

#[test]
fn the_detail_pane_lists_every_member_with_its_own_reading() {
    let mut m = model();
    // One member that is not running at all, and one that is running but whose
    // own reading this sample never read.
    m.activities[0].members[0].joined = false;
    m.activities[0].members[1].detail = alloc::string::String::new();
    let mut sb = Switchboard::new(&m);
    sb.select_section(Section::Activities);
    let item = sb
        .activities
        .selected_item()
        .expect("an activity is selected");
    let facts = super::member_facts(item).expect("a group with members lists them");
    let rows = pairs(&facts);

    assert_eq!(rows.len(), item.members.len(), "one row per member, always");
    assert_eq!(rows[0].0, item.members[0].name);
    assert_eq!(
        rows[0].1,
        super::MEMBER_NOT_RUNNING,
        "a member that is not running is not a zero reading"
    );
    assert!(
        rows[1].1.contains(Unmeasured::Unavailable.reason()),
        "a running member with no reading states that, not a plausible figure: {}",
        rows[1].1
    );

    // A group with no members at all has nothing to list, and says so rather
    // than drawing an empty plate.
    let mut empty = model();
    empty.activities[0].members.clear();
    let mut sb = Switchboard::new(&empty);
    sb.select_section(Section::Activities);
    assert!(super::member_facts(
        sb.activities
            .selected_item()
            .expect("an activity is selected")
    )
    .is_none());
}

#[test]
fn the_activity_selection_survives_a_refresh_and_drops_when_its_group_closes() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Activities);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());

    // Click the second group's own header row: the group whose commands the
    // reader reached is the group the detail describes.
    let second = model().activities[1].id;
    let (x, y) = activity_button_centre(&sb, b, &theme, 3, 0);
    click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert_eq!(sb.activities.selected, Some(second));

    // A refresh that reorders the groups keeps the reader on the same one,
    // re-found by its stable id rather than by where it used to sit.
    let mut moved_model = model();
    moved_model.activities.reverse();
    sb.set_model(&moved_model);
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.activities.selected, Some(second));
    assert_eq!(
        sb.activities.selected_item().map(|item| item.id),
        Some(second)
    );

    // A refresh in which that group has closed drops the selection to what is
    // left rather than describing a group that no longer exists.
    let mut closed = model();
    closed.activities.retain(|item| item.id != second);
    sb.set_model(&closed);
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_ne!(sb.activities.selected, Some(second));
    assert_eq!(sb.activities.selected, Some(closed.activities[0].id));

    // With no groups at all there is nothing to select.
    sb.set_model(&SwitchboardModel::new("Switchboard"));
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    assert_eq!(sb.activities.selected, None);
    assert!(sb.activities.selected_item().is_none());
}

#[test]
fn both_themes_and_the_heavier_contrast_path_render() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut sb = Switchboard::new(&model());
        sb.select_section(Section::Activities);
        let b = bounds();
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        assert!(
            has_ink(&surface, b),
            "the Activities screen must paint under every theme"
        );
    }
}

#[test]
fn activity_switch_and_close_activate_by_pointer() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
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
    let mut sb = Switchboard::new(&model());
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
    let sb = Switchboard::new(&model());
    assert_eq!(
        sb.activities.entries[0].close.role(),
        ControlRole::Destructive
    );
    assert_eq!(
        sb.activities.entries[0].close.state().disposition(),
        ControlDisposition::NeedsConfirmation
    );
}

#[test]
fn uncontrollable_activity_fails_closed() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.activities[0].can_control = false;
    let mut sb = Switchboard::new(&m);
    sb.select_section(Section::Activities);
    assert_eq!(
        sb.activities.entries[0].pause_resume.state().disposition(),
        ControlDisposition::DeniedByAuthority
    );
    assert_eq!(
        sb.activities.entries[0].close.state().disposition(),
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
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Activities);
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let (x, y) = centre(info.item_rect(1));
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a member row is display-only"
    );
}

#[test]
fn keyboard_reaches_every_activity_header_button() {
    let mut sb = Switchboard::new(&model());
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
    assert!(sb.activities.rename.is_some());
    assert_eq!(sb.on_key(Key::Named(NamedKey::Escape)), None);
    assert!(sb.activities.rename.is_none());
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

/// Begin an inline rename of the first activity's header by pointer.
fn begin_first_rename(sb: &mut Switchboard, b: Rect, theme: &Theme) {
    sb.select_section(Section::Activities);
    let (x, y) = activity_button_centre(sb, b, theme, 0, 2);
    assert!(
        click(sb, b, Scale::ONE, theme, x, y).is_empty(),
        "beginning a rename emits nothing"
    );
    assert!(sb.activities.rename.is_some(), "the rename must begin");
}

#[test]
fn rename_commits_by_enter_and_reports_the_name() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(
        sb.activities.rename.as_ref().map(|e| e.field.text()),
        Some("activity 0"),
        "the field pre-fills with the current name"
    );
    assert_eq!(sb.on_key(Key::Char('!')), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::ActivityRenamed { index: 0 })
    );
    assert_eq!(sb.submitted_activity_name(), Some("activity 0!"));
    assert_eq!(sb.activities.entries[0].name, "activity 0!");
    assert!(sb.activities.rename.is_none());
}

#[test]
fn rename_escape_cancels_without_emitting() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(sb.on_key(Key::Char('!')), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Escape)), None);
    assert!(sb.activities.rename.is_none());
    assert_eq!(sb.submitted_activity_name(), None);
    assert_eq!(
        sb.activities.entries[0].name, "activity 0",
        "a cancel changes nothing"
    );
}

#[test]
fn rename_survives_a_refresh_that_moves_its_activity() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(sb.on_key(Key::Char('!')), None);

    // The refresh reorders the list: id 100 moves from index 0 to index 5.
    let mut m = model();
    m.activities.rotate_left(1);
    sb.set_model(&m);

    let edit = sb
        .activities
        .rename
        .as_ref()
        .expect("the edit survives its activity");
    assert_eq!(edit.index, 5, "the edit re-locates its activity by id");
    assert_eq!(edit.field.text(), "activity 0!", "the typed text survives");
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::ActivityRenamed { index: 5 })
    );
    assert_eq!(sb.submitted_activity_name(), Some("activity 0!"));
    assert_eq!(sb.activities.entries[5].name, "activity 0!");
}

#[test]
fn rename_drops_when_its_activity_vanishes() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    begin_first_rename(&mut sb, b, &theme);

    let mut m = model();
    m.activities.remove(0);
    sb.set_model(&m);

    assert!(
        sb.activities.rename.is_none(),
        "an edit never re-attaches to a different activity"
    );
    assert_eq!(sb.submitted_activity_name(), None);
}

#[test]
fn submitted_name_clears_on_the_next_refresh() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    begin_first_rename(&mut sb, b, &theme);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::ActivityRenamed { index: 0 })
    );
    assert!(sb.submitted_activity_name().is_some());
    sb.set_model(&model());
    assert_eq!(
        sb.submitted_activity_name(),
        None,
        "a committed name is read before the next sample"
    );
}

#[test]
fn set_model_cannot_complete_a_press_begun_on_a_replaced_activity_row() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Activities);
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());
    let (x, y) = activity_button_centre(&sb, b, &theme, 0, 0);

    assert_eq!(
        sb.on_pointer(&moved(x, y), b, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(sb.on_pointer(&PRESS, b, Scale::ONE, &theme, font()), None);
    sb.set_model(&model());

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
