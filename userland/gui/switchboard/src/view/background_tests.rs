//! Unit tests for the Background section: its job cards, the selected
//! job's commands, the throttle switch, and what the section says when
//! nothing registers a job.

use tairix_controls::testkit::high_contrast;
use tairix_controls::{
    ActivityState, AuthorityState, ControlDisposition, ControlState, ProgressValue,
};
use tairix_geometry::Scale;
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use super::{JobControl, JobSummary};
use crate::view::test_support::{
    bounds, card_body_centre, card_slot, centre, click, font, has_ink, key, model,
};
use crate::view::{
    resolve_section_frame, Section, SectionView, Switchboard, SwitchboardAction, SwitchboardModel,
    UNMEASURED_READING,
};

/// A model carrying exactly the named jobs, each able to pause and cancel.
fn jobs(names: &[&str]) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for name in names {
        m.jobs.push(JobSummary {
            name: alloc::string::String::from(*name),
            detail: alloc::string::String::from("copying"),
            activity: ActivityState::Progress(ProgressValue::new(400)),
            can_pause: true,
            can_cancel: true,
        });
    }
    m
}

/// A Switchboard showing Background with exactly the named jobs.
fn shown(names: &[&str]) -> Switchboard {
    let mut sb = Switchboard::new(&jobs(names));
    sb.select_section(Section::Jobs);
    sb
}

/// Paint the whole screen once, so a render test exercises the real path.
fn paint(sb: &mut Switchboard, theme: &Theme) -> Surface {
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, theme, font());
    surface
}

#[test]
fn keyboard_activates_a_job_footer() {
    // One job, because walking the cursor down a longer list would select
    // the job it lands on: the commands always name the selected job.
    let mut sb = shown(&["copy"]);
    // The commands moved from the card's own footer into the anchored rail,
    // so the cursor now walks the cards first and the commands after them.
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    let action = key(&mut sb, Key::Named(NamedKey::Enter));
    assert_eq!(
        action,
        Some(SwitchboardAction::Job {
            index: 0,
            control: JobControl::Pause
        })
    );
}

#[test]
fn keyboard_reaches_the_job_cancel_footer() {
    let mut sb = shown(&["copy"]);
    // Cancel is the rail's second command, so it is one stop below Pause
    // rather than one action to its right.
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(
        key(&mut sb, Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Job {
            index: 0,
            control: JobControl::Cancel
        })
    );
}

/// A point in the body of the job card at `index`.
///
/// A job card carries no footer buttons of its own — its commands live in
/// the anchored rail — so every point on it is body; the shared helper still
/// checks that against the card's own (empty) footer layout rather than
/// assuming it.
fn job_body_centre(sb: &Switchboard, theme: &Theme, index: usize) -> (i32, i32) {
    let item = card_slot(sb, bounds(), theme, index);
    let footer = sb.jobs.cards[index].footer_rects(item, Scale::ONE, theme);
    card_body_centre(item, &footer)
}

/// The centre of the job rail's command at `command`.
fn job_rail_centre(sb: &Switchboard, theme: &Theme, command: usize) -> (i32, i32) {
    let layout = sb.compute_layout(bounds(), Scale::ONE, theme);
    let frame = resolve_section_frame(layout.content, sb.jobs.anatomy(), Scale::ONE, theme);
    let content = sb
        .jobs
        .rail_content(&frame, Scale::ONE, theme)
        .expect("the default window seats the job rail");
    let rect = sb
        .jobs
        .rail
        .item_rect(content, command, Scale::ONE, theme)
        .expect("the rail seats its commands");
    centre(rect)
}

#[test]
fn a_press_on_a_job_card_body_selects_that_job() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = shown(&["copy", "index"]);
    assert_eq!(
        sb.jobs.selected.as_deref(),
        Some("copy"),
        "the first job is the one open to begin with"
    );

    let (x, y) = job_body_centre(&sb, &theme, 1);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);

    // Pressing a card opens its detail: the job it is about becomes the
    // selected one, and the pane describes that job.
    assert_eq!(sb.jobs.selected.as_deref(), Some("index"));
    assert_eq!(sb.jobs.selected_index(), Some(1));
    assert_eq!(
        sb.jobs.selected_item().map(|item| item.name.as_str()),
        Some("index")
    );
    assert!(
        actions.is_empty(),
        "a body press opens the detail; it is not a command: {actions:?}"
    );
}

