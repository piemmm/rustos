//! Headless unit tests for the desktop session glue.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_abi::notify_ipc::{NotifyBody, NotifyRequest, NotifySeverity, NotifyTitle};
use tairix_abi::switchboard_ipc::{
    CommandSection, SwitchboardCommand, SwitchboardRequest, SEAT_REPORT_OWNERS_MAX,
};
use tairix_abi::sysinfo::CACHE_LABEL_MAX;
use tairix_abi::{
    AppInfoHeader, Errno, ProcId, ABI_VERSION_CURRENT, APPINFO_MAGIC, APPINFO_WIRE_MAX,
    BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX, LIBRARY_ICON_MAX, SYSCALL_TABLE_HASH_LEN,
};
use tairix_controls::{PointerState, Section};
use tairix_cursor::CursorTheme;
use tairix_icon::{
    artwork_cache, icon_artwork_path, ArtworkCache, IconArtworkSource, IconKind, IconSet,
    NoArtwork, MAX_ARTWORK_BYTES,
};
use tairix_log::{Event, Sink};
use tairix_proglib::{
    user_library_path, BundlePath, Catalog, DisplayName, EntryId, IconAsset, LibraryCategory,
    LibraryEntry, MACHINE_LIBRARY_PATH, MAX_CATALOG_LEN,
};
use tairix_reclaim::{CacheLedger, PressureBand, ReclaimCache, ReportedPressure};
use tairix_taskbar::{
    icon_cache, ActivateOutcome, IconEpoch, LibraryRow, PinView, TaskId, TaskbarConfig,
    TaskbarRenderer, TaskbarRepaint, TaskbarResponse,
};
use tairix_taskpins::PinTarget;
use tairix_theme::{Appearance, CursorKind, Metrics, Theme, ThemeError, ThemeId};
use tairix_wm::{
    chrome_cache, cursor_cache, ChromeEpoch, Color, Compositor, Corners, InputEvent, InputResponse,
    Key, NamedKey, Point, PointerButton, Rect, Scale, Surface, WindowActivationState, WindowChrome,
    WindowId,
};

use crate::{
    build_pin_views, command_section, deliver_pending_open, load_icon_set, load_library,
    maybe_send_seat_report, open_tray, resolve_library_icons, serve_switchboard_request,
    ArtworkFileReader, ArtworkSandbox, DesktopSession, DesktopShell, DragOrigin, IconRasteriser,
    InputSource, LaunchTable, LockOutcome, LockedDrain, OwnerWindow, PinBridge, PinEditError,
    PinIconSource, PinService, ResolvedPin, ScreenLock, SessionFileReader, SessionFileWriter,
    SessionInputResponse, SessionInputRouter, SessionPins, ShellOutcome, SwitchboardMailbox,
    SwitchboardOutcome, SwitchboardRefusal, SwitchboardServe, TaskBridge, TaskbarPresenter,
    Unlocker, SWITCHBOARD_RUN_PATH, UNNAMED_ACCOUNT,
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
pub(crate) struct MemoryAssets {
    files: Vec<(String, Vec<u8>)>,
}

impl MemoryAssets {
    pub(crate) fn with(mut self, path: &str, bytes: &[u8]) -> Self {
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
pub(crate) struct MemoryWriter {
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

/// The seat every shell under test is charged to.
const TEST_SEAT: u64 = 1;

/// A 1080p 32-bit frame: the backing the shells under test derive their
/// rasterised-asset budgets from, so the ceiling exercised here is the
/// real derivation rather than a number invented for the test.
const TEST_FRAME_BYTES: usize = 1920 * 1080 * 4;

/// Discards audit records. These tests assert session behaviour; the
/// caches' audit path is covered where it is defined.
struct SilentSink;

impl Sink for SilentSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

static TEST_SINK: SilentSink = SilentSink;

/// The gauge the shells under test are governed by, held at normal for
/// its whole life so tests running in parallel cannot perturb one
/// another. A test that moves the band declares its own gauge.
static NORMAL_PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// A glyph cache at normal pressure for a renderer used on its own,
/// outside a whole shell.
fn test_icon_cache() -> ReclaimCache<IconKind, Surface, IconEpoch> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    icon_cache(TEST_SEAT, TEST_FRAME_BYTES, &NORMAL_PRESSURE, &TEST_SINK)
}

/// The window-furniture cache every compositor under test is built with,
/// at normal pressure and through the shipping desktop policy — the
/// compositor takes it as an argument exactly as the session hands it one.
pub(crate) fn test_chrome_cache() -> ReclaimCache<WindowId, WindowChrome, ChromeEpoch> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    chrome_cache(TEST_SEAT, TEST_FRAME_BYTES, &NORMAL_PRESSURE, &TEST_SINK)
}

/// The pressure gauge every compositor under test is built over — the same
/// one its caches read, so the desktop under test has a single notion of
/// how tight memory is, exactly as the shipping session does.
pub(crate) fn test_pressure() -> &'static ReportedPressure {
    &NORMAL_PRESSURE
}

/// A shell for `config`, with both rasterised-asset caches built through
/// the shipping desktop policy at normal pressure.
pub(crate) fn shell_for(config: TaskbarConfig) -> DesktopShell {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    DesktopShell::new(
        config,
        TEST_SEAT,
        TEST_FRAME_BYTES,
        &NORMAL_PRESSURE,
        &TEST_SINK,
    )
}

/// A shell and a compositor for `config`, both governed by `pressure`
/// rather than the shared gauge — what a test that *moves* the band needs,
/// since the shared one must stay at normal for the tests beside it.
pub(crate) fn desktop_over(
    config: TaskbarConfig,
    display: DisplayMode,
    pressure: &'static ReportedPressure,
) -> (DesktopShell, Compositor) {
    let shell = DesktopShell::new(config, TEST_SEAT, TEST_FRAME_BYTES, pressure, &TEST_SINK);
    let compositor = Compositor::new(
        display,
        Color::rgb(0, 0, 0),
        chrome_cache(TEST_SEAT, TEST_FRAME_BYTES, pressure, &TEST_SINK),
        pressure,
    )
    .expect("the compositor allocates");
    (shell, compositor)
}

/// The desktop's caches, built exactly as the session composes them: its own
/// artwork, the taskbar's icon glyphs, the window manager's cursors, and the
/// decorated windows' furniture. All four live in the session process, so all
/// four are its rows to report.
fn desktop_caches() -> Vec<Option<CacheLedger>> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    vec![
        artwork_cache(
            "session.desktop-artwork",
            TEST_SEAT,
            TEST_FRAME_BYTES,
            &NORMAL_PRESSURE,
            &TEST_SINK,
        )
        .ledger(),
        test_icon_cache().ledger(),
        cursor_cache(TEST_SEAT, TEST_FRAME_BYTES, &NORMAL_PRESSURE, &TEST_SINK).ledger(),
        test_chrome_cache().ledger(),
    ]
}

#[test]
fn every_desktop_cache_reports_under_a_renderable_label() {
    // The session hands these ledgers to the system's cache monitor, which
    // renders each label verbatim as its own row. A label the wire record
    // refuses would cost the desktop a row rather than show a broken one, so
    // every name is checked where the session composes it.
    for ledger in desktop_caches() {
        let ledger = ledger.expect("a desktop cache is a classified reclaim cache");
        let record = ledger.to_record().expect("the label fits the wire record");
        assert!(record.label().len() <= CACHE_LABEL_MAX);
        assert!(record
            .label()
            .bytes()
            .all(|byte| (0x20..0x7f).contains(&byte)));
    }
}

#[test]
fn the_desktop_caches_report_under_distinct_labels() {
    // The reporter is keyed by label so a rebuilt cache replaces its own row
    // instead of double-counting. Two of the desktop's caches sharing a name
    // would silently collapse into one row, hiding whichever registered
    // first.
    let mut labels: Vec<String> = desktop_caches()
        .into_iter()
        .map(|ledger| {
            String::from(
                ledger
                    .expect("a desktop cache is a classified reclaim cache")
                    .to_record()
                    .expect("the label fits the wire record")
                    .label(),
            )
        })
        .collect();
    let composed = labels.len();
    labels.sort();
    labels.dedup();
    assert_eq!(labels.len(), composed);
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
        0,
    );
    router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        comp,
        taskbar,
        0,
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
        test_chrome_cache(),
        test_pressure(),
    )
    .expect("the compositor allocates")
}

#[test]
fn present_adds_a_bar_window_placed_and_rounded() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );

    let id = presenter.bar_window().expect("the bar was presented");
    assert_eq!(comp.window_count(), 1);
    assert!(presenter.popup_window().is_none(), "the menu is closed");

    let layout = session.taskbar().layout(Scale::ONE);
    let window = comp.window(id).expect("the bar window exists");
    assert_eq!(window.origin(), layout.bar.origin);
    assert_eq!(window.corners(), Corners::from_radius(layout.corner_radius));
    assert_eq!(window.client_size(), (layout.bar.width, layout.bar.height));
}

#[test]
fn presenting_twice_reuses_the_bar_window() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    let first = presenter.bar_window().expect("first present");
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    let second = presenter.bar_window().expect("second present");

    assert_eq!(first, second, "the same window is reused");
    assert_eq!(comp.window_count(), 1, "no second bar window is created");
}

/// The screen rectangle a presented window occupies, for asserting which
/// surface a repaint actually touched.
fn window_rect(comp: &Compositor, id: WindowId) -> Rect {
    let window = comp.window(id).expect("the window is presented");
    Rect::new(
        window.origin().x,
        window.origin().y,
        window.client_size().0,
        window.client_size().1,
    )
}

#[test]
fn presenting_an_empty_latch_repaints_nothing() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    comp.composite();
    assert!(!comp.has_damage(), "the first paint has been drained");

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::NONE,
        &mut NoArtwork,
    );

    assert!(
        !comp.has_damage(),
        "a pointer that changed nothing must not dirty a pixel"
    );
}

#[test]
fn presenting_one_latched_surface_leaves_the_others_alone() {
    let mut session = session();
    session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    open_library(&mut router, &mut comp, session.taskbar_mut());

    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    comp.composite();
    let popup = presenter.popup_window().expect("the popup is presented");
    let popup_rect = window_rect(&comp, popup);

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint {
            library: true,
            ..TaskbarRepaint::NONE
        },
        &mut NoArtwork,
    );

    assert_eq!(
        comp.composite().bounds(),
        popup_rect,
        "only the popup was repainted; the bar keeps its pixels"
    );
}

#[test]
fn a_surface_with_no_window_is_presented_however_empty_the_latch() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::NONE,
        &mut NoArtwork,
    );

    assert!(
        presenter.bar_window().is_some(),
        "the first paint cannot wait for a latch that only reports changes"
    );
}

#[test]
fn a_density_change_repaints_every_surface() {
    let session = session();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    comp.composite();
    let bar = presenter.bar_window().expect("the bar is presented");
    let before = window_rect(&comp, bar);

    assert!(comp.set_scale(Scale::from_percent(200).expect("a valid density")));
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::NONE,
        &mut NoArtwork,
    );

    let after = window_rect(&comp, bar);
    assert_ne!(
        before, after,
        "the bar re-laid at the new density, though the taskbar model never changed"
    );
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

    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );

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

    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
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

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
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
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    open_library(&mut router, &mut comp, session.taskbar_mut());

    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
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
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    let first = presenter.bar_window().expect("first present");

    assert!(comp.remove(first), "an embedder removed the bar window");
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
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
        .unwrap();
    let mut comp = compositor();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    let id = presenter.bar_window().expect("the bar was presented");
    let dark_radius = session.taskbar().layout(Scale::ONE).corner_radius;
    assert_eq!(
        comp.window(id).expect("the bar window").corners(),
        Corners::Rounded {
            radius: dark_radius
        }
    );

    session.set_theme(ThemeId(100)).unwrap();
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );

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
        0,
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut comp,
        session.taskbar_mut(),
        0,
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
        0,
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
        0,
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
        0,
    );
    assert_eq!(response, SessionInputResponse::Ignored);
}

#[test]
fn motion_updates_the_pointer_and_reaches_the_desktop_when_it_hits_no_window() {
    let mut session = session();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();

    let response = router.handle(
        InputEvent::PointerMoved {
            to: Point::new(640, 480),
        },
        &mut comp,
        session.taskbar_mut(),
        0,
    );

    // Motion that lands on no window is the desktop's: it reaches the
    // session rather than being swallowed, which is what lets the desktop's
    // icons take a hover.
    assert_eq!(
        response,
        SessionInputResponse::WindowManager(InputResponse::DesktopPointerMoved)
    );
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
        0,
    );
    router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
        0,
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
        0,
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
        0,
    );
    router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
        0,
    );
    assert!(router.begin_move(&comp));

    let response = router.handle(
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
        0,
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
        0,
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut comp,
        session.taskbar_mut(),
        0,
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
        0,
    );
    let response = router.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut comp,
        session.taskbar_mut(),
        0,
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
    shell_for(TaskbarConfig::bottom_bar(1920, 1080))
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

const PRIMARY_RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

/// A press released well inside the bar's long-press threshold, in
/// monotonic nanoseconds since the press.
const QUICK_PRESS_NS: u64 = 50_000_000;

