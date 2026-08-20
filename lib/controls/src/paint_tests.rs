//! Unit tests for the shared Reactive Alloy plate colour recipe.
//!
//! Every drawn family resolves its plate, rim, and label through
//! [`resolve_frame`], so the design boards' colour invariants are pinned here
//! once rather than re-asserted per family: a coloured plate always carries a
//! rim of the same colour, a role states itself on the edge and the label
//! before it states itself on the plate, and a press colours a control rather
//! than merely darkening it.
//!
//! The container pointer-routing rule every collection shares is pinned here
//! for the same reason: which children one sample reaches is one decision,
//! not one per family.

use alloc::vec::Vec;

use tairix_geometry::Point;
use tairix_input::{InputEvent, PointerButton};
use tairix_raster::Color;
use tairix_theme::{Appearance, Rgba, Theme};

use crate::paint::{
    grab_after, ground_fill, resolve_frame, route_pointer, ChromeLayer, FrameColors,
};
use crate::state::{
    AuthorityState, ControlRole, ControlState, FocusState, PlateSeating, PointerState,
    ValidationState,
};
use crate::testkit::high_contrast;

fn pointer(pointer: PointerState) -> ControlState {
    ControlState {
        pointer,
        ..ControlState::idle()
    }
}

/// Every role in the vocabulary, so a seating invariant is asserted across the
/// whole set rather than a sample of it.
const EVERY_ROLE: [ControlRole; 7] = [
    ControlRole::Neutral,
    ControlRole::Primary,
    ControlRole::Recommended,
    ControlRole::Destructive,
    ControlRole::Recovery,
    ControlRole::Navigation,
    ControlRole::System,
];

/// Every state that changes how a frame resolves: each disposition crossed
/// with each pointer relationship and each focus relationship.
///
/// The seating rules are absolutes ("never an edge on the bar", "never hide a
/// focus ring"), so they are checked exhaustively rather than on the handful of
/// states a renderer happens to produce today.
fn every_state() -> impl Iterator<Item = ControlState> {
    let dispositions = [
        ControlState::idle(),
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
            authority: AuthorityState::NeedsConfirmation,
            ..ControlState::idle()
        },
        ControlState {
            validation: ValidationState::Pending,
            ..ControlState::idle()
        },
    ];
    let pointers = [
        PointerState::None,
        PointerState::Hover,
        PointerState::Pressed,
        PointerState::DragSource,
        PointerState::DragTarget,
    ];
    let focuses = [(false, false), (true, false), (false, true)];
    dispositions.into_iter().flat_map(move |base| {
        pointers.into_iter().flat_map(move |pointer| {
            focuses
                .into_iter()
                .map(move |(focused, in_focus_field)| ControlState {
                    pointer,
                    focus: FocusState {
                        focused,
                        in_focus_field,
                    },
                    ..base
                })
        })
    })
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
fn hover_washes_a_quiet_plate_and_lifts_its_rim_without_colouring_its_label() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let rest = resolve_frame(&theme, ControlRole::Neutral, ControlState::idle());
    let frame = resolve_frame(&theme, ControlRole::Neutral, pointer(PointerState::Hover));
    assert_eq!(frame.plate, rgb(palette.surface_hover));
    assert_ne!(
        frame.plate, rest.plate,
        "the plate itself washes, so a control with no rim still reports the pointer"
    );
    assert_eq!(frame.rim, rgb(palette.rim_active));
    assert_eq!(frame.label, rgb(palette.on_surface));
}

