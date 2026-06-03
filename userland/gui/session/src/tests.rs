//! Headless unit tests for the desktop session glue.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_cursor::CursorTheme;
use rustos_icon::{IconKind, IconSet};
use rustos_taskbar::{MenuAction, MenuEntryId, SessionControl, TaskbarConfig, TaskbarResponse};
use rustos_theme::{Appearance, CursorKind, Metrics, Theme, ThemeError, ThemeId};

use crate::{load_icon_set, DesktopSession, GraphicsAssetReader, SessionEvent};

const LABEL: &str = "Toggle Light/Dark";

/// A valid SVG asset (a single filled triangle on a square grid) that decodes
/// to a non-empty vector form, so loading it is observably different from the
/// built-in fallback.
const VALID_SVG: &[u8] = br##"<svg viewBox="0 0 24 24">
    <polygon points="2,2 22,2 12,22" fill="#ffaa00"/>
</svg>"##;

/// Bytes that are not a decodable SVG document at all, so the per-kind decode
/// returns an error and the loader falls back to the built-in artwork.
const MALFORMED_SVG: &[u8] = b"this is not an SVG document";

/// An in-memory [`GraphicsAssetReader`]: a path→bytes table standing in for
/// the VFS, returning [`Errno::NotFound`] for any path it does not hold.
#[derive(Default)]
struct MemoryAssets {
    files: Vec<(String, Vec<u8>)>,
}

impl MemoryAssets {
    fn with(mut self, path: &str, bytes: &[u8]) -> Self {
        self.files.push((String::from(path), bytes.to_vec()));
        self
    }
}

impl GraphicsAssetReader for MemoryAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, bytes)| bytes.clone())
            .ok_or(Errno::NotFound)
    }
}

#[test]
fn loads_icon_assets_and_falls_back_per_kind() {
    let mut reader = MemoryAssets::default()
        .with("/System/Graphics/Icons/network.svg", VALID_SVG)
        .with("/System/Graphics/Icons/volume.svg", VALID_SVG);
    let icons = load_icon_set(&mut reader);

    assert!(icons.is_loaded(IconKind::Network));
    assert!(icons.is_loaded(IconKind::Volume));
    // The kinds whose assets were absent keep their built-in glyph.
    assert!(!icons.is_loaded(IconKind::Battery));
    assert!(!icons.is_loaded(IconKind::Bell));
    assert!(!icons.is_loaded(IconKind::Generic));
}

#[test]
fn empty_icon_source_is_the_builtin_set() {
    let icons = load_icon_set(&mut MemoryAssets::default());
    assert_eq!(icons, IconSet::builtin());
}

#[test]
fn malformed_icon_asset_falls_back_to_builtin() {
    let mut reader = MemoryAssets::default().with("/System/Graphics/Icons/bell.svg", MALFORMED_SVG);
    let icons = load_icon_set(&mut reader);
    assert!(!icons.is_loaded(IconKind::Bell));
}

#[test]
fn loads_cursor_assets_for_the_active_theme_and_falls_back_per_kind() {
    let session = session();
    let mut reader =
        MemoryAssets::default().with("/System/Graphics/Cursors/cursor.arrow.svg", VALID_SVG);
    let cursors = session.load_cursors(&mut reader);

    let builtin = CursorTheme::builtin();
    // The arrow asset loaded, so its cursor differs from the built-in arrow.
    assert_ne!(
        cursors.cursor(CursorKind::Arrow),
        builtin.cursor(CursorKind::Arrow)
    );
    // Every other kind had no asset, so it kept the built-in cursor.
    for kind in [
        CursorKind::Text,
        CursorKind::Pointer,
        CursorKind::Move,
        CursorKind::Busy,
    ] {
        assert_eq!(cursors.cursor(kind), builtin.cursor(kind), "{kind:?}");
    }
}

#[test]
fn empty_cursor_source_is_the_builtin_set() {
    let session = session();
    let cursors = session.load_cursors(&mut MemoryAssets::default());

    let builtin = CursorTheme::builtin();
    for kind in [
        CursorKind::Arrow,
        CursorKind::Text,
        CursorKind::Pointer,
        CursorKind::Move,
        CursorKind::Busy,
    ] {
        assert_eq!(cursors.cursor(kind), builtin.cursor(kind), "{kind:?}");
    }
}

#[test]
fn malformed_cursor_asset_falls_back_to_builtin() {
    let session = session();
    let mut reader =
        MemoryAssets::default().with("/System/Graphics/Cursors/cursor.arrow.svg", MALFORMED_SVG);
    let cursors = session.load_cursors(&mut reader);
    assert_eq!(
        cursors.cursor(CursorKind::Arrow),
        CursorTheme::builtin().cursor(CursorKind::Arrow)
    );
}

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
