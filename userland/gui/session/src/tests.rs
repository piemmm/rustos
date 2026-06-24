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
    Color, Compositor, Corners, InputEvent, InputResponse, Point, PointerButton, Scale, Surface,
    WindowId,
};

use crate::{
    load_icon_set, DesktopSession, DesktopShell, GraphicsAssetReader, InputSource, SessionEvent,
    SessionInputResponse, SessionInputRouter, ShellOutcome, TaskBridge, TaskbarPresenter,
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

    let layout = session.taskbar().layout(Scale::ONE);
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

    let layout = session.taskbar().menu_layout(Scale::ONE);
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
    let dark_corners = Corners::from_radius(session.taskbar().layout(Scale::ONE).corner_radius);
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

    let switched_corners = Corners::from_radius(session.taskbar().layout(Scale::ONE).corner_radius);
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
    // the machine off, so the embedder performs it.
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
    let rect = session.taskbar().layout(Scale::ONE).start_button;
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

    let slot = session.taskbar().layout(Scale::ONE).tasks.first().copied();
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

// ---- desktop shell: driving the desktop from a live input stream ----

/// An in-memory [`InputSource`]: a queue of events, optionally faulting once
/// the queue is drained, standing in for the kernel's input channel.
struct MemoryInput {
    events: Vec<InputEvent>,
    next: usize,
    fault: Option<Errno>,
}

impl MemoryInput {
    fn new(events: &[InputEvent]) -> Self {
        Self {
            events: events.to_vec(),
            next: 0,
            fault: None,
        }
    }

    fn faulting(events: &[InputEvent], fault: Errno) -> Self {
        Self {
            events: events.to_vec(),
            next: 0,
            fault: Some(fault),
        }
    }
}

impl InputSource for MemoryInput {
    fn poll(&mut self) -> Result<Option<InputEvent>, Errno> {
        if let Some(event) = self.events.get(self.next).copied() {
            self.next += 1;
            return Ok(Some(event));
        }
        match self.fault.take() {
            Some(errno) => Err(errno),
            None => Ok(None),
        }
    }
}

fn shell() -> DesktopShell {
    DesktopShell::new(TaskbarConfig::bottom_bar(1920, 1080), LABEL)
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRIMARY_PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};

#[test]
fn pump_opens_the_menu_and_presents_the_popup() {
    let mut shell = shell();
    let mut comp = compositor();
    let at = start_button_point(shell.session());

    let outcomes = shell
        .pump(
            &mut MemoryInput::new(&[moved(at.x, at.y), PRIMARY_PRESS]),
            &mut comp,
        )
        .expect("an in-memory source does not fault");

    assert_eq!(
        outcomes,
        [
            ShellOutcome::Ignored,
            ShellOutcome::Session(SessionEvent::Forward(TaskbarResponse::StartMenuToggled {
                open: true,
            })),
        ]
    );
    assert!(shell.session().taskbar().start_menu().is_open());
    assert!(
        shell.presenter().popup_window().is_some(),
        "opening the menu re-presents and adds the popup window"
    );
    assert_eq!(comp.window_count(), 2, "the bar and popup are both present");
}

#[test]
fn handle_routes_a_window_press_to_the_window_manager() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);

    assert_eq!(
        outcome,
        ShellOutcome::WindowManager(InputResponse::Activated {
            window,
            local: Point::new(50, 50),
        })
    );
    assert_eq!(shell.router().focused(), Some(window));
    assert!(
        shell.presenter().bar_window().is_none(),
        "a window-manager action does not present the bar"
    );
}

#[test]
fn selecting_the_appearance_toggle_switches_the_theme() {
    let mut shell = shell();
    let mut comp = compositor();
    let start = start_button_point(shell.session());

    // Open the menu, then press the appearance-toggle row (the last entry,
    // appended after the session controls).
    shell
        .pump(
            &mut MemoryInput::new(&[moved(start.x, start.y), PRIMARY_PRESS]),
            &mut comp,
        )
        .expect("source does not fault");
    let toggle_row = *shell
        .session()
        .taskbar()
        .menu_layout(Scale::ONE)
        .entries
        .last()
        .expect("the menu has an appearance-toggle row");
    let at = Point::new(toggle_row.left() + 1, toggle_row.top() + 1);

    let outcomes = shell
        .pump(
            &mut MemoryInput::new(&[moved(at.x, at.y), PRIMARY_PRESS]),
            &mut comp,
        )
        .expect("source does not fault");

    assert_eq!(
        outcomes.last(),
        Some(&ShellOutcome::Session(SessionEvent::AppearanceChanged(
            ThemeId::LIGHT
        )))
    );
    assert_eq!(
        shell.session().active_theme().id(),
        ThemeId::LIGHT,
        "the shell applied the light/dark switch itself"
    );
    assert!(
        !shell.session().taskbar().start_menu().is_open(),
        "selecting the toggle closed the menu"
    );
    assert!(
        shell.presenter().popup_window().is_none(),
        "the closed menu's popup window was removed on re-present"
    );
}

