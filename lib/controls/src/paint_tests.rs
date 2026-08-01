//! Unit tests for the shared Reactive Alloy plate colour recipe.
//!
//! Every drawn family resolves its plate, rim, and label through
//! [`resolve_frame`], so the design boards' colour invariants are pinned here
//! once rather than re-asserted per family: a coloured plate always carries a
//! rim of the same colour, a role states itself on the edge and the label
//! before it states itself on the plate, and a press colours a control rather
//! than merely darkening it.

use tairix_raster::Color;
use tairix_theme::{Rgba, Theme};

use crate::paint::{resolve_frame, FrameColors};
use crate::state::{
    AuthorityState, ControlRole, ControlState, FocusState, PointerState, ValidationState,
};
use crate::testkit::high_contrast;

fn pointer(pointer: PointerState) -> ControlState {
    ControlState {
        pointer,
        ..ControlState::idle()
    }
}

/// A resting control that belongs to a highlighted Focus Field but does not
/// itself hold the keyboard.
fn field_member() -> ControlState {
    ControlState {
        focus: FocusState {
            focused: false,
            in_focus_field: true,
        },
        ..ControlState::idle()
    }
}

fn rgb(rgba: Rgba) -> Color {
    Color::from(rgba)
}

/// A resolved frame's four facts as a comparable tuple. `FrameColors` is a
/// paint result rather than a value type, so it carries no derives of its
/// own and a test that wants "these two draw identically" spells it out.
fn parts(frame: &FrameColors) -> (Color, Color, Color, bool) {
    (frame.plate, frame.rim, frame.label, frame.focused)
}

/// A filled plate while pressed, mirroring the recipe's darkening.
fn pressed_fill(rgba: Rgba) -> Color {
    Color::from(rgba.mix(Rgba::rgb(0, 0, 0), 220))
}

/// A filled plate while hovered, mirroring the recipe's lightening.
fn hovered_fill(rgba: Rgba) -> Color {
    Color::from(rgba.mix(Rgba::rgb(255, 255, 255), 90))
}

#[test]
fn primary_is_filled_with_the_accent_and_its_rim_matches() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let frame = resolve_frame(&theme, ControlRole::Primary, pointer(PointerState::None));
    assert_eq!(frame.plate, rgb(palette.accent));
    assert_eq!(
        frame.rim, frame.plate,
        "a coloured plate has a matching rim"
    );
    assert_eq!(frame.label, rgb(palette.on_accent));
}

#[test]
fn recovery_is_filled_with_the_recovery_role() {
    let theme = Theme::dark();
    let frame = resolve_frame(&theme, ControlRole::Recovery, pointer(PointerState::None));
    assert_eq!(frame.plate, rgb(theme.palette().recovery));
    assert_eq!(frame.rim, frame.plate);
    assert_eq!(frame.label, rgb(theme.palette().on_accent));
}

#[test]
fn recommended_is_outlined_in_the_accent_over_the_raised_plate() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let frame = resolve_frame(
        &theme,
        ControlRole::Recommended,
        pointer(PointerState::None),
    );
    assert_eq!(frame.plate, rgb(palette.surface_raised));
    assert_eq!(frame.rim, rgb(palette.accent));
    assert_eq!(frame.label, frame.rim, "an outlined role colours its label");
}

#[test]
fn destructive_is_outlined_in_danger_not_filled() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let frame = resolve_frame(
        &theme,
        ControlRole::Destructive,
        pointer(PointerState::None),
    );
    assert_eq!(frame.plate, rgb(palette.surface_raised));
    assert_eq!(frame.rim, rgb(palette.danger));
    assert_eq!(frame.label, rgb(palette.danger));
}

#[test]
fn neutral_navigation_and_system_stay_quiet() {
    let theme = Theme::dark();
    let palette = theme.palette();
    for role in [
        ControlRole::Neutral,
        ControlRole::Navigation,
        ControlRole::System,
    ] {
        let frame = resolve_frame(&theme, role, pointer(PointerState::None));
        assert_eq!(frame.plate, rgb(palette.surface_raised), "{role:?}");
        assert_eq!(frame.rim, rgb(palette.rim), "{role:?}");
        assert_eq!(frame.label, rgb(palette.on_surface), "{role:?}");
    }
}

#[test]
fn press_promotes_an_outlined_control_to_a_filled_one() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let frame = resolve_frame(
        &theme,
        ControlRole::Destructive,
        pointer(PointerState::Pressed),
    );
    assert_eq!(frame.plate, pressed_fill(palette.danger));
    assert_eq!(frame.rim, frame.plate, "the promoted edge matches the fill");
    assert_eq!(frame.label, rgb(palette.on_accent));
}

