//! Headless unit tests for the desktop session glue.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver::display::{DisplayFormat, DisplayMode};
use rustos_abi::Errno;
use rustos_cursor::CursorTheme;
use rustos_icon::{IconKind, IconSet};
use rustos_taskbar::{
    ActivateOutcome, MenuAction, MenuEntryId, SessionControl, TaskId, TaskbarConfig,
    TaskbarRenderer, TaskbarResponse,
};
use rustos_theme::{Appearance, CursorKind, Metrics, Theme, ThemeError, ThemeId};
use rustos_wm::{
    Color, Compositor, Corners, InputEvent, InputResponse, Point, PointerButton, Surface, WindowId,
};

use crate::{
    load_icon_set, DesktopSession, GraphicsAssetReader, SessionEvent, SessionInputResponse,
    SessionInputRouter, TaskbarPresenter,
};

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

/// A headless 1920×1080 RGBA compositor over an opaque black background.
fn compositor() -> Compositor {
    let mode = DisplayMode {
        width_px: 1920,
        height_px: 1080,
        stride_bytes: 1920 * 4,
        format: DisplayFormat::Rgba8888,
    };
    Compositor::new(
        mode,
        Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        },
    )
    .expect("the compositor allocates")
}

#[test]
fn present_adds_a_bar_window_placed_and_rounded() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );

    let id = presenter.bar_window().expect("the bar was presented");
    assert_eq!(comp.window_count(), 1);
    assert!(presenter.popup_window().is_none(), "the menu is closed");

    let layout = session.taskbar().layout();
    let window = comp.window(id).expect("the bar window exists");
    assert_eq!(window.origin(), layout.bar.origin);
    assert_eq!(window.corners(), Corners::from_radius(layout.corner_radius));
    assert_eq!(window.surface().width(), layout.bar.width);
    assert_eq!(window.surface().height(), layout.bar.height);
}

#[test]
fn presenting_twice_reuses_the_bar_window() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );
    let first = presenter.bar_window().expect("first present");
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );
    let second = presenter.bar_window().expect("second present");

    assert_eq!(first, second, "the same window is reused");
    assert_eq!(comp.window_count(), 1, "no second bar window is created");
}

#[test]
fn opening_the_menu_presents_a_popup_window() {
    let mut session = session();
    session.taskbar_mut().start_menu_mut().toggle();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );

    let popup = presenter.popup_window().expect("the popup was presented");
    assert_eq!(comp.window_count(), 2, "bar and popup are both present");

    let layout = session.taskbar().menu_layout();
    let window = comp.window(popup).expect("the popup window exists");
    assert_eq!(window.origin(), layout.panel.origin);
    assert_eq!(window.corners(), Corners::from_radius(layout.corner_radius));
}

#[test]
fn closing_the_menu_removes_the_popup_window() {
    let mut session = session();
    session.taskbar_mut().start_menu_mut().toggle();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );
    let popup = presenter.popup_window().expect("the popup is open");

    session.taskbar_mut().start_menu_mut().toggle();
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );

    assert!(
        presenter.popup_window().is_none(),
        "the popup was dismissed"
    );
    assert!(comp.window(popup).is_none(), "its window was removed");
    assert_eq!(comp.window_count(), 1, "only the bar remains");
    assert!(presenter.bar_window().is_some(), "the bar stays presented");
}

#[test]
fn teardown_removes_every_window() {
    let mut session = session();
    session.taskbar_mut().start_menu_mut().toggle();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );
    assert_eq!(comp.window_count(), 2);

    presenter.teardown(&mut comp);
    assert_eq!(comp.window_count(), 0);
    assert!(presenter.bar_window().is_none());
    assert!(presenter.popup_window().is_none());
}

#[test]
fn present_recreates_the_bar_when_its_window_was_removed() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );
    let first = presenter.bar_window().expect("first present");

    assert!(comp.remove(first), "an embedder removed the bar window");
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );

    let second = presenter.bar_window().expect("the bar was re-created");
    assert_ne!(first, second, "a fresh window id is minted");
    assert!(comp.window(second).is_some());
    assert_eq!(comp.window_count(), 1);
}

#[test]
fn a_theme_switch_re_rounds_the_presented_bar() {
    let mut session = session();
    session
        .register_theme(custom_dark(ThemeId(100), 99))
        .expect("a fresh id registers");
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );
    let id = presenter.bar_window().expect("the bar was presented");
    let dark_corners = Corners::from_radius(session.taskbar().layout().corner_radius);
    assert_eq!(
        comp.window(id).expect("the bar window").corners(),
        dark_corners
    );

    session
        .set_theme(ThemeId(100))
        .expect("the id is registered");
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        session.active_theme(),
    );

    let switched_corners = Corners::from_radius(session.taskbar().layout().corner_radius);
    assert_eq!(switched_corners, Corners::from_radius(99));
    assert_eq!(
        comp.window(id).expect("the same bar window").corners(),
        switched_corners,
        "the switched corner radius reached the presented bar"
    );
    assert_eq!(comp.window_count(), 1);
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

/// A filled opaque test window the input router can hit-test against.
fn opaque_window(comp: &mut Compositor, origin: Point, width: u32, height: u32) -> WindowId {
    let surface =
        Surface::filled(width, height, Color::rgb(0, 120, 255).premultiply()).expect("surface");
    comp.add_window(origin, surface)
}

