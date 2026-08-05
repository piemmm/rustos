//! Unit tests for the Recovery section: its rows' restart and force
//! actions and their confirmation posture.

use tairix_geometry::Scale;
use tairix_input::{Key, NamedKey};
use tairix_theme::Theme;

use tairix_controls::{ControlDisposition, ControlRole, RecoveryState};

use super::{RecoveryControl, RecoveryItem};
use crate::view::test_support::{bounds, centre, click, font, model};
use crate::view::{Section, Switchboard, SwitchboardAction, SwitchboardModel};

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