/// A press held far past the bar's long-press threshold, in monotonic
/// nanoseconds since the press. The bar owns the threshold itself; the
/// session only needs a hold no reasonable threshold could call quick.
const LONG_PRESS_NS: u64 = 5_000_000_000;

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
            0,
        )
        .expect("an in-memory source does not fault");

    assert_eq!(
        outcomes,
        [
            ShellOutcome::WindowManager(InputResponse::DesktopPointerMoved),
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
    shell.present(&mut comp);
    let bar = shell.presenter().bar_window();
    assert!(bar.is_some(), "the desktop paints its bar before any input");

    shell.handle(moved(250, 250), &mut comp, 0);
    comp.composite();
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp, 0);

    assert_eq!(
        outcome,
        ShellOutcome::WindowManager(InputResponse::Activated {
            window,
            local: Point::new(50, 50),
        })
    );
    assert_eq!(shell.router().focused(), Some(window));
    assert_eq!(
        shell.presenter().bar_window(),
        bar,
        "a window-manager action reuses the bar window rather than re-creating it"
    );
    let bar_rect = window_rect(&comp, bar.expect("the bar is painted"));
    assert!(
        comp.composite().bounds().intersection(&bar_rect).is_empty(),
        "activating a window repaints that window's furniture, never the bar"
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
        test_chrome_cache(),
        test_pressure(),
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
        0,
    );

    assert_eq!(result, Err(Errno::NotFound));
    assert!(
        shell.session().taskbar().library().is_open(),
        "the event drained before the fault was still applied"
    );
}

#[test]
fn pump_coalesces_adjacent_pointer_motions_over_one_window() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    // Initial move to window and focus it so we get ClientPointerMoved.
    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);

    // A run of N pointer motions over one window.
    let events = &[moved(251, 251), moved(252, 252), moved(253, 253)];
    let outcomes = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0],
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved {
            window,
            local: Point::new(53, 53),
        })
    );
    // Observable: pointer position reflects the last sample.
    assert_eq!(shell.router().pointer(), Point::new(253, 253));
}

#[test]
fn pump_motion_run_interrupted_by_different_outcome_does_not_collapse_across_interruption() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);

    // Motion, then Release, then Motion.
    let events = &[moved(251, 251), PRIMARY_RELEASE, moved(252, 252)];
    let outcomes = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(outcomes.len(), 3);
    assert_eq!(
        outcomes[0],
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved {
            window,
            local: Point::new(51, 51),
        })
    );
    assert_eq!(
        outcomes[1],
        ShellOutcome::WindowManager(InputResponse::ClientPointerReleased {
            window,
            local: Point::new(51, 51),
        })
    );
    assert_eq!(
        outcomes[2],
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved {
            window,
            local: Point::new(52, 52),
        })
    );
}

#[test]
fn pump_does_not_coalesce_adjacent_non_motion_outcomes() {
    let mut shell = shell();
    let mut comp = compositor();
    let _window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);

    // Release, then Press.
    let events = &[PRIMARY_RELEASE, PRIMARY_PRESS];
    let outcomes = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(outcomes.len(), 2);
    assert!(matches!(
        outcomes[0],
        ShellOutcome::WindowManager(InputResponse::ClientPointerReleased { .. })
    ));
    assert!(matches!(
        outcomes[1],
        ShellOutcome::WindowManager(InputResponse::Activated { .. })
    ));
}

#[test]
fn pump_does_not_coalesce_interleaved_motions_over_two_windows() {
    let mut shell = shell();
    let mut comp = compositor();
    let w1 = opaque_window(&mut comp, Point::new(100, 100), 100, 100);
    let w2 = opaque_window(&mut comp, Point::new(300, 300), 100, 100);

    // Focus w1.
    shell.handle(moved(150, 150), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);

    // Sequence that forces different window motion outcomes by changing focus.
    let events = &[
        moved(151, 151), // Moved(w1)
        moved(155, 155), // Moved(w1) - collapses into event 0
        moved(350, 350), // Moved(w1) - still collapses (w1 focused), local is (250, 250) clamped to 99
        PRIMARY_PRESS,   // Activated(w2) - Interrupts the run
        moved(351, 351), // Moved(w2)
        moved(150, 150), // Moved(w2) - collapses (w2 focused)
        PRIMARY_PRESS,   // Activated(w1) - Interrupts the run
        moved(152, 152), // Moved(w1)
    ];
    let outcomes = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    // Expected sequence:
    // 0: Moved(w1) (collapsed events 0, 1, 2)
    // 1: Activated(w2)
    // 2: Moved(w2) (collapsed events 3, 4)
    // 3: Activated(w1)
    // 4: Moved(w1)
    assert_eq!(outcomes.len(), 5);
    assert!(matches!(
        outcomes[0],
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { window, .. }) if window == w1
    ));
    assert!(matches!(
        outcomes[1],
        ShellOutcome::WindowManager(InputResponse::Activated { window, .. }) if window == w2
    ));
    assert!(matches!(
        outcomes[2],
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { window, .. }) if window == w2
    ));
    assert!(matches!(
        outcomes[3],
        ShellOutcome::WindowManager(InputResponse::Activated { window, .. }) if window == w1
    ));
    assert!(matches!(
        outcomes[4],
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { window, .. }) if window == w1
    ));
}

#[test]
fn pump_coalescing_is_safe_because_handle_runs_per_event() {
    let mut shell = shell();
    let mut comp = compositor();
    let _window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    // Focus the window.
    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);

    // Run of motions.
    let events = &[moved(251, 251), moved(252, 252), moved(253, 253)];
    let _ = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    // Observable: pointer position reflects the last sample.
    assert_eq!(shell.router().pointer(), Point::new(253, 253));
}

#[test]
fn motion_is_ignored_and_repaints_only_when_the_hover_changes() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let bar = shell.presenter().bar_window().expect("the bar is painted");
    comp.composite();

    // Motion over the empty desktop (far from the bar). It reaches the
    // session as the desktop's own motion — the bar draws nothing for it.
    let outcomes = shell
        .pump(&mut MemoryInput::new(&[moved(900, 500)]), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(
        outcomes,
        [ShellOutcome::WindowManager(
            InputResponse::DesktopPointerMoved
        )]
    );
    let bar_rect = window_rect(&comp, bar);
    assert!(
        comp.composite().bounds().intersection(&bar_rect).is_empty(),
        "a motion that crosses no control repaints no part of the bar"
    );

    // Motion onto the library button. The bar is itself a compositor
    // window, so the window manager reports the motion over it; nothing is
    // forwarded, because no application owns the bar.
    let outcomes = shell
        .pump(&mut MemoryInput::new(&[moved(24, 1060)]), &mut comp, 0)
        .expect("source does not fault");
    assert_eq!(
        outcomes,
        [ShellOutcome::WindowManager(
            InputResponse::ClientPointerMoved {
                window: bar,
                local: Point::new(24, 20),
            }
        )]
    );

    // The hover changed, so the bar — and only the bar — was repainted.
    assert_eq!(
        shell.presenter().bar_window(),
        Some(bar),
        "the same bar window is reused"
    );
    assert_eq!(
        shell.session().taskbar().library_button().state().pointer,
        PointerState::Hover
    );
    let damage = comp.composite().bounds();
    assert_eq!(
        damage.union(&bar_rect),
        damage,
        "the hover change repainted the whole bar"
    );
}