#[test]
fn press_darkens_and_hover_lightens_a_filled_plate() {
    let theme = Theme::dark();
    let accent = theme.palette().accent;
    let rest = resolve_frame(&theme, ControlRole::Primary, pointer(PointerState::None));
    let hover = resolve_frame(&theme, ControlRole::Primary, pointer(PointerState::Hover));
    let press = resolve_frame(&theme, ControlRole::Primary, pointer(PointerState::Pressed));
    assert_eq!(hover.plate, hovered_fill(accent));
    assert_eq!(press.plate, pressed_fill(accent));
    assert_ne!(rest.plate, hover.plate);
    assert_ne!(rest.plate, press.plate);
    assert_eq!(hover.rim, hover.plate);
    assert_eq!(press.rim, press.plate);
}

#[test]
fn press_colours_a_quiet_control_edge_and_label() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let frame = resolve_frame(&theme, ControlRole::Neutral, pointer(PointerState::Pressed));
    assert_eq!(frame.plate, rgb(palette.surface_pressed));
    assert_eq!(frame.rim, rgb(palette.rim_active));
    assert_eq!(frame.label, rgb(palette.rim_active));
}

#[test]
fn hover_lifts_a_quiet_rim_without_colouring_its_label() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let frame = resolve_frame(&theme, ControlRole::Neutral, pointer(PointerState::Hover));
    assert_eq!(frame.plate, rgb(palette.surface_raised));
    assert_eq!(frame.rim, rgb(palette.rim_active));
    assert_eq!(frame.label, rgb(palette.on_surface));
}

#[test]
fn focus_lifts_a_quiet_rim_and_is_reported_to_the_renderer() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut state = ControlState::idle();
    state.focus.focused = true;
    let frame = resolve_frame(&theme, ControlRole::Neutral, state);
    assert_eq!(frame.rim, rgb(palette.rim_active));
    assert!(frame.focused);
}

#[test]
fn focus_field_membership_lifts_a_members_rim_without_giving_it_the_ring() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let rest = resolve_frame(&theme, ControlRole::Neutral, ControlState::idle());
    let member = resolve_frame(&theme, ControlRole::Neutral, field_member());

    assert_ne!(
        member.rim, rest.rim,
        "a field member states its membership on the edge"
    );
    assert_ne!(
        member.rim,
        rgb(palette.rim_active),
        "a partial lift, so the member never matches the focused control"
    );
    assert_eq!(
        member.plate, rest.plate,
        "membership is an edge state, not a plate state"
    );
    assert_eq!(member.label, rest.label);
    assert!(!member.focused, "a member draws no focus ring");
}

#[test]
fn focus_wins_over_field_membership_on_the_same_control() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let state = ControlState {
        focus: FocusState {
            focused: true,
            in_focus_field: true,
        },
        ..ControlState::idle()
    };
    let frame = resolve_frame(&theme, ControlRole::Neutral, state);
    assert_eq!(
        frame.rim,
        rgb(palette.rim_active),
        "the focused member takes the full active rim, not the partial lift"
    );
    assert!(frame.focused);
}

#[test]
fn focus_field_never_puts_a_foreign_edge_on_a_filled_plate() {
    let theme = Theme::dark();
    for role in [ControlRole::Primary, ControlRole::Recovery] {
        let frame = resolve_frame(&theme, role, field_member());
        assert_eq!(
            frame.rim, frame.plate,
            "{role:?} keeps a coloured plate's matching rim"
        );
    }
    // A pressed outlined control is filled too, so the same holds there.
    let pressed = ControlState {
        pointer: PointerState::Pressed,
        focus: FocusState {
            focused: false,
            in_focus_field: true,
        },
        ..ControlState::idle()
    };
    let frame = resolve_frame(&theme, ControlRole::Destructive, pressed);
    assert_eq!(frame.rim, frame.plate);
}

#[test]
fn focus_field_reaches_the_full_active_rim_under_heavy_contrast() {
    let theme = high_contrast();
    let frame = resolve_frame(&theme, ControlRole::Neutral, field_member());
    assert_eq!(
        frame.rim,
        rgb(theme.palette().rim_active),
        "contrast before glow: a partial blend would wash out"
    );
}

