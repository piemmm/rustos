//! Headless unit tests for the desktop session glue.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_abi::notify_ipc::{NotifyBody, NotifyRequest, NotifySeverity, NotifyTitle};
use tairix_abi::{
    AppInfoHeader, Errno, ABI_VERSION_CURRENT, APPINFO_MAGIC, APPINFO_WIRE_MAX, BUNDLE_ID_MAX,
    BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX, LIBRARY_ICON_MAX, SYSCALL_TABLE_HASH_LEN,
};
use tairix_controls::PointerState;
use tairix_cursor::CursorTheme;
use tairix_icon::{IconKind, IconSet};
use tairix_proglib::{
    user_library_path, BundlePath, Catalog, DisplayName, EntryId, IconAsset, LibraryCategory,
    LibraryEntry, MACHINE_LIBRARY_PATH, MAX_CATALOG_LEN,
};
use tairix_taskbar::{
    ActivateOutcome, LibraryRow, PinView, TaskId, TaskbarConfig, TaskbarRenderer, TaskbarResponse,
};
use tairix_theme::{Appearance, CursorKind, Metrics, Theme, ThemeError, ThemeId};
use tairix_wm::{
    Color, Compositor, Corners, InputEvent, InputResponse, Point, PointerButton, Scale, Surface,
    WindowActivationState, WindowId,
};

use crate::{
    load_icon_set, load_library, DesktopSession, DesktopShell, InputSource, PinBridge,
    PinEditError, PinIconSource, PinService, SessionFileReader, SessionFileWriter,
    SessionInputResponse, SessionInputRouter, SessionPins, ShellOutcome, TaskBridge,
    TaskbarPresenter,
};
use tairix_window::PinDecision;

/// A valid SVG asset (a single filled triangle on a square grid) that decodes
/// to a non-empty vector form, so loading it is observably different from the
/// built-in fallback.
const VALID_SVG: &[u8] = br##"<svg viewBox="0 0 24 24">
    <polygon points="2,2 22,2 12,22" fill="#ffaa00"/>
</svg>"##;

/// Bytes that are not a decodable SVG document at all, so the per-kind decode
/// returns an error and the loader falls back to the built-in artwork.
const MALFORMED_SVG: &[u8] = b"this is not an SVG document";

/// An in-memory [`SessionFileReader`]: a path→bytes table standing in for
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

impl SessionFileReader for MemoryAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, bytes)| bytes.clone())
            .ok_or(Errno::NotFound)
    }
}

/// An in-memory [`SessionFileWriter`]: a path→bytes table recording writes,
/// with an optional forced error to exercise the refusal paths.
#[derive(Default)]
struct MemoryWriter {
    files: BTreeMap<String, Vec<u8>>,
    force_error: Option<Errno>,
}

impl MemoryWriter {
    fn with_error(mut self, error: Errno) -> Self {
        self.force_error = Some(error);
        self
    }

    fn written(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }
}

impl SessionFileReader for &mut MemoryAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        (**self).read(path)
    }
}

impl SessionFileWriter for MemoryWriter {
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
        if let Some(err) = self.force_error {
            return Err(err);
        }
        self.files.insert(String::from(path), bytes.to_vec());
        Ok(())
    }
}

impl SessionFileWriter for &mut MemoryWriter {
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
        (**self).write(path, bytes)
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
    DesktopSession::new(TaskbarConfig::bottom_bar(1920, 1080))
}

// ---- fixtures --------------------------------------------------------

/// A validated catalog entry for `/Apps/<stem>.app`.
fn entry(stem: &str, name: &str, category: LibraryCategory) -> LibraryEntry {
    LibraryEntry::new(
        EntryId::new(&alloc::format!("os.tairix.{stem}")).expect("id"),
        DisplayName::new(name).expect("name"),
        BundlePath::new(&alloc::format!("/Apps/{stem}.app")).expect("bundle"),
        category,
        None,
    )
}

/// A catalog declaring one entry per `(stem, name, category)` triple.
fn catalog(entries: &[(&str, &str, LibraryCategory)]) -> Catalog {
    let mut catalog = Catalog::new();
    for &(stem, name, category) in entries {
        catalog.insert(entry(stem, name, category)).expect("fits");
    }
    catalog
}

/// The standard fixture: two Office entries and one Games entry, so the
/// popup shows two folders (taxonomy order: Office before Games) and the
/// Office names sort as Calc before Write.
fn office_and_games() -> Catalog {
    catalog(&[
        ("write", "Write", LibraryCategory::Office),
        ("calc", "Calc", LibraryCategory::Office),
        ("chess", "Chess", LibraryCategory::Games),
    ])
}

/// Move the pointer to `(x, y)` and press the primary button there.
fn press_at(
    router: &mut SessionInputRouter,
    comp: &mut Compositor,
    taskbar: &mut tairix_taskbar::Taskbar,
    x: i32,
    y: i32,
) -> SessionInputResponse {
    router.handle(
        InputEvent::PointerMoved {
            to: Point::new(x, y),
        },
        comp,
        taskbar,
    );
    router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        comp,
        taskbar,
    )
}