#[test]
fn begin_move_through_the_shell_arms_a_grab_on_the_focused_window() {
    let mut shell = shell();
    let mut comp = compositor();
    opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
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

    shell.handle(moved(300, 400), &mut comp, 0);

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

    shell.handle(moved(250, 250), &mut comp, 0);
    assert_eq!(
        shell.cursor().kind(),
        CursorKind::Text,
        "over the window the pointer takes the window's cursor hint"
    );

    shell.handle(moved(900, 500), &mut comp, 0);
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
    let unscaled = comp.window(bar).expect("bar window").client_size().1;

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
        .client_size()
        .1;
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
    shell.handle(moved(24, 1060), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    shell.handle(moved(150, 150), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    shell.handle(moved(150, 150), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    shell.handle(moved(at.x, at.y), &mut comp, 0);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    shell.handle(moved(150, 150), &mut comp, 0);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    shell.handle(moved(700, 400), &mut comp, 0);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    let mut shell = shell_for(TaskbarConfig::bottom_bar(WIDTH, HEIGHT));
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
    let mut comp = Compositor::new(
        mode,
        Color::rgb(0, 0, 0),
        test_chrome_cache(),
        test_pressure(),
    )
    .expect("compositor");

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
            shell.handle(moved(at.x, at.y), comp, 0),
            shell.handle(PRIMARY_PRESS, comp, 0),
            shell.handle(
                InputEvent::PointerReleased {
                    button: PointerButton::Primary,
                },
                comp,
                0,
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

/// A headless desktop for the session's own trusted windows: a shell over a
/// bottom taskbar and a compositor at a small fixed mode.
fn headless_desktop() -> (DesktopShell, Compositor) {
    let shell = shell_for(TaskbarConfig::bottom_bar(640, 480));
    let mode = DisplayMode {
        width_px: 640,
        height_px: 480,
        stride_bytes: 640 * 4,
        format: DisplayFormat::Rgba8888,
    };
    let compositor = Compositor::new(
        mode,
        Color::rgb(0, 0, 0),
        test_chrome_cache(),
        test_pressure(),
    )
    .expect("compositor builds");
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
    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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

    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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
    let (mut shell, mut comp) = headless_desktop();
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
    shell.handle(moved(24, 1060), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    shell.handle(moved(at.x, at.y), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    shell.handle(moved(24, 1060), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);

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

    // A press on a program row arms the pin drag; the launch is the
    // release that follows without the pointer leaving the row.
    let at = row_at(&shell, "Calc");
    let outcome = shell.handle(moved(at.x, at.y), &mut comp, 0);
    assert_eq!(outcome, ShellOutcome::Ignored);
    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp, 0),
        ShellOutcome::Ignored
    );
    let outcome = shell.handle(PRIMARY_RELEASE, &mut comp, 0);

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

    // (f) pin_at clamps an out-of-range index to the end
    let (mut pins, _) = SessionPins::load(&mut MemoryAssets::default(), Some(home));
    let index = pins
        .pin_at(
            &mut writer,
            99,
            PinTarget::Bundle(BundlePath::new("/Apps/editor.app").unwrap()),
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
    assert_eq!(service.take_drag_for(DragOrigin::Window(9)), None);
    assert!(service.drag_armed());

    // the program library is a different origin too, and claims nothing a
    // window offered
    assert_eq!(service.take_drag_for(DragOrigin::Library), None);
    assert!(service.drag_armed());

    // same window consumes
    assert_eq!(
        service.take_drag_for(DragOrigin::Window(7)),
        Some(PinTarget::Bundle(bundle))
    );
    assert!(!service.drag_armed());

    // second offer replaces
    service.drag_offered(7, "/Apps/one.app");
    service.drag_offered(7, "/Apps/two.app");
    assert_eq!(
        service.take_drag_for(DragOrigin::Window(7)),
        Some(PinTarget::Bundle(BundlePath::new("/Apps/two.app").unwrap()))
    );

    // a library drag arms and is consumed on the same terms, carrying the
    // catalogued entry rather than a path guessed from a row label
    let entry = EntryId::new("os.tairix.editor").unwrap();
    service.offer_drag(DragOrigin::Library, PinTarget::Entry(entry.clone()));
    assert!(service.drag_armed());
    service.withdraw_drag(DragOrigin::Window(7)); // no effect
    assert!(service.drag_armed());
    assert_eq!(
        service.take_drag_for(DragOrigin::Library),
        Some(PinTarget::Entry(entry))
    );
    assert!(!service.drag_armed());

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
    let library = catalog(&[("editor", "Editor", LibraryCategory::Office)]);
    let dragged_entry = EntryId::new("os.tairix.editor").unwrap();

    // Nothing armed: a release is never a drop.
    let mut idle = service();
    assert_eq!(
        resolve_pin_drop(
            &mut idle,
            Some(DragOrigin::Window(7)),
            &library,
            &layout,
            on_band
        ),
        None
    );

    // A release from an unserved window leaves the offer armed.
    let mut unserved = service();
    assert!(unserved.drag_offered(7, "/Apps/editor.app"));
    assert_eq!(
        resolve_pin_drop(&mut unserved, None, &library, &layout, on_band),
        None
    );
    assert!(unserved.drag_armed());

    // A release from the offering window over the pin band pins at the
    // drop index and persists the store.
    let mut landing = service();
    assert!(landing.drag_offered(7, "/Apps/editor.app"));
    assert_eq!(
        resolve_pin_drop(
            &mut landing,
            Some(DragOrigin::Window(7)),
            &library,
            &layout,
            on_band
        ),
        Some(PinDecision::Pinned)
    );
    assert!(!landing.drag_armed());
    assert_eq!(landing.pins().list().len(), 1);
    assert!(landing.take_dirty());

    // A program dragged out of the library popup pins on exactly the same
    // terms: one drop path, whichever surface the gesture started on.
    let mut dragged = service();
    dragged.offer_drag(DragOrigin::Library, PinTarget::Entry(dragged_entry.clone()));
    assert_eq!(
        resolve_pin_drop(
            &mut dragged,
            Some(DragOrigin::Window(7)),
            &library,
            &layout,
            on_band
        ),
        None,
        "a window's release cannot claim the library's drag"
    );
    assert!(dragged.drag_armed());
    assert_eq!(
        resolve_pin_drop(
            &mut dragged,
            Some(DragOrigin::Library),
            &library,
            &layout,
            on_band
        ),
        Some(PinDecision::Pinned)
    );
    assert_eq!(
        dragged.pins().list().get(0),
        Some(&PinTarget::Entry(dragged_entry)),
        "the store records the catalogued entry, not a guessed path"
    );

    // A release from the offering window away from the band ends the
    // gesture without pinning (the offer is consumed either way).
    let mut stray = service();
    assert!(stray.drag_offered(7, "/Apps/editor.app"));
    assert_eq!(
        resolve_pin_drop(
            &mut stray,
            Some(DragOrigin::Window(7)),
            &library,
            &layout,
            Point::new(2, 2)
        ),
        None
    );
    assert!(!stray.drag_armed());
    assert_eq!(stray.pins().list().len(), 0);
}

/// Open the library popup and press the row labelled `label`, returning the
/// entry that row names and the point pressed. Mirrors the user gesture:
/// the Library button, then the row itself.
fn press_library_row(
    shell: &mut DesktopShell,
    comp: &mut Compositor,
    label: &str,
) -> (EntryId, Point) {
    shell.handle(moved(24, 1060), comp, 0);
    shell.handle(PRIMARY_PRESS, comp, 0);
    shell.handle(PRIMARY_RELEASE, comp, 0);
    let layout = shell.session().taskbar().library_layout(Scale::ONE);
    let index = shell
        .session()
        .taskbar()
        .library()
        .rows()
        .iter()
        .position(|row| matches!(row, LibraryRow::Entry { name, .. } if name.as_str() == label))
        .expect("a row with that label");
    let LibraryRow::Entry { id, .. } = shell.session().taskbar().library().rows()[index].clone()
    else {
        panic!("expected a program row");
    };
    let (_, rect) = layout
        .rows
        .iter()
        .find(|(shown, _)| *shown == index)
        .expect("the row is on screen");
    let at = Point::new(
        rect.left() + i32::try_from(rect.width / 2).expect("fits"),
        rect.top() + i32::try_from(rect.height / 2).expect("fits"),
    );
    shell.handle(moved(at.x, at.y), comp, 0);
    assert_eq!(
        shell.handle(PRIMARY_PRESS, comp, 0),
        ShellOutcome::Ignored,
        "a press only arms the gesture"
    );
    (id, at)
}

/// A point far enough from `from` that the shared drag detector must call
/// the motion a drag rather than a click.
fn past_drag_threshold(from: Point) -> Point {
    let travel = i32::try_from(tairix_browse::DRAG_THRESHOLD_PX).expect("small") + 1;
    Point::new(from.x, from.y + travel)
}

/// The centre of the bar's pin band — where a drop pins.
fn on_the_pin_band(shell: &DesktopShell) -> Point {
    let layout = shell.session().taskbar().layout(Scale::ONE);
    Point::new(
        layout.task_list.left() + 10,
        layout.task_list.top() + i32::try_from(layout.task_list.height / 2).expect("fits"),
    )
}

/// A pin service over the in-memory seams with an empty store.
fn library_pin_service() -> PinService<MemoryAssets, MemoryWriter> {
    PinService::new(
        MemoryAssets::default(),
        MemoryWriter::default(),
        SessionPins::load(&mut MemoryAssets::default(), Some("/Users/alice")).0,
    )
}

/// Apply one shell outcome to `service` exactly as the live event loop
/// does, so the host test drives the same glue the running desktop does.
fn route_pin_drag(
    outcome: &ShellOutcome,
    shell: &DesktopShell,
    service: &mut PinService<MemoryAssets, MemoryWriter>,
    catalog: &Catalog,
    pointer: Point,
) -> Option<PinDecision> {
    match outcome {
        ShellOutcome::Taskbar(TaskbarResponse::PinDragOffered { entry }) => {
            service.offer_drag(DragOrigin::Library, PinTarget::Entry(entry.clone()));
            None
        }
        ShellOutcome::Taskbar(TaskbarResponse::PinDragDropped) => {
            let layout = shell.session().taskbar().layout(Scale::ONE);
            crate::pins::resolve_pin_drop(
                service,
                Some(DragOrigin::Library),
                catalog,
                &layout,
                pointer,
            )
        }
        ShellOutcome::Taskbar(
            TaskbarResponse::PinDragWithdrawn | TaskbarResponse::LibraryDismissed,
        ) => {
            service.withdraw_drag(DragOrigin::Library);
            None
        }
        _ => None,
    }
}

#[test]
fn dragging_a_program_out_of_the_library_onto_the_pin_band_pins_it() {
    let mut shell = shell();
    let mut comp = compositor();
    let library = office_and_games();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(library.clone());
    let mut service = library_pin_service();

    let (id, press) = press_library_row(&mut shell, &mut comp, "Chess");
    let dragging = past_drag_threshold(press);
    let offered = shell.handle(moved(dragging.x, dragging.y), &mut comp, 0);
    assert_eq!(
        offered,
        ShellOutcome::Taskbar(TaskbarResponse::PinDragOffered { entry: id.clone() })
    );
    route_pin_drag(&offered, &shell, &mut service, &library, dragging);
    assert!(service.drag_armed());

    let band = on_the_pin_band(&shell);
    shell.handle(moved(band.x, band.y), &mut comp, 0);
    let dropped = shell.handle(PRIMARY_RELEASE, &mut comp, 0);
    assert_eq!(
        dropped,
        ShellOutcome::Taskbar(TaskbarResponse::PinDragDropped)
    );
    assert_eq!(
        route_pin_drag(&dropped, &shell, &mut service, &library, band),
        Some(PinDecision::Pinned)
    );

    // The store records the catalogued entry itself, so the pin references
    // an application the library can vouch for rather than a guessed path.
    assert_eq!(
        service.pins().list().get(0),
        Some(&PinTarget::Entry(id)),
        "the pin names the entry that was dragged"
    );
    assert!(!service.drag_armed(), "the gesture is fully unwound");
}

#[test]
fn a_library_drag_released_away_from_the_pin_band_pins_nothing() {
    let mut shell = shell();
    let mut comp = compositor();
    let library = office_and_games();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(library.clone());
    let mut service = library_pin_service();

    let (_, press) = press_library_row(&mut shell, &mut comp, "Chess");
    let dragging = past_drag_threshold(press);
    let offered = shell.handle(moved(dragging.x, dragging.y), &mut comp, 0);
    route_pin_drag(&offered, &shell, &mut service, &library, dragging);

    // Released over the desktop, nowhere near the bar.
    shell.handle(moved(600, 300), &mut comp, 0);
    let dropped = shell.handle(PRIMARY_RELEASE, &mut comp, 0);
    assert_eq!(
        route_pin_drag(
            &dropped,
            &shell,
            &mut service,
            &library,
            Point::new(600, 300)
        ),
        None
    );
    assert!(service.pins().list().is_empty());
    assert!(
        !service.drag_armed(),
        "one gesture, one decision: the offer never lingers"
    );
}

#[test]
fn dragging_a_program_that_is_already_pinned_does_not_duplicate_it() {
    let mut shell = shell();
    let mut comp = compositor();
    let library = office_and_games();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(library.clone());
    let mut service = library_pin_service();
    let chess = EntryId::new("os.tairix.chess").unwrap();
    assert_eq!(
        service.pin_target_at(0, PinTarget::Entry(chess.clone()), &library),
        PinDecision::Pinned
    );

    let (_, press) = press_library_row(&mut shell, &mut comp, "Chess");
    let dragging = past_drag_threshold(press);
    let offered = shell.handle(moved(dragging.x, dragging.y), &mut comp, 0);
    route_pin_drag(&offered, &shell, &mut service, &library, dragging);

    let band = on_the_pin_band(&shell);
    shell.handle(moved(band.x, band.y), &mut comp, 0);
    let dropped = shell.handle(PRIMARY_RELEASE, &mut comp, 0);
    assert_eq!(
        route_pin_drag(&dropped, &shell, &mut service, &library, band),
        Some(PinDecision::AlreadyPinned)
    );
    assert_eq!(service.pins().list().len(), 1, "still pinned exactly once");
}

#[test]
fn a_drag_of_a_program_that_left_the_catalog_is_refused_and_pins_nothing() {
    let mut shell = shell();
    let mut comp = compositor();
    let library = office_and_games();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(library.clone());
    let mut service = library_pin_service();

    let (_, press) = press_library_row(&mut shell, &mut comp, "Chess");
    let dragging = past_drag_threshold(press);
    let offered = shell.handle(moved(dragging.x, dragging.y), &mut comp, 0);
    route_pin_drag(&offered, &shell, &mut service, &library, dragging);

    // The program is uninstalled while the pointer is still down: the drop
    // re-checks the catalog, so a pin that could never launch is refused.
    let without_chess = catalog(&[
        ("write", "Write", LibraryCategory::Office),
        ("calc", "Calc", LibraryCategory::Office),
    ]);
    let band = on_the_pin_band(&shell);
    shell.handle(moved(band.x, band.y), &mut comp, 0);
    let dropped = shell.handle(PRIMARY_RELEASE, &mut comp, 0);
    assert_eq!(
        route_pin_drag(&dropped, &shell, &mut service, &without_chess, band),
        Some(PinDecision::Refused)
    );
    assert!(service.pins().list().is_empty());
}

#[test]
fn dismissing_the_popup_mid_drag_withdraws_the_offer() {
    let mut shell = shell();
    let mut comp = compositor();
    let library = office_and_games();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(library.clone());
    let mut service = library_pin_service();

    let (_, press) = press_library_row(&mut shell, &mut comp, "Chess");
    let dragging = past_drag_threshold(press);
    let offered = shell.handle(moved(dragging.x, dragging.y), &mut comp, 0);
    route_pin_drag(&offered, &shell, &mut service, &library, dragging);
    assert!(service.drag_armed());

    let escape = key_press(Key::Named(NamedKey::Escape));
    let withdrawn = shell.handle(escape, &mut comp, 0);
    assert!(matches!(
        withdrawn,
        ShellOutcome::Taskbar(
            TaskbarResponse::PinDragWithdrawn | TaskbarResponse::LibraryDismissed
        )
    ));
    route_pin_drag(&withdrawn, &shell, &mut service, &library, dragging);
    assert!(
        !service.drag_armed(),
        "a withdrawn drag can never pin later"
    );
    assert!(service.pins().list().is_empty());
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
    shell.handle(moved(at.x, at.y), &mut comp, 0);
    let outcome = shell.handle(SECONDARY_PRESS, &mut comp, 0);

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

    shell.handle(moved(first_row.x, first_row.y), &mut comp, 0);
    // Many controls activate on release.
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
    let outcome = shell.handle(
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        &mut comp,
        0,
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
    shell.handle(moved(150, 150), &mut comp, 0);
    let outcome = shell.handle(SECONDARY_PRESS, &mut comp, 0);

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

pub(crate) fn manifest_fixture(name: &str, icon: Option<&str>) -> Vec<u8> {
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
    let mut shell = shell_for(TaskbarConfig::bottom_bar(W, H));
    let mode = DisplayMode {
        width_px: W,
        height_px: H,
        stride_bytes: W * 4,
        format: DisplayFormat::Rgba8888,
    };
    let mut comp = Compositor::new(
        mode,
        Color::rgb(0, 0, 0),
        test_chrome_cache(),
        test_pressure(),
    )
    .expect("compositor");

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
    let _ = shell.handle(moved(card.x, card.y), &mut comp, 0);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp, 0);
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
    assert!(!tracker.note_refused(7, tairix_abi::Errno::WouldBlock, 1_000));
    assert!(!tracker.is_unresponsive(7));
    assert_eq!(tracker.unresponsive_count(), 0);

    // Refusals inside the threshold keep the verdict unchanged.
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::WouldBlock,
        1_000 + crate::UNRESPONSIVE_AFTER_NS / 2,
    ));
    assert!(!tracker.is_unresponsive(7));

    // The refusal that crosses the threshold flags the owner — exactly once.
    assert!(tracker.note_refused(
        7,
        tairix_abi::Errno::WouldBlock,
        1_000 + crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(tracker.is_unresponsive(7));
    assert_eq!(tracker.unresponsive_count(), 1);
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::WouldBlock,
        2_000 + crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert_eq!(tracker.unresponsive_count(), 1);
}

#[test]
fn hang_tracker_clears_on_an_accepted_delivery() {
    let mut tracker = crate::HangTracker::new();

    // A suspect that drains before the threshold was never unresponsive:
    // clearing it changes nothing the tray must repaint.
    assert!(!tracker.note_refused(7, tairix_abi::Errno::WouldBlock, 0));
    assert!(!tracker.note_delivered(7));

    // A flagged owner that drains recovers, and the change is reported.
    assert!(!tracker.note_refused(7, tairix_abi::Errno::WouldBlock, 0));
    assert!(tracker.note_refused(
        7,
        tairix_abi::Errno::WouldBlock,
        crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(tracker.note_delivered(7));
    assert!(!tracker.is_unresponsive(7));
    assert_eq!(tracker.unresponsive_count(), 0);

    // The suspicion window restarts from scratch after a recovery.
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::WouldBlock,
        2 * crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(!tracker.is_unresponsive(7));
}

#[test]
fn hang_tracker_treats_only_backpressure_as_evidence() {
    let mut tracker = crate::HangTracker::new();

    // A torn-down mailbox is the reap path's business, not hang evidence —
    // and it drops any standing suspicion so a recycled task id starts clean.
    assert!(!tracker.note_refused(7, tairix_abi::Errno::WouldBlock, 0));
    assert!(!tracker.note_refused(7, tairix_abi::Errno::NotFound, 1));
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::WouldBlock,
        crate::UNRESPONSIVE_AFTER_NS + 2,
    ));
    assert!(!tracker.is_unresponsive(7));

    // Any other refusal is no evidence either way — including
    // `LengthOutOfRange`, the syscall layer's malformed-call error, which
    // must never be miscounted as the receiver hanging.
    assert!(!tracker.note_refused(9, tairix_abi::Errno::PermissionDenied, 0));
    assert!(!tracker.note_refused(9, tairix_abi::Errno::MessageTooLarge, 0));
    assert!(!tracker.note_refused(9, tairix_abi::Errno::LengthOutOfRange, 0));
    assert!(!tracker.is_unresponsive(9));
    assert_eq!(tracker.unresponsive_count(), 0);
}

/// A continuous run of `LengthOutOfRange` refusals — the syscall layer's
/// malformed-call error, not the mailbox-full backpressure signal — never
/// accumulates into a verdict, even once enough time has passed that real
/// backpressure would have crossed the threshold. This is the regression
/// case for the bug where the tracker keyed on an errno that also meant
/// "malformed call": a bug in the sender must never read as the receiver
/// hanging.
#[test]
fn hang_tracker_never_flags_a_malformed_call_refusal() {
    let mut tracker = crate::HangTracker::new();

    assert!(!tracker.note_refused(7, tairix_abi::Errno::LengthOutOfRange, 0));
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(!tracker.note_refused(
        7,
        tairix_abi::Errno::LengthOutOfRange,
        10 * crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(!tracker.is_unresponsive(7));
    assert_eq!(tracker.unresponsive_count(), 0);
}

#[test]
fn hang_tracker_forget_reports_only_a_standing_verdict() {
    let mut tracker = crate::HangTracker::new();

    // Forgetting an unknown or merely-suspect owner changes nothing.
    assert!(!tracker.forget(7));
    assert!(!tracker.note_refused(7, tairix_abi::Errno::WouldBlock, 0));
    assert!(!tracker.forget(7));

    // Forgetting a flagged owner (its exit was reaped) clears the verdict
    // and reports the change so the tray repaints.
    assert!(!tracker.note_refused(8, tairix_abi::Errno::WouldBlock, 0));
    assert!(tracker.note_refused(
        8,
        tairix_abi::Errno::WouldBlock,
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
        assert!(!tracker.note_refused(owner, tairix_abi::Errno::WouldBlock, 0));
        assert!(tracker.note_refused(
            owner,
            tairix_abi::Errno::WouldBlock,
            crate::UNRESPONSIVE_AFTER_NS,
        ));
    }
    assert_eq!(tracker.unresponsive_count(), 3);

    // The count is a u16 for the tray summary; a pathological census
    // saturates rather than wrapping.
    for owner in 3..70_000u64 {
        let _ = tracker.note_refused(owner, tairix_abi::Errno::WouldBlock, 0);
        let _ = tracker.note_refused(
            owner,
            tairix_abi::Errno::WouldBlock,
            crate::UNRESPONSIVE_AFTER_NS,
        );
    }
    assert_eq!(tracker.unresponsive_count(), u16::MAX);
}

/// `unresponsive_owners` walks exactly the flagged set, in ascending
/// order, naming neither a merely-suspect owner nor one that has since
/// recovered or been forgotten.
#[test]
fn hang_tracker_unresponsive_owners_names_exactly_the_flagged_set() {
    let mut tracker = crate::HangTracker::new();
    assert_eq!(
        tracker.unresponsive_owners().collect::<Vec<_>>(),
        Vec::<u64>::new()
    );

    // A merely-suspect owner (below the threshold) is not named.
    assert!(!tracker.note_refused(9, tairix_abi::Errno::WouldBlock, 0));
    assert!(tracker.unresponsive_owners().next().is_none());

    // Two owners cross the threshold and are both named, ascending.
    assert!(tracker.note_refused(
        9,
        tairix_abi::Errno::WouldBlock,
        crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert!(!tracker.note_refused(3, tairix_abi::Errno::WouldBlock, 0));
    assert!(tracker.note_refused(
        3,
        tairix_abi::Errno::WouldBlock,
        crate::UNRESPONSIVE_AFTER_NS,
    ));
    assert_eq!(
        tracker.unresponsive_owners().collect::<Vec<_>>(),
        vec![3, 9]
    );

    // A recovered owner drops out; a forgotten one too.
    assert!(tracker.note_delivered(3));
    assert_eq!(tracker.unresponsive_owners().collect::<Vec<_>>(), vec![9]);
    assert!(tracker.forget(9));
    assert!(tracker.unresponsive_owners().next().is_none());
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
        power_capable: false,
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

/// The capsule's gestures reach the session as the bar's own open request:
/// a quick press and release asks for the live task list, and a press held
/// past the bar's long-press threshold asks for recovery instead. The press
/// itself acts on nothing — the gesture resolves on release.
#[test]
fn capsule_gestures_ask_the_session_to_open_switchboard() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);

    // Hovering the capsule expands its instrument readout as a popover
    // window; that is presentation, and the press only begins the gesture.
    let capsule = capsule_point(&shell);
    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp, 0);
    assert!(shell.session().taskbar().tray().is_expanded());
    assert!(shell.presenter().readout_window().is_some());
    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp, 0),
        ShellOutcome::Ignored,
        "the press begins the gesture and acts on nothing yet"
    );

    // Released promptly, it asks for the running-task section.
    assert_eq!(
        shell.handle(PRIMARY_RELEASE, &mut comp, QUICK_PRESS_NS),
        ShellOutcome::Taskbar(TaskbarResponse::OpenSwitchboard {
            section: Section::Tasks,
        })
    );

    // Held past the threshold, the same gesture asks for recovery.
    let _ = shell.handle(PRIMARY_PRESS, &mut comp, 0);
    assert_eq!(
        shell.handle(PRIMARY_RELEASE, &mut comp, LONG_PRESS_NS),
        ShellOutcome::Taskbar(TaskbarResponse::OpenSwitchboard {
            section: Section::Recovery,
        })
    );
}

/// A primary release the taskbar does not claim still ends the window
/// manager's in-flight move-grab: offering releases to the bar first must
/// not swallow them.
#[test]
fn a_release_off_the_capsule_still_ends_a_window_grab() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    let _ = shell.handle(moved(250, 250), &mut comp, 0);
    let _ = shell.handle(PRIMARY_PRESS, &mut comp, 0);
    assert!(shell.begin_move(&comp), "the focused window is grabbable");
    let _ = shell.handle(moved(280, 280), &mut comp, 0);

    assert_eq!(
        shell.handle(PRIMARY_RELEASE, &mut comp, 0),
        ShellOutcome::WindowManager(InputResponse::MoveEnded { window })
    );
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
    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp, 0);
    let outcome = shell.handle(InputEvent::PointerScrolled { dx: 0, dy: 1 }, &mut comp, 0);
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
    let _ = shell.handle(moved(600, 300), &mut comp, 0);
    let outcome = shell.handle(InputEvent::PointerScrolled { dx: 0, dy: 1 }, &mut comp, 0);
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
    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp, 0);
    let outcome = shell.handle(MIDDLE_PRESS, &mut comp, 0);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::TaskActivated {
            id: first_task,
            outcome: ActivateOutcome::Activated,
        })
    );

    // The handover made the second task the previous one: middle-click
    // toggles back.
    let outcome = shell.handle(MIDDLE_PRESS, &mut comp, 0);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::TaskActivated {
            id: second_task,
            outcome: ActivateOutcome::Activated,
        })
    );

    // A middle press on the open desktop is inert.
    let _ = shell.handle(moved(600, 300), &mut comp, 0);
    assert_eq!(
        shell.handle(MIDDLE_PRESS, &mut comp, 0),
        ShellOutcome::Ignored
    );
}

// ---- switchboard command channel --------------------------------------

/// The pid the launch table records the desktop's own monitor instance
/// under, launched from the session's one `SWITCHBOARD_RUN_PATH`.
const MONITOR_PID: u64 = 40;

/// This session's own kernel-attested identity, as the window channel's
/// create reply already carries it.
fn session_proc_id() -> ProcId {
    ProcId::from_raw([7u8; tairix_abi::PROC_ID_LEN])
}

/// A launch table holding a live monitor instance, exactly as bring-up
/// records it.
fn monitor_launched() -> LaunchTable {
    let mut launched = LaunchTable::new();
    launched.record(MONITOR_PID, "Switchboard", SWITCHBOARD_RUN_PATH);
    launched
}

/// A fixed owner→window map standing in for the session's live window
/// registry: an owner not in it currently owns no window on this seat.
struct FakeOwnerWindows {
    windows: Vec<(u64, WindowId)>,
}

impl FakeOwnerWindows {
    fn none() -> Self {
        Self {
            windows: Vec::new(),
        }
    }

    fn of(owner: u64, window: WindowId) -> Self {
        Self {
            windows: vec![(owner, window)],
        }
    }
}

impl OwnerWindow for FakeOwnerWindows {
    fn window_of(&self, owner: u64) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|(id, _)| *id == owner)
            .map(|(_, window)| *window)
    }
}