#[test]
fn a_rim_owning_disposition_outranks_focus_field_membership() {
    let theme = Theme::dark();
    // Each of these says something the user needs more than which group the
    // control belongs to, so none of them may be softened by a lift.
    let cases = [
        ControlState {
            enabled: false,
            ..ControlState::idle()
        },
        ControlState {
            authority: AuthorityState::Denied,
            ..ControlState::idle()
        },
        ControlState {
            authority: AuthorityState::NeedsCapability,
            ..ControlState::idle()
        },
        ControlState {
            authority: AuthorityState::FailedClosed,
            ..ControlState::idle()
        },
        ControlState {
            validation: ValidationState::Pending,
            ..ControlState::idle()
        },
    ];
    for state in cases {
        let member = ControlState {
            focus: FocusState {
                focused: false,
                in_focus_field: true,
            },
            ..state
        };
        for role in [ControlRole::Neutral, ControlRole::Primary] {
            assert_eq!(
                parts(&resolve_frame(&theme, role, member)),
                parts(&resolve_frame(&theme, role, state)),
                "{:?}/{role:?} must draw identically in or out of a Focus Field",
                state.disposition()
            );
        }
    }
}

#[test]
fn a_control_awaiting_confirmation_still_joins_the_focus_field() {
    let theme = Theme::dark();
    // Unlike the four above, this one is actionable and takes its plain role
    // emphasis, so nothing is being softened by the lift.
    let state = ControlState {
        authority: AuthorityState::NeedsConfirmation,
        ..ControlState::idle()
    };
    let member = ControlState {
        focus: FocusState {
            focused: false,
            in_focus_field: true,
        },
        ..state
    };
    assert_ne!(
        resolve_frame(&theme, ControlRole::Neutral, member).rim,
        resolve_frame(&theme, ControlRole::Neutral, state).rim
    );
}

#[test]
fn light_theme_lifts_a_field_member_too() {
    let theme = Theme::light();
    let rest = resolve_frame(&theme, ControlRole::Neutral, ControlState::idle());
    let member = resolve_frame(&theme, ControlRole::Neutral, field_member());
    assert_ne!(member.rim, rest.rim);
    assert_eq!(member.plate, rest.plate);
}

#[test]
fn disabled_overrides_every_role_with_the_quiet_muted_treatment() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let disabled = ControlState {
        enabled: false,
        pointer: PointerState::Hover,
        ..ControlState::idle()
    };
    for role in [
        ControlRole::Primary,
        ControlRole::Destructive,
        ControlRole::Recovery,
        ControlRole::Neutral,
    ] {
        let frame = resolve_frame(&theme, role, disabled);
        assert_eq!(frame.plate, rgb(palette.surface), "{role:?}");
        assert_eq!(frame.rim, rgb(palette.border), "{role:?}");
        assert_eq!(frame.label, rgb(palette.on_surface_muted), "{role:?}");
    }
}

#[test]
fn denial_outlines_the_denied_role_over_the_controls_own_role() {
    let theme = Theme::dark();
    let palette = theme.palette();
    for authority in [AuthorityState::Denied, AuthorityState::NeedsCapability] {
        let state = ControlState {
            authority,
            ..ControlState::idle()
        };
        let frame = resolve_frame(&theme, ControlRole::Primary, state);
        assert_eq!(frame.plate, rgb(palette.surface_raised), "{authority:?}");
        assert_eq!(frame.rim, rgb(palette.denied), "{authority:?}");
        assert_eq!(frame.label, rgb(palette.denied), "{authority:?}");
    }
}

#[test]
fn failed_closed_outlines_the_recovery_role_and_pending_the_active_rim() {
    let theme = Theme::dark();
    let palette = theme.palette();

    let failed = ControlState {
        authority: AuthorityState::FailedClosed,
        ..ControlState::idle()
    };
    let frame = resolve_frame(&theme, ControlRole::Primary, failed);
    assert_eq!(frame.rim, rgb(palette.recovery));
    assert_eq!(frame.label, rgb(palette.recovery));

    let checking = ControlState {
        validation: ValidationState::Pending,
        ..ControlState::idle()
    };
    let frame = resolve_frame(&theme, ControlRole::Primary, checking);
    assert_eq!(frame.rim, rgb(palette.rim_active));
    assert_eq!(frame.label, rgb(palette.rim_active));
}

#[test]
fn light_theme_keeps_the_same_invariants() {
    let theme = Theme::light();
    let palette = theme.palette();
    let filled = resolve_frame(&theme, ControlRole::Primary, pointer(PointerState::None));
    assert_eq!(filled.plate, rgb(palette.accent));
    assert_eq!(filled.rim, filled.plate);
    assert_eq!(filled.label, rgb(palette.on_accent));

    let outlined = resolve_frame(
        &theme,
        ControlRole::Destructive,
        pointer(PointerState::None),
    );
    assert_eq!(outlined.plate, rgb(palette.surface_raised));
    assert_eq!(outlined.rim, rgb(palette.danger));
    assert_eq!(outlined.label, rgb(palette.danger));
}