/// Open the popup by pressing the Library button, asserting it opened.
fn open_library(
    router: &mut SessionInputRouter,
    comp: &mut Compositor,
    taskbar: &mut tairix_taskbar::Taskbar,
) {
    let centre = Point::new(24, 1060); // library slot centre for 1920x1080 bottom bar
    assert_eq!(
        press_at(router, comp, taskbar, centre.x, centre.y),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
    assert!(taskbar.library().is_open());
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

    presenter.present(&mut comp, &mut renderer, session.taskbar());

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

    presenter.present(&mut comp, &mut renderer, session.taskbar());
    let first = presenter.bar_window().expect("first present");
    presenter.present(&mut comp, &mut renderer, session.taskbar());
    let second = presenter.bar_window().expect("second present");

    assert_eq!(first, second, "the same window is reused");
    assert_eq!(comp.window_count(), 1, "no second bar window is created");
}

#[test]
fn opening_the_popup_presents_a_popup_window() {
    let mut session = session();
    session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    open_library(&mut router, &mut comp, session.taskbar_mut());

    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(&mut comp, &mut renderer, session.taskbar());

    let popup = presenter.popup_window().expect("the popup was presented");
    assert_eq!(comp.window_count(), 2, "bar and popup are both present");

    let layout = session.taskbar().library_layout(Scale::ONE);
    let window = comp.window(popup).expect("the popup window exists");
    assert_eq!(window.origin(), layout.panel.origin);
    assert_eq!(window.corners(), Corners::from_radius(layout.corner_radius));
}

#[test]
fn closing_the_popup_removes_the_popup_window() {
    let mut session = session();
    session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    open_library(&mut router, &mut comp, session.taskbar_mut());

    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(&mut comp, &mut renderer, session.taskbar());
    let popup = presenter.popup_window().expect("the popup is open");

    let centre = Point::new(24, 1060); // library slot centre
    press_at(
        &mut router,
        &mut comp,
        session.taskbar_mut(),
        centre.x,
        centre.y,
    );
    assert!(!session.taskbar().library().is_open());

    presenter.present(&mut comp, &mut renderer, session.taskbar());

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
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    open_library(&mut router, &mut comp, session.taskbar_mut());

    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(&mut comp, &mut renderer, session.taskbar());
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

    presenter.present(&mut comp, &mut renderer, session.taskbar());
    let first = presenter.bar_window().expect("first present");

    assert!(comp.remove(first), "an embedder removed the bar window");
    presenter.present(&mut comp, &mut renderer, session.taskbar());

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
        .unwrap();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new();
    let mut presenter = TaskbarPresenter::new();

    presenter.present(&mut comp, &mut renderer, session.taskbar());
    let id = presenter.bar_window().expect("the bar was presented");
    let dark_radius = session.taskbar().layout(Scale::ONE).corner_radius;
    assert_eq!(
        comp.window(id).expect("the bar window").corners(),
        Corners::Rounded {
            radius: dark_radius
        }
    );

    session.set_theme(ThemeId(100)).unwrap();
    presenter.present(&mut comp, &mut renderer, session.taskbar());

    let light_radius = session.taskbar().layout(Scale::ONE).corner_radius;
    assert_ne!(dark_radius, light_radius);
    assert_eq!(
        comp.window(id).expect("the same bar window").corners(),
        Corners::Rounded {
            radius: light_radius
        },
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

#[test]
fn new_starts_dark_with_an_empty_library() {
    let session = session();
    assert_eq!(session.active_theme().id(), ThemeId::DARK);
    assert!(session.taskbar().library().catalog().is_empty());
    assert!(!session.taskbar().library().is_open());
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
        session.taskbar().layout(Scale::ONE).corner_radius,
        99,
        "set_theme relays the new metrics to the taskbar"
    );

    session.set_theme(ThemeId::LIGHT).expect("light theme");
    assert_eq!(session.active_theme().id(), ThemeId::LIGHT);
    assert_eq!(
        session.taskbar().layout(Scale::ONE).corner_radius,
        Theme::light().metrics().taskbar_corner_radius,
        "switching theme relays the light metrics to the taskbar"
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

#[test]
fn primary_press_over_the_bar_routes_to_the_taskbar() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();

    // The files button centre (pressed first: a press on the library
    // button opens the modal popup, after which any bar press is the
    // popup's to dismiss).
    let response = press_at(&mut router, &mut comp, session.taskbar_mut(), 72, 1060);
    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::OpenFiles)
    );

    // The library button centre.
    let response = press_at(&mut router, &mut comp, session.taskbar_mut(), 24, 1060);
    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

#[test]
fn the_bar_wins_over_a_window_beneath_it() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    // A window placed under the bottom bar must not steal a press on the bar.
    opaque_window(&mut comp, Point::new(0, 1000), 400, 80);

    let response = press_at(&mut router, &mut comp, session.taskbar_mut(), 24, 1060);

    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

#[test]
fn primary_press_over_a_window_routes_to_the_window_manager() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    let response = press_at(&mut router, &mut comp, session.taskbar_mut(), 250, 250);

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
fn secondary_press_over_a_window_routes_to_the_window_manager() {
    // A right-click over a window must reach the window manager (which
    // delivers it to the client so it can open its context menu) — the
    // session router must not swallow it, as its catch-all once did.
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
            button: PointerButton::Secondary,
        },
        &mut comp,
        session.taskbar_mut(),
    );

    assert_eq!(
        response,
        SessionInputResponse::WindowManager(InputResponse::SecondaryActivated {
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

    let response = press_at(&mut router, &mut comp, session.taskbar_mut(), 900, 500);

    assert_eq!(
        response,
        SessionInputResponse::WindowManager(InputResponse::DesktopPressed)
    );
}

#[test]
fn the_open_popup_is_modal_and_a_press_off_it_dismisses_it() {
    let mut session = session();
    session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    // A window beneath the popup's click-away press; it must stay unfocused.
    let _window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    open_library(&mut router, &mut comp, session.taskbar_mut());
    assert!(session.taskbar().library().is_open());

    // A press over a window beneath is claimed by the modal popup and
    // dismisses it, rather than reaching the window manager.
    let response = press_at(&mut router, &mut comp, session.taskbar_mut(), 250, 250);
    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::LibraryDismissed)
    );
    assert!(!session.taskbar().library().is_open());
    assert_eq!(
        router.focused(),
        None,
        "the window beneath was not activated"
    );

    // While open, a KeyPressed routes to the popup.
    open_library(&mut router, &mut comp, session.taskbar_mut());
    let response = router.handle(
        InputEvent::KeyPressed {
            key: tairix_wm::Key::Named(tairix_wm::NamedKey::Down),
            modifiers: tairix_wm::Modifiers::default(),
        },
        &mut comp,
        session.taskbar_mut(),
    );
    assert_eq!(response, SessionInputResponse::Ignored);
    assert_eq!(session.taskbar().library().current(), Some(0));

    // Escape closes the popup.
    let response = router.handle(
        InputEvent::KeyPressed {
            key: tairix_wm::Key::Named(tairix_wm::NamedKey::Escape),
            modifiers: tairix_wm::Modifiers::default(),
        },
        &mut comp,
        session.taskbar_mut(),
    );
    assert_eq!(
        response,
        SessionInputResponse::Taskbar(TaskbarResponse::LibraryDismissed)
    );
    assert!(!session.taskbar().library().is_open());

    // A PointerScrolled while open does NOT reach the window manager.
    open_library(&mut router, &mut comp, session.taskbar_mut());
    let response = router.handle(
        InputEvent::PointerScrolled { dx: 0, dy: 10 },
        &mut comp,
        session.taskbar_mut(),
    );
    assert_eq!(response, SessionInputResponse::Ignored);
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

    router.handle(
        InputEvent::PointerMoved {
            to: Point::new(24, 1060),
        },
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
        !session.taskbar().library().is_open(),
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
    DesktopShell::new(TaskbarConfig::bottom_bar(1920, 1080))
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRIMARY_PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};

const SECONDARY_PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Secondary,
};

#[test]
fn pump_opens_the_popup_and_presents_it() {
    let mut shell = shell();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    let mut comp = compositor();

    let outcomes = shell
        .pump(
            &mut MemoryInput::new(&[moved(24, 1060), PRIMARY_PRESS]),
            &mut comp,
        )
        .expect("an in-memory source does not fault");

    assert_eq!(
        outcomes,
        [
            ShellOutcome::Ignored,
            ShellOutcome::Taskbar(TaskbarResponse::OpenLibrary),
        ]
    );
    assert!(shell.session().taskbar().library().is_open());
    assert!(
        shell.presenter().popup_window().is_some(),
        "opening the popup re-presents and adds the popup window"
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
fn sync_background_relays_a_programmatic_theme_switch() {
    let mut shell = shell();
    let mode = DisplayMode {
        width_px: 1920,
        height_px: 1080,
        stride_bytes: 1920 * 4,
        format: DisplayFormat::Rgba8888,
    };
    let mut comp = Compositor::new(
        mode,
        shell.session().active_theme().palette().desktop.into(),
    )
    .expect("the compositor allocates");

    assert!(
        !shell.sync_background(&mut comp),
        "a compositor built over the active theme is already in step"
    );

    shell.session_mut().set_theme(ThemeId::LIGHT).unwrap();
    assert!(shell.sync_background(&mut comp));
    assert_eq!(
        comp.background(),
        shell.session().active_theme().palette().desktop.into()
    );
}

#[test]
fn pump_propagates_a_source_fault_after_applying_prior_events() {
    let mut shell = shell();
    let mut comp = compositor();

    let result = shell.pump(
        &mut MemoryInput::faulting(&[moved(24, 1060), PRIMARY_PRESS], Errno::NotFound),
        &mut comp,
    );

    assert_eq!(result, Err(Errno::NotFound));
    assert!(
        shell.session().taskbar().library().is_open(),
        "the event drained before the fault was still applied"
    );
}

#[test]
fn motion_is_ignored_and_does_not_present_the_bar() {
    let mut shell = shell();
    let mut comp = compositor();

    // Motion over the empty desktop (far from the bar).
    let outcomes = shell
        .pump(&mut MemoryInput::new(&[moved(900, 500)]), &mut comp)
        .expect("source does not fault");

    assert_eq!(outcomes, [ShellOutcome::Ignored]);
    assert!(
        shell.presenter().bar_window().is_none(),
        "motion does not present the bar"
    );

    // Motion onto the library button.
    let outcomes = shell
        .pump(&mut MemoryInput::new(&[moved(24, 1060)]), &mut comp)
        .expect("source does not fault");
    assert_eq!(outcomes, [ShellOutcome::Ignored]);

    // Verify hover feedback happened (the bar was presented).
    assert!(shell.presenter().bar_window().is_some());
    assert_eq!(
        shell.session().taskbar().library_button().state().pointer,
        PointerState::Hover
    );
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
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    // Move onto the library button and press.
    shell.handle(moved(24, 1060), &mut comp);
    shell.handle(PRIMARY_PRESS, &mut comp);
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
        ShellOutcome::Taskbar(TaskbarResponse::TaskActivated {
            id: task,
            outcome: ActivateOutcome::Minimised,
        })
    );
    assert!(!comp.window(window).expect("still tracked").is_visible());
    assert_eq!(shell.router().focused(), None);

    // A second click restores and re-focuses it.
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::TaskActivated {
            id: task,
            outcome: ActivateOutcome::Activated,
        })
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
    let mut shell = DesktopShell::new(TaskbarConfig::bottom_bar(WIDTH, HEIGHT));
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(catalog(&[("files", "Files", LibraryCategory::Office)]));
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
    let start = centre(shell.session().taskbar().layout(Scale::ONE).library);
    let row = |shell: &DesktopShell, label: &str| -> Point {
        let layout = shell.session().taskbar().library_layout(Scale::ONE);
        let index = shell
            .session()
            .taskbar()
            .library()
            .rows()
            .iter()
            .position(|r| matches!(r, LibraryRow::Entry { name, .. } if name.as_str() == label))
            .expect("labelled row");
        let (_, rect) = layout
            .rows
            .iter()
            .find(|(i, _)| *i == index)
            .expect("row rect");
        centre(*rect)
    };

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

    // Library button: the popup opens.
    let outcomes = click(&mut shell, &mut comp, start);
    assert!(
        outcomes.contains(&ShellOutcome::Taskbar(TaskbarResponse::OpenLibrary)),
        "library click must open the popup, got {outcomes:?}"
    );

    // The "Files" row: the launcher fires and the popup closes.
    let files_row = row(&shell, "Files");
    let outcomes = click(&mut shell, &mut comp, files_row);
    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            ShellOutcome::Taskbar(TaskbarResponse::LibraryLaunch { entry }) if entry.as_str() == "os.tairix.files"
        )),
        "files-row click must launch, got {outcomes:?}"
    );
    assert!(!shell.session().taskbar().library().is_open());

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
        "window click must activate the window, got {outcomes:?}"
    );

    // Clicking the library button again reopens the popup.
    let outcomes = click(&mut shell, &mut comp, start);
    assert!(
        outcomes.contains(&ShellOutcome::Taskbar(TaskbarResponse::OpenLibrary)),
        "second library click must reopen the popup, got {outcomes:?}"
    );

    // A click away (outside the popup) dismisses it.
    let outcomes = click(&mut shell, &mut comp, in_window);
    assert!(
        outcomes.contains(&ShellOutcome::Taskbar(TaskbarResponse::LibraryDismissed)),
        "click away must dismiss the popup, got {outcomes:?}"
    );
    assert!(!shell.session().taskbar().library().is_open());

    // The window once more: activated again.
    let outcomes = click(&mut shell, &mut comp, in_window);
    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            ShellOutcome::WindowManager(InputResponse::Activated { window: w, .. }) if *w == window
        )),
        "post-dismiss window click must activate the window, got {outcomes:?}"
    );
}