/// A recording [`SwitchboardMailbox`] whose every send lands: each command
/// the session sent, in order, addressed to the instance it named. The
/// live seam makes one attempt per event, never a retry loop, so counting
/// sends here is exactly the guarantee under test.
#[derive(Default)]
struct RecordingMailbox {
    sent: Vec<(u64, SwitchboardCommand)>,
}

impl SwitchboardMailbox for RecordingMailbox {
    fn send(&mut self, pid: u64, command: SwitchboardCommand) -> bool {
        self.sent.push((pid, command));
        true
    }
}

/// A relaunch seam recording what it was asked to launch, standing in for
/// the session's one attested spawn-and-record path.
#[derive(Default)]
struct Relaunches {
    launched: Vec<(String, String)>,
}

/// Serve one request as the attested monitor would send it.
fn serve_as_monitor(
    shell: &mut DesktopShell,
    comp: &mut Compositor,
    launched: &mut LaunchTable,
    owners: &dyn OwnerWindow,
    relaunches: &mut Relaunches,
    request: &SwitchboardRequest,
) -> Result<SwitchboardOutcome, SwitchboardRefusal> {
    serve_switchboard_request(
        SwitchboardServe {
            shell,
            compositor: comp,
            launched,
            owner_windows: owners,
            relaunch: &mut |_: &mut LaunchTable, run_path: &str, label: &str| {
                relaunches
                    .launched
                    .push((String::from(run_path), String::from(label)));
            },
            self_proc_id: session_proc_id(),
        },
        MONITOR_PID,
        &request.to_le_bytes(),
    )
}

/// A successful publish answers with this session's own attested identity,
/// which the monitor needs to authenticate the commands the session later
/// sends it; the summary reaches the capsule on the same call.
#[test]
fn a_publish_relays_the_summary_and_answers_with_the_session_identity() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut launched = monitor_launched();
    let mut relaunches = Relaunches::default();

    let outcome = serve_as_monitor(
        &mut shell,
        &mut comp,
        &mut launched,
        &FakeOwnerWindows::none(),
        &mut relaunches,
        &SwitchboardRequest::PublishSummary {
            summary: tray_summary(2),
        },
    );

    assert_eq!(
        outcome,
        Ok(SwitchboardOutcome::Published(session_proc_id()))
    );
    assert_eq!(
        shell.session().taskbar().tray().signal().state().activity,
        tairix_controls::ActivityState::Working,
        "the published summary drove the capsule"
    );
}

/// Only the monitor this session launched may call: every operation from an
/// unattested caller is refused as a permission denial and leaves the model
/// untouched.
#[test]
fn an_unattested_caller_is_refused_for_every_operation() {
    let window = {
        let mut shell = shell();
        let mut comp = compositor();
        open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor")
    };
    let requests = [
        SwitchboardRequest::PublishSummary {
            summary: tray_summary(2),
        },
        SwitchboardRequest::ActivateOwner { owner: 41 },
        SwitchboardRequest::RestartOwner { owner: 41 },
    ];

    for request in &requests {
        let mut shell = shell();
        let mut comp = compositor();
        let mut launched = monitor_launched();
        launched.record(41, "Editor", "/Apps/Editor.app/Run");
        let mut relaunches = Relaunches::default();

        let refusal = serve_switchboard_request(
            SwitchboardServe {
                shell: &mut shell,
                compositor: &mut comp,
                launched: &mut launched,
                owner_windows: &FakeOwnerWindows::of(41, window),
                relaunch: &mut |_: &mut LaunchTable, run_path: &str, label: &str| {
                    relaunches
                        .launched
                        .push((String::from(run_path), String::from(label)));
                },
                self_proc_id: session_proc_id(),
            },
            // Not the launch table's live monitor: an orphan, a foreign
            // process, or a copy launched by hand.
            MONITOR_PID + 1,
            &request.to_le_bytes(),
        );

        assert_eq!(refusal, Err(SwitchboardRefusal::Unattested));
        assert_eq!(
            refusal.unwrap_err().errno(),
            Errno::PermissionDenied,
            "an unattested caller is a permission denial"
        );
        assert_eq!(
            shell.session().taskbar().tray().signal().state().activity,
            tairix_controls::ActivityState::Idle,
            "a refused call publishes nothing"
        );
        assert!(
            relaunches.launched.is_empty(),
            "a refused call launches nothing"
        );
    }
}

/// A malformed frame from the attested monitor is refused fail-closed and
/// changes nothing.
#[test]
fn a_malformed_switchboard_frame_is_refused() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut launched = monitor_launched();
    let mut relaunches = Relaunches::default();

    let refusal = serve_switchboard_request(
        SwitchboardServe {
            shell: &mut shell,
            compositor: &mut comp,
            launched: &mut launched,
            owner_windows: &FakeOwnerWindows::none(),
            relaunch: &mut |_: &mut LaunchTable, run_path: &str, label: &str| {
                relaunches
                    .launched
                    .push((String::from(run_path), String::from(label)));
            },
            self_proc_id: session_proc_id(),
        },
        MONITOR_PID,
        b"not a switchboard frame",
    )
    .expect_err("a malformed frame is refused");

    assert!(matches!(refusal, SwitchboardRefusal::Malformed(_)));
    assert_eq!(
        shell.session().taskbar().tray().signal().state().activity,
        tairix_controls::ActivityState::Idle
    );
}