#[test]
fn pump_propagates_a_source_fault_after_applying_prior_events() {
    let mut shell = shell();
    let mut comp = compositor();
    let at = start_button_point(shell.session());

    let result = shell.pump(
        &mut MemoryInput::faulting(&[moved(at.x, at.y), PRIMARY_PRESS], Errno::NotFound),
        &mut comp,
    );

    assert_eq!(result, Err(Errno::NotFound));
    assert!(
        shell.session().taskbar().start_menu().is_open(),
        "the event drained before the fault was still applied"
    );
}

#[test]
fn motion_is_ignored_and_does_not_present_the_bar() {
    let mut shell = shell();
    let mut comp = compositor();

    let outcomes = shell
        .pump(&mut MemoryInput::new(&[moved(640, 480)]), &mut comp)
        .expect("source does not fault");

    assert_eq!(outcomes, [ShellOutcome::Ignored]);
    assert_eq!(shell.router().pointer(), Point::new(640, 480));
    assert!(
        shell.presenter().bar_window().is_none(),
        "a pure motion event presents nothing"
    );
    assert_eq!(comp.window_count(), 0);
}

#[test]
fn begin_move_through_the_shell_arms_a_grab_on_the_focused_window() {
    let mut shell = shell();
    let mut comp = compositor();
    opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp);
    shell.handle(PRIMARY_PRESS, &mut comp);
    assert!(shell.begin_move(&comp));
    assert!(shell.router().is_moving());
}

#[test]
fn set_icons_installs_a_loaded_set_and_the_bar_still_presents() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut reader = MemoryAssets::default().with("/System/Graphics/Icons/network.svg", VALID_SVG);
    shell.set_icons(load_icon_set(&mut reader));

    shell.present(&mut comp);

    assert!(shell.presenter().bar_window().is_some());
    assert_eq!(comp.window_count(), 1);
}

#[test]
fn set_scale_rescales_the_bar_transparently_and_is_idempotent() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let bar = shell
        .presenter()
        .bar_window()
        .expect("the bar is presented");
    let unscaled = comp.window(bar).expect("bar window").surface().height();

    // Switching the desktop density drives the compositor (the single source
    // of truth) and re-lays the bar in place, transparent to the taskbar.
    let doubled = Scale::from_percent(200).expect("200% is in range");
    assert!(
        shell.set_scale(doubled, &mut comp),
        "a new output scale changes the desktop"
    );
    assert_eq!(comp.scale(), doubled, "the compositor owns the new density");
    let scaled = comp
        .window(bar)
        .expect("the bar window is reused")
        .surface()
        .height();
    assert_eq!(scaled, unscaled * 2, "the bar re-laid at the new density");

    // An app reads its window's density here; it never sets it.
    assert_eq!(comp.window_scale(bar), Some(doubled));

    // Setting the scale already in effect re-presents nothing.
    assert!(!shell.set_scale(doubled, &mut comp));
}

#[test]
fn teardown_removes_the_presented_windows() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.session_mut().taskbar_mut().start_menu_mut().toggle();
    shell.present(&mut comp);
    assert_eq!(comp.window_count(), 2, "bar and popup present");

    shell.teardown(&mut comp);

    assert_eq!(comp.window_count(), 0);
    assert!(shell.presenter().bar_window().is_none());
    assert!(shell.presenter().popup_window().is_none());
}

// ---- running-task list ↔ window stack ----

/// A small opaque application content surface for an opened window.
fn app_surface() -> Surface {
    Surface::filled(320, 240, Color::rgb(40, 160, 90).premultiply()).expect("surface allocates")
}

/// The centre of the task slot at `index` on the bar laid out at 100%.
fn task_slot_point(shell: &DesktopShell, index: usize) -> Point {
    let slot = shell
        .session()
        .taskbar()
        .layout(Scale::ONE)
        .tasks
        .get(index)
        .copied()
        .expect("the task has a slot");
    assert!(!slot.is_empty(), "the task slot has a region");
    Point::new(slot.left() + 1, slot.top() + 1)
}

#[test]
fn open_window_lists_focuses_and_presents() {
    let mut shell = shell();
    let mut comp = compositor();

    let window = shell
        .open_window(&mut comp, Point::new(300, 200), app_surface(), "Editor")
        .expect("a fresh task id is available");

    // The window is on screen and focused; the bar lists it as the focused task.
    assert!(comp.window(window).is_some());
    assert_eq!(shell.router().focused(), Some(window));
    let task = shell
        .tasks()
        .task_for(window)
        .expect("the window is tracked");
    assert_eq!(shell.session().taskbar().tasks().len(), 1);
    assert_eq!(shell.session().taskbar().tasks().focused(), Some(task));
    // Opening re-presents the bar, so both the app window and the bar exist.
    assert!(shell.presenter().bar_window().is_some());
    assert_eq!(comp.window_count(), 2);
}