// --- The trusted file picker (plans/APPWIN.md AW5, CU6) -----------------

use crate::picker::{PickConclusion, PickerSlot, SessionPicker, PICKER_ORIGIN};
use tairix_abi::input::{KeyInput, KeyValue, Modifiers, NamedKeyCode};
use tairix_browse::render::chrome_height;
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
    let shell = DesktopShell::new(TaskbarConfig::bottom_bar(640, 480));
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

/// `starting_at` opens the picker at the named directory (the user's home
/// in production), so its first listing is that directory's — proven by
/// choosing the file directly, without first descending from the root.
#[test]
fn picker_starting_at_opens_at_the_named_directory() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker =
        SessionPicker::new(TreeSource::fixture).starting_at(vec![String::from("Docs")]);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");
    // `Docs/` holds only `notes.txt`, which is selected first; one Enter
    // chooses it, so the picker must have opened *in* `Docs`, not the root.
    let enter = pressed(KeyValue::Named(NamedKeyCode::Enter));
    let concluded = picker
        .handle_key(&enter, &mut shell, &mut comp)
        .expect("choosing the file concludes");
    assert_eq!(
        concluded.conclusion,
        PickConclusion::Chosen(String::from("/Docs/notes.txt"))
    );
}

/// A `starting_at` directory that cannot be listed falls back to the root
/// rather than refusing the pick, so the user can still choose a file.
#[test]
fn picker_starting_at_unlistable_home_falls_back_to_root() {
    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture)
        .starting_at(vec![String::from("Nowhere"), String::from("missing")]);
    picker
        .begin(7, &mut shell, &mut comp)
        .expect("a bad home falls back to the listable root, not a refusal");
    assert!(
        picker.wm_id().is_some(),
        "the picker opened at the root fallback"
    );
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

    // The picker resolves its font from the active theme's UI size; the
    // click row must be computed from the same font so it lands on row 0.
    let theme = shell.session().active_theme();
    let font = crate::picker::picker_font(theme);
    // The first entry row sits directly below the chrome (the command toolbar
    // strip over the breadcrumb path bar), so compute it from the shared
    // `chrome_height` the renderer reserves.
    let row = i32::try_from(chrome_height(font, theme)).expect("a small chrome height");
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