/// An `ActivateOwner` naming an owner with no live window on this seat is
/// `NotFound`: the session never guesses which window was meant.
#[test]
fn activate_owner_refuses_an_owner_with_no_live_window() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut launched = monitor_launched();
    let mut relaunches = Relaunches::default();

    let refusal = serve_as_monitor(
        &mut shell,
        &mut comp,
        &mut launched,
        &FakeOwnerWindows::none(),
        &mut relaunches,
        &SwitchboardRequest::ActivateOwner { owner: 41 },
    )
    .expect_err("an unknown owner is refused");

    assert_eq!(refusal, SwitchboardRefusal::UnknownOwner);
    assert_eq!(refusal.errno(), Errno::NotFound);
}

/// An `ActivateOwner` for a live owner raises exactly that owner's window,
/// through the same focus path a taskbar press drives.
#[test]
fn activate_owner_raises_the_named_owners_window() {
    let mut shell = shell();
    let mut comp = compositor();
    let wanted = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    let other = open_app(&mut shell, &mut comp, Point::new(360, 240), "Files");
    let wanted_task = shell.tasks().task_for(wanted).expect("tracked");
    assert_eq!(shell.router().focused(), Some(other), "the later window");
    let mut launched = monitor_launched();
    let mut relaunches = Relaunches::default();

    let outcome = serve_as_monitor(
        &mut shell,
        &mut comp,
        &mut launched,
        &FakeOwnerWindows::of(41, wanted),
        &mut relaunches,
        &SwitchboardRequest::ActivateOwner { owner: 41 },
    );

    assert_eq!(outcome, Ok(SwitchboardOutcome::Plain));
    assert_eq!(shell.router().focused(), Some(wanted));
    assert_eq!(
        shell.session().taskbar().tasks().focused(),
        Some(wanted_task)
    );
}

/// A `RestartOwner` naming an owner the desktop never launched is
/// `NotFound`, and nothing is spawned on a guess.
#[test]
fn restart_owner_refuses_an_owner_with_no_recorded_launch() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut launched = monitor_launched();
    let mut relaunches = Relaunches::default();

    let refusal = serve_as_monitor(
        &mut shell,
        &mut comp,
        &mut launched,
        &FakeOwnerWindows::none(),
        &mut relaunches,
        &SwitchboardRequest::RestartOwner { owner: 41 },
    )
    .expect_err("an unrecorded owner is refused");

    assert_eq!(refusal, SwitchboardRefusal::UnknownOwner);
    assert_eq!(refusal.errno(), Errno::NotFound);
    assert!(relaunches.launched.is_empty());
}

/// A `RestartOwner` re-launches exactly the bundle the launch table
/// recorded that owner from — its attested bundle identity, never a name
/// the caller supplied.
#[test]
fn restart_owner_relaunches_the_recorded_bundle() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut launched = monitor_launched();
    launched.record(41, "Editor", "/Apps/Editor.app/Run");
    let mut relaunches = Relaunches::default();

    let outcome = serve_as_monitor(
        &mut shell,
        &mut comp,
        &mut launched,
        &FakeOwnerWindows::none(),
        &mut relaunches,
        &SwitchboardRequest::RestartOwner { owner: 41 },
    );

    assert_eq!(outcome, Ok(SwitchboardOutcome::Plain));
    assert_eq!(
        relaunches.launched,
        vec![(String::from("/Apps/Editor.app/Run"), String::from("Editor"))]
    );
}

/// The bar's own gesture decides the section; the session relays it
/// unchanged onto the wire.
#[test]
fn every_bar_section_maps_onto_its_wire_section() {
    assert_eq!(command_section(Section::Tasks), CommandSection::Tasks);
    assert_eq!(command_section(Section::Jobs), CommandSection::Jobs);
    assert_eq!(command_section(Section::Pressure), CommandSection::Pressure);
    assert_eq!(
        command_section(Section::Activities),
        CommandSection::Activities
    );
    assert_eq!(command_section(Section::Recovery), CommandSection::Recovery);
    assert_eq!(command_section(Section::Overview), CommandSection::Overview);
}

/// With an instance live, the press sends exactly one `OpenPanel` on the
/// section the bar asked for, and nothing is left pending.
#[test]
fn the_tray_press_sends_one_open_panel_on_the_asked_section() {
    let mut pending = None;
    let mut mailbox = RecordingMailbox::default();
    let mut revived = false;

    let spawned = open_tray(
        &mut pending,
        CommandSection::Recovery,
        Some(MONITOR_PID),
        &mut mailbox,
        || {
            revived = true;
            None
        },
    );

    assert_eq!(spawned, None, "a live instance is not respawned");
    assert!(!revived, "a live instance is not revived");
    assert_eq!(
        mailbox.sent,
        vec![(
            MONITOR_PID,
            SwitchboardCommand::OpenPanel {
                section: CommandSection::Recovery,
            }
        )]
    );
    assert_eq!(pending, None, "a delivered open is not also pending");
}

/// With no instance live the press is the demand for one: the session
/// revives it and holds the section until that instance's first publish
/// proves it is listening. A second press replaces the pending open rather
/// than queueing behind it, and the delivered open is never re-sent.
#[test]
fn a_pending_open_is_delivered_on_the_next_publish_and_not_re_sent() {
    let mut pending = None;
    let mut mailbox = RecordingMailbox::default();

    let spawned = open_tray(
        &mut pending,
        CommandSection::Tasks,
        None,
        &mut mailbox,
        || Some(MONITOR_PID),
    );
    assert_eq!(spawned, Some(MONITOR_PID), "the press revived an instance");
    assert_eq!(pending, Some(CommandSection::Tasks));
    assert!(
        mailbox.sent.is_empty(),
        "nothing is sent to a dead instance"
    );

    // Pressing again before it comes up replaces the pending section.
    let _ = open_tray(
        &mut pending,
        CommandSection::Recovery,
        None,
        &mut mailbox,
        || Some(MONITOR_PID),
    );
    assert_eq!(pending, Some(CommandSection::Recovery));
    assert!(mailbox.sent.is_empty());

    // The instance's first publish delivers it, exactly once.
    deliver_pending_open(&mut pending, MONITOR_PID, &mut mailbox);
    assert_eq!(
        mailbox.sent,
        vec![(
            MONITOR_PID,
            SwitchboardCommand::OpenPanel {
                section: CommandSection::Recovery,
            }
        )]
    );
    assert_eq!(pending, None);

    deliver_pending_open(&mut pending, MONITOR_PID, &mut mailbox);
    assert_eq!(mailbox.sent.len(), 1, "a delivered open is never re-sent");
}

/// The seat report is sent only when the unresponsive set changed and only
/// to a live instance, and it always carries the truthful total even when
/// more owners are hung than one frame can name.
#[test]
fn the_seat_report_is_sent_only_on_change_and_tells_the_whole_truth() {
    let mut mailbox = RecordingMailbox::default();

    maybe_send_seat_report(false, Some(MONITOR_PID), 3, &[11, 12, 13], &mut mailbox);
    assert!(mailbox.sent.is_empty(), "an unchanged set sends nothing");

    maybe_send_seat_report(true, None, 3, &[11, 12, 13], &mut mailbox);
    assert!(mailbox.sent.is_empty(), "nothing is sent with none live");

    // More owners hung than the frame can name: the named few are bounded,
    // the total stays truthful.
    let hung: Vec<u64> = (1..=u64::try_from(SEAT_REPORT_OWNERS_MAX).unwrap() + 4).collect();
    maybe_send_seat_report(true, Some(MONITOR_PID), 12, &hung, &mut mailbox);

    let (pid, command) = mailbox.sent.first().copied().expect("one report");
    assert_eq!(pid, MONITOR_PID);
    let SwitchboardCommand::SeatReport { report } = command else {
        panic!("the change sent a seat report");
    };
    assert_eq!(report.total(), 12, "the total counts every hung owner");
    assert_eq!(report.owners(), &hung[..SEAT_REPORT_OWNERS_MAX]);
    assert_eq!(mailbox.sent.len(), 1);
}

/// A mailbox whose every send is refused — the instance is gone, or is
/// still starting and has not bound one — counting the attempts the
/// session made.
#[derive(Default)]
struct RefusingMailbox {
    attempts: usize,
}

impl SwitchboardMailbox for RefusingMailbox {
    fn send(&mut self, _pid: u64, _command: SwitchboardCommand) -> bool {
        self.attempts += 1;
        false
    }
}

/// A refused send is attempted exactly once, never retried in place: the
/// seam is one attempt per event, so a wedged monitor cannot spin the
/// desktop.
#[test]
fn a_refused_mailbox_send_is_attempted_once_rather_than_retried() {
    let mut mailbox = RefusingMailbox::default();
    let mut pending = None;

    let _ = open_tray(
        &mut pending,
        CommandSection::Tasks,
        Some(MONITOR_PID),
        &mut mailbox,
        || None,
    );
    assert_eq!(mailbox.attempts, 1, "one press is one attempt");

    maybe_send_seat_report(true, Some(MONITOR_PID), 1, &[11], &mut mailbox);
    assert_eq!(mailbox.attempts, 2, "one change is one attempt");
}

/// A press that lands while the monitor is still starting is held, not
/// lost, and the instance's own first publish carries it through.
///
/// The gap is real and wide: the launch table names the instance from the
/// moment it is spawned, but the process binds its command mailbox only
/// once its bundle has loaded and its program runs — whole seconds on a
/// cold boot. A press in that gap used to be sent to a mailbox that did
/// not exist yet and silently vanish, so the capsule did nothing.
#[test]
fn a_press_while_the_monitor_is_still_starting_opens_on_its_first_publish() {
    let mut starting = RefusingMailbox::default();
    let mut pending = None;

    let revived = open_tray(
        &mut pending,
        CommandSection::Recovery,
        Some(MONITOR_PID),
        &mut starting,
        || panic!("a live instance is never revived; it is about to publish"),
    );
    assert_eq!(revived, None);
    assert_eq!(starting.attempts, 1, "the press is attempted once");
    assert_eq!(
        pending,
        Some(CommandSection::Recovery),
        "the refused gesture is held, section and all"
    );

    // The instance finishes starting and publishes its first summary.
    let mut ready = RecordingMailbox::default();
    deliver_pending_open(&mut pending, MONITOR_PID, &mut ready);
    assert_eq!(
        ready.sent,
        vec![(
            MONITOR_PID,
            SwitchboardCommand::OpenPanel {
                section: CommandSection::Recovery
            }
        )],
        "the held press opens the section the user asked for"
    );
    assert_eq!(pending, None, "delivered once, never re-sent");
}

/// A pending open the monitor's mailbox refuses is put back rather than
/// dropped, so back-pressure delays the panel by one publish instead of
/// losing the press.
#[test]
fn a_pending_open_refused_by_a_full_mailbox_survives_to_the_next_publish() {
    let mut full = RefusingMailbox::default();
    let mut pending = Some(CommandSection::Jobs);

    deliver_pending_open(&mut pending, MONITOR_PID, &mut full);
    assert_eq!(full.attempts, 1, "one publish is one attempt");
    assert_eq!(
        pending,
        Some(CommandSection::Jobs),
        "a refused delivery keeps the press pending"
    );

    let mut drained = RecordingMailbox::default();
    deliver_pending_open(&mut pending, MONITOR_PID, &mut drained);
    assert_eq!(drained.sent.len(), 1, "the next publish delivers it");
    assert_eq!(pending, None);
}

// --- The system quick-actions menu's session half (T13) -----------------

use crate::confirm::{
    build_dialog, prompt_font, Answer, ConfirmPrompt, CONFIRM_ORIGIN, WIN_HEIGHT, WIN_WIDTH,
};
use crate::relay_power;
use tairix_abi::PowerAction;

/// The prompt-local centre of the action button at `index`, resolved through
/// the very dialog and font the prompt draws, so a test presses the button
/// rather than a re-derived guess at where it sits.
fn prompt_action_centre(action: PowerAction, index: usize, shell: &DesktopShell) -> Point {
    let theme = shell.session().active_theme();
    let rects = build_dialog(action).action_rects(
        Rect::new(0, 0, WIN_WIDTH, WIN_HEIGHT),
        Scale::ONE,
        theme,
        prompt_font(theme),
    );
    let rect = rects[index];
    assert!(rect.width > 0, "the button fitted the action band");
    Point::new(
        rect.origin.x + i32::try_from(rect.width / 2).expect("a small button width"),
        rect.origin.y + i32::try_from(rect.height / 2).expect("a small button height"),
    )
}

/// `ask` opens exactly one prompt, at its deterministic origin; a second
/// request while one is showing is refused rather than stacking a prompt
/// over the question already asked.
#[test]
fn the_confirmation_prompt_opens_once_and_refuses_a_second() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();

    assert!(confirm.ask(PowerAction::PowerOff, &mut shell, &mut comp));
    let wm = confirm.wm_id().expect("a prompt window is showing");
    assert_eq!(comp.window(wm).expect("live").origin(), CONFIRM_ORIGIN);
    assert_eq!(confirm.pending(), Some(PowerAction::PowerOff));

    assert!(
        !confirm.ask(PowerAction::Restart, &mut shell, &mut comp),
        "one prompt at a time"
    );
    assert_eq!(
        confirm.pending(),
        Some(PowerAction::PowerOff),
        "the question already asked stands"
    );
}