#[test]
fn a_press_on_the_rail_resolves_the_command_for_the_pressed_job() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = shown(&["copy", "index"]);
    let (x, y) = job_body_centre(&sb, &theme, 1);
    assert!(click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty());

    // The commands live in the rail rather than on the card, so the press
    // that opened the job and the command that acts on it must agree about
    // which job is meant.
    let (rx, ry) = job_rail_centre(&sb, &theme, 0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, rx, ry).contains(&SwitchboardAction::Job {
            index: 1,
            control: JobControl::Pause
        })
    );
}

#[test]
fn a_press_on_a_disabled_or_denied_job_card_selects_nothing() {
    let theme = Theme::dark();
    let b = bounds();
    for state in [
        ControlState::disabled(),
        ControlState::idle().with_authority(AuthorityState::Denied),
    ] {
        let mut sb = shown(&["copy", "index"]);
        sb.jobs.cards[1].set_state(state);
        let (x, y) = job_body_centre(&sb, &theme, 1);
        let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
        assert_eq!(
            sb.jobs.selected.as_deref(),
            Some("copy"),
            "a card that is not actionable must not become the open job"
        );
        assert!(actions.is_empty(), "{actions:?}");
    }
}

// --- The section frame -------------------------------------------------

#[test]
fn the_section_asks_for_a_detail_pane_a_rail_and_a_footer() {
    let sb = shown(&["copy"]);
    let anatomy = sb.jobs.anatomy();
    assert!(anatomy.detail_width > 0, "a job's detail needs a pane");
    assert!(anatomy.rail_width > 0, "a job's commands need a rail");
    assert!(
        anatomy.footer_height > 0,
        "the throttle switch needs a footer band"
    );
    assert_eq!(
        anatomy.impact_width, 0,
        "a job reports progress, not a resource cost"
    );
}

// --- The absence -------------------------------------------------------

#[test]
fn an_empty_list_says_no_interface_rather_than_showing_nothing() {
    let sb = Switchboard::new(&SwitchboardModel::new("Switchboard"));
    let line = super::jobs_absence();
    assert!(line.contains(UNMEASURED_READING), "{line}");
    assert!(line.contains("no interface"), "{line}");
    assert!(sb.jobs.items.is_empty());
}

#[test]
fn the_absence_says_no_registry_exists() {
    assert!(
        super::NO_REGISTRY.contains("registry"),
        "the reader must be told what is missing, not merely that something is"
    );
}

#[test]
fn an_empty_list_offers_no_cursor_stops_and_no_commands() {
    let sb = Switchboard::new(&SwitchboardModel::new("Switchboard"));
    assert_eq!(sb.jobs.focus_span(), 0);
    assert!(sb.jobs.rail.is_empty());
    assert_eq!(sb.jobs.selected, None);
}

#[test]
fn the_auto_throttle_switch_is_refused_while_nothing_can_act_on_it() {
    let sb = Switchboard::new(&SwitchboardModel::new("Switchboard"));
    assert_eq!(
        sb.jobs.throttle.state().disposition(),
        ControlDisposition::DisabledByState,
        "a switch nothing can act on is disabled, not denied by authority"
    );
    assert!(!sb.jobs.throttle.is_on());
}

// --- Selection ---------------------------------------------------------

#[test]
fn selection_follows_the_job_when_the_list_reorders() {
    let mut sb = shown(&["copy", "index"]);
    sb.set_model(&jobs(&["copy", "index"]));
    assert_eq!(sb.jobs.selected.as_deref(), Some("copy"));
    sb.set_model(&jobs(&["index", "copy"]));
    assert_eq!(
        sb.jobs.selected.as_deref(),
        Some("copy"),
        "a refresh that reorders the list must not re-point the selection"
    );
    assert_eq!(sb.jobs.selected_index(), Some(1));
}

#[test]
fn selection_drops_to_the_first_job_when_the_selected_one_finishes() {
    let mut sb = shown(&["copy", "index"]);
    sb.set_model(&jobs(&["index"]));
    assert_eq!(sb.jobs.selected.as_deref(), Some("index"));
}

// --- Painting ----------------------------------------------------------

#[test]
fn both_themes_and_the_heavier_contrast_path_render_the_absence() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut sb = Switchboard::new(&SwitchboardModel::new("Switchboard"));
        sb.select_section(Section::Jobs);
        let surface = paint(&mut sb, &theme);
        assert!(
            has_ink(&surface, bounds()),
            "the Background screen must say why it is empty under every theme"
        );
    }
}

#[test]
fn a_populated_list_paints_its_detail_and_rail() {
    let theme = Theme::dark();
    let mut sb = shown(&["copy"]);
    let surface = paint(&mut sb, &theme);
    assert!(has_ink(&surface, bounds()));
}

#[test]
fn the_fixture_model_still_reaches_the_background_section() {
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Jobs);
    assert_eq!(sb.section(), Section::Jobs);
}