/// A click anywhere on the shared command toolbar strip runs a read-only
/// navigation command (the same toolbar the file manager draws) — it never
/// concludes or cancels the pick, and the picker window stays showing.
#[test]
fn picker_toolbar_clicks_never_conclude_the_pick() {
    use tairix_browse::render::toolbar_height;
    use tairix_browse::WIN_WIDTH;

    let (mut shell, mut comp) = picker_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");

    // Sweep the toolbar strip's middle row: every click is a read-only
    // command (or an inert gap / disabled tool), so none may conclude the
    // pick or tear the window down.
    let y = i32::try_from(toolbar_height(shell.session().active_theme()) / 2)
        .expect("a small strip height");
    let width = i32::try_from(WIN_WIDTH).expect("a bounded window width");
    let mut x = 0;
    while x < width {
        assert_eq!(
            picker.handle_click(Point::new(x, y), &mut shell, &mut comp),
            None,
            "a toolbar-strip click must not conclude the pick"
        );
        assert!(picker.wm_id().is_some(), "the picker stays showing");
        x += 4;
    }
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

#[test]
fn load_library_merges_machine_and_user_stores() {
    let machine_conf = "os.tairix.editor.name Editor\nos.tairix.editor.bundle /Apps/editor.app\nos.tairix.editor.category Office\n";
    let user_conf = "os.tairix.editor.name My Editor\nos.tairix.files.name Files\nos.tairix.files.bundle /Apps/files.app\nos.tairix.files.category Office\n";

    let mut reader = MemoryAssets::default()
        .with(MACHINE_LIBRARY_PATH, machine_conf.as_bytes())
        .with(
            &user_library_path("/Users/alice").unwrap(),
            user_conf.as_bytes(),
        );

    // (a) absent stores -> empty catalog, no warnings
    let loaded = load_library(&mut MemoryAssets::default(), None);
    assert!(loaded.catalog.is_empty());
    assert!(loaded.warnings.is_empty());

    // (b) machine store parses -> entries listed
    let loaded = load_library(&mut reader, None);
    assert_eq!(loaded.catalog.len(), 1);
    assert!(loaded.warnings.is_empty());

    // (c) user overlay merges (overlay name wins)
    let loaded = load_library(&mut reader, Some("/Users/alice"));
    assert_eq!(loaded.catalog.len(), 2);
    let record = loaded
        .catalog
        .get(&EntryId::new("os.tairix.editor").unwrap())
        .unwrap();
    let tairix_proglib::Record::Entry(entry) = record else {
        panic!("expected entry")
    };
    assert_eq!(entry.name().as_str(), "My Editor");

    // (d) malformed machine store -> empty catalog + warning
    let mut reader = MemoryAssets::default().with(MACHINE_LIBRARY_PATH, b"malformed");
    let loaded = load_library(&mut reader, None);
    assert!(loaded.catalog.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains(MACHINE_LIBRARY_PATH));
    assert!(loaded.warnings[0].ends_with("; using an empty catalog\n"));

    // (e) oversized store
    let mut reader =
        MemoryAssets::default().with(MACHINE_LIBRARY_PATH, &vec![b'a'; MAX_CATALOG_LEN + 1]);
    let loaded = load_library(&mut reader, None);
    assert!(loaded.catalog.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("oversized"));

    // (f) non-UTF-8
    let mut reader = MemoryAssets::default().with(MACHINE_LIBRARY_PATH, b"\xff\xfe");
    let loaded = load_library(&mut reader, None);
    assert!(loaded.catalog.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("not valid UTF-8"));

    // (g) home None -> overlay never read. With no machine store, the
    // overlay's own declaration (files) survives alone: its editor line
    // names no bundle, so it is a patch, and a patch whose identifier no
    // document declares is discarded by the merge.
    let mut reader = MemoryAssets::default().with(
        &user_library_path("/Users/alice").unwrap(),
        user_conf.as_bytes(),
    );
    let loaded = load_library(&mut reader, Some("/Users/alice"));
    assert_eq!(loaded.catalog.len(), 1);
    assert!(
        loaded
            .catalog
            .entry(&EntryId::new("os.tairix.files").unwrap())
            .is_some(),
        "the overlay's own declaration stands without a machine store"
    );
    let loaded = load_library(&mut reader, None);
    assert!(loaded.catalog.is_empty());
}

#[test]
fn shell_set_library_hands_catalog_to_popup_and_refreshes_open_one() {
    let mut shell = shell();
    let mut comp = compositor();

    let cat1 = catalog(&[("write", "Write", LibraryCategory::Office)]);
    shell.set_library(&mut comp, cat1);
    assert_eq!(shell.session().taskbar().library().catalog().len(), 1);

    // Open popup.
    shell.handle(moved(24, 1060), &mut comp);
    shell.handle(PRIMARY_PRESS, &mut comp);
    assert!(shell.session().taskbar().library().is_open());
    assert_eq!(shell.session().taskbar().library().rows().len(), 2); // 1 folder + 1 entry

    // Refresh in place.
    let cat2 = office_and_games();
    shell.set_library(&mut comp, cat2);
    assert_eq!(shell.session().taskbar().library().catalog().len(), 3);
    assert!(
        shell.session().taskbar().library().is_open(),
        "popup stays open"
    );
    // 2 folders + 3 entries = 5 rows
    assert_eq!(shell.session().taskbar().library().rows().len(), 5);
}

#[test]
fn shell_raise_window_shows_and_focuses_tracked_tasks() {
    let mut shell = shell();
    let mut comp = compositor();

    let id = shell
        .open_window(&mut comp, Point::new(200, 200), app_surface(), "Files")
        .unwrap();

    // Minimise.
    let at = task_slot_point(&shell, 0);
    shell.handle(moved(at.x, at.y), &mut comp);
    shell.handle(PRIMARY_PRESS, &mut comp);
    assert!(!comp.window(id).unwrap().is_visible());

    // Raise.
    assert!(shell.raise_window(&mut comp, id));
    assert!(comp.window(id).unwrap().is_visible());
    assert_eq!(shell.router().focused(), Some(id));
}

#[test]
fn full_launch_flow() {
    let mut shell = shell();
    let mut comp = compositor();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(catalog(&[("calc", "Calc", LibraryCategory::Office)]));

    // Open popup.
    shell.handle(moved(24, 1060), &mut comp);
    shell.handle(PRIMARY_PRESS, &mut comp);

    let row_at = |shell: &DesktopShell, label: &str| -> Point {
        let layout = shell.session().taskbar().library_layout(Scale::ONE);
        let index = shell
            .session()
            .taskbar()
            .library()
            .rows()
            .iter()
            .position(|r| matches!(r, LibraryRow::Entry { name, .. } if name.as_str() == label))
            .expect("labelled row");
        let (_, rect) = layout
            .rows
            .iter()
            .find(|(i, _)| *i == index)
            .expect("row rect");
        Point::new(
            rect.left() + i32::try_from(rect.width / 2).expect("fits"),
            rect.top() + i32::try_from(rect.height / 2).expect("fits"),
        )
    };

    let at = row_at(&shell, "Calc");
    let outcome = shell.handle(moved(at.x, at.y), &mut comp);
    assert_eq!(outcome, ShellOutcome::Ignored);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);

    let ShellOutcome::Taskbar(TaskbarResponse::LibraryLaunch { entry }) = outcome else {
        panic!("expected launch, got {outcome:?}");
    };
    assert_eq!(entry.as_str(), "os.tairix.calc");
}

#[test]
fn session_pins_load_matrix() {
    let home = "/Users/alice";
    let path = tairix_taskpins::user_pins_path(home).unwrap();

    // (a) absent store -> empty no warning
    let (pins, warning) = SessionPins::load(&mut MemoryAssets::default(), Some(home));
    assert!(pins.list().is_empty());
    assert!(warning.is_none());

    // (b) valid store -> parsed
    let conf = "entry os.tairix.files\nbundle /Apps/editor.app\n";
    let mut reader = MemoryAssets::default().with(&path, conf.as_bytes());
    let (pins, warning) = SessionPins::load(&mut reader, Some(home));
    assert_eq!(pins.list().len(), 2);
    assert!(warning.is_none());

    // (c) malformed -> empty + warning
    let mut reader = MemoryAssets::default().with(&path, b"invalid line");
    let (pins, warning) = SessionPins::load(&mut reader, Some(home));
    assert!(pins.list().is_empty());
    let w = warning.expect("warning");
    assert!(w.contains(&path));
    assert!(w.contains("unknown pin key"));

    // (d) oversize
    let mut reader =
        MemoryAssets::default().with(&path, &vec![b'#'; tairix_taskpins::MAX_PINS_LEN + 1]);
    let (pins, warning) = SessionPins::load(&mut reader, Some(home));
    assert!(pins.list().is_empty());
    assert!(warning.unwrap().contains("longer than any valid pin store"));

    // (e) non-UTF-8
    let mut reader = MemoryAssets::default().with(&path, b"\xff\xfe");
    let (pins, warning) = SessionPins::load(&mut reader, Some(home));
    assert!(pins.list().is_empty());
    assert!(warning.unwrap().contains("not valid UTF-8"));

    // (f) no home
    let (pins, warning) = SessionPins::load(&mut MemoryAssets::default(), None);
    assert!(pins.list().is_empty());
    assert!(warning.is_none());
}