/// `Escape` declines: the prompt closes, the answer is a decline, and a
/// decline relays nothing to the holder of the authority.
#[test]
fn escape_declines_the_prompt_and_relays_nothing() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();
    assert!(confirm.ask(PowerAction::Restart, &mut shell, &mut comp));
    let wm = confirm.wm_id().expect("showing");

    let escape = pressed(KeyValue::Named(NamedKeyCode::Escape));
    assert_eq!(
        confirm.handle_key(&escape, &mut shell, &mut comp),
        Some(Answer::Declined)
    );
    assert_eq!(confirm.wm_id(), None, "the prompt window is closed");
    assert!(comp.window(wm).is_none(), "and gone from the compositor");

    let mut mailbox = RecordingMailbox::default();
    assert_eq!(
        relay_power(Answer::Declined, Some(MONITOR_PID), &mut mailbox),
        None
    );
    assert!(mailbox.sent.is_empty(), "a decline sends nothing");
}

/// The safe button holds keyboard focus when the prompt opens, so `Enter`
/// without a deliberate move to the other button answers "no".
#[test]
fn enter_on_an_untouched_prompt_declines() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();
    assert!(confirm.ask(PowerAction::PowerOff, &mut shell, &mut comp));

    let enter = pressed(KeyValue::Named(NamedKeyCode::Enter));
    assert_eq!(
        confirm.handle_key(&enter, &mut shell, &mut comp),
        Some(Answer::Declined),
        "the answer a stray Enter gives is the safe one"
    );
}

/// Moving focus to the destructive button and pressing `Enter` confirms the
/// transition that was asked about, and confirming relays exactly one
/// command to the holder.
#[test]
fn moving_focus_then_enter_confirms_and_relays_exactly_once() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();
    assert!(confirm.ask(PowerAction::Restart, &mut shell, &mut comp));

    let tab = pressed(KeyValue::Named(NamedKeyCode::Tab));
    assert_eq!(
        confirm.handle_key(&tab, &mut shell, &mut comp),
        None,
        "moving focus is not an answer"
    );
    let enter = pressed(KeyValue::Named(NamedKeyCode::Enter));
    let answer = confirm
        .handle_key(&enter, &mut shell, &mut comp)
        .expect("the focused button answers");
    assert_eq!(answer, Answer::Confirmed(PowerAction::Restart));
    assert_eq!(confirm.wm_id(), None, "the prompt is already down");

    let mut mailbox = RecordingMailbox::default();
    assert_eq!(relay_power(answer, Some(MONITOR_PID), &mut mailbox), None);
    assert_eq!(
        mailbox.sent,
        vec![(
            MONITOR_PID,
            SwitchboardCommand::Power {
                action: PowerAction::Restart
            }
        )],
        "one confirmation is one command"
    );
}

/// A press on the safe button declines; a press on the destructive one
/// confirms. Both resolve through the shared dialog's own button geometry.
#[test]
fn prompt_clicks_answer_through_the_shared_button_geometry() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();

    assert!(confirm.ask(PowerAction::PowerOff, &mut shell, &mut comp));
    let cancel = prompt_action_centre(PowerAction::PowerOff, 0, &shell);
    assert_eq!(
        confirm.handle_click(cancel, &mut shell, &mut comp),
        Some(Answer::Declined)
    );

    assert!(confirm.ask(PowerAction::PowerOff, &mut shell, &mut comp));
    let accept = prompt_action_centre(PowerAction::PowerOff, 1, &shell);
    assert_eq!(
        confirm.handle_click(accept, &mut shell, &mut comp),
        Some(Answer::Confirmed(PowerAction::PowerOff))
    );
}

/// A press inside the prompt but on neither button changes nothing and
/// leaves the question up, so no transition follows an idle click.
#[test]
fn a_press_off_the_prompt_buttons_answers_nothing() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();
    assert!(confirm.ask(PowerAction::Restart, &mut shell, &mut comp));

    assert_eq!(
        confirm.handle_click(Point::new(4, 4), &mut shell, &mut comp),
        None
    );
    assert_eq!(
        confirm.pending(),
        Some(PowerAction::Restart),
        "the question is still being asked"
    );
}

/// An abandoned prompt (the session tearing down, a log-out) is never a
/// confirmation: the window closes and nothing is left pending.
#[test]
fn an_abandoned_prompt_is_not_a_confirmation() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();
    assert!(confirm.ask(PowerAction::PowerOff, &mut shell, &mut comp));
    let wm = confirm.wm_id().expect("showing");

    confirm.abandon(&mut shell, &mut comp);

    assert_eq!(confirm.pending(), None);
    assert_eq!(confirm.wm_id(), None);
    assert!(comp.window(wm).is_none());
}

/// A confirmed transition with no live holder is not attempted here: the
/// desktop holds no power capability, so nothing is sent and the caller is
/// handed the reason to state.
#[test]
fn a_confirmation_with_no_live_holder_sends_nothing_and_says_why() {
    let mut mailbox = RecordingMailbox::default();

    let reason = relay_power(Answer::Confirmed(PowerAction::PowerOff), None, &mut mailbox)
        .expect("a reason to state");

    assert!(
        reason.contains("nothing was done"),
        "the diagnosis says the machine is untouched: {reason}"
    );
    assert!(mailbox.sent.is_empty(), "and nothing was relayed");
}

/// A confirmed transition the holder's mailbox refuses says so, rather
/// than passing for success. A shutdown the user confirmed and the machine
/// then ignored in silence is the worst outcome this prompt has.
#[test]
fn a_confirmation_the_holder_refuses_says_why_rather_than_passing_silently() {
    let mut mailbox = RefusingMailbox::default();

    let reason = relay_power(
        Answer::Confirmed(PowerAction::Restart),
        Some(MONITOR_PID),
        &mut mailbox,
    )
    .expect("a reason to state");

    assert!(
        reason.contains("nothing was done"),
        "the diagnosis says the machine is untouched: {reason}"
    );
    assert_eq!(mailbox.attempts, 1, "attempted once, not retried");
}

/// A theme switch behind a showing prompt redraws it, so nothing on screen
/// is left in the appearance just left behind.
#[test]
fn the_prompt_repaints_on_a_theme_switch() {
    let (mut shell, mut comp) = headless_desktop();
    let mut confirm = ConfirmPrompt::new();
    assert!(confirm.ask(PowerAction::PowerOff, &mut shell, &mut comp));
    let wm = confirm.wm_id().expect("showing");
    let before: Vec<_> = comp
        .window(wm)
        .expect("live")
        .content()
        .expect("content is retained")
        .pixels()
        .to_vec();

    shell.session_mut().set_appearance(Appearance::Light);
    confirm.repaint(&mut shell, &mut comp);

    let after = comp
        .window(wm)
        .expect("still live")
        .content()
        .expect("content is retained")
        .pixels();
    assert_ne!(
        before.as_slice(),
        after,
        "the prompt is drawn in the appearance now in use"
    );
}

/// The menu's appearance rows switch the desktop's theme in place: the
/// registry's active theme changes and the taskbar is re-themed with it, so
/// the bar and the desktop never disagree about which appearance is in use.
#[test]
fn setting_an_appearance_switches_the_theme_and_re_themes_the_bar() {
    let mut session = DesktopSession::new(TaskbarConfig::bottom_bar(640, 480));
    assert_eq!(
        session.active_theme().appearance(),
        Appearance::Dark,
        "dark is the default"
    );

    assert_eq!(session.set_appearance(Appearance::Light), ThemeId::LIGHT);
    assert_eq!(session.active_theme().appearance(), Appearance::Light);
    assert_eq!(
        session.taskbar().theme().id(),
        ThemeId::LIGHT,
        "the bar re-themed with the desktop"
    );

    assert_eq!(session.set_appearance(Appearance::Dark), ThemeId::DARK);
    assert_eq!(session.active_theme().appearance(), Appearance::Dark);
    assert_eq!(session.taskbar().theme().id(), ThemeId::DARK);
}

// ---- screen lock ----

/// A fake [`Unlocker`] that answers from a scripted list of verdicts, oldest
/// first, and records every password it was ever offered — so a test can
/// assert both what was typed and what came back for it.
#[derive(Default)]
struct ScriptedUnlocker {
    answers: Vec<Result<bool, Errno>>,
    offered: Vec<String>,
}

impl ScriptedUnlocker {
    /// Answers `answers` in order as it is offered passwords, one verdict
    /// per offer. Offered past the end of the script, it refuses.
    fn scripted(mut answers: Vec<Result<bool, Errno>>) -> Self {
        answers.reverse();
        Self {
            answers,
            offered: Vec::new(),
        }
    }

    /// Refuses every password it is ever offered.
    fn refusing() -> Self {
        Self::scripted(Vec::new())
    }
}

impl Unlocker for ScriptedUnlocker {
    fn verify(&mut self, password: &str) -> Result<bool, Errno> {
        self.offered.push(String::from(password));
        self.answers.pop().unwrap_or(Ok(false))
    }
}

/// A key press with no modifiers held.
fn key_press(key: Key) -> InputEvent {
    InputEvent::KeyPressed {
        key,
        modifiers: tairix_wm::Modifiers::default(),
    }
}

/// Type `password`'s characters into the lock one key at a time, then press
/// Enter, returning the outcome of that final, submitting event.
fn submit(
    lock: &mut ScreenLock,
    password: &str,
    unlocker: &mut dyn Unlocker,
    shell: &DesktopShell,
    comp: &mut Compositor,
) -> LockOutcome {
    for ch in password.chars() {
        lock.handle(&key_press(Key::Char(ch)), unlocker, shell, comp);
    }
    lock.handle(
        &key_press(Key::Named(NamedKey::Enter)),
        unlocker,
        shell,
        comp,
    )
}

/// The lock's own window, found the same way the rest of this suite finds a
/// window it has no id for: by asking the compositor what is showing there.
/// A `ScreenLock` exposes no window-id accessor of its own, and the lock
/// covers its own origin for as long as it is engaged.
fn locked_window(comp: &Compositor) -> WindowId {
    comp.window_at(Point::new(0, 0))
        .expect("the lock covers its own origin while engaged")
}

#[test]
fn engaging_locks_the_screen_and_covers_the_whole_screen_rectangle() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();

    assert!(lock.engage("ann", &shell, &mut comp));

    assert!(lock.is_locked());
    assert_eq!(comp.window_count(), 1, "exactly one lock window");
    let id = locked_window(&comp);
    assert_eq!(window_rect(&comp, id), comp.screen_rect());
}

#[test]
fn engaging_an_already_locked_screen_is_idempotent() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));

    assert!(
        lock.engage("someone else", &shell, &mut comp),
        "a lock already up answers true and changes nothing"
    );

    assert!(lock.is_locked());
    assert_eq!(comp.window_count(), 1, "still exactly one lock window");
}

/// An empty account name heads the prompt with exactly [`UNNAMED_ACCOUNT`] —
/// not merely with some other, unspecified stand-in text — and a non-empty
/// name is used instead. Neither string is observable through a public
/// accessor, so this probes the one thing that is: what the lock draws.
#[test]
fn an_empty_account_name_heads_the_prompt_with_the_unnamed_placeholder() {
    let shell = shell();
    let mut comp_empty = compositor();
    let mut comp_placeholder = compositor();
    let mut comp_named = compositor();
    let mut empty = ScreenLock::new();
    let mut placeholder = ScreenLock::new();
    let mut named = ScreenLock::new();

    assert!(empty.engage("", &shell, &mut comp_empty));
    assert!(placeholder.engage(UNNAMED_ACCOUNT, &shell, &mut comp_placeholder));
    assert!(named.engage("ann", &shell, &mut comp_named));

    let empty_id = locked_window(&comp_empty);
    let placeholder_id = locked_window(&comp_placeholder);
    let named_id = locked_window(&comp_named);
    let content = |comp: &Compositor, id: WindowId| {
        comp.window(id)
            .expect("live")
            .content()
            .expect("content is retained")
            .clone()
    };
    let empty_surface = content(&comp_empty, empty_id);
    let placeholder_surface = content(&comp_placeholder, placeholder_id);
    let named_surface = content(&comp_named, named_id);

    assert_eq!(
        empty_surface, placeholder_surface,
        "an empty account name renders exactly like the UNNAMED_ACCOUNT placeholder"
    );
    assert_ne!(
        empty_surface, named_surface,
        "a non-empty account name is used instead of the placeholder"
    );
}

#[test]
fn a_wrong_password_leaves_the_screen_locked() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::scripted(vec![Ok(false)]);

    let outcome = submit(&mut lock, "wrong", &mut unlocker, &shell, &mut comp);

    assert_eq!(outcome, LockOutcome::Pending);
    assert!(
        lock.is_locked(),
        "a refused password leaves the screen locked"
    );
}

/// A broker the lock cannot reach at all is not the same answer as a real
/// refusal, and it is certainly not the same as a verified user: both are
/// "still locked", which is the fail-closed property under test here.
#[test]
fn an_unreachable_broker_leaves_the_screen_locked() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::scripted(vec![Err(Errno::NotFound)]);

    let outcome = submit(&mut lock, "whatever", &mut unlocker, &shell, &mut comp);

    assert_eq!(
        outcome,
        LockOutcome::Pending,
        "a broker that cannot be reached never unlocks the screen"
    );
    assert!(
        lock.is_locked(),
        "an error from the broker fails closed, exactly like a real refusal"
    );
}

#[test]
fn a_correct_password_unlocks_the_screen_and_removes_the_lock_window() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let id = locked_window(&comp);
    let mut unlocker = ScriptedUnlocker::scripted(vec![Ok(true)]);

    let outcome = submit(
        &mut lock,
        "correct horse battery staple",
        &mut unlocker,
        &shell,
        &mut comp,
    );

    assert_eq!(outcome, LockOutcome::Unlocked);
    assert!(!lock.is_locked());
    assert!(
        comp.window(id).is_none(),
        "the lock window is gone from the compositor"
    );
}

