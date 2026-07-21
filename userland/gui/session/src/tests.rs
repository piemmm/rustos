//! Headless unit tests for the desktop session glue.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_abi::Errno;
use tairix_cursor::CursorTheme;
use tairix_icon::{IconKind, IconSet};
use tairix_taskbar::{
    ActivateOutcome, MenuAction, MenuEntryId, SessionControl, TaskId, TaskbarConfig,
    TaskbarRenderer, TaskbarResponse,
};
use tairix_theme::{Appearance, CursorKind, Metrics, Theme, ThemeError, ThemeId};
use tairix_wm::{
    Color, Compositor, Corners, InputEvent, InputResponse, Point, PointerButton, Scale, Surface,
    WindowActivationState, WindowId,
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
            taskbar_corner_radius,
            ..*base.metrics()
        },
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        base.contrast(),
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
fn selecting_the_appearance_toggle_recolours_the_desktop() {
    let mut shell = shell();
    // Build the compositor over the active (dark) theme's desktop colour,
    // exactly as the live session binary does at bring-up.
    let mode = DisplayMode {
        width_px: 1920,
        height_px: 1080,
        stride_bytes: 1920 * 4,
        format: DisplayFormat::Rgba8888,
    };
    let mut comp =
        Compositor::new(mode, shell.desktop_background()).expect("the compositor allocates");
    let dark = comp.background();
    let start = start_button_point(shell.session());

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
    shell
        .pump(
            &mut MemoryInput::new(&[
                moved(toggle_row.left() + 1, toggle_row.top() + 1),
                PRIMARY_PRESS,
            ]),
            &mut comp,
        )
        .expect("source does not fault");

    assert_ne!(
        comp.background(),
        dark,
        "the toggle recoloured the desktop behind the windows"
    );
    assert_eq!(
        comp.background(),
        shell.desktop_background(),
        "the compositor background tracks the active theme's desktop colour"
    );
    comp.composite();
    assert_eq!(
        comp.back_buffer().get(0, 0),
        Some(comp.background().premultiply()),
        "the recomposed desktop pixel shows the new theme"
    );
}

#[test]
fn sync_background_relays_a_programmatic_theme_switch() {
    let mut shell = shell();
    let mode = DisplayMode {
        width_px: 1920,
        height_px: 1080,
        stride_bytes: 1920 * 4,
        format: DisplayFormat::Rgba8888,
    };
    let mut comp =
        Compositor::new(mode, shell.desktop_background()).expect("the compositor allocates");

    assert!(
        !shell.sync_background(&mut comp),
        "a compositor built over the active theme is already in step"
    );

    shell.session_mut().toggle_appearance();
    assert!(shell.sync_background(&mut comp));
    assert_eq!(comp.background(), shell.desktop_background());
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

// ---- desktop shell: the pointer cursor ----

#[test]
fn refresh_cursor_installs_the_pointer_from_the_first_frame() {
    let mut shell = shell();
    let mut comp = compositor();

    assert!(
        comp.cursor_bounds().is_none(),
        "no pointer is shown before the shell installs one"
    );

    shell.refresh_cursor(&mut comp);

    assert!(
        comp.cursor_bounds().is_some(),
        "the shell installs a pointer cursor so the desktop shows one"
    );
    assert_eq!(
        shell.cursor().kind(),
        CursorKind::Arrow,
        "the plain arrow shows over the empty desktop"
    );
}

#[test]
fn the_pointer_tracks_motion() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.refresh_cursor(&mut comp);
    let before = comp.cursor_bounds().expect("the pointer is shown");

    shell.handle(moved(300, 400), &mut comp);

    let after = comp.cursor_bounds().expect("the pointer is still shown");
    assert_eq!(shell.router().pointer(), Point::new(300, 400));
    assert_eq!(
        (after.left() - before.left(), after.top() - before.top()),
        (300, 400),
        "the cursor hotspot moved with the pointer"
    );
}

#[test]
fn the_pointer_shape_follows_the_window_under_it() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);
    assert!(comp.set_window_cursor(window, CursorKind::Text));

    shell.handle(moved(250, 250), &mut comp);
    assert_eq!(
        shell.cursor().kind(),
        CursorKind::Text,
        "over the window the pointer takes the window's cursor hint"
    );

    shell.handle(moved(900, 500), &mut comp);
    assert_eq!(
        shell.cursor().kind(),
        CursorKind::Arrow,
        "back over the desktop it is the plain arrow"
    );
}