#[test]
fn edit_persistence_and_refusal() {
    let home = "/Users/alice";
    let path = tairix_taskpins::user_pins_path(home).unwrap();
    let mut reader = MemoryAssets::default();
    let (mut pins, _) = SessionPins::load(&mut reader, Some(home));

    let mut writer = MemoryWriter::default();
    let entry = EntryId::new("os.tairix.files").unwrap();

    // (a) pin_entry persists
    let index = pins.pin_entry(&mut writer, entry.clone()).expect("pinned");
    assert_eq!(index, 0);
    assert_eq!(pins.list().len(), 1);
    let written = writer.written(&path).expect("written");
    assert_eq!(written, b"entry os.tairix.files\n");

    // survives reload
    let mut reader = MemoryAssets::default().with(&path, written);
    let (pins2, _) = SessionPins::load(&mut reader, Some(home));
    assert_eq!(pins2.list().len(), 1);

    // (b) unpin rewrites
    pins.unpin(&mut writer, 0).expect("unpinned");
    assert!(pins.list().is_empty());
    assert_eq!(writer.written(&path).unwrap(), b"");

    // (c) refusing writer leaves memory unchanged
    pins.pin_entry(&mut writer, entry).expect("pinned again");
    let mut refusing = MemoryWriter::default().with_error(Errno::PermissionDenied);
    let res = pins.unpin(&mut refusing, 0);
    assert_eq!(res, Err(PinEditError::Write(Errno::PermissionDenied)));
    assert_eq!(pins.list().len(), 1);

    // (d) duplicates refuse
    let res = pins.pin_entry(&mut writer, EntryId::new("os.tairix.files").unwrap());
    assert_eq!(res, Err(PinEditError::AlreadyPinned));

    // (e) no home refuses even with healthy writer
    let (mut pins_no_home, _) = SessionPins::load(&mut MemoryAssets::default(), None);
    let res = pins_no_home.pin_entry(&mut writer, EntryId::new("any").unwrap());
    assert_eq!(res, Err(PinEditError::NoHome));

    // (f) pin_bundle_at clamps index
    let (mut pins, _) = SessionPins::load(&mut MemoryAssets::default(), Some(home));
    let index = pins
        .pin_bundle_at(
            &mut writer,
            99,
            BundlePath::new("/Apps/editor.app").unwrap(),
        )
        .expect("pinned");
    assert_eq!(index, 0);
}

#[test]
fn pin_resolution() {
    let mut catalog = Catalog::new();
    let entry_id = EntryId::new("os.tairix.files").unwrap();
    catalog
        .insert(LibraryEntry::new(
            entry_id.clone(),
            DisplayName::new("Files").unwrap(),
            BundlePath::new("/Apps/files.app").unwrap(),
            LibraryCategory::Office,
            Some(IconAsset::new("files.svg").unwrap()),
        ))
        .unwrap();

    let mut list = tairix_taskpins::PinList::default();
    list.pin(tairix_taskpins::PinTarget::Entry(entry_id.clone()))
        .unwrap();
    list.pin(tairix_taskpins::PinTarget::Entry(
        EntryId::new("missing").unwrap(),
    ))
    .unwrap();
    list.pin(tairix_taskpins::PinTarget::Bundle(
        BundlePath::new("/Apps/editor.app").unwrap(),
    ))
    .unwrap();
    list.pin(tairix_taskpins::PinTarget::Bundle(
        BundlePath::new("/Apps/no-manifest.app").unwrap(),
    ))
    .unwrap();

    let mut reader = MemoryAssets::default().with(
        "/Apps/editor.app/AppInfo",
        &manifest_fixture("Editor", Some("edit.svg")),
    );

    let resolved = crate::pins::resolve_pins(&mut reader, &list, &catalog);
    assert_eq!(resolved.len(), 4);

    // 1. Entry in catalog
    assert_eq!(resolved[0].label, "Files");
    assert_eq!(
        resolved[0].run_path,
        Some(String::from("/Apps/files.app/Run"))
    );
    assert_eq!(
        resolved[0].icon,
        Some(PinIconSource {
            bundle: String::from("/Apps/files.app"),
            asset: String::from("files.svg"),
        })
    );

    // 2. Uncatalogued entry
    assert_eq!(resolved[1].label, "missing");
    assert_eq!(resolved[1].run_path, None);

    // 3. Bundle with manifest
    assert_eq!(resolved[2].label, "Editor");
    assert_eq!(
        resolved[2].run_path,
        Some(String::from("/Apps/editor.app/Run"))
    );
    assert_eq!(
        resolved[2].icon,
        Some(PinIconSource {
            bundle: String::from("/Apps/editor.app"),
            asset: String::from("edit.svg"),
        })
    );

    // 4. Bundle with no manifest
    assert_eq!(resolved[3].label, "no-manifest");
    assert_eq!(resolved[3].run_path, None);

    // 5. Oversize manifest refused
    let mut reader =
        MemoryAssets::default().with("/Apps/big.app/AppInfo", &vec![0; APPINFO_WIRE_MAX + 1]);
    let mut list = tairix_taskpins::PinList::default();
    list.pin(tairix_taskpins::PinTarget::Bundle(
        BundlePath::new("/Apps/big.app").unwrap(),
    ))
    .unwrap();
    let resolved = crate::pins::resolve_pins(&mut reader, &list, &Catalog::new());
    assert_eq!(resolved[0].label, "big");
    assert_eq!(resolved[0].run_path, None);
}

#[test]
fn pin_service_decisions() {
    let mut reader =
        MemoryAssets::default().with("/Apps/valid.app/AppInfo", &manifest_fixture("Valid", None));
    let writer = MemoryWriter::default();
    let pins = SessionPins::load(&mut reader, Some("/Users/alice")).0;
    let mut service = PinService::new(reader, writer, pins);

    // ok -> Pinned + dirty
    assert!(!service.take_dirty());
    assert_eq!(
        service.pin_bundle_at(0, "/Apps/valid.app"),
        PinDecision::Pinned
    );
    assert!(service.take_dirty());
    assert!(!service.take_dirty());

    // AlreadyPinned
    assert_eq!(
        service.pin_bundle_at(0, "/Apps/valid.app"),
        PinDecision::AlreadyPinned
    );

    // Refused (bad path)
    assert_eq!(service.pin_bundle_at(0, "not-a-path"), PinDecision::Refused);

    // Refused (missing manifest)
    assert_eq!(
        service.pin_bundle_at(0, "/Apps/missing.app"),
        PinDecision::Refused
    );

    // Full
    let mut reader = MemoryAssets::default();
    for i in 0..tairix_taskpins::MAX_PINS {
        let path = format!("/Apps/app{i}.app");
        reader
            .files
            .push((format!("{path}/AppInfo"), manifest_fixture("App", None)));
    }
    reader.files.push((
        String::from("/Apps/full.app/AppInfo"),
        manifest_fixture("Full", None),
    ));

    let mut writer = MemoryWriter::default();
    let pins = SessionPins::load(&mut reader, Some("/Users/alice")).0;
    let mut service = PinService::new(&mut reader, &mut writer, pins);
    for i in 0..tairix_taskpins::MAX_PINS {
        let path = format!("/Apps/app{i}.app");
        assert_eq!(service.pin_bundle_at(i, &path), PinDecision::Pinned);
    }
    assert_eq!(
        service.pin_bundle_at(99, "/Apps/full.app"),
        PinDecision::Full
    );
}

