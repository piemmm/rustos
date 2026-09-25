//! The window's state is the compositor's answer, never the client's
//! request.

use super::*;

#[test]
fn a_new_window_is_restored_with_no_extent_yet() {
    let shell = Shell::new();
    assert_eq!(shell.state(), WindowSizeState::Restored);
    assert_eq!(shell.extent(), None);
    assert!(!shell.focused());
    assert!(shell.seated());
    assert!(shell.shown());
    assert!(shell.running());
    assert_eq!(Shell::default(), shell);
}

#[test]
fn a_request_is_not_adopted_until_the_compositor_answers() {
    let mut shell = Shell::new();
    assert_eq!(
        shell.request(WindowSizeState::Fullscreen),
        Some(WindowSizeState::Fullscreen)
    );
    assert_eq!(
        shell.state(),
        WindowSizeState::Restored,
        "asking is not being answered"
    );
    assert!(shell.resized(1920, 1080, WindowSizeState::Fullscreen));
    assert_eq!(shell.state(), WindowSizeState::Fullscreen);
    assert_eq!(shell.extent(), Some((1920, 1080)));
}

#[test]
fn asking_for_the_state_the_window_is_already_in_sends_nothing() {
    let mut shell = Shell::new();
    shell.resized(800, 600, WindowSizeState::Restored);
    assert_eq!(shell.request(WindowSizeState::Restored), None);
    assert_eq!(
        shell.request(WindowSizeState::Maximized),
        Some(WindowSizeState::Maximized)
    );
}

#[test]
fn an_echoed_resize_reports_that_nothing_moved() {
    let mut shell = Shell::new();
    assert!(shell.resized(800, 600, WindowSizeState::Restored));
    assert!(
        !shell.resized(800, 600, WindowSizeState::Restored),
        "the same extent and state repainted the window"
    );
    assert!(shell.resized(801, 600, WindowSizeState::Restored));
    assert!(shell.resized(801, 600, WindowSizeState::Maximized));
}

#[test]
fn leaving_fullscreen_returns_to_the_state_it_was_entered_from() {
    let mut shell = Shell::new();
    shell.resized(1280, 720, WindowSizeState::Maximized);
    assert_eq!(shell.fullscreen_toggle(), WindowSizeState::Fullscreen);
    shell.resized(1920, 1080, WindowSizeState::Fullscreen);
    assert_eq!(
        shell.fullscreen_toggle(),
        WindowSizeState::Maximized,
        "a maximised window came back restored"
    );

    let mut restored = Shell::new();
    restored.resized(800, 600, WindowSizeState::Restored);
    restored.resized(1920, 1080, WindowSizeState::Fullscreen);
    assert_eq!(restored.fullscreen_toggle(), WindowSizeState::Restored);
}

#[test]
fn a_run_of_fullscreen_events_does_not_lose_the_state_to_return_to() {
    let mut shell = Shell::new();
    shell.resized(1280, 720, WindowSizeState::Maximized);
    // A resize *while* fullscreen — a display mode change, say — must
    // not overwrite where leaving it should land.
    shell.resized(1920, 1080, WindowSizeState::Fullscreen);
    shell.resized(2560, 1440, WindowSizeState::Fullscreen);
    assert_eq!(shell.fullscreen_toggle(), WindowSizeState::Maximized);
}

#[test]
fn losing_focus_is_reported_once() {
    let mut shell = Shell::new();
    assert!(!shell.focus(true), "gaining focus lets go of nothing");
    assert!(shell.focused());
    assert!(shell.focus(false), "losing focus lets go of held keys");
    assert!(!shell.focus(false), "a second report is not a second loss");
}

#[test]
fn losing_the_seat_stops_the_clock_and_drops_focus() {
    let mut shell = Shell::new();
    shell.focus(true);
    assert!(shell.seat(false), "the running state moved");
    assert!(!shell.running());
    assert!(
        !shell.focused(),
        "a window with no seat does not hold focus"
    );
    assert!(!shell.seat(false), "the same report twice is one edge");
    assert!(shell.seat(true));
    assert!(shell.running());
}

#[test]
fn a_minimized_window_stops_until_it_is_shown_again() {
    let mut shell = Shell::new();
    shell.resized(1280, 720, WindowSizeState::Restored);
    shell.focus(true);
    shell.minimized();
    assert!(
        !shell.shown() && !shell.running(),
        "a minimized window kept drawing"
    );
    shell.focus(false);
    assert!(!shell.running(), "losing focus is not being shown");
    shell.focus(true);
    assert!(
        shell.running(),
        "regaining focus did not bring the window back"
    );

    shell.minimized();
    shell.resized(1280, 720, WindowSizeState::Restored);
    assert!(
        shell.running(),
        "a size given back did not bring the window back"
    );

    shell.minimized();
    shell.seat(false);
    shell.focus(true);
    assert!(!shell.running(), "a window shown on no seat ran");
}