#[test]
fn close_window_removes_the_task_and_unfocuses() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = shell
        .open_window(&mut comp, Point::new(300, 200), app_surface(), "Editor")
        .expect("opens");

    assert!(shell.close_window(&mut comp, window));

    assert!(comp.window(window).is_none(), "the window is gone");
    assert!(shell.tasks().is_empty(), "the bridge forgot it");
    assert!(shell.session().taskbar().tasks().is_empty());
    assert_eq!(shell.router().focused(), None, "focus was dropped");
    // Closing an already-closed window changes nothing.
    assert!(!shell.close_window(&mut comp, window));
}

#[test]
fn clicking_a_task_minimises_then_restores_its_window() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = shell
        .open_window(&mut comp, Point::new(300, 200), app_surface(), "Editor")
        .expect("opens");
    let task = shell.tasks().task_for(window).expect("tracked");
    let at = task_slot_point(&shell, 0);

    // First click on the focused, non-minimised task minimises it: the window
    // is hidden and focus is dropped.
    shell.handle(moved(at.x, at.y), &mut comp);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::Session(SessionEvent::Forward(TaskbarResponse::TaskActivated {
            id: task,
            outcome: ActivateOutcome::Minimised,
        }))
    );
    assert!(!comp.window(window).expect("still tracked").is_visible());
    assert_eq!(shell.router().focused(), None);

    // A second click restores and re-focuses it.
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::Session(SessionEvent::Forward(TaskbarResponse::TaskActivated {
            id: task,
            outcome: ActivateOutcome::Activated,
        }))
    );
    assert!(comp.window(window).expect("tracked").is_visible());
    assert_eq!(shell.router().focused(), Some(window));
}

#[test]
fn clicking_a_window_directly_moves_the_bar_highlight() {
    let mut shell = shell();
    let mut comp = compositor();
    let first = shell
        .open_window(&mut comp, Point::new(100, 100), app_surface(), "First")
        .expect("opens");
    let second = shell
        .open_window(&mut comp, Point::new(900, 100), app_surface(), "Second")
        .expect("opens");
    let first_task = shell.tasks().task_for(first).expect("tracked");

    // The second window is focused; clicking the first window directly moves
    // both the window manager's focus and the bar's highlight to it.
    shell.handle(moved(150, 150), &mut comp);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);
    assert!(matches!(
        outcome,
        ShellOutcome::WindowManager(InputResponse::Activated { window, .. }) if window == first
    ));
    assert_eq!(shell.router().focused(), Some(first));
    assert_eq!(
        shell.session().taskbar().tasks().focused(),
        Some(first_task),
        "the bar highlight followed the direct click"
    );
    let _ = second;
}

#[test]
fn pressing_the_desktop_clears_the_bar_highlight() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = shell
        .open_window(&mut comp, Point::new(300, 200), app_surface(), "Editor")
        .expect("opens");
    assert!(shell.session().taskbar().tasks().focused().is_some());

    // A press on empty desktop drops window-manager focus and clears the
    // highlight, leaving the task listed.
    shell.handle(moved(700, 400), &mut comp);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::WindowManager(InputResponse::DesktopPressed)
    );
    assert_eq!(shell.session().taskbar().tasks().focused(), None);
    assert!(
        shell.tasks().task_for(window).is_some(),
        "the task is still listed"
    );
}

#[test]
fn the_bridge_maps_windows_to_tasks_both_ways() {
    let mut bridge = TaskBridge::new();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let mut session = session();

    let window = bridge
        .open(
            &mut comp,
            &mut router,
            session.taskbar_mut(),
            Point::new(10, 10),
            app_surface(),
            "App",
        )
        .expect("opens");
    let task = bridge.task_for(window).expect("tracked");
    assert_eq!(bridge.window_for(task), Some(window));
    assert_eq!(bridge.len(), 1);
}

#[test]
fn activating_an_unknown_task_changes_nothing() {
    let bridge = TaskBridge::new();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    assert!(!bridge.activate(
        &mut comp,
        &mut router,
        TaskId(999),
        ActivateOutcome::Activated
    ));
    assert!(!bridge.activate(
        &mut comp,
        &mut router,
        TaskId(999),
        ActivateOutcome::Unknown
    ));
}

#[test]
fn syncing_focus_to_an_untracked_window_leaves_the_highlight() {
    let mut bridge = TaskBridge::new();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let mut session = session();
    let tracked = bridge
        .open(
            &mut comp,
            &mut router,
            session.taskbar_mut(),
            Point::new(10, 10),
            app_surface(),
            "App",
        )
        .expect("opens");
    let task = bridge.task_for(tracked).expect("tracked");
    assert_eq!(session.taskbar().tasks().focused(), Some(task));

    // A window the bridge never tracked (the bar's own surface, say) does not
    // disturb the highlighted task.
    let stranger = opaque_window(&mut comp, Point::new(500, 500), 100, 100);
    bridge.sync_focus(session.taskbar_mut(), Some(stranger));
    assert_eq!(session.taskbar().tasks().focused(), Some(task));

    // Clearing focus (a desktop press) does drop it.
    bridge.sync_focus(session.taskbar_mut(), None);
    assert_eq!(session.taskbar().tasks().focused(), None);
}