#[test]
fn pin_service_drag_management() {
    let mut service = PinService::new(
        MemoryAssets::default(),
        MemoryWriter::default(),
        SessionPins::default(),
    );
    let path = "/Apps/editor.app";
    let bundle = BundlePath::new(path).unwrap();

    // offer
    assert!(service.drag_offered(7, path));
    assert!(service.drag_armed());

    // different window leaves armed
    assert_eq!(service.take_drag_for(9), None);
    assert!(service.drag_armed());

    // same window consumes
    assert_eq!(service.take_drag_for(7), Some(bundle.clone()));
    assert!(!service.drag_armed());

    // second offer replaces
    service.drag_offered(7, "/Apps/one.app");
    service.drag_offered(7, "/Apps/two.app");
    assert_eq!(
        service.take_drag_for(7),
        Some(BundlePath::new("/Apps/two.app").unwrap())
    );

    // withdraw
    service.drag_offered(7, path);
    service.drag_withdrawn(9); // no effect
    assert!(service.drag_armed());
    service.drag_withdrawn(7);
    assert!(!service.drag_armed());

    // malformed path
    assert!(!service.drag_offered(7, "not-a-path"));
    assert!(!service.drag_armed());
}

#[test]
fn resolve_pin_drop_pins_on_the_band_and_ends_the_gesture_elsewhere() {
    use crate::pins::resolve_pin_drop;
    use tairix_geometry::{Point, Scale};
    use tairix_taskbar::{Taskbar, TaskbarConfig};
    use tairix_theme::Theme;

    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    let on_band = Point::new(
        layout.task_list.left() + 10,
        layout.task_list.top() + i32::try_from(layout.task_list.height / 2).unwrap_or(0),
    );
    let service = || {
        let reader = MemoryAssets::default().with(
            "/Apps/editor.app/AppInfo",
            &manifest_fixture("Editor", None),
        );
        PinService::new(reader, MemoryWriter::default(), {
            SessionPins::load(&mut MemoryAssets::default(), Some("/Users/alice")).0
        })
    };

    // Nothing armed: a release is never a drop.
    let mut idle = service();
    assert_eq!(resolve_pin_drop(&mut idle, Some(7), &layout, on_band), None);

    // A release from an unserved window leaves the offer armed.
    let mut unserved = service();
    assert!(unserved.drag_offered(7, "/Apps/editor.app"));
    assert_eq!(
        resolve_pin_drop(&mut unserved, None, &layout, on_band),
        None
    );
    assert!(unserved.drag_armed());

    // A release from the offering window over the pin band pins at the
    // drop index and persists the store.
    let mut landing = service();
    assert!(landing.drag_offered(7, "/Apps/editor.app"));
    assert_eq!(
        resolve_pin_drop(&mut landing, Some(7), &layout, on_band),
        Some(PinDecision::Pinned)
    );
    assert!(!landing.drag_armed());
    assert_eq!(landing.pins().list().len(), 1);
    assert!(landing.take_dirty());

    // A release from the offering window away from the band ends the
    // gesture without pinning (the offer is consumed either way).
    let mut stray = service();
    assert!(stray.drag_offered(7, "/Apps/editor.app"));
    assert_eq!(
        resolve_pin_drop(&mut stray, Some(7), &layout, Point::new(2, 2)),
        None
    );
    assert!(!stray.drag_armed());
    assert_eq!(stray.pins().list().len(), 0);
}

#[test]
fn secondary_press_over_pin_opens_taskbar_menu() {
    let mut shell = shell();
    let mut comp = compositor();

    // Set one pin.
    let views = vec![PinView::new("Files", IconKind::AppBundle)];
    shell.set_pins(&mut comp, views);
    assert_eq!(shell.session().taskbar().pins().len(), 1);

    // Secondary press over the pin slot.
    let at = pin_slot_point(&shell, 0);
    shell.handle(moved(at.x, at.y), &mut comp);
    let outcome = shell.handle(SECONDARY_PRESS, &mut comp);

    assert_eq!(outcome, ShellOutcome::Ignored);
    assert!(shell.session().taskbar().menu().is_open());
    assert!(shell.presenter().menu_window().is_some());

    // While menu is open, it is modal.
    // Primary press inside menu (e.g. at menu origin, usually first row is Open/Launch).
    let menu_rect = shell
        .session()
        .taskbar()
        .menu_layout(Scale::ONE)
        .unwrap()
        .panel;
    let first_row = Point::new(menu_rect.left() + 10, menu_rect.top() + 10);

    shell.handle(moved(first_row.x, first_row.y), &mut comp);
    // Many controls activate on release.
    shell.handle(PRIMARY_PRESS, &mut comp);
    let outcome = shell.handle(
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        &mut comp,
    );

    // For a not-running pin, first row should be ActivatePin (which means launch).
    if let ShellOutcome::Taskbar(TaskbarResponse::ActivatePin { index }) = outcome {
        assert_eq!(index, 0);
    } else {
        panic!("expected ActivatePin, got {outcome:?}");
    }

    assert!(!shell.session().taskbar().menu().is_open());
    // Presenter should have removed the window.
    assert!(shell.presenter().menu_window().is_none());
}

#[test]
fn secondary_press_over_window_reaches_window_manager() {
    let mut shell = shell();
    let mut comp = compositor();

    let _win = opaque_window(&mut comp, Point::new(100, 100), 200, 200);
    shell.handle(moved(150, 150), &mut comp);
    let outcome = shell.handle(SECONDARY_PRESS, &mut comp);

    // Existing behaviour: secondary press over window is handled by WM (e.g. for context menu).
    if let ShellOutcome::WindowManager(InputResponse::SecondaryActivated { .. }) = outcome {
        // ok
    } else {
        panic!("expected SecondaryActivated, got {outcome:?}");
    }
}

#[test]
fn shell_set_pins_re_presents_and_updates_length() {
    let mut shell = shell();
    let mut comp = compositor();

    assert_eq!(shell.session().taskbar().pins().len(), 0);

    let views = vec![
        PinView::new("One", IconKind::AppBundle),
        PinView::new("Two", IconKind::AppBundle),
    ];
    shell.set_pins(&mut comp, views);
    assert_eq!(shell.session().taskbar().pins().len(), 2);

    // Check that it's presented (compositor window for bar exists and is updated)
    assert!(shell.presenter().bar_window().is_some());
}

fn centre(rect: tairix_wm::Rect) -> Point {
    assert!(!rect.is_empty());
    #[allow(clippy::cast_possible_wrap)]
    Point::new(
        rect.left() + (rect.width / 2) as i32,
        rect.top() + (rect.height / 2) as i32,
    )
}

fn pin_slot_point(shell: &DesktopShell, index: usize) -> Point {
    let layout = shell.session().taskbar().layout(Scale::ONE);
    let slot = layout.pins.get(index).expect("pin slot");
    centre(*slot)
}

fn manifest_fixture(name: &str, icon: Option<&str>) -> Vec<u8> {
    let mut h = AppInfoHeader {
        magic: APPINFO_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: 0,
        capability_count: 0,
        mime_count: 0,
        id_len: 2,
        name_len: u8::try_from(name.len()).unwrap(),
        version_len: 1,
        library_icon_len: u8::try_from(icon.map_or(0, str::len)).unwrap(),
        library: tairix_abi::LibraryCategory::to_wire(Some(tairix_abi::LibraryCategory::Other)),
        reserved0: [0; 3],
        id: [0; BUNDLE_ID_MAX],
        name: [0; BUNDLE_NAME_MAX],
        version: [0; BUNDLE_VERSION_MAX],
        library_icon: [0; LIBRARY_ICON_MAX],
        syscall_table_hash: [0; SYSCALL_TABLE_HASH_LEN],
        content_hash: [0; 32],
        signer_pubkey: [0; 32],
        signature: [0; 64],
    };
    h.id[0..2].copy_from_slice(b"fi");
    h.name[..name.len()].copy_from_slice(name.as_bytes());
    h.version[0] = b'1';
    if let Some(icon) = icon {
        h.library_icon[..icon.len()].copy_from_slice(icon.as_bytes());
    }
    h.to_le_bytes().to_vec()
}