#[test]
fn set_cursors_installs_loaded_artwork_and_keeps_the_pointer_shown() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.refresh_cursor(&mut comp);
    let mut reader =
        MemoryAssets::default().with("/System/Graphics/Cursors/cursor.arrow.svg", VALID_SVG);
    let theme = shell.session().load_cursors(&mut reader);

    shell.set_cursors(theme, &mut comp);

    assert_eq!(
        shell.cursor().registry().active_id().name(),
        "desktop",
        "the loaded cursor set is active, replacing the built-in one"
    );
    assert!(
        comp.cursor_bounds().is_some(),
        "swapping the artwork re-renders the pointer rather than blanking it"
    );
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

/// The activation state of `window`'s decoration frame.
fn activation(comp: &Compositor, window: WindowId) -> WindowActivationState {
    comp.window_frame(window)
        .expect("the window is decorated")
        .furniture()
        .activation
}

/// Open and decorate a served application window, exactly as
/// `ShellWindowHost::window_opened` does in the live serve loop: the shell
/// opens the bare window, then the window manager dresses it with frame
/// furniture.
fn open_app(
    shell: &mut DesktopShell,
    comp: &mut Compositor,
    origin: Point,
    title: &str,
) -> WindowId {
    let window = shell
        .open_window(comp, origin, app_surface(), title)
        .expect("opens");
    assert!(
        shell.decorate_window(comp, window, title, false),
        "a served application window is decorated"
    );
    window
}

#[test]
fn open_window_decorates_the_window_with_a_titled_frame() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");

    // The window manager owns the frame; the channel title labels its title bar.
    let frame = comp.window_frame(window).expect("the window is decorated");
    assert_eq!(frame.title_bar().title(), "Editor");
    let furniture = frame.furniture();
    assert!(furniture.movable, "the title bar can move the window");
    assert!(
        !furniture.resizable,
        "the default app presents a fixed-size window"
    );
    // Freshly opened and focused, so its frame shows active.
    assert_eq!(furniture.activation, WindowActivationState::Active);

    // The decoration reserves a band *around* the content: the outer bounds
    // grow, the client insets, and the client never covers the furniture.
    let client = comp.window_client_rect(window).expect("client");
    let outer = comp.window(window).expect("live").bounds();
    assert!(
        outer.width > client.width && outer.height > client.height,
        "the reserved frame band grows the outer bounds"
    );
    assert_eq!(
        (client.width, client.height),
        (app_surface().width(), app_surface().height()),
        "the client keeps the app's requested content size"
    );
}

#[test]
fn the_active_frame_follows_the_focused_window() {
    let mut shell = shell();
    let mut comp = compositor();
    let first = open_app(&mut shell, &mut comp, Point::new(100, 100), "First");
    let second = open_app(&mut shell, &mut comp, Point::new(900, 100), "Second");

    // The most-recently opened window holds focus and shows the active frame;
    // the other is inactive. Exactly one active frame at a time.
    assert_eq!(activation(&comp, second), WindowActivationState::Active);
    assert_eq!(activation(&comp, first), WindowActivationState::Inactive);

    // A direct click on the first window's content moves focus, and the active
    // frame follows it.
    shell.handle(moved(150, 150), &mut comp);
    shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(shell.router().focused(), Some(first));
    assert_eq!(activation(&comp, first), WindowActivationState::Active);
    assert_eq!(activation(&comp, second), WindowActivationState::Inactive);
}

