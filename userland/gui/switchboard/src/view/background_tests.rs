//! Unit tests for the Background section: its job cards' footer actions.

use tairix_input::{Key, NamedKey};

use super::JobControl;
use crate::view::test_support::model;
use crate::view::{Section, Switchboard, SwitchboardAction};

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
