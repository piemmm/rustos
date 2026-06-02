//! Headless unit tests for the desktop session glue.

use rustos_taskbar::{MenuAction, MenuEntryId, SessionControl, TaskbarConfig, TaskbarResponse};
use rustos_theme::{Appearance, Metrics, Theme, ThemeError, ThemeId};

use crate::{DesktopSession, SessionEvent};

const LABEL: &str = "Toggle Light/Dark";

fn session() -> DesktopSession {
    DesktopSession::new(TaskbarConfig::bottom_bar(1920, 1080), LABEL)
}

/// A custom theme cloned from a built-in but with a distinctive taskbar corner
/// radius, so a test can *observe* that a theme switch reached the taskbar.
fn custom_dark(id: ThemeId, taskbar_corner_radius: u32) -> Theme {
    let base = Theme::dark();
    Theme::new(
        id,
        "Custom Dark",
        Appearance::Dark,
        *base.palette(),
        Metrics {
            window_corner_radius: 8,
            taskbar_corner_radius,
            popup_corner_radius: 6,
            border_thickness: 1,
        },
        base.fonts().clone(),
        base.cursors().clone(),
    )
}

fn toggle_entry_id(session: &DesktopSession) -> MenuEntryId {
    session
        .taskbar()
        .start_menu()
        .entries()
        .iter()
        .find(|entry| entry.action == MenuAction::ToggleAppearance)
        .expect("the session seeds an appearance-toggle entry")
        .id
}

#[test]
fn new_starts_dark_and_seeds_the_appearance_toggle() {
    let session = session();
    assert_eq!(session.active_theme().id(), ThemeId::DARK);
    assert_eq!(session.active_theme().appearance(), Appearance::Dark);

    let entry = session
        .taskbar()
        .start_menu()
        .entries()
        .iter()
        .find(|entry| entry.action == MenuAction::ToggleAppearance)
        .expect("the toggle entry is present");
    assert_eq!(entry.label(), LABEL);
}

#[test]
fn resolving_the_toggle_entry_switches_appearance() {
    let mut session = session();
    let id = toggle_entry_id(&session);

    let event = session.resolve(TaskbarResponse::MenuEntrySelected {
        id,
        action: MenuAction::ToggleAppearance,
    });
    assert_eq!(event, SessionEvent::AppearanceChanged(ThemeId::LIGHT));
    assert_eq!(session.active_theme().appearance(), Appearance::Light);

    let event = session.resolve(TaskbarResponse::MenuEntrySelected {
        id,
        action: MenuAction::ToggleAppearance,
    });
    assert_eq!(event, SessionEvent::AppearanceChanged(ThemeId::DARK));
    assert_eq!(session.active_theme().appearance(), Appearance::Dark);
}

#[test]
fn non_toggle_responses_are_forwarded_unchanged() {
    let mut session = session();

    assert_eq!(
        session.resolve(TaskbarResponse::ClockPressed),
        SessionEvent::Forward(TaskbarResponse::ClockPressed)
    );

    // A session control is forwarded: the session holds no capability to power
    // the machine off, so the embedder performs it (`AGENTS.md` §10).
    let control = TaskbarResponse::MenuEntrySelected {
        id: MenuEntryId(3),
        action: MenuAction::Session(SessionControl::ShutDown),
    };
    assert_eq!(session.resolve(control), SessionEvent::Forward(control));

    // Forwarding never touched the theme.
    assert_eq!(session.active_theme().appearance(), Appearance::Dark);
}

#[test]
fn switching_theme_re_themes_the_taskbar() {
    let mut session = session();
    session
        .register_theme(custom_dark(ThemeId(100), 99))
        .expect("a fresh id registers");

    session
        .set_theme(ThemeId(100))
        .expect("the id is registered");
    assert_eq!(session.active_theme().id(), ThemeId(100));
    assert_eq!(
        session.taskbar().corner_radius(),
        99,
        "set_theme relays the new metrics to the taskbar"
    );

    // The custom theme is dark, so toggling switches to the built-in light
    // theme, whose taskbar corner radius differs.
    let now = session.toggle_appearance();
    assert_eq!(now, ThemeId::LIGHT);
    assert_eq!(session.active_theme().appearance(), Appearance::Light);
    assert_eq!(
        session.taskbar().corner_radius(),
        Theme::light().metrics().taskbar_corner_radius,
        "the toggle relays the built-in light metrics to the taskbar"
    );
}

#[test]
fn set_theme_fails_closed_on_unknown_id() {
    let mut session = session();
    let before = session.taskbar().corner_radius();

    let unknown = ThemeId(424_242);
    assert_eq!(
        session.set_theme(unknown),
        Err(ThemeError::UnknownTheme(unknown))
    );
    assert_eq!(session.active_theme().id(), ThemeId::DARK);
    assert_eq!(
        session.taskbar().corner_radius(),
        before,
        "a refused switch leaves the taskbar untouched"
    );
}

#[test]
fn register_theme_rejects_a_duplicate_id() {
    let mut session = session();
    assert_eq!(
        session.register_theme(custom_dark(ThemeId::DARK, 50)),
        Err(ThemeError::DuplicateId(ThemeId::DARK))
    );
}