#[test]
fn the_typed_password_is_offered_to_the_verifier_exactly_as_typed() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::scripted(vec![Ok(true)]);

    submit(&mut lock, "Hunter2!", &mut unlocker, &shell, &mut comp);

    assert_eq!(unlocker.offered, vec![String::from("Hunter2!")]);
}

/// After a refusal the next submission must offer a password typed fresh,
/// never the previous attempt still sitting in the field with more
/// characters appended to or retained alongside it.
#[test]
fn the_password_is_erased_after_every_attempt() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::scripted(vec![Ok(false), Ok(true)]);

    submit(&mut lock, "wrong", &mut unlocker, &shell, &mut comp);
    submit(&mut lock, "right", &mut unlocker, &shell, &mut comp);

    assert_eq!(
        unlocker.offered,
        vec![String::from("wrong"), String::from("right")],
        "the second attempt is never the first with more characters appended"
    );
}

/// Escape, Enter on an empty field, Tab, and a printable key are all
/// harmless against a verifier that refuses everything: none of them is a
/// second way out of the lock besides a verified password. Escape especially
/// must not be mistaken for a cancel-and-dismiss.
#[test]
fn no_key_dismisses_the_lock_without_a_verified_password() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::refusing();

    lock.handle(
        &key_press(Key::Named(NamedKey::Escape)),
        &mut unlocker,
        &shell,
        &mut comp,
    );
    assert!(lock.is_locked(), "Escape does not unlock the screen");

    lock.handle(
        &key_press(Key::Named(NamedKey::Enter)),
        &mut unlocker,
        &shell,
        &mut comp,
    );
    assert!(lock.is_locked(), "Enter on an empty field does not unlock");

    lock.handle(
        &key_press(Key::Named(NamedKey::Tab)),
        &mut unlocker,
        &shell,
        &mut comp,
    );
    assert!(lock.is_locked(), "Tab does not unlock");

    lock.handle(&key_press(Key::Char('x')), &mut unlocker, &shell, &mut comp);
    assert!(
        lock.is_locked(),
        "a printable key does not unlock by itself"
    );
}

#[test]
fn a_pointer_press_does_not_unlock_and_reaches_nothing_else() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::refusing();

    let outcome = lock.handle(&PRIMARY_PRESS, &mut unlocker, &shell, &mut comp);

    assert_eq!(outcome, LockOutcome::Pending);
    assert!(lock.is_locked(), "a pointer press never unlocks the screen");
    assert_eq!(
        comp.window_count(),
        1,
        "the press reaches nothing beyond the lock's own window"
    );
    assert!(
        unlocker.offered.is_empty(),
        "a press alone offers the verifier no password"
    );
}

#[test]
fn keep_topmost_raises_the_lock_above_a_window_added_after_it() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let lock_id = locked_window(&comp);

    let intruder = opaque_window(&mut comp, Point::new(0, 0), 10, 10);
    assert_eq!(
        comp.window_at(Point::new(0, 0)),
        Some(intruder),
        "a window added after the lock is on top of it by default"
    );

    lock.keep_topmost(&mut comp);

    assert_eq!(
        comp.window_at(Point::new(0, 0)),
        Some(lock_id),
        "keep_topmost raises the lock back above it"
    );
}

#[test]
fn abandon_takes_the_lock_down_and_unlocks_nothing() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let id = locked_window(&comp);

    lock.abandon(&mut comp);

    assert!(!lock.is_locked());
    assert!(
        comp.window(id).is_none(),
        "the lock window is gone from the compositor"
    );
}

#[test]
fn repaint_while_locked_keeps_exactly_one_lock_window_and_stays_locked() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));

    lock.repaint(&shell, &mut comp);

    assert!(lock.is_locked(), "a repaint never uncovers the session");
    assert_eq!(
        comp.window_count(),
        1,
        "a repaint never adds a second lock window"
    );
}

// --- The locked drain -------------------------------------------------
//
// One wake's worth of seat events fed into a locked screen. What matters
// here is what happens to the events *behind* the one that unlocks: they
// are the tail of the gesture that typed the password, and they must be
// discarded rather than delivered into the desktop that has just become
// visible.

/// Feed one drain's worth of events, as the embedder's loop does: every
/// event goes to [`LockedDrain::feed`], including the ones after an unlock.
fn drain(
    events: &[InputEvent],
    lock: &mut ScreenLock,
    unlocker: &mut dyn Unlocker,
    shell: &DesktopShell,
    comp: &mut Compositor,
) -> LockedDrain {
    let mut drain = LockedDrain::new();
    for event in events {
        drain.feed(lock, event, unlocker, shell, comp);
    }
    drain
}

/// The typing that unlocks the screen, as a drainable batch: the password's
/// characters, the submitting Enter, and then the tail that is still queued
/// behind it at the instant the lock comes down.
fn unlocking_batch(password: &str, tail: &[InputEvent]) -> Vec<InputEvent> {
    let mut events: Vec<InputEvent> = password
        .chars()
        .map(|ch| key_press(Key::Char(ch)))
        .collect();
    events.push(key_press(Key::Named(NamedKey::Enter)));
    events.extend_from_slice(tail);
    events
}

#[test]
fn a_drain_that_never_unlocks_feeds_every_event_to_the_lock() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::refusing();

    let drained = drain(
        &unlocking_batch("wrong", &[key_press(Key::Char('x'))]),
        &mut lock,
        &mut unlocker,
        &shell,
        &mut comp,
    );

    assert!(!drained.unlocked(), "a refused password unlocks nothing");
    assert!(lock.is_locked(), "the screen is still secured");
    assert_eq!(
        unlocker.offered,
        vec![String::from("wrong")],
        "the one submission reached the verifier"
    );
}

/// The security property: once the password is verified part-way through a
/// batch, nothing after it in that batch is delivered anywhere. Were the
/// tail routed on, the keystrokes still queued behind a password entry
/// would land in whatever holds focus on the desktop that just appeared.
#[test]
fn a_mid_batch_unlock_discards_the_rest_of_the_drain() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::scripted(vec![Ok(true)]);
    // A keystroke and a submitting Enter behind the unlock: were they
    // routed on, this second Enter would offer "s" as a password.
    let tail = [
        key_press(Key::Char('s')),
        key_press(Key::Named(NamedKey::Enter)),
        PRIMARY_PRESS,
    ];

    let drained = drain(
        &unlocking_batch("correct", &tail),
        &mut lock,
        &mut unlocker,
        &shell,
        &mut comp,
    );

    assert!(drained.unlocked(), "the lock came down during the drain");
    assert!(!lock.is_locked(), "and stayed down");
    assert_eq!(
        unlocker.offered,
        vec![String::from("correct")],
        "the Enter still queued behind the unlock never offered a second password"
    );
    assert_eq!(
        comp.window_count(),
        0,
        "the discarded tail added nothing to the screen"
    );
}

/// The discard is observable, not merely internal: the lock places the
/// pointer for every sample it is given, so a motion sample still queued
/// behind the unlock would jerk the cursor of the desktop that has just
/// become visible. Nothing typed or moved at a locked screen may surface.
#[test]
fn a_pointer_sample_behind_the_unlock_never_moves_the_desktop_cursor() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.refresh_cursor(&mut comp);
    let resting = comp.cursor_bounds().expect("the pointer is shown");
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::scripted(vec![Ok(true)]);

    let drained = drain(
        &unlocking_batch("correct", &[moved(1500, 900)]),
        &mut lock,
        &mut unlocker,
        &shell,
        &mut comp,
    );

    assert!(drained.unlocked(), "the lock came down during the drain");
    assert_eq!(
        comp.cursor_bounds().expect("the pointer is still shown"),
        resting,
        "the discarded sample never reached the handler that would have moved it"
    );
}

/// A `ScreenLock` that was never engaged is inert: every method is a
/// harmless no-op against a compositor that never sees a lock window.
#[test]
fn an_unengaged_lock_is_harmless() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    let mut unlocker = ScriptedUnlocker::refusing();

    assert_eq!(
        lock.handle(&PRIMARY_PRESS, &mut unlocker, &shell, &mut comp),
        LockOutcome::Pending
    );
    assert!(!lock.is_locked());
    assert_eq!(comp.window_count(), 0, "nothing was ever added");

    lock.keep_topmost(&mut comp);
    lock.repaint(&shell, &mut comp);
    lock.abandon(&mut comp);

    assert!(!lock.is_locked());
    assert_eq!(
        comp.window_count(),
        0,
        "an idle lock never touches the compositor"
    );
}

/// Rasterises a solid square of the requested size, counting how many
/// times it was asked, so a test can prove a decode was reused rather
/// than repeated. `refuse` makes it reject everything, standing in for a
/// malformed asset.
struct CountingRasteriser {
    calls: usize,
    refuse: bool,
}

impl CountingRasteriser {
    const fn working() -> Self {
        Self {
            calls: 0,
            refuse: false,
        }
    }

    const fn refusing() -> Self {
        Self {
            calls: 0,
            refuse: true,
        }
    }
}

impl IconRasteriser for CountingRasteriser {
    fn rasterise(&mut self, side: u32, _icon: &[u8]) -> Option<Vec<u8>> {
        self.calls += 1;
        if self.refuse {
            return None;
        }
        let area = (side as usize).checked_mul(side as usize)?.checked_mul(4)?;
        Some(vec![0xC3; area])
    }
}

/// A bundle asset the fake reader will serve. The bytes never reach a
/// real decoder here — the rasteriser above is injected — so any
/// non-empty payload stands for the artwork.
fn artwork_source(bundle: &str) -> PinIconSource {
    PinIconSource {
        bundle: String::from(bundle),
        asset: String::from("icon.svg"),
    }
}

fn artwork_assets(bundles: &[&str]) -> ArtworkFileReader<MemoryAssets> {
    let mut assets = MemoryAssets::default();
    for bundle in bundles {
        assets = assets.with(&artwork_source(bundle).path(), VALID_SVG);
    }
    ArtworkFileReader(assets)
}

/// The shared decode cache, built through the one desktop policy the
/// session itself uses, so these tests exercise the shipping budget.
fn test_artwork_cache(gauge: &'static ReportedPressure, output_bytes: usize) -> ArtworkCache {
    artwork_cache(
        "session.test-artwork",
        TEST_SEAT,
        output_bytes,
        gauge,
        &TEST_SINK,
    )
}

#[test]
fn pin_artwork_is_decoded_once_per_path_and_side() {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut reader = artwork_assets(&["/Apps/One.app"]);
    let mut rasteriser = ArtworkSandbox(CountingRasteriser::working());
    let path = artwork_source("/Apps/One.app").path();

    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 16)
        .is_some());
    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 16)
        .is_some());
    assert_eq!(rasteriser.0.calls, 1, "the second lookup is a cache hit");

    // A different side is a different entry: the artwork is rasterised
    // again at the new geometry rather than scaled from the old one.
    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 32)
        .is_some());
    assert_eq!(rasteriser.0.calls, 2);
}

#[test]
fn a_refused_pin_icon_is_refused_once_not_on_every_refresh() {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut reader = artwork_assets(&["/Apps/Bad.app"]);
    let mut rasteriser = ArtworkSandbox(CountingRasteriser::refusing());
    let path = artwork_source("/Apps/Bad.app").path();

    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 16)
        .is_none());
    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 16)
        .is_none());
    assert_eq!(rasteriser.0.calls, 1, "the refusal is remembered");
}

#[test]
fn pin_artwork_never_outgrows_the_budget_its_output_allows() {
    // A store full of distinct icon paths must not be able to grow the
    // session without limit: the keys come from bundles, so the bound is
    // the budget, not the caller's restraint.
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let tiny_output_bytes = 320 * 240 * 4;
    let hard = tairix_reclaim::CacheBudget::from_backing(tiny_output_bytes).hard();
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, tiny_output_bytes);
    let bundles: Vec<String> = (0..64).map(|i| format!("/Apps/App{i}.app")).collect();
    let refs: Vec<&str> = bundles.iter().map(String::as_str).collect();
    let mut reader = artwork_assets(&refs);
    let mut rasteriser = ArtworkSandbox(CountingRasteriser::working());

    for bundle in &bundles {
        let path = artwork_source(bundle).path();
        assert!(
            cache
                .path_artwork(&mut reader, &mut rasteriser, &path, 32)
                .is_some(),
            "every pin still gets its artwork, cached or not"
        );
    }
    assert!(
        cache.charged_bytes() <= hard,
        "charged {} exceeds the {hard}-byte ceiling",
        cache.charged_bytes()
    );
    // …and the ceiling was reached by evicting, not by refusing
    // everything: the cache is still doing its job at its bound.
    assert!(
        cache.charged_bytes() > 0,
        "a bounded cache still retains what it can"
    );
}

#[test]
fn pin_artwork_is_given_back_under_pressure_and_wiped_on_teardown() {
    // A gauge of this test's own, so moving the band cannot perturb the
    // shared one other tests hold at normal.
    static PRESSURED: ReportedPressure = ReportedPressure::unknown();
    PRESSURED.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&PRESSURED, TEST_FRAME_BYTES);
    let mut reader = artwork_assets(&["/Apps/One.app"]);
    let mut rasteriser = ArtworkSandbox(CountingRasteriser::working());
    let path = artwork_source("/Apps/One.app").path();

    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 16)
        .is_some());
    assert!(cache.charged_bytes() > 0);

    PRESSURED.report(PressureBand::Mild);
    assert!(cache.trim() > 0, "mild pressure releases disposable UI");
    assert_eq!(cache.charged_bytes(), 0);

    // A lookup while pressure holds retains nothing, so it hands back no
    // artwork and the draw site falls back to its built-in glyph:
    // correctness never depended on the artwork, and answering pressure by
    // re-acquiring the pixels it just released would defeat the release.
    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 16)
        .is_none());
    assert_eq!(cache.charged_bytes(), 0, "no growth under pressure");

    PRESSURED.report(PressureBand::Normal);
    assert!(cache
        .path_artwork(&mut reader, &mut rasteriser, &path, 16)
        .is_some());
    assert!(cache.charged_bytes() > 0, "retention resumes when it may");

    cache.teardown();
    assert_eq!(
        cache.charged_bytes(),
        0,
        "a seat's rendered artwork does not outlive its session"
    );
}