#[test]
fn focus_keeps_the_quiet_rim_so_its_ring_is_the_only_accent_line() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut state = ControlState::idle();
    state.focus.focused = true;
    let frame = resolve_frame(&theme, ControlRole::Neutral, state);
    assert_eq!(
        frame.rim,
        rgb(palette.rim),
        "an accent rim outside the accent ring is a doubled border"
    );
    assert!(frame.focused);

    // The pointer arriving on a focused control must not put the second line
    // back: the wash reports the pointer instead.
    for pointer in [PointerState::Hover, PointerState::Pressed] {
        let mut on_it = state;
        on_it.pointer = pointer;
        let frame = resolve_frame(&theme, ControlRole::Neutral, on_it);
        assert_eq!(frame.rim, rgb(palette.rim), "{pointer:?} lifted the rim");
        assert_ne!(
            frame.plate,
            rgb(palette.surface_raised),
            "{pointer:?} is then stated nowhere at all"
        );
    }
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
        rgb(palette.rim),
        "the focused member states focus with its ring, not with a lifted edge"
    );
    assert_ne!(
        frame.rim,
        resolve_frame(&theme, ControlRole::Neutral, field_member()).rim,
        "and so cannot be mistaken for a mere member of the field"
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
fn a_panel_seated_control_always_wears_its_resolved_plate_and_rim() {
    let theme = Theme::dark();
    for state in every_state() {
        for role in EVERY_ROLE {
            let frame = resolve_frame(&theme, role, state);
            assert_eq!(
                frame.face(PlateSeating::Panel),
                Some((frame.plate, frame.rim)),
                "{role:?} on a panel is a plate with a rim in every state"
            );
        }
    }
}

#[test]
fn a_bar_seated_control_never_wears_a_rim_in_any_state() {
    // The bar is one surface: an icon on it may wash, tint, bead, or seam, but
    // it may never draw a perimeter of its own in *any* state, or a strip of
    // icons reads as a row of boxes.
    let theme = Theme::dark();
    for state in every_state() {
        for role in EVERY_ROLE {
            let frame = resolve_frame(&theme, role, state);
            if let Some((plate, rim)) = frame.face(PlateSeating::Bar) {
                assert_eq!(
                    rim,
                    plate,
                    "{role:?}/{:?} put an edge on a bar-seated control",
                    state.disposition()
                );
            }
        }
    }
}

#[test]
fn a_bar_seated_control_is_bare_only_while_it_has_nothing_to_state() {
    let theme = Theme::dark();
    let quiet_rest = resolve_frame(&theme, ControlRole::Neutral, ControlState::idle());
    assert_eq!(
        quiet_rest.face(PlateSeating::Bar),
        None,
        "a resting quiet icon is the bar it sits in"
    );

    // Anything the control has to say raises its plate: the pointer, the
    // keyboard, a role colour, or a disposition.
    let speaking = [
        pointer(PointerState::Hover),
        pointer(PointerState::Pressed),
        ControlState {
            focus: FocusState {
                focused: true,
                in_focus_field: false,
            },
            ..ControlState::idle()
        },
        ControlState {
            enabled: false,
            ..ControlState::idle()
        },
        ControlState {
            authority: AuthorityState::Denied,
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
    for state in speaking {
        assert!(
            resolve_frame(&theme, ControlRole::Neutral, state)
                .face(PlateSeating::Bar)
                .is_some(),
            "{:?} must be visible on the bar",
            state.disposition()
        );
    }
    for role in [
        ControlRole::Primary,
        ControlRole::Recovery,
        ControlRole::Recommended,
        ControlRole::Destructive,
    ] {
        assert!(
            resolve_frame(&theme, role, ControlState::idle())
                .face(PlateSeating::Bar)
                .is_some(),
            "{role:?} carries a colour, so it is never bare"
        );
    }
}

#[test]
fn a_bare_bar_seated_control_never_hides_a_focus_ring() {
    // `face` returning `None` skips the plate painter, and the focus ring is
    // drawn inside the plate — so a bare frame must never be a focused one.
    let theme = Theme::dark();
    for state in every_state() {
        for role in EVERY_ROLE {
            let frame = resolve_frame(&theme, role, state);
            assert!(
                frame.face(PlateSeating::Bar).is_some() || !frame.focused,
                "{role:?} would drop the focus ring of a focused control"
            );
        }
    }
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

// --- Container pointer routing -----------------------------------------

/// The children `route_pointer` names, in order, ignoring the empty slots.
fn targets(hovered: &mut Option<usize>, armed: Option<usize>, over: Option<usize>) -> Vec<usize> {
    route_pointer(hovered, armed, over)
        .into_iter()
        .flatten()
        .collect()
}

#[test]
fn crossing_a_boundary_reaches_the_child_left_and_the_child_entered() {
    let mut hovered = Some(0);
    assert_eq!(targets(&mut hovered, None, Some(1)), alloc::vec![0, 1]);
    assert_eq!(hovered, Some(1));
}

#[test]
fn motion_within_one_child_reaches_only_that_child() {
    let mut hovered = Some(1);
    assert_eq!(targets(&mut hovered, None, Some(1)), alloc::vec![1]);
}

#[test]
fn leaving_the_container_reaches_only_the_child_left() {
    let mut hovered = Some(2);
    assert_eq!(targets(&mut hovered, None, None), alloc::vec![2]);
    assert_eq!(hovered, None);
}

#[test]
fn an_unarmed_motion_never_reaches_more_than_two_children() {
    for from in 0..8 {
        for to in 0..8 {
            let mut hovered = Some(from);
            assert!(targets(&mut hovered, None, Some(to)).len() <= 2);
        }
    }
}

#[test]
fn a_pressed_child_stays_in_the_stream_wherever_the_pointer_goes() {
    let mut hovered = Some(1);
    let reached = targets(&mut hovered, Some(0), Some(2));
    assert!(
        reached.contains(&0),
        "the pressed child must keep receiving"
    );
    assert_eq!(reached, alloc::vec![0, 1, 2]);
}

#[test]
fn the_pressed_child_is_never_named_twice() {
    let mut hovered = Some(0);
    assert_eq!(targets(&mut hovered, Some(0), Some(0)), alloc::vec![0]);
}

#[test]
fn a_press_grabs_the_child_under_the_pointer_and_its_release_lets_go() {
    let press = InputEvent::PointerPressed {
        button: PointerButton::Primary,
    };
    let release = InputEvent::PointerReleased {
        button: PointerButton::Primary,
    };
    let moved = InputEvent::PointerMoved {
        to: Point::new(0, 0),
    };
    assert_eq!(grab_after(None, &press, Some(3)), Some(3));
    assert_eq!(grab_after(Some(3), &moved, Some(1)), Some(3));
    assert_eq!(grab_after(Some(3), &release, Some(1)), None);
}

// --- Floating desktop chrome -------------------------------------------

#[test]
fn a_background_on_floating_chrome_keeps_its_colour_and_takes_the_layers_alpha() {
    for theme in [Theme::dark(), Theme::light()] {
        let p = *theme.palette();
        let chrome = theme.clone().floating();
        for fill in [
            p.surface,
            p.surface_raised,
            p.surface_hover,
            p.surface_pressed,
        ] {
            for (layer, alpha) in [
                (ChromeLayer::Ground, p.chrome_alpha),
                (ChromeLayer::Plate, p.chrome_plate_alpha),
            ] {
                let laid = ground_fill(&chrome, fill, layer);
                assert_eq!(
                    (laid.r, laid.g, laid.b),
                    (fill.r, fill.g, fill.b),
                    "{}: {layer:?} retinted the theme's own colour",
                    theme.name()
                );
                assert_eq!(laid.a, alpha, "{}: {layer:?}", theme.name());
                assert_eq!(
                    ground_fill(&theme, fill, layer),
                    fill,
                    "{}: an ordinary surface let the desktop through",
                    theme.name()
                );
            }
        }
        assert!(
            p.chrome_alpha < p.chrome_plate_alpha,
            "{}: a plate no more solid than its ground is a hole in the glass",
            theme.name()
        );
    }
}

/// A quiet plate is a *background* and goes see-through on floating chrome; a
/// role fill is the statement itself and must not, or a primary action would
/// be diluted by whatever wallpaper happened to be behind it.
#[test]
fn a_role_fill_stays_solid_on_floating_chrome_but_a_quiet_plate_does_not() {
    for theme in [Theme::dark(), Theme::light()] {
        let chrome = theme.clone().floating();
        let resting = pointer(PointerState::None);

        let quiet = resolve_frame(&chrome, ControlRole::Neutral, resting);
        assert_eq!(
            quiet.plate,
            rgb(theme
                .palette()
                .surface_raised
                .with_alpha(theme.palette().chrome_plate_alpha)),
            "{}: a quiet plate covered the backdrop",
            theme.name()
        );
        assert_eq!(
            quiet.rim,
            resolve_frame(&theme, ControlRole::Neutral, resting).rim,
            "{}: the rim is the plate's edge, drawn to be seen",
            theme.name()
        );

        for role in [ControlRole::Primary, ControlRole::Recovery] {
            assert_eq!(
                resolve_frame(&chrome, role, resting).plate,
                resolve_frame(&theme, role, resting).plate,
                "{}: {role:?} diluted its own statement",
                theme.name()
            );
        }
    }
}

/// The pointer wash is the whole of the feedback for a control that wears no
/// perimeter — a taskbar icon — so on chrome it must still part company with
/// the surface under it, in whichever direction the appearance requires.
#[test]
fn the_pointer_wash_on_floating_chrome_reads_against_the_ground_on_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let p = *theme.palette();
        let chrome = theme.clone().floating();
        let wash = ground_fill(&chrome, p.surface_hover, ChromeLayer::Plate);
        let ground = ground_fill(&chrome, p.surface_raised, ChromeLayer::Ground);
        assert!(wash.a < 255, "{}: the wash covers", theme.name());

        let weight = |c: Rgba| u32::from(c.r) + u32::from(c.g) + u32::from(c.b);
        let lighter = weight(wash) > weight(ground);
        assert_eq!(
            lighter,
            theme.appearance() == Appearance::Dark,
            "{}: the wash moves the wrong way for this appearance",
            theme.name()
        );
    }
}