#[test]
fn minimizing_or_closing_the_focused_window_leaves_no_active_frame() {
    let mut shell = shell();
    let mut comp = compositor();
    let a = open_app(&mut shell, &mut comp, Point::new(100, 100), "A");
    let b = open_app(&mut shell, &mut comp, Point::new(900, 100), "B");

    // Minimizing the focused window drops focus and deactivates its frame; no
    // other window becomes active.
    assert!(shell.minimize_window(&mut comp, b));
    assert_eq!(shell.router().focused(), None);
    assert_eq!(activation(&comp, b), WindowActivationState::Inactive);
    assert_eq!(activation(&comp, a), WindowActivationState::Inactive);

    // Focusing then closing a window likewise leaves no active frame.
    shell.handle(moved(150, 150), &mut comp);
    shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(activation(&comp, a), WindowActivationState::Active);
    assert!(shell.close_window(&mut comp, a));
    assert_eq!(shell.router().focused(), None);
    assert_eq!(activation(&comp, b), WindowActivationState::Inactive);
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

/// The AW3 QEMU vertical's full click-through, replayed on the host with
/// the production shell construction and the ramfb console geometry: pin
/// → start button (menu opens) → "Files" row (launch) → the served
/// window (activate) → start button (menu reopens) → the appearance
/// toggle (theme switches) → the window again (activate). Each staged
/// outcome must appear exactly as the vertical's marker chain assumes,
/// so a routing regression fails here in milliseconds, never as a QEMU
/// timeout.
#[test]
#[allow(clippy::too_many_lines)] // One linear replay of the whole staged click-through.
fn aw3_click_through_produces_the_staged_outcomes() {
    const WIDTH: u32 = 1024;
    const HEIGHT: u32 = 768;
    let mut shell = DesktopShell::new(TaskbarConfig::bottom_bar(WIDTH, HEIGHT), LABEL);
    let files_id = tairix_taskbar::LauncherId(1);
    let _ = shell
        .session_mut()
        .taskbar_mut()
        .start_menu_mut()
        .add_launcher(files_id, "Files");
    let mode = DisplayMode {
        width_px: WIDTH,
        height_px: HEIGHT,
        stride_bytes: WIDTH * 4,
        format: DisplayFormat::Rgba8888,
    };
    let mut comp = Compositor::new(mode, Color::rgb(0, 0, 0)).expect("compositor");

    let centre = |rect: tairix_wm::Rect| -> Point {
        assert!(!rect.is_empty());
        #[allow(clippy::cast_possible_wrap)]
        Point::new(
            rect.left() + (rect.width / 2) as i32,
            rect.top() + (rect.height / 2) as i32,
        )
    };
    let start = centre(shell.session().taskbar().layout(Scale::ONE).start_button);
    let row = |shell: &DesktopShell, label: &str| -> Point {
        let index = shell
            .session()
            .taskbar()
            .start_menu()
            .entries()
            .iter()
            .position(|e| e.label() == label)
            .expect("labelled row");
        centre(shell.session().taskbar().menu_layout(Scale::ONE).entries[index])
    };
    let files_row = row(&shell, "Files");
    let toggle_row = row(&shell, LABEL);

    let click = |shell: &mut DesktopShell, comp: &mut Compositor, at: Point| -> Vec<ShellOutcome> {
        vec![
            shell.handle(moved(at.x, at.y), comp),
            shell.handle(PRIMARY_PRESS, comp),
            shell.handle(
                InputEvent::PointerReleased {
                    button: PointerButton::Primary,
                },
                comp,
            ),
        ]
    };

    // Start button: the menu opens.
    let outcomes = click(&mut shell, &mut comp, start);
    assert!(
        outcomes.contains(&ShellOutcome::Session(SessionEvent::Forward(
            TaskbarResponse::StartMenuToggled { open: true }
        ))),
        "start click must open the menu, got {outcomes:?}"
    );

    // The "Files" row: the launcher fires and the menu closes.
    let outcomes = click(&mut shell, &mut comp, files_row);
    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            ShellOutcome::Session(SessionEvent::Forward(TaskbarResponse::MenuEntrySelected {
                action: MenuAction::Launch(id),
                ..
            })) if *id == files_id
        )),
        "files-row click must select the launcher, got {outcomes:?}"
    );
    assert!(!shell.session().taskbar().start_menu().is_open());

    // The spawned app's window opens exactly as the production serve
    // path opens it: through the shell (composited window + taskbar
    // task + running-task bookkeeping), at the session's cascade
    // origin, sized as the shipped file manager sizes itself.
    let origin = Point::new(
        crate::windows::CASCADE_ORIGIN,
        crate::windows::CASCADE_ORIGIN,
    );
    let surface =
        Surface::filled(480, 320, Color::rgb(0x20, 0x20, 0x24).premultiply()).expect("surface");
    let window = shell
        .open_window(&mut comp, origin, surface, "Files")
        .expect("the served window opens");
    // The window manager decorates the served window, exactly as
    // `ShellWindowHost::window_opened` does in the live serve loop — the app
    // itself draws no chrome.
    assert!(shell.decorate_window(&mut comp, window, "Files", false));
    assert_eq!(
        comp.window_frame(window)
            .expect("the presented window is decorated")
            .title_bar()
            .title(),
        "Files"
    );
    let in_window = Point::new(origin.x + 240, origin.y + 160);

    // Clicking the window activates it (the session delivers Focus +
    // Pressed app-ward — the vertical's second and third witnesses).
    let outcomes = click(&mut shell, &mut comp, in_window);
    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            ShellOutcome::WindowManager(InputResponse::Activated { window: w, .. }) if *w == window
        )),
        "window click must activate the served window, got {outcomes:?}"
    );

    // Start button again: the menu reopens.
    let outcomes = click(&mut shell, &mut comp, start);
    assert!(
        outcomes.contains(&ShellOutcome::Session(SessionEvent::Forward(
            TaskbarResponse::StartMenuToggled { open: true }
        ))),
        "second start click must reopen the menu, got {outcomes:?}"
    );

    // The appearance toggle: the theme switches and the menu closes.
    let outcomes = click(&mut shell, &mut comp, toggle_row);
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, ShellOutcome::Session(SessionEvent::AppearanceChanged(_)))),
        "toggle click must switch the appearance, got {outcomes:?}"
    );
    assert!(!shell.session().taskbar().start_menu().is_open());

    // The window once more: activated again (the vertical's final
    // delivery, keying the light-theme screendump and the guest PASS).
    let outcomes = click(&mut shell, &mut comp, in_window);
    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            ShellOutcome::WindowManager(InputResponse::Activated { window: w, .. }) if *w == window
        )),
        "post-toggle window click must activate the window, got {outcomes:?}"
    );
}