// ---- notification relay ---------------------------------------------

/// The full producer→desktop notification path on the host: a producer's
/// raise lands keyed to its attested identity; another producer cannot clear
/// it; the click-to-dismiss gesture routes through the session router to the
/// bar and clears the model; and a producer clearing its own is idempotent.
#[test]
fn notifications_relay_raise_dismiss_and_isolate_producers() {
    const W: u32 = 1024;
    const H: u32 = 768;
    let mut shell = DesktopShell::new(TaskbarConfig::bottom_bar(W, H));
    let mode = DisplayMode {
        width_px: W,
        height_px: H,
        stride_bytes: W * 4,
        format: DisplayFormat::Rgba8888,
    };
    let mut comp = Compositor::new(mode, Color::rgb(0, 0, 0)).expect("compositor");

    // Producer 42 raises a notification: it lands keyed to producer 42.
    shell.apply_notify(
        &mut comp,
        42,
        NotifyRequest::Raise {
            key: 1,
            severity: NotifySeverity::Warning,
            title: NotifyTitle::new("Battery low").expect("title"),
            body: NotifyBody::new("12% remaining").expect("body"),
        },
    );
    assert_eq!(
        shell
            .session()
            .taskbar()
            .notifications()
            .notification_count(),
        1
    );
    let note = shell
        .session()
        .taskbar()
        .notifications()
        .notification(0)
        .expect("present");
    assert_eq!(note.producer, 42);
    assert_eq!(note.title.as_str(), "Battery low");

    // A different producer cannot clear producer 42's notification.
    shell.apply_notify(&mut comp, 99, NotifyRequest::Clear { key: 1 });
    assert_eq!(
        shell
            .session()
            .taskbar()
            .notifications()
            .notification_count(),
        1
    );

    // Clicking the card routes through the session router to the bar and
    // clears the model — proving the router forwards a press on the popover
    // (which sits above the bar, not on it) to the taskbar.
    let card = {
        let layout = shell
            .session()
            .taskbar()
            .notifications_layout(Scale::ONE)
            .expect("popover");
        let rect = layout.cards[0].card;
        #[allow(clippy::cast_possible_wrap)]
        Point::new(
            rect.left() + (rect.width / 2) as i32,
            rect.top() + (rect.height / 2) as i32,
        )
    };
    let _ = shell.handle(moved(card.x, card.y), &mut comp);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::DismissNotification {
            producer: 42,
            key: 1,
        })
    );
    assert!(!shell
        .session()
        .taskbar()
        .notifications()
        .has_notifications());

    // Producer 42 clearing its own now-gone notification is a harmless no-op.
    shell.apply_notify(&mut comp, 42, NotifyRequest::Clear { key: 1 });
    assert!(!shell
        .session()
        .taskbar()
        .notifications()
        .has_notifications());
}

// ---- window-owner responsiveness (vigil) -----------------------------

#[test]
fn hang_tracker_flags_only_after_threshold_of_backpressure() {
    let mut tracker = crate::HangTracker::new();

    // The first refusal opens the suspicion window but proves nothing yet.
    assert!(!tracker.note_refused(7, tairix_abi::Errno::LengthOutOfRange, 1_000));
    assert!(!tracker.is_unresponsive(7));
    assert_eq!(tracker.unresponsive_count(), 0);

    // Refusals inside the threshold keep the verdict unchanged.
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        1_000 + crate::UNRESPONSIVE_AFTER_NS / 2,
    ));
    assert!(!tracker.is_unresponsive(7));

    // The refusal that crosses the threshold flags the owner — exactly once.
    assert!(tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        1_000 + crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(tracker.is_unresponsive(7));
    assert_eq!(tracker.unresponsive_count(), 1);
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        2_000 + crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert_eq!(tracker.unresponsive_count(), 1);
}

#[test]
fn hang_tracker_clears_on_an_accepted_delivery() {
    let mut tracker = crate::HangTracker::new();

    // A suspect that drains before the threshold was never unresponsive:
    // clearing it changes nothing the tray must repaint.
    assert!(!tracker.note_refused(7, tairix_abi::Errno::LengthOutOfRange, 0));
    assert!(!tracker.note_delivered(7));

    // A flagged owner that drains recovers, and the change is reported.
    assert!(!tracker.note_refused(7, tairix_abi::Errno::LengthOutOfRange, 0));
    assert!(tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(tracker.note_delivered(7));
    assert!(!tracker.is_unresponsive(7));
    assert_eq!(tracker.unresponsive_count(), 0);

    // The suspicion window restarts from scratch after a recovery.
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        2 * crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(!tracker.is_unresponsive(7));
}

#[test]
fn hang_tracker_treats_only_backpressure_as_evidence() {
    let mut tracker = crate::HangTracker::new();

    // A torn-down mailbox is the reap path's business, not hang evidence —
    // and it drops any standing suspicion so a recycled task id starts clean.
    assert!(!tracker.note_refused(7, tairix_abi::Errno::LengthOutOfRange, 0));
    assert!(!tracker.note_refused(7, tairix_abi::Errno::NotFound, 1));
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        crate::UNRESPONSIVE_AFTER_NS + 2,
    ));
    assert!(!tracker.is_unresponsive(7));

    // Any other refusal is no evidence either way.
    assert!(!tracker.note_refused(9, tairix_abi::Errno::PermissionDenied, 0));
    assert!(!tracker.note_refused(9, tairix_abi::Errno::MessageTooLarge, 0));
    assert!(!tracker.is_unresponsive(9));
    assert_eq!(tracker.unresponsive_count(), 0);
}

