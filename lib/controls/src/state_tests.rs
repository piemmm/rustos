//! Unit tests for the typed control-state vocabulary.
//!
//! These cover the two pieces of derived behaviour the vocabulary owns — the
//! spec §13 [`ControlDisposition`] taxonomy and the size-toggle "next action" —
//! plus the fail-closed clamping of [`ProgressValue`]. The plain sub-state
//! enums and the [`ControlState`] builders are exercised through those paths.

use crate::state::{
    ActivityState, AuthorityState, ControlDisposition, ControlState, FocusState, PointerState,
    ProgressValue, SizeAction, ValidationState, WindowFurnitureState, WindowSizeState,
};

// --- ControlDisposition (spec §13) --------------------------------------

#[test]
fn idle_control_is_interactive_and_actionable() {
    let s = ControlState::idle();
    assert_eq!(s.disposition(), ControlDisposition::Interactive);
    assert!(s.is_actionable());
}

#[test]
fn disabled_beats_every_other_state() {
    // Even a denied, pending, confirm-needed control reads as disabled first:
    // the object state making it unavailable is the highest-precedence case.
    let s = ControlState::disabled()
        .with_authority(AuthorityState::Denied)
        .with_validation(ValidationState::Pending);
    assert_eq!(s.disposition(), ControlDisposition::DisabledByState);
    assert!(!s.is_actionable());
}

#[test]
fn denied_and_needs_capability_are_authority_denials_not_disabled() {
    // The whole point of spec §13: an authority denial must not collapse into a
    // plain disabled look.
    for authority in [AuthorityState::Denied, AuthorityState::NeedsCapability] {
        let s = ControlState::idle().with_authority(authority);
        assert_eq!(s.disposition(), ControlDisposition::DeniedByAuthority);
        assert!(!s.is_actionable());
    }
}

#[test]
fn failed_closed_is_its_own_disposition() {
    let s = ControlState::idle().with_authority(AuthorityState::FailedClosed);
    assert_eq!(s.disposition(), ControlDisposition::FailedClosed);
    assert!(!s.is_actionable());
}

#[test]
fn needs_confirmation_is_actionable_but_distinct() {
    let s = ControlState::idle().with_authority(AuthorityState::NeedsConfirmation);
    assert_eq!(s.disposition(), ControlDisposition::NeedsConfirmation);
    // A consequential action is still actionable (it dispatches through a
    // confirmation step), unlike a denial.
    assert!(s.is_actionable());
}

#[test]
fn pending_validation_shows_a_pending_check_only_when_allowed() {
    let pending = ControlState::idle().with_validation(ValidationState::Pending);
    assert_eq!(pending.disposition(), ControlDisposition::PendingCheck);
    assert!(!pending.is_actionable());

    // Authority denial outranks a pending value: the user needs to know it is
    // blocked by authority, not merely awaiting a check.
    let denied_and_pending = pending.with_authority(AuthorityState::Denied);
    assert_eq!(
        denied_and_pending.disposition(),
        ControlDisposition::DeniedByAuthority
    );
}

#[test]
fn non_pending_validation_does_not_block_an_allowed_control() {
    for validation in [ValidationState::Valid, ValidationState::Warning] {
        let s = ControlState::idle().with_validation(validation);
        assert_eq!(s.disposition(), ControlDisposition::Interactive);
    }
    // Invalid does not by itself change the disposition taxonomy (a renderer
    // still marks the value invalid via the field); it is not a spec §13 case.
    let invalid = ControlState::idle().with_validation(ValidationState::Invalid);
    assert_eq!(invalid.disposition(), ControlDisposition::Interactive);
}

// --- Builders compose independently -------------------------------------

#[test]
fn builders_set_only_their_field() {
    let s = ControlState::idle()
        .with_focus(FocusState::FOCUSED)
        .with_pointer(PointerState::Pressed)
        .with_activity(ActivityState::Working);
    assert!(s.focus.focused);
    assert_eq!(s.pointer, PointerState::Pressed);
    assert_eq!(s.activity, ActivityState::Working);
    // Untouched fields keep their resting values.
    assert_eq!(s.authority, AuthorityState::Allowed);
    assert_eq!(s.disposition(), ControlDisposition::Interactive);
}

#[test]
fn focus_field_membership_is_orthogonal_to_focus() {
    let f = FocusState {
        focused: false,
        in_focus_field: true,
    };
    assert!(!f.focused);
    assert!(f.in_focus_field);
    assert_eq!(FocusState::default(), FocusState::UNFOCUSED);
}

#[test]
fn control_state_default_is_idle() {
    assert_eq!(ControlState::default(), ControlState::idle());
}

// --- ProgressValue clamps (fail closed) ---------------------------------

#[test]
fn progress_value_clamps_above_full() {
    assert_eq!(ProgressValue::new(5000).permille(), 1000);
    assert!(ProgressValue::new(5000).is_complete());
    assert_eq!(ProgressValue::new(0), ProgressValue::EMPTY);
    assert_eq!(ProgressValue::new(1000), ProgressValue::FULL);
    assert!(!ProgressValue::new(999).is_complete());
}

// --- Window furniture: size-toggle next action (spec §11.22) ------------

#[test]
fn size_toggle_shows_the_next_action() {
    assert_eq!(
        WindowSizeState::Restored.next_size_action(),
        SizeAction::Maximize
    );
    assert_eq!(
        WindowSizeState::Maximized.next_size_action(),
        SizeAction::Restore
    );
}

#[test]
fn furniture_state_delegates_size_action() {
    let restored = WindowFurnitureState {
        size: WindowSizeState::Restored,
        ..WindowFurnitureState::default()
    };
    assert_eq!(restored.size_action(), SizeAction::Maximize);

    let maximized = WindowFurnitureState {
        size: WindowSizeState::Maximized,
        ..WindowFurnitureState::default()
    };
    assert_eq!(maximized.size_action(), SizeAction::Restore);
}

#[test]
fn furniture_state_default_is_inactive_restored_locked() {
    let f = WindowFurnitureState::default();
    assert_eq!(f.activation, crate::state::WindowActivationState::Inactive);
    assert_eq!(f.size, WindowSizeState::Restored);
    assert!(!f.movable);
    assert!(!f.resizable);
}