// --- The trusted file picker (plans/APPWIN.md AW5, CU6) -----------------

use crate::picker::{PickConclusion, PickerSlot, SessionPicker, PICKER_ORIGIN};
use tairix_abi::input::{KeyInput, KeyValue, Modifiers, NamedKeyCode};
use tairix_browse::render::row_height;
use tairix_browse::{DirectorySource, Entry};

/// An in-memory directory tree keyed by the joined component path, the
/// picker's stand-in for the session-authority VFS listing.
struct TreeSource {
    dirs: alloc::collections::BTreeMap<String, Vec<Entry>>,
}

impl TreeSource {
    /// `/` holding `Docs/` (with `notes.txt`) and the file `readme.md`.
    fn fixture() -> Self {
        let mut dirs = alloc::collections::BTreeMap::new();
        dirs.insert(
            String::new(),
            vec![Entry::directory("Docs"), Entry::file("readme.md")],
        );
        dirs.insert(String::from("Docs"), vec![Entry::file("notes.txt")]);
        Self { dirs }
    }
}

impl DirectorySource for TreeSource {
    fn list(&mut self, components: &[String]) -> Result<Vec<Entry>, Errno> {
        let key = components.join("/");
        self.dirs.get(&key).cloned().ok_or(Errno::NotFound)
    }
}

/// A source refusing every listing, standing in for a session whose
/// filesystem reach was stripped.
struct RefusingSource;

impl DirectorySource for RefusingSource {
    fn list(&mut self, _components: &[String]) -> Result<Vec<Entry>, Errno> {
        Err(Errno::PermissionDenied)
    }
}

fn picker_desktop() -> (DesktopShell, Compositor) {
    let shell = DesktopShell::new(TaskbarConfig::bottom_bar(640, 480), LABEL);
    let mode = DisplayMode {
        width_px: 640,
        height_px: 480,
        stride_bytes: 640 * 4,
        format: DisplayFormat::Rgba8888,
    };
    let compositor = Compositor::new(mode, Color::rgb(0, 0, 0)).expect("compositor builds");
    (shell, compositor)
}

fn pressed(key: KeyValue) -> KeyInput {
    KeyInput::Pressed {
        key,
        modifiers: Modifiers::default(),
    }
}

/// `begin` opens the picker window at its fixed origin; the single slot
/// refuses a second pick while one is showing.
#[test]
fn picker_begin_opens_one_window_and_enforces_the_single_slot() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");
    let wm = picker.wm_id().expect("a picker window is showing");
    assert_eq!(
        comp.window(wm).expect("live").origin(),
        PICKER_ORIGIN,
        "the picker is placed at its one deterministic origin"
    );
    assert_eq!(
        picker.begin(9, &mut shell, &mut comp),
        Err(Errno::AlreadyExists),
        "one picker at a time"
    );
}