/// A point guaranteed to lie inside the taskbar's start button.
fn start_button_point(session: &DesktopSession) -> Point {
    let rect = session.taskbar().layout().start_button;
    assert!(!rect.is_empty(), "the start button has a region");
    Point::new(rect.left() + 1, rect.top() + 1)
}

#[test]
fn primary_press_over_the_bar_routes_to_the_taskbar() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let at = start_button_point(&session);

    router.handle(
        InputEvent::PointerMoved { to: at },
        &mut comp,
        session.taskbar_mut(),
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::StartMenuToggled { open: true })
    );
    assert!(session.taskbar().start_menu().is_open());
}

#[test]
fn the_bar_wins_over_a_window_beneath_it() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    // A window placed under the bottom bar must not steal a press on the bar.
    opaque_window(&mut comp, Point::new(0, 1000), 400, 80);
    let at = start_button_point(&session);

    router.handle(
        InputEvent::PointerMoved { to: at },
        &mut comp,
        session.taskbar_mut(),
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::StartMenuToggled { open: true })
    );
}

#[test]
fn primary_press_over_a_window_routes_to_the_window_manager() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    router.handle(
        InputEvent::PointerMoved {
            to: Point::new(250, 250),
        },
        &mut comp,
        session.taskbar_mut(),
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::WindowManager(InputResponse::Activated {
            window,
            local: Point::new(50, 50),
        })
    );
    assert_eq!(router.focused(), Some(window));
}

#[test]
fn primary_press_on_the_empty_desktop_routes_to_the_window_manager() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();

    router.handle(
        InputEvent::PointerMoved {
            to: Point::new(900, 500),
        },
        &mut comp,
        session.taskbar_mut(),
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::WindowManager(InputResponse::DesktopPressed)
    );
}

#[test]
fn an_open_menu_is_modal_and_a_press_off_it_dismisses_it() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let at = start_button_point(&session);

    router.handle(
        InputEvent::PointerMoved { to: at },
        &mut comp,
        session.taskbar_mut(),
    );
    router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );
    assert!(session.taskbar().start_menu().is_open());

    // A press far from the bar and popup is still claimed by the modal menu
    // and dismisses it, rather than reaching the window manager.
    router.handle(
        InputEvent::PointerMoved {
            to: Point::new(900, 500),
        },
        &mut comp,
        session.taskbar_mut(),
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::StartMenuDismissed)
    );
    assert!(!session.taskbar().start_menu().is_open());
}

#[test]
fn motion_updates_the_pointer_and_is_otherwise_ignored() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();

    let response = router.handle(
        InputEvent::PointerMoved {
            to: Point::new(640, 480),
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(response, SessionInputResponse::Ignored);
    assert_eq!(router.pointer(), Point::new(640, 480));
}

#[test]
fn a_window_drag_continues_while_the_pointer_is_over_the_bar() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    router.handle(
        InputEvent::PointerMoved {
            to: Point::new(250, 250),
        },
        &mut comp,
        session.taskbar_mut(),
    );
    router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );
    assert!(
        router.begin_move(&comp),
        "a focused window starts a move-grab"
    );

    // Dragging the pointer down over the bar must keep moving the window, not
    // hand the motion to the taskbar.
    let response = router.handle(
        InputEvent::PointerMoved {
            to: Point::new(250, 1060),
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::WindowManager(InputResponse::Moved {
            window,
            origin: Point::new(200, 1010),
        })
    );
    assert!(router.is_moving());
}

#[test]
fn a_primary_release_ends_a_move_grab() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    router.handle(
        InputEvent::PointerMoved {
            to: Point::new(250, 250),
        },
        &mut comp,
        session.taskbar_mut(),
    );
    router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );
    assert!(router.begin_move(&comp));

    let response = router.handle(
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::WindowManager(InputResponse::MoveEnded { window })
    );
    assert!(!router.is_moving());
}

#[test]
fn a_non_primary_press_is_ignored() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let at = start_button_point(&session);

    router.handle(
        InputEvent::PointerMoved { to: at },
        &mut comp,
        session.taskbar_mut(),
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(response, SessionInputResponse::Ignored);
    assert!(
        !session.taskbar().start_menu().is_open(),
        "a secondary press did nothing"
    );
}

#[test]
fn a_press_with_no_running_task_activates_a_fresh_one() {
    // Guards the ActivateOutcome import and exercises the taskbar task path
    // through the session router end to end.
    let mut session = session();
    session.taskbar_mut().tasks_mut().add(TaskId(1), "Editor");
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();

    let slot = session.taskbar().layout().tasks.first().copied();
    let Some(slot) = slot else {
        panic!("the inserted task has a slot");
    };
    let at = Point::new(slot.left() + 1, slot.top() + 1);

    router.handle(
        InputEvent::PointerMoved { to: at },
        &mut comp,
        session.taskbar_mut(),
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    let SessionInputResponse::Taskbar(TaskbarResponse::TaskActivated { outcome, .. }) = response
    else {
        panic!("a press on a task slot activates it, got {response:?}");
    };
    assert_eq!(outcome, ActivateOutcome::Activated);
}