/// The tint the fake rasteriser paints an application's *own* bundle icon
/// in, so a test can prove a slot came from the bundle and not the shipped
/// fallback.
const BUNDLE_TINT: u8 = 0xB1;

/// The tint it paints the shipped application-bundle master in.
const SHIPPED_TINT: u8 = 0x5A;

/// Rasterises a solid square tinted by the asset's first byte, so a test
/// can tell which asset a slot was painted from. Alpha is opaque, so the
/// tint survives premultiplication unchanged.
struct TaggedRasteriser {
    calls: usize,
}

impl TaggedRasteriser {
    const fn new() -> Self {
        Self { calls: 0 }
    }
}

impl IconRasteriser for TaggedRasteriser {
    fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<Vec<u8>> {
        self.calls += 1;
        let tag = *icon.first()?;
        let area = (side as usize).checked_mul(side as usize)?;
        let mut pixels = Vec::with_capacity(area.checked_mul(4)?);
        for _ in 0..area {
            pixels.extend_from_slice(&[tag, tag, tag, 0xFF]);
        }
        Some(pixels)
    }
}

/// A [`SessionFileReader`] that counts every read, so a test can prove the
/// shared cache read an asset once and served the rest from memory.
struct CountingAssets {
    assets: MemoryAssets,
    reads: usize,
}

impl CountingAssets {
    const fn new(assets: MemoryAssets) -> Self {
        Self { assets, reads: 0 }
    }
}

impl SessionFileReader for CountingAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        self.reads += 1;
        self.assets.read(path)
    }
}

/// A catalog entry for `/Apps/<stem>.app` declaring its own icon asset, so
/// the library popup has an application icon to resolve.
fn entry_with_icon(stem: &str, name: &str, asset: Option<&str>) -> LibraryEntry {
    LibraryEntry::new(
        EntryId::new(&format!("os.tairix.{stem}")).expect("id"),
        DisplayName::new(name).expect("name"),
        BundlePath::new(&format!("/Apps/{stem}.app")).expect("bundle"),
        LibraryCategory::Utilities,
        asset.map(|asset| IconAsset::new(asset).expect("asset")),
    )
}

/// The shipped application-bundle master, so the fallback rung has
/// something to resolve to.
fn shipped_app_bundle_master(assets: MemoryAssets) -> MemoryAssets {
    assets.with(&icon_artwork_path(IconKind::AppBundle), &[SHIPPED_TINT])
}

/// A session showing an open library popup over `catalog`.
fn open_library_over(catalog: Catalog) -> (DesktopSession, Compositor) {
    let mut session = session();
    session.taskbar_mut().library_mut().set_catalog(catalog);
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    open_library(&mut router, &mut comp, session.taskbar_mut());
    (session, comp)
}

/// The rows the open popup currently shows that are launchable entries —
/// exactly the rows that may carry an application's own artwork.
fn shown_entry_rows(session: &DesktopSession) -> Vec<usize> {
    let bar = session.taskbar();
    bar.library_layout(Scale::ONE)
        .rows
        .iter()
        .filter(|&&(index, _)| {
            matches!(
                bar.library().rows().get(index),
                Some(LibraryRow::Entry { .. })
            )
        })
        .map(|&(index, _)| index)
        .collect()
}

/// The opaque tint a resolved row was painted in, or `None` when the row
/// carries no artwork at all and the shared slot draws its built-in glyph.
fn row_tint(session: &DesktopSession, row: usize) -> Option<u8> {
    session
        .taskbar()
        .library()
        .row_artwork(row)
        .and_then(|art| art.pixels().first().copied())
        .map(|pixel| pixel.r)
}

#[test]
fn a_bundle_icon_is_read_and_decoded_once_and_reused_by_the_shared_cache() {
    // Two pushes of the same strip must cost one read and one decode: the
    // pins and the library share the one cache the shell owns, so a
    // re-resolve before a paint is a lookup, not a fresh decode of the
    // same file.
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let source = artwork_source("/Apps/One.app");
    let mut reader = ArtworkFileReader(CountingAssets::new(
        MemoryAssets::default().with(&source.path(), &[BUNDLE_TINT]),
    ));
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());
    let resolved = vec![ResolvedPin {
        label: String::from("One"),
        entry: None,
        run_path: Some(String::from("/Apps/One.app/Run")),
        icon: Some(source),
    }];

    let first = build_pin_views(
        &resolved,
        &[None],
        &mut reader,
        &mut rasteriser,
        &mut cache,
        24,
    );
    let again = build_pin_views(
        &resolved,
        &[None],
        &mut reader,
        &mut rasteriser,
        &mut cache,
        24,
    );

    assert_eq!(reader.0.reads, 1, "the asset is read once, then cached");
    assert_eq!(rasteriser.0.calls, 1, "and decoded once");
    let tint = |views: &[PinView]| {
        views
            .first()
            .and_then(PinView::artwork)
            .and_then(|art| art.pixels().first().copied())
            .map(|pixel| pixel.r)
    };
    assert_eq!(tint(&first), Some(BUNDLE_TINT));
    assert_eq!(
        tint(&again),
        Some(BUNDLE_TINT),
        "the reused entry is the same artwork, not an empty slot"
    );
}

#[test]
fn a_library_row_draws_its_own_applications_icon() {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut cat = Catalog::new();
    cat.insert(entry_with_icon("one", "One", Some("icon.svg")))
        .expect("fits");
    let (mut session, _comp) = open_library_over(cat);
    let mut reader = ArtworkFileReader(shipped_app_bundle_master(
        MemoryAssets::default().with("/Apps/one.app/Resources/icon.svg", &[BUNDLE_TINT]),
    ));
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());

    resolve_library_icons(
        session.taskbar_mut(),
        Scale::ONE,
        &mut reader,
        &mut rasteriser,
        &mut cache,
    );

    let rows = shown_entry_rows(&session);
    assert_eq!(rows.len(), 1, "one entry, one row");
    assert_eq!(
        row_tint(&session, rows[0]),
        Some(BUNDLE_TINT),
        "the row shows the application's own icon, not the shipped master"
    );
}

#[test]
fn a_library_row_whose_asset_will_not_serve_falls_back_and_never_blanks() {
    // Three ways an application's own icon can fail — the file is absent,
    // it is over the artwork read bound, and it will not decode — and all
    // three land on the shipped application-bundle artwork rather than
    // leaving a hole in the popup.
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut cat = Catalog::new();
    for (stem, name) in [("gone", "Gone"), ("huge", "Huge"), ("junk", "Junk")] {
        cat.insert(entry_with_icon(stem, name, Some("icon.svg")))
            .expect("fits");
    }
    let (mut session, _comp) = open_library_over(cat);
    let oversize = vec![BUNDLE_TINT; MAX_ARTWORK_BYTES + 1];
    let mut reader = ArtworkFileReader(shipped_app_bundle_master(
        MemoryAssets::default()
            .with("/Apps/huge.app/Resources/icon.svg", &oversize)
            .with("/Apps/junk.app/Resources/icon.svg", &[]),
    ));
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());

    resolve_library_icons(
        session.taskbar_mut(),
        Scale::ONE,
        &mut reader,
        &mut rasteriser,
        &mut cache,
    );

    let rows = shown_entry_rows(&session);
    assert_eq!(rows.len(), 3, "three entries, three rows");
    for row in rows {
        assert_eq!(
            row_tint(&session, row),
            Some(SHIPPED_TINT),
            "row {row} falls back to the shipped artwork rather than blanking"
        );
    }
}

#[test]
fn a_library_row_with_no_declared_icon_falls_back_to_the_shipped_artwork() {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut cat = Catalog::new();
    cat.insert(entry_with_icon("plain", "Plain", None))
        .expect("fits");
    let (mut session, _comp) = open_library_over(cat);
    let mut reader = ArtworkFileReader(shipped_app_bundle_master(MemoryAssets::default()));
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());

    resolve_library_icons(
        session.taskbar_mut(),
        Scale::ONE,
        &mut reader,
        &mut rasteriser,
        &mut cache,
    );

    let rows = shown_entry_rows(&session);
    assert_eq!(
        row_tint(&session, rows[0]),
        Some(SHIPPED_TINT),
        "an application that declares no icon still shows one"
    );
}

#[test]
fn the_library_resolves_artwork_only_for_the_rows_it_shows() {
    // A big library must not decode an icon nobody is looking at: only the
    // rows inside the popup's viewport are resolved, and the decode count
    // is exactly the shown rows.
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut cat = Catalog::new();
    let mut assets = MemoryAssets::default();
    for index in 0..MAX_CATALOG_LEN.min(96) {
        let stem = format!("app{index:02}");
        cat.insert(entry_with_icon(
            &stem,
            &format!("App {index:02}"),
            Some("icon.svg"),
        ))
        .expect("fits");
        assets = assets.with(
            &format!("/Apps/{stem}.app/Resources/icon.svg"),
            &[BUNDLE_TINT],
        );
    }
    let (mut session, _comp) = open_library_over(cat);
    let mut reader = ArtworkFileReader(CountingAssets::new(shipped_app_bundle_master(assets)));
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());

    let shown = shown_entry_rows(&session);
    let total = session.taskbar().library().rows().len();
    assert!(
        shown.len() < total,
        "the fixture must overflow its viewport: {} shown of {total}",
        shown.len()
    );

    resolve_library_icons(
        session.taskbar_mut(),
        Scale::ONE,
        &mut reader,
        &mut rasteriser,
        &mut cache,
    );

    assert_eq!(
        rasteriser.0.calls,
        shown.len(),
        "one decode per shown row, none for the rest"
    );
    assert_eq!(reader.0.reads, shown.len(), "and one read per shown row");
    for row in 0..total {
        let resolved = row_tint(&session, row).is_some();
        assert_eq!(
            resolved,
            shown.contains(&row),
            "row {row} resolved={resolved} but shown={}",
            shown.contains(&row)
        );
    }
}

#[test]
fn a_desktop_with_no_artwork_at_all_still_draws_every_icon_from_its_glyphs() {
    // The freshly-installed and headless-graphics case: nothing under the
    // shipped graphics store, no bundle icons, nothing readable at all.
    // Every surface the bar and its popup draw must still be the one the
    // built-in glyph path produces — no blank slot anywhere.
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut cat = Catalog::new();
    cat.insert(entry_with_icon("one", "One", Some("icon.svg")))
        .expect("fits");
    let (mut session, mut comp) = open_library_over(cat);
    let mut reader = ArtworkFileReader(MemoryAssets::default());
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());

    // The pin strip: a pin whose bundle declares an icon nothing will
    // serve still yields a view, so the strip keeps its slot.
    let resolved = vec![ResolvedPin {
        label: String::from("One"),
        entry: None,
        run_path: Some(String::from("/Apps/one.app/Run")),
        icon: Some(artwork_source("/Apps/one.app")),
    }];
    let views = build_pin_views(
        &resolved,
        &[None],
        &mut reader,
        &mut rasteriser,
        &mut cache,
        24,
    );
    assert_eq!(views.len(), 1, "the pin is still shown");
    assert!(
        views[0].artwork().is_none(),
        "with nothing to read there is no artwork — the slot draws its glyph"
    );

    session.taskbar_mut().tasks_mut().add(TaskId(1), "Editor");
    resolve_library_icons(
        session.taskbar_mut(),
        Scale::ONE,
        &mut reader,
        &mut rasteriser,
        &mut cache,
    );
    for row in shown_entry_rows(&session) {
        assert_eq!(row_tint(&session, row), None, "row {row} has no artwork");
    }

    // Present the whole bar and popup twice: once through the seams that
    // can serve nothing, once through the do-nothing lookup. Identical
    // pixels means the empty-store desktop is exactly the glyph desktop,
    // and a bar drawn from glyphs is not a blank bar.
    session.taskbar_mut().set_pins(views);
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();
    let mut source = IconArtworkSource::new(&mut cache, &mut reader, &mut rasteriser);
    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut source,
    );
    let drawn: Vec<(Vec<_>, Vec<_>)> = [presenter.bar_window(), presenter.popup_window()]
        .into_iter()
        .map(|id| {
            let content = comp
                .window(id.expect("presented"))
                .expect("live")
                .content()
                .expect("content is retained")
                .pixels()
                .to_vec();
            let uniform: Vec<_> = content.iter().take(1).copied().collect();
            (content, uniform)
        })
        .collect();
    for (content, uniform) in &drawn {
        assert!(
            content.iter().any(|pixel| Some(pixel) != uniform.first()),
            "a bar or popup drawn from glyphs is not one flat colour"
        );
    }

    let mut glyphs = TaskbarPresenter::new();
    let mut plain = compositor();
    glyphs.present(
        &mut plain,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );
    for (index, id) in [glyphs.bar_window(), glyphs.popup_window()]
        .into_iter()
        .enumerate()
    {
        let content = plain
            .window(id.expect("presented"))
            .expect("live")
            .content()
            .expect("content is retained")
            .pixels();
        assert_eq!(
            drawn[index].0.as_slice(),
            content,
            "an unreadable artwork store draws exactly the glyph desktop"
        );
    }
}