#[test]
fn hang_tracker_forget_reports_only_a_standing_verdict() {
    let mut tracker = crate::HangTracker::new();

    // Forgetting an unknown or merely-suspect owner changes nothing.
    assert!(!tracker.forget(7));
    assert!(!tracker.note_refused(7, tairix_abi::Errno::LengthOutOfRange, 0));
    assert!(!tracker.forget(7));

    // Forgetting a flagged owner (its exit was reaped) clears the verdict
    // and reports the change so the tray repaints.
    assert!(!tracker.note_refused(8, tairix_abi::Errno::LengthOutOfRange, 0));
    assert!(tracker.note_refused(
        8,
        tairix_abi::Errno::LengthOutOfRange,
        crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(tracker.forget(8));
    assert!(!tracker.is_unresponsive(8));
    assert_eq!(tracker.unresponsive_count(), 0);
}

#[test]
fn hang_tracker_counts_every_flagged_owner_and_saturates() {
    let mut tracker = crate::HangTracker::new();
    for owner in 0..3u64 {
        assert!(!tracker.note_refused(owner, tairix_abi::Errno::LengthOutOfRange, 0));
        assert!(tracker.note_refused(
            owner,
            tairix_abi::Errno::LengthOutOfRange,
            crate::UNRESPONSIVE_AFTER_NS,
        ));
    }
    assert_eq!(tracker.unresponsive_count(), 3);

    // The count is a u16 for the tray summary; a pathological census
    // saturates rather than wrapping.
    for owner in 3..70_000u64 {
        let _ = tracker.note_refused(owner, tairix_abi::Errno::LengthOutOfRange, 0);
        let _ = tracker.note_refused(
            owner,
            tairix_abi::Errno::LengthOutOfRange,
            crate::UNRESPONSIVE_AFTER_NS,
        );
    }
    assert_eq!(tracker.unresponsive_count(), u16::MAX);
}

// ---- switchboard tray relay -------------------------------------------

/// A well-formed summary fixture: `jobs` background jobs, nothing else
/// notable, a modest CPU reading.
fn tray_summary(jobs: u16) -> tairix_abi::switchboard_ipc::TraySummary {
    tairix_abi::switchboard_ipc::TraySummary {
        jobs,
        recovery: 0,
        cpu_busy_permille: tairix_abi::switchboard_ipc::TrayPermille::new(120).expect("permille"),
        pressure: None,
        top_task: None,
    }
}

/// The centre of the Switchboard capsule's slot on the bar laid out at 100%.
fn capsule_point(shell: &DesktopShell) -> Point {
    let slot = shell.session().taskbar().layout(Scale::ONE).switchboard;
    assert!(!slot.is_empty(), "the capsule slot has a region");
    #[allow(clippy::cast_possible_wrap)]
    Point::new(
        slot.left() + (slot.width / 2) as i32,
        slot.top() + (slot.height / 2) as i32,
    )
}

/// The relay drives the capsule from the published summary and falls back to
/// calm when the feed clears (the service exited): the session-side halves of
/// the T9/T10 feed contract.
#[test]
fn tray_relay_drives_the_capsule_and_clears_to_calm() {
    let mut shell = shell();
    let mut comp = compositor();

    // No feed yet: the capsule rests calm.
    let state = shell.session().taskbar().tray().signal().state();
    assert_eq!(state.activity, tairix_controls::ActivityState::Idle);

    // A published summary with background jobs drives the working seam.
    shell.set_tray_summary(&mut comp, Some(tray_summary(2)));
    let state = shell.session().taskbar().tray().signal().state();
    assert_eq!(state.activity, tairix_controls::ActivityState::Working);

    // The service died: the reap path clears the feed and the capsule
    // returns to calm rather than freezing the dead service's last summary.
    shell.set_tray_summary(&mut comp, None);
    let state = shell.session().taskbar().tray().signal().state();
    assert_eq!(state.activity, tairix_controls::ActivityState::Idle);
}

/// The session's own delivery evidence flags the capsule's hung posture and
/// releases it — independent of the service feed.
#[test]
fn tray_relay_flags_and_releases_the_hung_posture() {
    let mut shell = shell();
    let mut comp = compositor();

    shell.set_tray_unresponsive(&mut comp, 1);
    let state = shell.session().taskbar().tray().signal().state();
    assert_eq!(state.recovery, tairix_controls::RecoveryState::Hung);

    shell.set_tray_unresponsive(&mut comp, 0);
    let state = shell.session().taskbar().tray().signal().state();
    assert_eq!(state.recovery, tairix_controls::RecoveryState::None);
}

/// A primary press on the capsule pins the instrument readout open (it
/// survives the pointer leaving), a press inside the open readout is claimed
/// inert, and a press away from the bar releases the pin while the desktop
/// acts on the press as usual.
#[test]
fn capsule_press_pins_the_readout_and_a_press_away_releases_it() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);

    // Press the capsule: the readout pins open and is presented as a
    // popover window.
    let capsule = capsule_point(&shell);
    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp);
    assert_eq!(outcome, ShellOutcome::Ignored, "pinning is presentation");
    assert!(shell.session().taskbar().tray().is_pinned());
    assert!(shell.presenter().readout_window().is_some());

    // The pin holds without hover: move the pointer to the open desktop.
    let _ = shell.handle(moved(600, 300), &mut comp);
    assert!(shell.session().taskbar().tray().is_expanded());
    assert!(shell.presenter().readout_window().is_some());

    // A press inside the readout is claimed inert: nothing happens, the pin
    // holds.
    let readout = shell
        .session()
        .taskbar()
        .tray_readout_layout(Scale::ONE)
        .expect("the pinned readout has a panel");
    #[allow(clippy::cast_possible_wrap)]
    let inside = Point::new(
        readout.panel.left() + (readout.panel.width / 2) as i32,
        readout.panel.top() + (readout.panel.height / 2) as i32,
    );
    let _ = shell.handle(moved(inside.x, inside.y), &mut comp);
    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp),
        ShellOutcome::Ignored
    );
    assert!(shell.session().taskbar().tray().is_pinned());

    // A press away from the bar releases the pin; the readout window is
    // withdrawn on the same wake.
    let _ = shell.handle(moved(600, 300), &mut comp);
    let _ = shell.handle(PRIMARY_PRESS, &mut comp);
    assert!(!shell.session().taskbar().tray().is_pinned());
    assert!(!shell.session().taskbar().tray().is_expanded());
    assert!(shell.presenter().readout_window().is_none());
}

/// A scroll over the capsule cycles the running tasks through the session
/// router (the bar activates the next task); a scroll elsewhere never
/// touches the task list.
#[test]
fn scroll_over_the_capsule_cycles_tasks_through_the_router() {
    let mut shell = shell();
    let mut comp = compositor();
    let first = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    let _second = open_app(&mut shell, &mut comp, Point::new(360, 240), "Files");
    let first_task = shell.tasks().task_for(first).expect("tracked");

    // The second window holds focus; scrolling forward over the capsule
    // wraps to the first task and activates it.
    let capsule = capsule_point(&shell);
    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp);
    let outcome = shell.handle(InputEvent::PointerScrolled { dx: 0, dy: 1 }, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::TaskActivated {
            id: first_task,
            outcome: ActivateOutcome::Activated,
        })
    );
    assert_eq!(
        shell.session().taskbar().tasks().focused(),
        Some(first_task)
    );

    // A scroll away from the capsule leaves the task list alone.
    let _ = shell.handle(moved(600, 300), &mut comp);
    let outcome = shell.handle(InputEvent::PointerScrolled { dx: 0, dy: 1 }, &mut comp);
    assert!(!matches!(outcome, ShellOutcome::Taskbar(_)));
    assert_eq!(
        shell.session().taskbar().tasks().focused(),
        Some(first_task)
    );
}

/// A middle press over the capsule switches to the previous task (the
/// MRU-of-two), and is inert anywhere else.
#[test]
fn middle_press_over_the_capsule_switches_to_the_previous_task() {
    const MIDDLE_PRESS: InputEvent = InputEvent::PointerPressed {
        button: PointerButton::Middle,
    };
    let mut shell = shell();
    let mut comp = compositor();
    let first = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    let second = open_app(&mut shell, &mut comp, Point::new(360, 240), "Files");
    let first_task = shell.tasks().task_for(first).expect("tracked");
    let second_task = shell.tasks().task_for(second).expect("tracked");

    // Opening `second` after `first` made `first` the previous task.
    let capsule = capsule_point(&shell);
    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp);
    let outcome = shell.handle(MIDDLE_PRESS, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::TaskActivated {
            id: first_task,
            outcome: ActivateOutcome::Activated,
        })
    );

    // The handover made the second task the previous one: middle-click
    // toggles back.
    let outcome = shell.handle(MIDDLE_PRESS, &mut comp);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::TaskActivated {
            id: second_task,
            outcome: ActivateOutcome::Activated,
        })
    );

    // A middle press on the open desktop is inert.
    let _ = shell.handle(moved(600, 300), &mut comp);
    assert_eq!(shell.handle(MIDDLE_PRESS, &mut comp), ShellOutcome::Ignored);
}