/// A refused root listing refuses the pick verbatim and leaves the slot
/// idle for a later request.
#[test]
fn picker_begin_fails_closed_when_the_listing_is_refused() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(|| RefusingSource);
    assert_eq!(
        picker.begin(7, &mut shell, &mut comp),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(picker.wm_id(), None, "nothing half-open remains");
}

/// Enter descends into a selected directory and chooses a selected file,
/// concluding with the shared absolute-path spelling and closing the
/// picker window.
#[test]
fn picker_keys_navigate_and_choose_the_selected_file() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");
    let wm = picker.wm_id().expect("showing");

    // Enter on the selected `Docs/` descends; Enter on `notes.txt`
    // concludes the pick.
    let enter = pressed(KeyValue::Named(NamedKeyCode::Enter));
    assert_eq!(picker.handle_key(&enter, &mut shell, &mut comp), None);
    let concluded = picker
        .handle_key(&enter, &mut shell, &mut comp)
        .expect("choosing a file concludes");
    assert_eq!(concluded.for_window, 7);
    assert_eq!(
        concluded.conclusion,
        PickConclusion::Chosen(String::from("/Docs/notes.txt"))
    );
    assert_eq!(picker.wm_id(), None, "the picker window is closed");
    assert!(comp.window(wm).is_none(), "and gone from the compositor");
}

/// Down then Enter chooses the second root entry (the file) without
/// descending; Backspace climbs back up after a descent.
#[test]
fn picker_selection_and_climb_track_the_browser() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");

    let down = pressed(KeyValue::Named(NamedKeyCode::Down));
    let enter = pressed(KeyValue::Named(NamedKeyCode::Enter));
    assert_eq!(picker.handle_key(&down, &mut shell, &mut comp), None);
    let concluded = picker
        .handle_key(&enter, &mut shell, &mut comp)
        .expect("the root file concludes");
    assert_eq!(
        concluded.conclusion,
        PickConclusion::Chosen(String::from("/readme.md"))
    );
}

/// A primary click resolves through the one shared hit-test: the first
/// entry row descends into `Docs/`, and the file row inside concludes.
#[test]
fn picker_clicks_resolve_rows_through_the_shared_hit_test() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");

    let row = i32::try_from(row_height()).expect("a small row height");
    // The first entry row sits directly below the path bar.
    let first_row = Point::new(4, row);
    assert_eq!(
        picker.handle_click(first_row, &mut shell, &mut comp),
        None,
        "a directory row descends without concluding"
    );
    let concluded = picker
        .handle_click(first_row, &mut shell, &mut comp)
        .expect("the file row concludes");
    assert_eq!(
        concluded.conclusion,
        PickConclusion::Chosen(String::from("/Docs/notes.txt"))
    );
    // A click on the path bar concludes nothing.
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");
    assert_eq!(
        picker.handle_click(Point::new(4, 0), &mut shell, &mut comp),
        None
    );
}

/// Escape cancels: the conclusion is `Cancelled`, the window is closed,
/// and the slot is free for the next pick.
#[test]
fn picker_escape_cancels_and_frees_the_slot() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");
    let escape = pressed(KeyValue::Named(NamedKeyCode::Escape));
    let concluded = picker
        .handle_key(&escape, &mut shell, &mut comp)
        .expect("escape concludes");
    assert_eq!(concluded.for_window, 7);
    assert_eq!(concluded.conclusion, PickConclusion::Cancelled);
    assert_eq!(picker.wm_id(), None);
    picker
        .begin(9, &mut shell, &mut comp)
        .expect("the slot is free again");
}

/// `abort_for` takes the picker down only for its own requesting window,
/// delivering no conclusion.
#[test]
fn picker_abort_is_scoped_to_the_requesting_window() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");

    // A different window's death leaves the pick showing.
    picker.abort_for(9, &mut shell, &mut comp);
    assert!(picker.wm_id().is_some());
    // The requesting window's death takes it down.
    picker.abort_for(7, &mut shell, &mut comp);
    assert_eq!(picker.wm_id(), None);
}
