//! Headless unit tests for the desktop session glue.

use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use tairix_abi::driver::display::{DamageRect, Display, DisplayFormat, DisplayMode};
use tairix_abi::notify_ipc::{NotifyBody, NotifyRequest, NotifySeverity, NotifyTitle};
use tairix_abi::switchboard_ipc::{
    CommandSection, FrameReport, SeatReport, SwitchboardCommand, SwitchboardRequest,
    SEAT_REPORT_OWNERS_MAX,
};
use tairix_abi::sysinfo::CACHE_LABEL_MAX;
use tairix_abi::window_ipc::{
    AppBar, AppMenu, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuRow,
};
use tairix_abi::{
    AppInfoHeader, DriverError, Errno, ProcId, ABI_VERSION_CURRENT, APPINFO_MAGIC, BUNDLE_ID_MAX,
    BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX, LIBRARY_ICON_MAX, SYSCALL_TABLE_HASH_LEN,
};
use tairix_controls::{ChainModel, Fact, FactList, PointerState};
use tairix_cursor::CursorTheme;
use tairix_greeter::{Verdict, Verifier, UNNAMED_ACCOUNT};
use tairix_icon::{
    artwork_cache, icon_artwork_path, ArtworkCache, ArtworkResolver, IconArtworkSource, IconKind,
    IconSet, InlineArtwork, NoArtwork, Resolved, MAX_ARTWORK_BYTES,
};
use tairix_log::{Event, Sink};
use tairix_proglib::{
    BundlePath, Catalog, DisplayName, EntryId, IconAsset, LibraryCategory, LibraryEntry,
    LIBRARY_PATH, LIBRARY_PUBLISHER, MAX_ENTRIES,
};
use tairix_reclaim::{CacheLedger, PressureBand, ReclaimCache, ReportedPressure};
use tairix_taskbar::{
    icon_cache, Edge, EntryRow, IconEpoch, LibraryRow, TaskId, TaskbarConfig, TaskbarRenderer,
    TaskbarRepaint, TaskbarResponse, PICKER_CLOSE_GRACE_NS, PICKER_OPEN_DELAY_NS,
};
use tairix_theme::{
    Appearance, CursorKind, Metrics, MotionInteraction, SurfaceGround, Theme, ThemeError, ThemeId,
    Timeline,
};
use tairix_wm::{
    chrome_cache, cursor_cache, frost_cache, ChromeEpoch, Color, Compositor, Corners, FrostEpoch,
    FrostedBackdrop, InputEvent, InputResponse, Key, NamedKey, Point, PointerButton, Rect, Scale,
    Surface, WindowActivationState, WindowChrome, WindowControlKind, WindowId,
};

use crate::artwork::ArtworkDesk;
use crate::menu::{ChainAction, ChainGeometry, ChainOutcome, ChainOwner, MenuChain, SurfaceKind};
use crate::shell::SettleWork;
use crate::{
    deliver_pending_open, desktop_info, drop_is_noteworthy, ensure_switchboard, load_icon_set,
    load_library, maybe_send_seat_report, open_tray, picker_cells, resolve_library_icons,
    resolve_window_identities, serve_switchboard_request, thumbnail, AppBarService,
    ArtworkFileReader, ArtworkSandbox, DesktopSession, DesktopShell, FrameContent, FramePacer,
    FrameReportGate, IconRasteriser, InputSource, LaunchTable, LockOutcome, LockedDrain,
    OwnerWindow, PresentedOwners, ScreenFade, ScreenLock, SessionFileReader, SessionInputResponse,
    SessionInputRouter, SessionWindows, ShellOutcome, ShellWindowHost, SwitchboardMailbox,
    SwitchboardOutcome, SwitchboardRefusal, SwitchboardServe, TaskBridge, TaskbarPresenter,
    BUNDLE_RUN_SUFFIX, DESKTOP_REVEALED, DESKTOP_REVEALED_MESSAGE, DESKTOP_SESSION_RANGE_END,
    DESKTOP_SESSION_RANGE_START, MAX_BAR_APPS, MIN_FRAME_REPORT_INTERVAL_NS, NO_DEADLINE_NS,
    SWITCHBOARD_RUN_PATH,
};
use tairix_window::WindowSizing;

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
    /// Every path read, in order, so a test can count reads.
    reads: Vec<String>,
}

impl MemoryAssets {
    pub(crate) fn with(mut self, path: &str, bytes: &[u8]) -> Self {
        self.files.push((String::from(path), bytes.to_vec()));
        self
    }

    /// How many times `path` has been read, so a test can assert that a
    /// resolution reads a manifest once rather than once per slot.
    fn reads(&self, path: &str) -> usize {
        self.reads
            .iter()
            .filter(|read| read.as_str() == path)
            .count()
    }
}

impl SessionFileReader for MemoryAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        self.reads.push(String::from(path));
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, bytes)| bytes.clone())
            .ok_or(Errno::NotFound)
    }
}

impl SessionFileReader for &mut MemoryAssets {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        (**self).read(path)
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

/// The frosted-backdrop cache every compositor under test is built with, on
/// the same terms as its furniture cache.
pub(crate) fn test_frost_cache() -> ReclaimCache<WindowId, FrostedBackdrop, FrostEpoch> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    frost_cache(TEST_SEAT, TEST_FRAME_BYTES, &NORMAL_PRESSURE, &TEST_SINK)
}

/// The pressure gauge every compositor under test is built over — the same
/// one its caches read, so the desktop under test has a single notion of
/// how tight memory is, exactly as the shipping session does.
pub(crate) fn test_pressure() -> &'static ReportedPressure {
    &NORMAL_PRESSURE
}

/// A distinct attested window owner. The bytes are opaque to everything
/// but equality, so `fill` only has to differ between owners.
pub(crate) fn window_owner(fill: u8) -> ProcId {
    ProcId::from_raw([fill; tairix_abi::PROC_ID_LEN])
}

/// A distinct attested process per `index`, for tests that need more of them
/// than a single byte spells.
fn window_owner_wide(index: usize) -> ProcId {
    let mut raw = [0u8; tairix_abi::PROC_ID_LEN];
    raw[..core::mem::size_of::<u64>()].copy_from_slice(&(index as u64 + 1).to_le_bytes());
    ProcId::from_raw(raw)
}

/// An icon-bar declaration offering *Quit*, with `default_action` as given.
fn app_bar(default_action: bool) -> AppBar {
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Item(AppMenuItem::new(
        AppMenuItemId::new(1).expect("non-zero"),
        AppMenuLabel::new("Quit").expect("short"),
    )))
    .expect("fits");
    AppBar {
        event_endpoint: 4,
        default_action,
        menu,
    }
}

/// Run `resolve` over a real artwork cache and a resolver that serves
/// nothing, so a slot's icon resolution is exercised without a decode.
fn with_artwork<T>(resolve: impl FnOnce(&mut dyn ArtworkResolver, &mut ArtworkCache) -> T) -> T {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut reader = ArtworkFileReader(MemoryAssets::default());
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());
    let mut inline = InlineArtwork::new(&mut reader, &mut rasteriser);
    resolve(&mut inline, &mut cache)
}

/// The processes the strip holds, in display order.
fn owners(service: &mut AppBarService, windows: &[(ProcId, TaskId)]) -> Vec<ProcId> {
    service
        .strip(windows, |_| None)
        .into_iter()
        .map(|group| group.owner)
        .collect()
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
        frost_cache(TEST_SEAT, TEST_FRAME_BYTES, pressure, &TEST_SINK),
        pressure,
    )
    .expect("the compositor allocates");
    (shell, compositor)
}

/// The desktop's caches, built exactly as the session composes them: its own
/// artwork, the taskbar's icon glyphs, the window manager's cursors, the
/// decorated windows' furniture, and the frosted windows' backdrops. All five
/// live in the session process, so all five are its rows to report.
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
        test_frost_cache().ledger(),
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

/// Toggle the open program-library popup shut with a press on the Library
/// button, through the taskbar's own router. The counterpart of
/// [`open_library`], and asserted the same way.
fn close_library(taskbar: &mut tairix_taskbar::Taskbar) {
    let mut input = tairix_taskbar::TaskbarInput::new();
    let at = centre(taskbar.layout(Scale::ONE).library);
    input.handle(InputEvent::PointerMoved { to: at }, taskbar, Scale::ONE, 0);
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            },
            taskbar,
            Scale::ONE,
            0,
        ),
        TaskbarResponse::LibraryDismissed
    );
    assert!(!taskbar.library().is_open());
}

/// Open the program-library popup through the taskbar's *own* router, which is
/// the only thing that can open it.
///
/// The tests that use this are about what the presenter draws and what the
/// artwork store resolves, not about which surface the desktop's seat hands a
/// press to. Driving the bar directly is therefore the honest fixture: no
/// window stack, no seat, nothing that could make the setup itself the thing
/// under test. The seat's own routing is exercised by [`Seat`].
fn open_library(taskbar: &mut tairix_taskbar::Taskbar) {
    let mut input = tairix_taskbar::TaskbarInput::new();
    let at = centre(taskbar.layout(Scale::ONE).library);
    input.handle(InputEvent::PointerMoved { to: at }, taskbar, Scale::ONE, 0);
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            },
            taskbar,
            Scale::ONE,
            0,
        ),
        TaskbarResponse::OpenLibrary
    );
    assert!(taskbar.library().is_open());
}

/// Open the popup by pressing the Library button on a shell that has
/// presented its bar, asserting it opened.
///
/// The shell routes the press against its own presenter, so this is the bar
/// as the user finds it: on screen, in the window stack, and claiming a press
/// nothing covers.
fn open_library_on(shell: &mut DesktopShell, comp: &mut Compositor) {
    shell.handle(moved(24, 1060), comp, 0);
    assert_eq!(
        shell.handle(PRIMARY_PRESS, comp, 0),
        ShellOutcome::Taskbar(TaskbarResponse::OpenLibrary)
    );
    assert!(shell.session().taskbar().library().is_open());
}

/// A headless 1920×1080 RGBA compositor over an opaque black background.
pub(crate) fn compositor() -> Compositor {
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
        test_frost_cache(),
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
    open_library(session.taskbar_mut());

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
fn the_presenter_owns_only_the_surfaces_it_placed() {
    // What the input router asks of whatever the compositor finds on top: a
    // window that is not one of these is something drawn *over* the bar, and
    // the press there is its own.
    let mut session = session();
    let mut comp = compositor();
    open_library(session.taskbar_mut());
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();
    let intruder = opaque_window(&mut comp, Point::new(0, 0), 40, 40);

    presenter.present(
        &mut comp,
        &mut renderer,
        session.taskbar(),
        TaskbarRepaint::ALL,
        &mut NoArtwork,
    );

    let bar = presenter.bar_window().expect("the bar is presented");
    let popup = presenter
        .popup_window()
        .expect("the open popup is presented");
    assert!(presenter.owns_window(bar));
    assert!(presenter.owns_window(popup));
    assert!(
        !presenter.owns_window(intruder),
        "a window the presenter never placed is not the bar's"
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
    open_library(session.taskbar_mut());

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
    open_library(session.taskbar_mut());

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

    // The Library button toggles the popup it opened shut, through the bar's
    // own router — the same one that opened it.
    close_library(session.taskbar_mut());
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
    session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    open_library(session.taskbar_mut());

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
        *base.fonts(),
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

/// The desktop's seat over a **presented** bar.
///
/// The seat resolves every pointer event against the window stack, so a bar
/// that was never presented is not a bar the pointer can rest on — it is
/// nowhere. These tests therefore stand up what the desktop stands up: the
/// session's taskbar model, a compositor, and the presenter that places the
/// bar's windows in it, and they bring the screen back up to date after each
/// event exactly as the shell's own settle does. Nothing here is a stand-in
/// for the production path; it *is* the production path, minus the shell's
/// window bookkeeping.
struct Seat {
    session: DesktopSession,
    comp: Compositor,
    presenter: TaskbarPresenter,
    renderer: TaskbarRenderer,
    router: SessionInputRouter,
}

impl Seat {
    /// A seat with the bar placed and nothing else on screen.
    fn new() -> Self {
        let mut seat = Self {
            session: session(),
            comp: compositor(),
            presenter: TaskbarPresenter::new(),
            renderer: TaskbarRenderer::new(test_icon_cache()),
            router: SessionInputRouter::new(),
        };
        seat.settle();
        seat
    }

    /// Re-resolve the pointer's focus and bring the bar's windows up to date —
    /// the two halves of the shell's own present, in the shell's own order.
    fn settle(&mut self) {
        self.router.refresh_pointer_focus(
            &mut self.comp,
            self.session.taskbar_mut(),
            &self.presenter,
        );
        let parts = self.session.taskbar_mut().take_repaint();
        self.presenter.present(
            &mut self.comp,
            &mut self.renderer,
            self.session.taskbar(),
            parts,
            &mut NoArtwork,
        );
    }

    /// Route one event at the monotonic time `now_ns`, then settle.
    fn at(&mut self, event: InputEvent, now_ns: u64) -> SessionInputResponse {
        let out = self.router.handle(
            event,
            &mut self.comp,
            self.session.taskbar_mut(),
            &self.presenter,
            now_ns,
        );
        self.settle();
        out
    }

    /// Route one event at time zero, then settle.
    fn handle(&mut self, event: InputEvent) -> SessionInputResponse {
        self.at(event, 0)
    }

    /// Move the pointer to `(x, y)`.
    fn moved_to(&mut self, x: i32, y: i32) -> SessionInputResponse {
        self.handle(moved(x, y))
    }

    /// Move the pointer to `(x, y)` and press the primary button there.
    fn press_at(&mut self, x: i32, y: i32) -> SessionInputResponse {
        self.moved_to(x, y);
        self.handle(PRIMARY_PRESS)
    }

    /// The centre of one of the bar's own laid-out regions, so a test aims
    /// where the bar actually draws rather than at a copied coordinate.
    fn centre_of(&self, region: impl Fn(&tairix_taskbar::BarLayout) -> Rect) -> Point {
        centre(region(&self.session.taskbar().layout(Scale::ONE)))
    }

    /// Add an opaque window to the stack, above everything already in it.
    fn window(&mut self, origin: Point, width: u32, height: u32) -> WindowId {
        opaque_window(&mut self.comp, origin, width, height)
    }
}

#[test]
fn primary_press_over_the_bar_routes_to_the_taskbar() {
    let mut seat = Seat::new();
    let library = seat.centre_of(|layout| layout.library);

    assert_eq!(
        seat.press_at(library.x, library.y),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

#[test]
fn primary_press_on_the_clock_is_claimed_by_the_bar_and_opens_no_menu() {
    let mut seat = Seat::new();
    // The clock's own layout rectangle, rather than a hand-copied coordinate.
    let clock = seat.centre_of(|layout| layout.clock);

    // The bar claimed it — a press it let through would have reached the
    // desktop behind and reported it — and the clock, being a reading rather
    // than a control, did nothing with it. A menu is what a *secondary* press
    // asks for, so `Ignored` is also the proof that none was asked for: an ask
    // would have arrived here as `TaskbarResponse::OpenMenu`.
    assert_eq!(
        seat.press_at(clock.x, clock.y),
        SessionInputResponse::Ignored,
        "a press on the bar is the bar's, even where it acts on nothing"
    );
}

#[test]
fn the_bar_wins_over_a_window_beneath_it() {
    let mut shell = shell();
    let mut comp = compositor();
    // A window placed under the bottom bar must not steal a press on the bar.
    // It is added before the bar is presented, so the bar is stacked above it
    // — which is what makes the press the bar's, and what a window dragged
    // over the bar afterwards undoes.
    opaque_window(&mut comp, Point::new(0, 1000), 400, 80);
    shell.present(&mut comp);

    shell.handle(moved(24, 1060), &mut comp, 0);

    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp, 0),
        ShellOutcome::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

#[test]
fn primary_press_over_a_window_routes_to_the_window_manager() {
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);

    assert_eq!(
        seat.press_at(250, 250),
        SessionInputResponse::WindowManager(InputResponse::Activated {
            window,
            local: Point::new(50, 50),
        })
    );
    assert_eq!(seat.router.focused(), Some(window));
}

#[test]
fn secondary_press_over_a_window_routes_to_the_window_manager() {
    // A right-click over a window must reach the window manager (which
    // delivers it to the client so it can open its context menu) — the
    // seat must not swallow it, as its catch-all once did.
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);

    seat.moved_to(250, 250);

    assert_eq!(
        seat.handle(SECONDARY_PRESS),
        SessionInputResponse::WindowManager(InputResponse::SecondaryActivated {
            window,
            local: Point::new(50, 50),
        })
    );
    assert_eq!(seat.router.focused(), Some(window));
}

#[test]
fn primary_press_on_the_empty_desktop_routes_to_the_window_manager() {
    let mut seat = Seat::new();

    assert_eq!(
        seat.press_at(900, 500),
        SessionInputResponse::WindowManager(InputResponse::DesktopPressed)
    );
}

#[test]
fn the_open_popup_is_modal_and_a_press_off_it_dismisses_it() {
    let mut seat = Seat::new();
    seat.session
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    // A window beneath the popup's click-away press; it must stay unfocused.
    let _window = seat.window(Point::new(200, 200), 300, 300);
    let library = seat.centre_of(|layout| layout.library);

    assert_eq!(
        seat.press_at(library.x, library.y),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );

    // A press over a window beneath is claimed by the modal popup and
    // dismisses it, rather than reaching the window manager. The popup holds
    // an active grab on the pointer, so where the pointer is does not matter.
    assert_eq!(
        seat.press_at(250, 250),
        SessionInputResponse::Taskbar(TaskbarResponse::LibraryDismissed)
    );
    assert!(!seat.session.taskbar().library().is_open());
    assert_eq!(
        seat.router.focused(),
        None,
        "the window beneath was not activated"
    );

    // While open, a KeyPressed routes to the popup.
    assert_eq!(
        seat.press_at(library.x, library.y),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
    assert_eq!(
        seat.handle(InputEvent::KeyPressed {
            key: tairix_wm::Key::Named(tairix_wm::NamedKey::Down),
            modifiers: tairix_wm::Modifiers::default(),
        }),
        SessionInputResponse::Ignored
    );
    assert_eq!(seat.session.taskbar().library().current(), Some(0));

    // Escape closes the popup.
    assert_eq!(
        seat.handle(InputEvent::KeyPressed {
            key: tairix_wm::Key::Named(tairix_wm::NamedKey::Escape),
            modifiers: tairix_wm::Modifiers::default(),
        }),
        SessionInputResponse::Taskbar(TaskbarResponse::LibraryDismissed)
    );
    assert!(!seat.session.taskbar().library().is_open());

    // A PointerScrolled while open does NOT reach the window manager.
    assert_eq!(
        seat.press_at(library.x, library.y),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
    assert_eq!(
        seat.handle(InputEvent::PointerScrolled { dx: 0, dy: 10 }),
        SessionInputResponse::Ignored
    );
}

#[test]
fn motion_updates_the_pointer_and_reaches_the_desktop_when_it_hits_no_window() {
    let mut seat = Seat::new();

    // Motion that lands on no window is the desktop's: it reaches the
    // session rather than being swallowed, which is what lets the desktop's
    // icons take a hover.
    assert_eq!(
        seat.moved_to(640, 480),
        SessionInputResponse::WindowManager(InputResponse::DesktopPointerMoved)
    );
    assert_eq!(seat.router.pointer(), Point::new(640, 480));
}

#[test]
fn a_window_drag_continues_while_the_pointer_is_over_the_bar() {
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);

    seat.press_at(250, 250);
    assert!(
        seat.router.begin_move(&seat.comp),
        "a focused window starts a move-grab"
    );

    // Dragging the pointer down over the bar must keep moving the window, not
    // hand the motion to the taskbar: the held button holds the pointer.
    assert_eq!(
        seat.moved_to(250, 1060),
        SessionInputResponse::WindowManager(InputResponse::Moved {
            window,
            origin: Point::new(200, 1010),
        })
    );
    assert!(seat.router.is_moving());
}

#[test]
fn a_primary_release_ends_a_move_grab() {
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);

    seat.press_at(250, 250);
    assert!(seat.router.begin_move(&seat.comp));

    assert_eq!(
        seat.handle(PRIMARY_RELEASE),
        SessionInputResponse::WindowManager(InputResponse::MoveEnded { window })
    );
    assert!(!seat.router.is_moving());
}

#[test]
fn a_secondary_press_on_the_bar_that_offers_no_menu_does_nothing() {
    let mut seat = Seat::new();
    let library = seat.centre_of(|layout| layout.library);

    seat.moved_to(library.x, library.y);

    assert_eq!(seat.handle(SECONDARY_PRESS), SessionInputResponse::Ignored);
    assert!(
        !seat.session.taskbar().library().is_open(),
        "a secondary press did nothing"
    );
}

/// A held button holds the pointer, so a gesture completes where it started
/// even once the pointer has left. A press inside a window's content, dragged
/// onto the bar and released there, is delivered to that window from start to
/// finish: the bar never sees any of it, and the window's own in-content drag
/// completes.
#[test]
fn a_held_button_keeps_the_gesture_where_it_started() {
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);
    let library = seat.centre_of(|layout| layout.library);

    seat.press_at(250, 250);
    // Dragged onto the bar. The window holds the pointer, so the bar is not
    // offered the motion and the window is told the drag continues.
    assert_eq!(
        seat.moved_to(library.x, library.y),
        SessionInputResponse::WindowManager(InputResponse::ClientPointerMoved {
            window,
            local: Point::new(0, 299),
        })
    );
    assert_eq!(
        seat.handle(PRIMARY_RELEASE),
        SessionInputResponse::WindowManager(InputResponse::ClientPointerReleased {
            window,
            local: Point::new(0, 299),
        }),
        "the release was claimed by the bar the pointer had reached"
    );

    // The grab over, the pointer is the bar's — it is what is drawn there — so
    // the next press opens the launcher rather than going back to the window.
    assert_eq!(
        seat.handle(PRIMARY_PRESS),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

/// The same, the other way round: a press on the bar keeps the pointer while
/// it slides onto a window, so the window is never told the pointer is in its
/// content and never activates behind the gesture.
#[test]
fn a_press_on_the_bar_keeps_the_pointer_while_it_slides_onto_a_window() {
    let mut seat = Seat::new();
    let _window = seat.window(Point::new(200, 200), 300, 300);
    let capsule = seat.centre_of(|layout| layout.switchboard);

    seat.press_at(capsule.x, capsule.y);
    assert_eq!(
        seat.moved_to(250, 250),
        SessionInputResponse::Ignored,
        "the window was told the pointer had entered its content mid-gesture"
    );
    // The capsule's own rule then applies: a press dragged off it fires
    // nothing at all (fail closed), so the release opens no section.
    assert_eq!(
        seat.at(PRIMARY_RELEASE, QUICK_PRESS_NS),
        SessionInputResponse::Ignored
    );

    // And with the grab over, the pointer is the window's again.
    assert!(matches!(
        seat.handle(PRIMARY_PRESS),
        SessionInputResponse::WindowManager(InputResponse::Activated { .. })
    ));
}

/// The grab ends when the *last* button comes up, not the first. Otherwise a
/// chord — press primary, press secondary, release primary — would hand the
/// rest of the gesture to whatever the pointer had wandered over.
#[test]
fn a_chord_keeps_the_grab_until_the_last_button_is_up() {
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);
    let library = seat.centre_of(|layout| layout.library);

    seat.press_at(250, 250);
    seat.handle(SECONDARY_PRESS);
    seat.moved_to(library.x, library.y);
    // Primary up, secondary still down: the window still owns the pointer.
    assert_eq!(
        seat.handle(PRIMARY_RELEASE),
        SessionInputResponse::WindowManager(InputResponse::ClientPointerReleased {
            window,
            local: Point::new(0, 299),
        })
    );
    // The last button is still down, so the pointer is still the window's: the
    // bar does not take a hover while another surface holds it.
    seat.moved_to(library.x, library.y + 1);
    assert_eq!(
        seat.session.taskbar().library_button().state().pointer,
        PointerState::None,
        "the bar lit up while another surface held the pointer"
    );

    // The last button up releases it, and the pointer goes to the bar it is
    // actually over.
    seat.handle(InputEvent::PointerReleased {
        button: PointerButton::Secondary,
    });
    assert_eq!(
        seat.handle(PRIMARY_PRESS),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

/// The seat's implicit grab is a function of the presses and releases it
/// *sees*, and an embedder-owned modal surface — the screen lock, the
/// pinboard's backdrop menu — takes the stream away mid-gesture: the release
/// that would end the grab is drained straight into that surface and never
/// reaches the seat.
///
/// `yield_pointer` is how the embedder says so. Without it the seat would hold
/// a grab for a button that can never come up, and the pointer could never be
/// resolved against the stack again — the bar would be unreachable for the rest
/// of the session.
#[test]
fn yielding_the_pointer_ends_a_gesture_whose_release_the_seat_will_never_see() {
    let mut seat = Seat::new();
    let _window = seat.window(Point::new(200, 200), 300, 300);
    let library = seat.centre_of(|layout| layout.library);

    // A press on the window takes the grab, and its release is then taken away
    // from the seat by a modal surface it does not route.
    seat.press_at(250, 250);
    seat.router
        .yield_pointer(&mut seat.comp, seat.session.taskbar_mut());

    // The pointer resolves against the stack again, so the bar can be reached.
    assert_eq!(
        seat.press_at(library.x, library.y),
        SessionInputResponse::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

/// Yielding also drops the hover, so nothing is left showing a lit control
/// behind a plate the user is looking at.
#[test]
fn yielding_the_pointer_drops_the_hover_it_was_showing() {
    let mut seat = Seat::new();
    let library = seat.centre_of(|layout| layout.library);
    seat.moved_to(library.x, library.y);
    assert_eq!(
        seat.session.taskbar().library_button().state().pointer,
        PointerState::Hover
    );

    seat.router
        .yield_pointer(&mut seat.comp, seat.session.taskbar_mut());

    assert_eq!(
        seat.session.taskbar().library_button().state().pointer,
        PointerState::None,
        "the bar kept a hover behind a surface it cannot be reached through"
    );
    // Said twice it changes nothing.
    seat.router
        .yield_pointer(&mut seat.comp, seat.session.taskbar_mut());
    assert_eq!(
        seat.session.taskbar().library_button().state().pointer,
        PointerState::None
    );
}

/// Keys follow the keyboard's own focus. A pointer resting on the bar must
/// never divert a keystroke from the window the user is typing in — the two
/// focuses are separate facts, and only a modal surface of the bar's takes the
/// keyboard.
#[test]
fn a_pointer_resting_on_the_bar_does_not_divert_the_keyboard() {
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);
    seat.press_at(250, 250);
    seat.handle(PRIMARY_RELEASE);
    assert_eq!(seat.router.focused(), Some(window));

    // The pointer moves onto the bar, which now holds it.
    let library = seat.centre_of(|layout| layout.library);
    seat.moved_to(library.x, library.y);

    let key = InputEvent::KeyPressed {
        key: tairix_wm::Key::Char('x'),
        modifiers: tairix_wm::Modifiers::default(),
    };
    assert_eq!(
        seat.handle(key),
        SessionInputResponse::WindowManager(InputResponse::Key {
            window,
            key: tairix_wm::Key::Char('x'),
            modifiers: tairix_wm::Modifiers::default(),
            pressed: true,
        }),
        "the typed key went to the bar the pointer happened to be over"
    );
}

/// Re-resolving the focus must not take the pointer away from a gesture: a
/// window being raised mid-drag (this very drag may have raised it) cannot be
/// allowed to hand the rest of the drag to something else.
#[test]
fn refreshing_the_focus_never_interrupts_a_drag() {
    let mut seat = Seat::new();
    let window = seat.window(Point::new(200, 200), 300, 300);
    seat.press_at(250, 250);
    assert!(seat.router.begin_move(&seat.comp));

    // The pointer is dragged over the bar and the screen is settled there,
    // which is where a refresh would otherwise retarget.
    seat.moved_to(250, 1060);
    seat.settle();

    assert!(seat.router.is_moving(), "the drag was interrupted");
    assert_eq!(
        seat.moved_to(250, 1050),
        SessionInputResponse::WindowManager(InputResponse::Moved {
            window,
            origin: Point::new(200, 1000),
        })
    );
}

#[test]
fn a_press_on_an_app_slot_reaches_the_taskbar_through_the_seat() {
    let mut seat = Seat::new();
    seat.session
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(1), "Editor");
    seat.session
        .taskbar_mut()
        .set_apps(vec![tairix_taskbar::AppSlot::new(
            "Editor",
            IconKind::AppBundle,
        )
        .with_windows(vec![TaskId(1)])]);
    seat.settle();

    let slot = seat
        .session
        .taskbar()
        .layout(Scale::ONE)
        .apps
        .first()
        .copied()
        .expect("the seated application has a slot");
    let at = Point::new(slot.left() + 1, slot.top() + 1);

    assert_eq!(
        seat.press_at(at.x, at.y),
        SessionInputResponse::Taskbar(TaskbarResponse::AppRaise { app: 0 }),
        "the application declared no default action, so the session raises its window"
    );
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
    // The bar is placed before any input, exactly as the desktop places it:
    // a press is the bar's because the bar is what is drawn under it, so a bar
    // that is not on screen claims nothing (fail closed).
    shell.present(&mut comp);

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
            // The motion lands on the bar, so it is the bar's: a hover is a
            // pixel-only change, latched rather than reported.
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

/// The centre of `rect`, where a test aims a click at a laid-out region.
fn rect_centre(rect: Rect) -> Point {
    Point::new(
        rect.left() + i32::try_from(rect.width / 2).unwrap_or(0),
        rect.top() + i32::try_from(rect.height / 2).unwrap_or(0),
    )
}

/// The reported defect: a window dragged over the bar covers the clock, and a
/// click where the clock is drawn was claimed by the bar — the clock popped
/// its menu instead of the click reaching the window that owns those pixels.
/// Nothing pins the bar topmost, so a window over it really is what the user
/// clicked.
#[test]
fn a_press_on_a_window_covering_the_bar_reaches_that_window() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let clock = rect_centre(shell.session().taskbar().layout(comp.scale()).clock);

    // A window dragged over the trailing end of the bar. It is added last, so
    // it sits above the bar exactly as raising it leaves it.
    let window = opaque_window(&mut comp, Point::new(clock.x - 40, clock.y - 40), 200, 200);
    assert_eq!(
        comp.window_at(clock),
        Some(window),
        "the window is what is drawn where the clock is"
    );

    shell.handle(moved(clock.x, clock.y), &mut comp, 0);
    let outcome = shell.handle(PRIMARY_PRESS, &mut comp, 0);

    assert_eq!(
        outcome,
        ShellOutcome::WindowManager(InputResponse::Activated {
            window,
            local: Point::new(40, 40),
        })
    );
    assert!(
        !matches!(outcome, ShellOutcome::Taskbar(TaskbarResponse::OpenMenu(_))),
        "the covered bar asked for the clock's menu on a click that was not its own"
    );
}

/// A window covers part of the bar, not all of it: the covered pixels are the
/// window's and the pixels still on show are the bar's. The claim is decided
/// per press position, never by "a window overlaps the bar somewhere".
#[test]
fn the_bar_still_claims_a_press_where_no_window_covers_it() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let clock = rect_centre(shell.session().taskbar().layout(comp.scale()).clock);
    let _covering = opaque_window(&mut comp, Point::new(clock.x - 40, clock.y - 40), 200, 200);

    // The Library button, at the leading end and clear of that window.
    shell.handle(moved(24, 1060), &mut comp, 0);
    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp, 0),
        ShellOutcome::Taskbar(TaskbarResponse::OpenLibrary)
    );
}

/// The same rule for the other button: a secondary press where a window
/// covers the bar opens that window's own context path, not the bar's menu.
#[test]
fn a_secondary_press_on_a_window_covering_the_bar_reaches_that_window() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let clock = rect_centre(shell.session().taskbar().layout(comp.scale()).clock);
    let window = opaque_window(&mut comp, Point::new(clock.x - 40, clock.y - 40), 200, 200);

    shell.handle(moved(clock.x, clock.y), &mut comp, 0);
    let outcome = shell.handle(SECONDARY_PRESS, &mut comp, 0);

    assert_eq!(
        outcome,
        ShellOutcome::WindowManager(InputResponse::SecondaryActivated {
            window,
            local: Point::new(40, 40),
        })
    );
    assert!(!matches!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::OpenMenu(_))
    ));
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
        test_frost_cache(),
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
    shell.present(&mut comp);

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

/// The desktop state a drain leaves, as one value two drains can be
/// compared on: where the pointer is, what holds focus, which surfaces are
/// placed, and the whole taskbar model behind them (hover, highlight, open
/// popup, drained repaint latch).
fn desktop_state(
    shell: &DesktopShell,
    comp: &Compositor,
) -> (Point, Option<WindowId>, TaskbarPresenter, usize, String) {
    (
        shell.router().pointer(),
        shell.router().focused(),
        *shell.presenter(),
        comp.window_count(),
        format!("{:?}", shell.session().taskbar()),
    )
}

/// The per-frame work a drain cost: presents, active-frame syncs, cursor
/// refreshes.
fn work_since(shell: &DesktopShell, before: SettleWork) -> (u32, u32, u32) {
    let now = shell.settle_work();
    (
        now.presents - before.presents,
        now.active_frame_syncs - before.active_frame_syncs,
        now.cursor_refreshes - before.cursor_refreshes,
    )
}

/// The regression: the shell settled the frame per *sample*, so a burst of
/// N motion samples cost N taskbar presents, N active-frame syncs and N
/// cursor refreshes to publish the one frame the run loop then presented.
#[test]
fn pump_settles_one_frame_for_a_whole_motion_batch() {
    let mut shell = shell();
    let mut comp = compositor();
    let _window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);
    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
    shell.handle(PRIMARY_RELEASE, &mut comp, 0);

    let path: Vec<InputEvent> = (0..16).map(|step| moved(251 + step, 251 + step)).collect();
    let before = shell.settle_work();
    let outcomes = shell
        .pump(&mut MemoryInput::new(&path), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(outcomes.len(), 1, "the motion run folds app-ward too");
    assert_eq!(
        work_since(&shell, before),
        (1, 1, 1),
        "sixteen samples settle the one frame they produced"
    );
    assert_eq!(shell.router().pointer(), Point::new(266, 266));
}

/// Folding the shell's own per-frame work may not change the desktop it
/// leaves: a scripted path drained as one batch must land exactly where the
/// same path lands when every sample settles its own frame.
#[test]
fn pump_leaves_the_same_desktop_as_one_handle_per_sample() {
    // Over the window, off it, along the bar, back over the window: the
    // path crosses every surface a hover repaints.
    let path = &[
        moved(251, 251),
        moved(260, 260),
        moved(700, 700),
        moved(24, 1060),
        moved(120, 1060),
        moved(400, 1075),
        moved(255, 255),
        moved(250, 250),
    ];

    let mut batched = shell();
    let mut batched_comp = compositor();
    let mut sampled = shell();
    let mut sampled_comp = compositor();
    for (shell, comp) in [
        (&mut batched, &mut batched_comp),
        (&mut sampled, &mut sampled_comp),
    ] {
        let _window = opaque_window(comp, Point::new(200, 200), 300, 300);
        shell.handle(moved(250, 250), comp, 0);
        shell.handle(PRIMARY_PRESS, comp, 0);
        shell.handle(PRIMARY_RELEASE, comp, 0);
    }

    let batched_before = batched.settle_work();
    let batched_outcomes = batched
        .pump(&mut MemoryInput::new(path), &mut batched_comp, 0)
        .expect("source does not fault");
    let sampled_before = sampled.settle_work();
    let sampled_outcomes: Vec<ShellOutcome> = path
        .iter()
        .map(|event| sampled.handle(*event, &mut sampled_comp, 0))
        .collect();

    assert_eq!(
        desktop_state(&batched, &batched_comp),
        desktop_state(&sampled, &sampled_comp),
        "one batch leaves the desktop the per-sample path leaves"
    );
    assert_eq!(
        work_since(&batched, batched_before),
        (1, 1, 1),
        "the batch settles once"
    );
    assert_eq!(
        work_since(&sampled, sampled_before),
        (
            u32::try_from(path.len()).expect("the path is short"),
            u32::try_from(path.len()).expect("the path is short"),
            u32::try_from(path.len()).expect("the path is short")
        ),
        "the per-sample path settles once per sample — the work the batch drops"
    );
    assert!(
        batched_outcomes.len() < sampled_outcomes.len(),
        "the app-ward outcomes still fold"
    );
}

/// Only latest-wins motion folds. A press, a release, and a key each act on
/// the state the samples around them left, so a mixed batch must still apply
/// in order and report every event.
#[test]
fn pump_applies_an_order_sensitive_batch_in_order() {
    let script = &[
        moved(251, 251),
        PRIMARY_PRESS,
        moved(260, 260),
        InputEvent::KeyPressed {
            key: Key::Named(NamedKey::Down),
            modifiers: tairix_wm::Modifiers::default(),
        },
    ];

    let mut batched = shell();
    let mut batched_comp = compositor();
    let mut sampled = shell();
    let mut sampled_comp = compositor();
    let mut windows = Vec::new();
    for (shell, comp) in [
        (&mut batched, &mut batched_comp),
        (&mut sampled, &mut sampled_comp),
    ] {
        windows.push(opaque_window(comp, Point::new(200, 200), 300, 300));
        shell.handle(moved(250, 250), comp, 0);
        shell.handle(PRIMARY_PRESS, comp, 0);
        shell.handle(PRIMARY_RELEASE, comp, 0);
    }
    let window = windows[0];

    let batched_before = batched.settle_work();
    let batched_outcomes = batched
        .pump(&mut MemoryInput::new(script), &mut batched_comp, 0)
        .expect("source does not fault");
    let sampled_outcomes: Vec<ShellOutcome> = script
        .iter()
        .map(|event| sampled.handle(*event, &mut sampled_comp, 0))
        .collect();

    assert_eq!(
        batched_outcomes, sampled_outcomes,
        "an order-sensitive batch reports every event, in order"
    );
    assert!(
        matches!(
            batched_outcomes.as_slice(),
            [
                ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { .. }),
                ShellOutcome::WindowManager(InputResponse::Activated { .. }),
                ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { .. }),
                ShellOutcome::WindowManager(InputResponse::Key { .. }),
            ]
        ),
        "the press between the two motions keeps them apart: {batched_outcomes:?}"
    );
    assert_eq!(
        desktop_state(&batched, &batched_comp),
        desktop_state(&sampled, &sampled_comp),
        "the batch leaves the desktop the per-sample path leaves"
    );
    assert_eq!(
        work_since(&batched, batched_before),
        (1, 1, 1),
        "four ordered events still settle one frame"
    );
    assert_eq!(
        batched.router().focused(),
        Some(window),
        "the press in the middle of the batch still moved focus"
    );
}

/// An idle wake costs nothing: a drain that found no event has no frame to
/// settle.
#[test]
fn pump_settles_nothing_when_the_source_is_empty() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);

    let before = shell.settle_work();
    let outcomes = shell
        .pump(&mut MemoryInput::new(&[]), &mut comp, 0)
        .expect("source does not fault");

    assert!(outcomes.is_empty());
    assert_eq!(work_since(&shell, before), (0, 0, 0));
}

/// A faulting source still leaves the screen showing the state the events it
/// did deliver put the model in.
#[test]
fn pump_settles_the_events_applied_before_a_fault() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);

    let before = shell.settle_work();
    let result = shell.pump(
        &mut MemoryInput::faulting(&[moved(24, 1060), PRIMARY_PRESS], Errno::NotFound),
        &mut comp,
        0,
    );

    assert_eq!(result, Err(Errno::NotFound));
    assert!(shell.session().taskbar().library().is_open());
    assert_eq!(
        work_since(&shell, before),
        (1, 1, 1),
        "the opened popup is presented despite the fault"
    );
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

/// A wheel delta is additive, so a run of ticks one way is one app-ward
/// event carrying their sum.
///
/// The regression: wheel ticks were the one gesture `pump` never folded, so
/// a fast scroll sent the owning app one event — and cost it one full
/// repaint — per tick, and could outrun its bounded event mailbox.
#[test]
fn pump_folds_a_run_of_wheel_ticks_over_one_window() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
    shell.handle(PRIMARY_RELEASE, &mut comp, 0);

    let events = &[
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
    ];
    let outcomes = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(
        outcomes,
        alloc::vec![ShellOutcome::WindowManager(InputResponse::AppScroll {
            window,
            dx: 0,
            dy: 3,
        })]
    );
}

/// A reversal is a separate gesture: folding it would move the app's scroll
/// model somewhere the tick-by-tick sequence would not, because a tick that
/// clamps at a range end is not recovered by the tick back.
#[test]
fn pump_ends_a_wheel_run_at_a_reversal() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
    shell.handle(PRIMARY_RELEASE, &mut comp, 0);

    let events = &[
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
        InputEvent::PointerScrolled { dx: 0, dy: -1 },
    ];
    let outcomes = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(
        outcomes,
        alloc::vec![
            ShellOutcome::WindowManager(InputResponse::AppScroll {
                window,
                dx: 0,
                dy: 2,
            }),
            ShellOutcome::WindowManager(InputResponse::AppScroll {
                window,
                dx: 0,
                dy: -1,
            }),
        ]
    );
}

/// Motion and wheel are different gestures over the same window, so neither
/// swallows the other.
#[test]
fn pump_keeps_a_wheel_tick_and_a_motion_apart() {
    let mut shell = shell();
    let mut comp = compositor();
    let _window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);

    shell.handle(moved(250, 250), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
    shell.handle(PRIMARY_RELEASE, &mut comp, 0);

    let events = &[
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
        moved(251, 251),
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
    ];
    let outcomes = shell
        .pump(&mut MemoryInput::new(events), &mut comp, 0)
        .expect("source does not fault");

    assert_eq!(outcomes.len(), 3);
    assert!(matches!(
        outcomes[0],
        ShellOutcome::WindowManager(InputResponse::AppScroll { dy: 1, .. })
    ));
    assert!(matches!(
        outcomes[1],
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved { .. })
    ));
    assert!(matches!(
        outcomes[2],
        ShellOutcome::WindowManager(InputResponse::AppScroll { dy: 1, .. })
    ));
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

    // Motion onto the library button, asked of the layout rather than
    // spelled out: the bar floats clear of the screen edge, so its buttons
    // are nowhere a screen coordinate can name. The pointer now rests on the
    // bar, so the motion is the bar's alone — the window manager is told the
    // pointer left and reports nothing, and nothing is forwarded to a client,
    // because the bar has none. A hover is a pixel-only change, latched on the
    // model rather than reported.
    let onto = centre(shell.session().taskbar().layout(Scale::ONE).library);
    let outcomes = shell
        .pump(
            &mut MemoryInput::new(&[moved(onto.x, onto.y)]),
            &mut comp,
            0,
        )
        .expect("source does not fault");
    assert_eq!(outcomes, [ShellOutcome::Ignored]);

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

/// The centre of the application slot at `index` on the bar laid out at
/// 100%.
fn slot_point(shell: &DesktopShell, index: usize) -> Point {
    let slot = shell
        .session()
        .taskbar()
        .layout(Scale::ONE)
        .apps
        .get(index)
        .copied()
        .expect("the application has a slot");
    assert!(!slot.is_empty(), "the application slot has a region");
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
fn a_minimised_window_comes_back_by_being_chosen_in_the_picker() {
    // A slot is an application, so clicking it never minimises. The title
    // bar's control minimises and the picker's cell brings it back.
    let mut shell = shell();
    let mut comp = compositor();
    let window = shell
        .open_window(&mut comp, Point::new(300, 200), app_surface(), "Editor")
        .expect("opens");
    let task = shell.tasks().task_for(window).expect("tracked");
    shell.set_apps(
        &mut comp,
        vec![tairix_taskbar::AppSlot::new("Editor", IconKind::AppBundle).with_windows(vec![task])],
    );
    let at = slot_point(&shell, 0);

    assert!(shell.minimize_window(&mut comp, window));
    assert!(!comp.window(window).expect("still tracked").is_visible());
    assert_eq!(shell.router().focused(), None);
    assert!(shell.session().taskbar().tasks().is_minimised(task));

    // A click on the slot raises rather than toggling: the window comes
    // back and no press can put it away again.
    shell.handle(moved(at.x, at.y), &mut comp, 0);
    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp, 0),
        ShellOutcome::Taskbar(TaskbarResponse::AppRaise { app: 0 })
    );
    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp, 0),
        ShellOutcome::Taskbar(TaskbarResponse::AppRaise { app: 0 }),
        "a second click is still a raise, never a minimise"
    );

    // Raising it — what a chosen picker cell asks the session for — shows,
    // raises, and focuses it.
    assert!(shell.raise_window(&mut comp, window));
    assert!(comp.window(window).expect("tracked").is_visible());
    assert_eq!(shell.router().focused(), Some(window));
    assert!(!shell.session().taskbar().tasks().is_minimised(task));
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
fn raising_an_unknown_task_changes_nothing() {
    let bridge = TaskBridge::new();
    let mut comp = compositor();
    let mut router = SessionInputRouter::new();
    assert!(!bridge.raise(&mut comp, &mut router, TaskId(999)));
    assert_eq!(router.focused(), None);
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
/// the production shell construction and the ramfb console geometry:
/// start button (menu opens) → "Files" row (launch) → the served
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
        test_frost_cache(),
        test_pressure(),
    )
    .expect("compositor");

    let centre = |rect: tairix_wm::Rect| -> Point {
        assert!(!rect.is_empty());
        rect.center()
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
use tairix_browse::{DirectorySource, Entry, Listing};

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
    fn list(&mut self, components: &[String]) -> Result<Listing, Errno> {
        let key = components.join("/");
        self.dirs
            .get(&key)
            .cloned()
            .map(Listing::Ready)
            .ok_or(Errno::NotFound)
    }
}

/// The centre of the open program-library popup's row for the entry shown as
/// `label`.
fn library_row_at(shell: &DesktopShell, label: &str) -> Point {
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
}

/// Drive one of the bar's own menus end to end: open the chain the bar asked
/// for, click the row labelled `label`, and answer the chosen row back through
/// the bar.
///
/// The whole round trip the session's serve loop drives, minus its own
/// plumbing: the bar hands over a model and an anchor, the chain places, draws
/// and grabs, and the bar reads the chosen row back into the typed response the
/// embedder carries out.
fn choose_bar_menu_row(
    shell: &mut DesktopShell,
    comp: &mut Compositor,
    request: tairix_taskbar::MenuRequest,
    label: &str,
) -> Option<TaskbarResponse> {
    // The chain's own ground, cloned because the borrow it comes from cannot
    // outlive the presents below.
    let theme = shell.session().floating_theme().clone();
    let geom = ChainGeometry {
        screen: comp.screen_rect(),
        scale: comp.scale(),
        theme: &theme,
        epoch: comp.chrome_epoch(),
    };
    let subject = request.subject.clone();
    let row = request
        .model
        .rows()
        .iter()
        .position(|row| row.drawn().label() == label)
        .expect("labelled row");
    let mut chain = MenuChain::new();
    chain
        .open(
            ChainOwner::Bar(request.subject),
            request.model,
            request.placement,
            &geom,
        )
        .expect("the bar's model opens");
    assert!(
        shell.present_menu_chain(comp, &chain, None),
        "a plate drawn"
    );
    let at = centre(chain.row_rect(0, row, &geom).expect("the row lays out"));
    chain.handle(&moved(at.x, at.y), at, &geom);
    chain.handle(&PRIMARY_PRESS, at, &geom);
    let acted = chain.handle(&PRIMARY_RELEASE, at, &geom);
    assert_eq!(acted, ChainAction::Closed, "the chosen row ends the chain");
    let answers = chain.take_answers();
    assert_eq!(answers.len(), 1, "exactly one answer per chain");
    let (owner, outcome) = answers.into_iter().next().expect("one answer");
    assert_eq!(owner, ChainOwner::Bar(subject.clone()));
    let ChainOutcome::Chosen(item) = outcome else {
        panic!("expected a chosen row, got {outcome:?}");
    };
    shell
        .session_mut()
        .taskbar_mut()
        .menu_chosen(&subject, item)
}

/// A source refusing every listing, standing in for a session whose
/// filesystem reach was stripped.
struct RefusingSource;

impl DirectorySource for RefusingSource {
    fn list(&mut self, _components: &[String]) -> Result<Listing, Errno> {
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
        test_frost_cache(),
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

    // The click row must be computed at the scale and theme the picker draws
    // with, so it lands on row 0.
    let theme = shell.session().active_theme();
    // The first entry row sits directly below the chrome (the command toolbar
    // strip), so compute it from the shared `chrome_height` the renderer
    // reserves.
    let row = i32::try_from(chrome_height(Scale::ONE, theme)).expect("a small chrome height");
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
    // A click on the chrome strip above the rows concludes nothing.
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");
    assert_eq!(
        picker.handle_click(Point::new(4, 0), &mut shell, &mut comp),
        None
    );
}

/// The picker's window title carries the directory it is browsing and
/// follows every navigation, so the user can always see where the session
/// is looking on the app's behalf.
#[test]
fn picker_title_carries_the_location_and_follows_a_navigation() {
    let (mut shell, mut comp) = headless_desktop();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    picker.begin(7, &mut shell, &mut comp).expect("accepted");
    let wm = picker.wm_id().expect("showing");
    let task = shell.tasks().task_for(wm).expect("the picker is a task");
    let labelled = |shell: &DesktopShell| {
        shell
            .session()
            .taskbar()
            .tasks()
            .entries()
            .iter()
            .find(|entry| entry.id == task)
            .map(|entry| entry.title.clone())
            .expect("the picker's entry")
    };

    assert_eq!(labelled(&shell), "Choose a file: /");

    // Descending into `Docs/` moves the title with it.
    let enter = pressed(KeyValue::Named(NamedKeyCode::Enter));
    assert_eq!(picker.handle_key(&enter, &mut shell, &mut comp), None);
    assert_eq!(labelled(&shell), "Choose a file: /Docs");

    // Moving the selection inside one directory is not a navigation, so the
    // title stands.
    let down = pressed(KeyValue::Named(NamedKeyCode::Down));
    assert_eq!(picker.handle_key(&down, &mut shell, &mut comp), None);
    assert_eq!(labelled(&shell), "Choose a file: /Docs");

    // Climbing back returns it.
    let back = pressed(KeyValue::Named(NamedKeyCode::Backspace));
    assert_eq!(picker.handle_key(&back, &mut shell, &mut comp), None);
    assert_eq!(labelled(&shell), "Choose a file: /");
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
    let y = i32::try_from(toolbar_height(Scale::ONE, shell.session().active_theme()) / 2)
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

/// A fake app-data service standing in for the library-admin command's
/// published scope, holding `overlay` when the account has one.
///
/// It speaks the real `appdata-v1` codec, so the session's foreign read goes
/// over the wire the service actually answers rather than a mock of it. The
/// word it is built with selects nothing: a published scope has no
/// bundle-shipped layer.
fn library_host(overlay: Option<&str>) -> tairix_appdata::fake::FakeService {
    let service = tairix_appdata::fake::FakeService::for_word("desktop");
    match overlay {
        Some(text) => service.with_foreign(LIBRARY_PUBLISHER, text),
        None => service,
    }
}

#[test]
fn load_library_merges_the_machine_store_and_the_published_overlay() {
    let machine_conf = "os.tairix.editor.name = Editor\nos.tairix.editor.bundle = /Apps/editor.app\nos.tairix.editor.category = Office\n";
    let user_conf = "os.tairix.editor.name = My Editor\nos.tairix.files.name = Files\nos.tairix.files.bundle = /Apps/files.app\nos.tairix.files.category = Office\n";

    // (a) neither layer present -> empty catalog, no warnings. An
    // application that publishes nothing is the ordinary fresh-account state
    // and is deliberately indistinguishable from one that has never run.
    let mut host = library_host(None);
    let loaded = load_library(&mut MemoryAssets::default(), &mut host);
    assert!(loaded.catalog.is_empty());
    assert!(loaded.warnings.is_empty());

    // (b) machine store reads -> entries listed
    let mut reader = MemoryAssets::default().with(LIBRARY_PATH, machine_conf.as_bytes());
    let mut host = library_host(None);
    let loaded = load_library(&mut reader, &mut host);
    assert_eq!(loaded.catalog.len(), 1);
    assert!(loaded.warnings.is_empty());

    // (c) the published overlay merges over it (the overlay's name wins)
    let mut host = library_host(Some(user_conf));
    let loaded = load_library(&mut reader, &mut host);
    assert_eq!(loaded.catalog.len(), 2);
    let record = loaded
        .catalog
        .get(&EntryId::new("os.tairix.editor").unwrap())
        .unwrap();
    let tairix_proglib::Record::Entry(entry) = record else {
        panic!("expected entry")
    };
    assert_eq!(entry.name().as_str(), "My Editor");

    // (d) a machine store the registry refuses -> empty catalog + warning
    let mut reader = MemoryAssets::default().with(LIBRARY_PATH, b"malformed");
    let mut host = library_host(None);
    let loaded = load_library(&mut reader, &mut host);
    assert!(loaded.catalog.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains(LIBRARY_PATH));
    assert!(loaded.warnings[0].ends_with("; using an empty catalog\n"));

    // (e) a machine store past the format's own document bound
    let mut reader = MemoryAssets::default().with(
        LIBRARY_PATH,
        &vec![b'a'; tairix_appconf::MAX_DOCUMENT_LEN + 1],
    );
    let mut host = library_host(None);
    let loaded = load_library(&mut reader, &mut host);
    assert!(loaded.catalog.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("too large"));

    // (f) non-UTF-8
    let mut reader = MemoryAssets::default().with(LIBRARY_PATH, b"\xff\xfe");
    let mut host = library_host(None);
    let loaded = load_library(&mut reader, &mut host);
    assert!(loaded.catalog.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("not valid UTF-8"));

    // (g) the overlay alone: its editor line names no bundle, so it is a
    // patch, and a patch whose identifier no layer declares is discarded by
    // the merge. Its own declaration (files) stands.
    let mut host = library_host(Some(user_conf));
    let loaded = load_library(&mut MemoryAssets::default(), &mut host);
    assert_eq!(loaded.catalog.len(), 1);
    assert!(
        loaded
            .catalog
            .entry(&EntryId::new("os.tairix.files").unwrap())
            .is_some(),
        "the overlay's own declaration stands without a machine store"
    );

    // (h) an overlay the session could not reach at all is its own warning:
    // that is the caller's refusal, not the publisher's silence.
    let mut host = library_host(None);
    host.refusal().set(Some(Errno::DeviceOffline));
    let loaded = load_library(&mut MemoryAssets::default(), &mut host);
    assert!(loaded.catalog.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains(LIBRARY_PUBLISHER));
    assert!(loaded.warnings[0].contains("DeviceOffline"));
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

    // Minimised from its title bar, so it is hidden and unfocused.
    assert!(shell.minimize_window(&mut comp, id));
    assert!(!comp.window(id).unwrap().is_visible());
    assert_eq!(shell.router().focused(), None);

    // Raised again — what a chosen picker cell asks for.
    assert!(shell.raise_window(&mut comp, id));
    assert!(comp.window(id).unwrap().is_visible());
    assert_eq!(shell.router().focused(), Some(id));
}

/// A raise brings the window's own transients up with it, so the keyboard must
/// land on the sheet that ended up on top and not on the window underneath it.
///
/// Focusing the owner instead is how a key reaches an application's client
/// while that client's modal sheet is still on screen — nothing dismisses the
/// sheet, because a picker cell is not a press on the client — so the
/// application would run a menu accelerator behind its own open sheet.
#[test]
fn shell_raise_window_focuses_the_sheet_its_window_has_open() {
    let mut shell = shell();
    let mut comp = compositor();

    let id = shell
        .open_window(&mut comp, Point::new(200, 200), app_surface(), "Terminal")
        .unwrap();
    let wm = comp.window(id).map(|_| id).expect("the window is placed");
    let sheet = shell
        .open_popup_window(&mut comp, wm, Point::new(210, 210), app_surface())
        .expect("the parent is a window");
    assert_eq!(shell.router().focused(), Some(sheet));

    // The bar's hover picker chooses the owner's window.
    assert!(shell.raise_window(&mut comp, id));

    assert_eq!(
        shell.router().focused(),
        Some(sheet),
        "the raise focused the window behind its own open sheet"
    );
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

    // A press on a program row arms the click; the launch is the release
    // that follows without the pointer leaving the row.
    let at = library_row_at(&shell, "Calc");
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
fn the_entry_menus_shortcut_row_asks_the_embedder_to_make_one() {
    let mut shell = shell();
    let mut comp = compositor();
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(catalog(&[("chess", "Chess", LibraryCategory::Games)]));

    // Open the popup, then ask for the row's own context menu.
    shell.handle(moved(24, 1060), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
    let at = library_row_at(&shell, "Chess");
    shell.handle(moved(at.x, at.y), &mut comp, 0);
    let ShellOutcome::Taskbar(TaskbarResponse::OpenMenu(request)) =
        shell.handle(SECONDARY_PRESS, &mut comp, 0)
    else {
        panic!("a secondary press on a library row asked for no menu");
    };

    // The shortcut row, found by the label its one definition gives it.
    let answered = choose_bar_menu_row(&mut shell, &mut comp, request, EntryRow::Shortcut.label());

    let Some(TaskbarResponse::CreateDesktopShortcut { entry }) = answered else {
        panic!("expected a shortcut request, got {answered:?}");
    };
    assert_eq!(entry.as_str(), "os.tairix.chess");
    assert!(
        !shell.session().taskbar().library().is_open(),
        "the modal popup gets out of the way of the icon it just asked for"
    );
}

#[test]
fn the_strip_groups_windows_under_the_process_that_owns_them() {
    let mut service = AppBarService::new();
    let one = window_owner(1);
    let two = window_owner(2);
    let windows = vec![(one, TaskId(0)), (two, TaskId(1)), (one, TaskId(2))];
    let strip = service.strip(&windows, |owner| {
        (owner == one).then(|| String::from("/Apps/terminal.app"))
    });
    assert_eq!(strip.len(), 2, "one slot per process, not per window");
    assert_eq!(strip[0].owner, one);
    assert_eq!(strip[0].bundle.as_deref(), Some("/Apps/terminal.app"));
    assert_eq!(strip[0].windows, vec![TaskId(0), TaskId(2)]);
    assert_eq!(strip[1].owner, two);
    assert_eq!(
        strip[1].bundle, None,
        "a process the desktop did not launch has no bundle to attest"
    );
    assert_eq!(strip[1].windows, vec![TaskId(1)]);
}

#[test]
fn a_declaration_holds_a_slot_with_no_windows_and_leaves_on_withdrawal() {
    let mut service = AppBarService::new();
    let owner = window_owner(1);
    assert!(!service.take_dirty());
    service.declare(owner, &app_bar(true)).expect("declared");
    assert!(
        service.take_dirty(),
        "a declaration latches the strip dirty"
    );

    let strip = service.strip(&[], |_| None);
    assert_eq!(strip.len(), 1, "a declaring application keeps its slot");
    assert_eq!(strip[0].windows, Vec::new());
    assert!(service.declaration(owner).is_some());

    // Re-declaring replaces the declaration whole rather than adding a slot.
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Info).expect("fits");
    service
        .declare(
            owner,
            &AppBar {
                event_endpoint: 9,
                default_action: false,
                menu,
            },
        )
        .expect("re-declared");
    assert_eq!(service.strip(&[], |_| None).len(), 1);
    let declared = service.declaration(owner).expect("held");
    assert!(!declared.default_action);
    assert_eq!(declared.menu.len(), 1);

    // The window engine proved the process gone: the slot goes with it.
    service.withdraw(owner);
    assert!(service.take_dirty());
    assert!(service.declaration(owner).is_none());
    assert!(service.strip(&[], |_| None).is_empty());
}

#[test]
fn a_window_alone_holds_a_slot_with_no_menu_and_no_default_action() {
    let mut service = AppBarService::new();
    let owner = window_owner(1);
    let strip = service.strip(&[(owner, TaskId(0))], |_| None);
    assert_eq!(strip.len(), 1, "no window is ever unreachable");
    let mut reader = MemoryAssets::default();
    let slots =
        with_artwork(|resolver, cache| service.slots(&strip, &mut reader, (resolver, cache, 24)));
    assert!(
        slots[0].menu().is_empty(),
        "the session invents no menu on an application's behalf"
    );
    assert!(!slots[0].handles_default());
}

#[test]
fn a_slot_keeps_its_place_while_it_lives() {
    let mut service = AppBarService::new();
    let (first, second, third) = (window_owner(1), window_owner(2), window_owner(3));
    service.declare(first, &app_bar(true)).expect("declared");
    service.declare(second, &app_bar(true)).expect("declared");
    assert_eq!(
        owners(&mut service, &[]),
        vec![first, second],
        "slots appear in the order the session first saw them"
    );

    // The middle application exits and a new one starts: the survivor keeps
    // its place and the newcomer joins the end, so the strip never
    // reshuffles under the pointer.
    service.withdraw(first);
    service.declare(third, &app_bar(false)).expect("declared");
    assert_eq!(owners(&mut service, &[]), vec![second, third]);
}

#[test]
fn a_declaration_is_refused_past_the_strips_bound() {
    let mut service = AppBarService::new();
    for index in 0..MAX_BAR_APPS {
        service
            .declare(window_owner_wide(index), &app_bar(false))
            .expect("fits");
    }
    assert_eq!(
        service.declare(window_owner_wide(MAX_BAR_APPS), &app_bar(false)),
        Err(Errno::NoSpace),
        "a fork bomb cannot grow the strip without bound"
    );
    // An application already on the bar may still re-declare at the bound.
    assert_eq!(
        service.declare(window_owner_wide(0), &app_bar(true)),
        Ok(())
    );
}

#[test]
fn a_slots_identity_is_the_signed_manifests_and_a_missing_one_states_only_a_name() {
    let mut service = AppBarService::new();
    let (named, bare, none) = (window_owner(1), window_owner(2), window_owner(3));
    let mut reader = MemoryAssets::default().with(
        "/Apps/terminal.app/AppInfo",
        &described_manifest_fixture("Terminal", "Runs a shell", "TAIRiX"),
    );
    let strip = service.strip(
        &[(named, TaskId(0)), (bare, TaskId(1)), (none, TaskId(2))],
        |owner| {
            if owner == named {
                Some(String::from("/Apps/terminal.app"))
            } else if owner == bare {
                Some(String::from("/Apps/ghost.app"))
            } else {
                None
            }
        },
    );
    let slots =
        with_artwork(|resolver, cache| service.slots(&strip, &mut reader, (resolver, cache, 24)));

    // The manifest's own fields, never anything the process claims.
    assert_eq!(slots[0].label(), "Terminal");
    assert_eq!(slots[0].identity().version, "1");
    assert_eq!(slots[0].identity().purpose.as_deref(), Some("Runs a shell"));
    assert_eq!(slots[0].identity().author.as_deref(), Some("TAIRiX"));

    // A bundle with no readable manifest states its leaf name and nothing
    // it did not read.
    assert_eq!(slots[1].label(), "ghost");
    assert_eq!(slots[1].identity().version, "");
    assert_eq!(slots[1].identity().purpose, None);

    // A process the desktop did not launch has nothing attesting an
    // identity, so its slot never wears a name it was handed.
    assert_eq!(slots[2].label(), "Application");
    assert_eq!(slots[2].identity().version, "");
}

#[test]
fn a_manifest_is_read_once_per_bundle_and_forgotten_when_nothing_runs_from_it() {
    let mut service = AppBarService::new();
    let (one, two) = (window_owner(1), window_owner(2));
    let mut reader = MemoryAssets::default().with(
        "/Apps/terminal.app/AppInfo",
        &manifest_fixture("Terminal", None),
    );
    let bundle = |_| Some(String::from("/Apps/terminal.app"));

    let strip = service.strip(&[(one, TaskId(0)), (two, TaskId(1))], bundle);
    let _ =
        with_artwork(|resolver, cache| service.slots(&strip, &mut reader, (resolver, cache, 24)));
    assert_eq!(
        reader.reads("/Apps/terminal.app/AppInfo"),
        1,
        "two applications from one bundle read its manifest once"
    );

    // The bundle's last application leaves, so its identity is dropped
    // rather than held for a process that will never return.
    let strip = service.strip(&[], bundle);
    assert!(strip.is_empty());
    let strip = service.strip(&[(one, TaskId(0))], bundle);
    let _ =
        with_artwork(|resolver, cache| service.slots(&strip, &mut reader, (resolver, cache, 24)));
    assert_eq!(reader.reads("/Apps/terminal.app/AppInfo"), 2);
}

#[test]
fn a_slot_carries_the_declaration_its_own_process_made() {
    let mut service = AppBarService::new();
    let (declaring, silent) = (window_owner(1), window_owner(2));
    service
        .declare(declaring, &app_bar(true))
        .expect("declared");
    let strip = service.strip(&[(silent, TaskId(0))], |_| None);
    let mut reader = MemoryAssets::default();
    let slots =
        with_artwork(|resolver, cache| service.slots(&strip, &mut reader, (resolver, cache, 24)));
    assert_eq!(slots.len(), 2);
    assert!(slots[0].handles_default(), "the declaring process's slot");
    assert!(!slots[0].menu().is_empty());
    assert!(
        !slots[1].handles_default(),
        "one process's declaration never reaches another's slot"
    );
    assert!(slots[1].menu().is_empty());
}

#[test]
fn a_thumbnail_scales_a_windows_frame_to_the_cell() {
    let magenta = Color {
        r: 255,
        g: 0,
        b: 255,
        a: 255,
    };
    let frame = Surface::filled(200, 120, magenta.premultiply()).expect("a frame");
    let scaled = thumbnail(&frame, 40, 24).expect("scaled");
    assert_eq!((scaled.width(), scaled.height()), (40, 24));
    assert_eq!(
        scaled.get(20, 12),
        Some(magenta.premultiply()),
        "the window's own pixels, resampled rather than re-drawn"
    );
    // A degenerate frame or cell is refused rather than guessed at, so the
    // cell falls back to the application's glyph.
    assert!(thumbnail(&frame, 0, 24).is_none());
    assert!(thumbnail(&Surface::new(0, 0).expect("empty"), 8, 8).is_none());
}

#[test]
fn picker_cells_caption_each_window_and_refuse_below_a_choice() {
    let mut bar = tairix_taskbar::Taskbar::new(
        TaskbarConfig::bottom_bar(1000, 800),
        &tairix_theme::Theme::dark().floating(),
    );
    bar.tasks_mut().add(TaskId(1), "Shell");
    bar.tasks_mut().add(TaskId(2), "Logs");
    bar.set_apps(vec![
        tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
            .with_windows(vec![TaskId(1), TaskId(2)]),
        tairix_taskbar::AppSlot::new("Editor", IconKind::AppBundle).with_windows(vec![TaskId(1)]),
    ]);
    let magenta = Color {
        r: 255,
        g: 0,
        b: 255,
        a: 255,
    };

    let cells = picker_cells(&bar, 0, |task| {
        (task == TaskId(1))
            .then(|| Surface::filled(32, 20, magenta.premultiply()).expect("thumbnail"))
    });
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].window(), TaskId(1));
    assert_eq!(cells[0].title(), "Shell");
    assert_eq!(
        cells[0].thumbnail().map(|art| (art.width(), art.height())),
        Some((32, 20))
    );
    assert_eq!(cells[1].title(), "Logs");
    assert!(
        cells[1].thumbnail().is_none(),
        "a window whose thumbnail is not prepared yet draws the glyph"
    );

    // One window is no choice, and an unknown application is no application.
    assert!(picker_cells(&bar, 1, |_| None).is_empty());
    assert!(picker_cells(&bar, 9, |_| None).is_empty());
}

#[test]
fn the_window_host_relays_a_declaration_and_its_withdrawal() {
    let (mut shell, mut comp) = (shell(), compositor());
    let mut windows = SessionWindows::new();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    let mut apps = AppBarService::new();
    let owner = window_owner(1);
    {
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut comp,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut apps,
            menu: &mut MenuChain::new(),
            seat_held: false,
        };
        tairix_window::WindowHost::app_bar_declared(&mut host, owner, &app_bar(true))
            .expect("the session lists it");
        tairix_window::WindowHost::app_bar_withdrawn(&mut host, window_owner(9));
    }
    assert!(apps.declaration(owner).is_some());
    assert!(
        apps.declaration(window_owner(9)).is_none(),
        "withdrawing a presence nobody declared changes nothing"
    );
    {
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut comp,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut apps,
            menu: &mut MenuChain::new(),
            seat_held: false,
        };
        tairix_window::WindowHost::app_bar_withdrawn(&mut host, owner);
    }
    assert!(apps.declaration(owner).is_none());
}

#[test]
fn secondary_press_over_an_app_slot_opens_the_menu_it_declared() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Item(AppMenuItem::new(
        AppMenuItemId::new(1).expect("non-zero"),
        AppMenuLabel::new("Quit").expect("short"),
    )))
    .expect("fits");
    shell.set_apps(
        &mut comp,
        vec![
            tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
                .with_declaration(menu, true),
        ],
    );

    let at = app_slot_point(&shell, 0);
    shell.handle(moved(at.x, at.y), &mut comp, 0);
    let ShellOutcome::Taskbar(TaskbarResponse::OpenMenu(request)) =
        shell.handle(SECONDARY_PRESS, &mut comp, 0)
    else {
        panic!("a secondary press on a slot asked for no menu");
    };
    assert_eq!(
        request.subject,
        tairix_taskbar::MenuSubject::App { app: 0 },
        "the menu names the slot's own application"
    );

    // The declared row is relayed back to the application by its own id: the
    // bar never interprets one.
    assert_eq!(
        choose_bar_menu_row(&mut shell, &mut comp, request, "Quit"),
        Some(TaskbarResponse::AppMenuChosen {
            app: 0,
            item: AppMenuItemId::new(1).expect("non-zero"),
        })
    );
}

#[test]
fn hovering_a_two_window_app_shows_the_picker_as_its_own_window() {
    let mut shell = shell();
    let mut comp = compositor();
    shell
        .session_mut()
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(1), "Shell");
    shell
        .session_mut()
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(2), "Logs");
    shell.set_apps(
        &mut comp,
        vec![
            tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
                .with_windows(vec![TaskId(1), TaskId(2)]),
        ],
    );
    assert!(shell.presenter().picker_window().is_none());

    let at = app_slot_point(&shell, 0);
    assert_eq!(
        shell.handle(moved(at.x, at.y), &mut comp, 0),
        ShellOutcome::Ignored,
        "arriving on the slot opens nothing: the picker waits out its dwell"
    );
    assert!(shell.presenter().picker_window().is_none());

    // The clock opens it, and the shell builds the cells out of the
    // thumbnails it prepared while the dwell ran.
    shell.tick_taskbar(&mut comp, PICKER_OPEN_DELAY_NS);
    assert!(shell.session().taskbar().picker().is_open());
    assert!(shell.presenter().picker_window().is_some());

    // Pressing a cell chooses that window: the shell raises it and the
    // picker goes down with the choice.
    let layout = shell
        .session()
        .taskbar()
        .picker_layout(Scale::ONE)
        .expect("open");
    let cell = centre(layout.cells[1]);
    shell.handle(moved(cell.x, cell.y), &mut comp, 0);
    assert_eq!(
        shell.handle(PRIMARY_PRESS, &mut comp, 0),
        ShellOutcome::Taskbar(TaskbarResponse::WindowChosen { id: TaskId(2) })
    );
    assert!(!shell.session().taskbar().picker().is_open());
    assert!(shell.presenter().picker_window().is_none());
    assert_eq!(
        shell.session().taskbar().tasks().focused(),
        Some(TaskId(2)),
        "the chosen window is the focused one"
    );
}

/// The reported freeze: a picker over a screenful of windows must not be built
/// by scaling every one of their frames in a single turn of the serve loop.
///
/// The dwell is the budget. One window is scaled per turn while the pointer
/// rests, the loop is free to serve everything else between those turns, and
/// the picker opens already drawn. What proves it here is the *shape* of the
/// work: one call, one thumbnail, and a park deadline that says more is owed.
#[test]
fn a_pickers_thumbnails_are_scaled_one_turn_at_a_time() {
    let mut shell = shell();
    let mut comp = compositor();
    // Three real windows with real pixels, all owned by one application.
    let frame = || Surface::filled(400, 300, Color::rgb(0, 120, 255).premultiply());
    let windows: Vec<TaskId> = (1..=3)
        .map(|n| {
            let surface = frame().expect("surface");
            let window = shell
                .open_window(&mut comp, Point::new(10 * n, 10 * n), surface, "Terminal")
                .expect("opens");
            shell
                .session()
                .taskbar()
                .tasks()
                .entries()
                .iter()
                .map(|entry| entry.id)
                .find(|task| shell.tasks().window_for(*task) == Some(window))
                .expect("a task")
        })
        .collect();
    shell.set_apps(
        &mut comp,
        vec![
            tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
                .with_windows(windows.clone()),
        ],
    );
    assert!(
        !shell.window_thumbnails_owed(),
        "nothing is hovered, so nothing is owed"
    );

    // Rest the pointer on the slot: the dwell arms and the work begins.
    let at = app_slot_point(&shell, 0);
    assert_eq!(
        shell.handle(moved(at.x, at.y), &mut comp, 0),
        ShellOutcome::Ignored
    );
    for owed in (0..windows.len()).rev() {
        assert!(
            shell.window_thumbnails_owed(),
            "{owed} windows still to scale, so the park is due now"
        );
        assert!(
            !shell.advance_window_thumbnails(&mut comp),
            "no picker is open yet, so nothing is drawn"
        );
    }
    assert!(
        !shell.window_thumbnails_owed(),
        "every window was scaled, one turn each"
    );

    // The dwell elapses and the picker opens already populated.
    shell.tick_taskbar(&mut comp, PICKER_OPEN_DELAY_NS);
    let picker = shell.session().taskbar().picker();
    assert!(picker.is_open());
    assert_eq!(picker.entries().len(), 3);
    let (width, height) = shell
        .session()
        .taskbar()
        .picker_thumbnail_size(comp.scale());
    for (index, entry) in picker.entries().iter().enumerate() {
        assert_eq!(
            entry.thumbnail().map(|art| (art.width(), art.height())),
            Some((width, height)),
            "cell {index} opened without its thumbnail"
        );
    }
}

/// A pointer that leaves drops the prepared pixels: thumbnails are held only
/// while there is a picker to show them in.
#[test]
fn leaving_a_slot_drops_the_thumbnails_it_was_preparing() {
    let mut shell = shell();
    let mut comp = compositor();
    let surface =
        Surface::filled(400, 300, Color::rgb(0, 120, 255).premultiply()).expect("surface");
    let window = shell
        .open_window(&mut comp, Point::new(10, 10), surface, "Terminal")
        .expect("opens");
    let task = shell
        .session()
        .taskbar()
        .tasks()
        .entries()
        .iter()
        .map(|entry| entry.id)
        .find(|task| shell.tasks().window_for(*task) == Some(window))
        .expect("a task");
    shell
        .session_mut()
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(90), "Second");
    shell.set_apps(
        &mut comp,
        vec![
            tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
                .with_windows(vec![task, TaskId(90)]),
        ],
    );

    let at = app_slot_point(&shell, 0);
    let _ = shell.handle(moved(at.x, at.y), &mut comp, 0);
    assert!(shell.window_thumbnails_owed());

    // Away, well inside the dwell.
    let _ = shell.handle(moved(at.x, at.y - 400), &mut comp, 1);
    assert!(!shell.advance_window_thumbnails(&mut comp));
    assert!(
        !shell.window_thumbnails_owed(),
        "the preparation went with the pointer"
    );
    shell.tick_taskbar(&mut comp, PICKER_OPEN_DELAY_NS);
    assert!(!shell.session().taskbar().picker().is_open());
}

// ---- the pointer's focus: what is drawn there is what gets the pointer ----

/// Seat a two-window application on the bar and rest the pointer on its slot
/// with the hover picker open, returning the slot's centre.
///
/// The picker is the strongest witness the bar has for "the pointer is on me":
/// it is a whole surface that exists only for as long as that is true.
fn hover_a_two_window_slot(shell: &mut DesktopShell, comp: &mut Compositor) -> Point {
    shell
        .session_mut()
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(1), "Shell");
    shell
        .session_mut()
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(2), "Logs");
    shell.set_apps(
        comp,
        vec![
            tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
                .with_windows(vec![TaskId(1), TaskId(2)]),
        ],
    );
    let at = app_slot_point(shell, 0);
    assert_eq!(
        shell.handle(moved(at.x, at.y), comp, 0),
        ShellOutcome::Ignored
    );
    shell.tick_taskbar(comp, PICKER_OPEN_DELAY_NS);
    assert!(shell.session().taskbar().picker().is_open());
    assert_eq!(shell.session().taskbar().apps().hover(), Some(0));
    at
}

/// The reported defect with no click in it at all: with the pointer resting on
/// an application slot, a window raised over the bar takes the pointer with it.
/// The slot must stop being hovered and the picker must go — a panel of window
/// thumbnails left floating over the window the user just brought forward is a
/// surface nobody asked for, and the next click on it would act on a gesture
/// the user made at something else.
///
/// Nothing moved, so nothing can be inferred from a position: the pointer is
/// still at the slot's own coordinates. Only the seat, which can see the stack,
/// knows — so it tells the bar.
#[test]
fn a_window_raised_over_a_hovered_slot_takes_the_hover_with_it() {
    let mut shell = shell();
    let mut comp = compositor();
    let at = hover_a_two_window_slot(&mut shell, &mut comp);

    // A window is raised over the bar, exactly where the pointer is resting.
    let covering = opaque_window(&mut comp, Point::new(at.x - 40, at.y - 40), 200, 200);
    assert_eq!(comp.window_at(at), Some(covering));
    shell.present(&mut comp);

    assert_eq!(
        shell.session().taskbar().apps().hover(),
        None,
        "the slot stayed lit under a window that took the pointer"
    );

    // The panel is on its closing grace, not gone on the instant: the same
    // grace is what lets a pointer travelling *towards* a cell cross the gap.
    // The clock ends it, so nothing is left floating over the window.
    shell.tick_taskbar(&mut comp, PICKER_OPEN_DELAY_NS + PICKER_CLOSE_GRACE_NS);
    assert!(
        !shell.session().taskbar().picker().is_open(),
        "the hover picker was left open over the window in front of the bar"
    );
    assert!(
        shell.presenter().picker_window().is_none(),
        "and its window was left in the stack"
    );
}

/// The same rule the ordinary way round: the pointer moves off the bar onto a
/// window. The hover goes with it, and the window gets the motion.
#[test]
fn moving_the_pointer_onto_a_window_takes_the_bars_hover_with_it() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = opaque_window(&mut comp, Point::new(200, 200), 300, 300);
    hover_a_two_window_slot(&mut shell, &mut comp);

    assert_eq!(
        shell.handle(moved(250, 250), &mut comp, 0),
        ShellOutcome::WindowManager(InputResponse::ClientPointerMoved {
            window,
            local: Point::new(50, 50),
        })
    );
    assert_eq!(shell.session().taskbar().apps().hover(), None);
    shell.tick_taskbar(&mut comp, PICKER_OPEN_DELAY_NS + PICKER_CLOSE_GRACE_NS);
    assert!(!shell.session().taskbar().picker().is_open());
}

/// The pointer can *arrive* without moving too: the window covering the bar
/// closes, and the slot under the pointer is hovered again — no jiggling the
/// mouse to provoke it.
///
/// The panel that was open over that slot survives the whole crossing. The
/// pointer never left it, so the window passing over the bar must not be what
/// takes it down: the arrival cancels the closing grace the departure armed.
#[test]
fn closing_a_covering_window_hands_the_hover_back_without_a_motion() {
    let mut shell = shell();
    let mut comp = compositor();
    let at = hover_a_two_window_slot(&mut shell, &mut comp);
    let covering = opaque_window(&mut comp, Point::new(at.x - 40, at.y - 40), 200, 200);
    shell.present(&mut comp);
    assert_eq!(shell.session().taskbar().apps().hover(), None);

    comp.remove(covering);
    shell.present(&mut comp);

    assert_eq!(
        shell.session().taskbar().apps().hover(),
        Some(0),
        "the pointer is on the slot again, so the slot is hovered again"
    );
    assert!(
        shell.session().taskbar().picker().is_open(),
        "the panel went down behind a window that merely passed over the bar"
    );
    shell.tick_taskbar(&mut comp, PICKER_OPEN_DELAY_NS + PICKER_CLOSE_GRACE_NS);
    assert!(
        shell.session().taskbar().picker().is_open(),
        "and the grace it was on was cancelled, not merely postponed"
    );
}

/// An arrival is not a gesture: the pointer being revealed on a multi-window
/// slot hovers it but opens no hover surface, and arms no dwell either. Only a
/// real motion, rested out, asks for a picker.
#[test]
fn an_arrival_on_a_slot_opens_no_hover_surface() {
    let mut shell = shell();
    let mut comp = compositor();
    shell
        .session_mut()
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(1), "Shell");
    shell
        .session_mut()
        .taskbar_mut()
        .tasks_mut()
        .add(TaskId(2), "Logs");
    shell.set_apps(
        &mut comp,
        vec![
            tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
                .with_windows(vec![TaskId(1), TaskId(2)]),
        ],
    );
    let at = app_slot_point(&shell, 0);
    // A window over the slot, then gone: the pointer arrives without moving.
    let covering = opaque_window(&mut comp, Point::new(at.x - 40, at.y - 40), 200, 200);
    let _ = shell.handle(moved(at.x, at.y), &mut comp, 0);
    comp.remove(covering);
    shell.present(&mut comp);

    assert_eq!(shell.session().taskbar().apps().hover(), Some(0));
    shell.tick_taskbar(&mut comp, PICKER_OPEN_DELAY_NS * 4);
    assert!(
        !shell.session().taskbar().picker().is_open(),
        "a window closing is not a gesture: it must not open a hover surface"
    );
}

/// The Switchboard capsule's instrument readout is the case where a stranded
/// hover surface is plainly visible: it opens *above* the bar, so a window
/// raised over the bar alone does not cover it. It has to close with the
/// pointer that opened it.
#[test]
fn a_window_raised_over_the_capsule_collapses_its_readout() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let capsule = capsule_point(&shell);
    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp, 0);
    assert!(shell.session().taskbar().tray().is_expanded());
    assert!(shell.presenter().readout_window().is_some());

    let covering = opaque_window(
        &mut comp,
        Point::new(capsule.x - 20, capsule.y - 20),
        60,
        60,
    );
    assert_eq!(comp.window_at(capsule), Some(covering));
    shell.present(&mut comp);

    assert!(
        !shell.session().taskbar().tray().is_expanded(),
        "the readout stayed expanded over the window that took the pointer"
    );
    assert!(shell.presenter().readout_window().is_none());
}

/// The pointer state of `window`'s Close command.
fn close_command_pointer(comp: &Compositor, window: WindowId) -> PointerState {
    comp.window_frame(window)
        .expect("the window is decorated")
        .title_bar()
        .control(WindowControlKind::Close)
        .expect("a window band seats every command")
        .state()
        .pointer
}

/// The other side of the same contract: a *window's* own decoration hover is
/// dropped when the pointer crosses onto the bar. The window manager is told
/// the pointer left, exactly as the bar is — otherwise a title-bar command
/// would stay lit while the user works the bar, advertising a press that would
/// no longer land on it.
#[test]
fn moving_the_pointer_onto_the_bar_unlights_a_windows_title_bar() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    // The Close command's own cell, asked of the title bar that laid it out
    // inside the band the frame reserved for it.
    let outer = comp.window(window).expect("live").bounds();
    let theme = shell.session().active_theme().clone();
    let frame = comp.window_frame(window).expect("the window is decorated");
    let band = frame.layout(outer, comp.scale(), &theme).title_bar;
    let cell = frame
        .title_bar()
        .layout(band, comp.scale(), &theme)
        .controls()
        .iter()
        .find(|(kind, _)| *kind == WindowControlKind::Close)
        .map(|(_, rect)| *rect)
        .expect("the frame seats a Close command");
    let on_close = centre(cell);

    shell.handle(moved(on_close.x, on_close.y), &mut comp, 0);
    assert_eq!(
        close_command_pointer(&comp, window),
        PointerState::Hover,
        "the pointer is on the Close command, so it is lit"
    );

    let onto = centre(shell.session().taskbar().layout(Scale::ONE).library);
    shell.handle(moved(onto.x, onto.y), &mut comp, 0);

    assert_eq!(
        close_command_pointer(&comp, window),
        PointerState::None,
        "a command stayed lit after the pointer crossed onto the bar"
    );
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
fn shell_set_apps_re_presents_and_updates_the_strip() {
    let mut shell = shell();
    let mut comp = compositor();

    assert_eq!(shell.session().taskbar().apps().len(), 0);

    shell.set_apps(
        &mut comp,
        vec![
            tairix_taskbar::AppSlot::new("One", IconKind::AppBundle),
            tairix_taskbar::AppSlot::new("Two", IconKind::AppBundle),
        ],
    );
    assert_eq!(shell.session().taskbar().apps().len(), 2);
    assert!(shell.presenter().bar_window().is_some());
}

fn centre(rect: tairix_wm::Rect) -> Point {
    assert!(!rect.is_empty());
    rect.center()
}

fn app_slot_point(shell: &DesktopShell, index: usize) -> Point {
    let layout = shell.session().taskbar().layout(Scale::ONE);
    let slot = layout.apps.get(index).expect("an application slot");
    centre(*slot)
}

/// A manifest stating the two optional identity fields as well, for the
/// information panel's own attestation.
fn described_manifest_fixture(name: &str, purpose: &str, author: &str) -> Vec<u8> {
    let mut bytes = manifest_fixture(name, None);
    let mut header = AppInfoHeader::from_bytes(&bytes).expect("the fixture decodes");
    header.purpose_len = u8::try_from(purpose.len()).expect("short");
    header.purpose[..purpose.len()].copy_from_slice(purpose.as_bytes());
    header.author_len = u8::try_from(author.len()).expect("short");
    header.author[..author.len()].copy_from_slice(author.as_bytes());
    bytes = header.to_le_bytes().to_vec();
    bytes
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
        purpose_len: 0,
        author_len: 0,
        library_icon_len: u8::try_from(icon.map_or(0, str::len)).unwrap(),
        library: tairix_abi::LibraryCategory::to_wire(Some(tairix_abi::LibraryCategory::Other)),
        reserved0: [0; 1],
        id: [0; BUNDLE_ID_MAX],
        name: [0; BUNDLE_NAME_MAX],
        version: [0; BUNDLE_VERSION_MAX],
        library_icon: [0; LIBRARY_ICON_MAX],
        purpose: [0; tairix_abi::BUNDLE_PURPOSE_MAX],
        author: [0; tairix_abi::BUNDLE_AUTHOR_MAX],
        syscall_table_hash: [0; SYSCALL_TABLE_HASH_LEN],
        content_hash: [0; 32],
        signer_pubkey: [0; 32],
        publisher_pubkey: [0; 32],
        publisher_cert: [0; 64],
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
        test_frost_cache(),
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
        layout.cards[0].card.center()
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
    slot.center()
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
            section: CommandSection::Tasks,
        })
    );

    // Held past the threshold, the same gesture asks for recovery.
    let _ = shell.handle(PRIMARY_PRESS, &mut comp, 0);
    assert_eq!(
        shell.handle(PRIMARY_RELEASE, &mut comp, LONG_PRESS_NS),
        ShellOutcome::Taskbar(TaskbarResponse::OpenSwitchboard {
            section: CommandSection::Recovery,
        })
    );
}

/// A wheel where a window covers the capsule is that window's, not the task
/// list's: the same "whatever is drawn there owns the gesture" rule as a
/// press, applied to the one scroll the bar claims.
#[test]
fn a_scroll_where_a_window_covers_the_capsule_reaches_that_window() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let capsule = capsule_point(&shell);
    let window = opaque_window(
        &mut comp,
        Point::new(capsule.x - 40, capsule.y - 40),
        200,
        200,
    );

    let _ = shell.handle(moved(capsule.x, capsule.y), &mut comp, 0);

    assert_eq!(
        shell.handle(InputEvent::PointerScrolled { dx: 0, dy: 1 }, &mut comp, 0),
        ShellOutcome::WindowManager(InputResponse::AppScroll {
            window,
            dx: 0,
            dy: 1,
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
        ShellOutcome::Taskbar(TaskbarResponse::WindowChosen { id: first_task })
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
        ShellOutcome::Taskbar(TaskbarResponse::WindowChosen { id: first_task })
    );

    // The handover made the second task the previous one: middle-click
    // toggles back.
    let outcome = shell.handle(MIDDLE_PRESS, &mut comp, 0);
    assert_eq!(
        outcome,
        ShellOutcome::Taskbar(TaskbarResponse::WindowChosen { id: second_task })
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

/// A live monitor is attested by *its own* launch record, so an earlier
/// instance still sitting in the table — one that has exited but not yet
/// been reaped — cannot answer for it.
///
/// The reported defect: the gate used to compare the caller against the
/// lowest-numbered entry for the monitor's bundle path, so a leftover
/// entry below the live instance turned every one of its calls into a
/// permission denial. The instance then read the denial as the session
/// disowning it and vanished, intermittently and silently.
#[test]
fn a_live_monitor_is_attested_beneath_an_older_entry_for_the_same_bundle() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut launched = LaunchTable::new();
    launched.record(MONITOR_PID - 1, "Switchboard", SWITCHBOARD_RUN_PATH);
    launched.record(MONITOR_PID, "Switchboard", SWITCHBOARD_RUN_PATH);
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
        Ok(SwitchboardOutcome::Published {
            session: session_proc_id(),
            publisher: MONITOR_PID,
        }),
        "the caller's own launch record attests it, whatever else the table holds"
    );
    assert_eq!(
        shell.session().taskbar().tray().signal().state().activity,
        tairix_controls::ActivityState::Working,
        "the published summary drove the capsule"
    );
}

/// An accepted publish names the instance that made it, so the session
/// talks back to the one it just attested rather than to whichever entry
/// the table lists first. Self-healing: publishing is the proof of life.
#[test]
fn an_accepted_publish_names_the_publishing_instance_not_the_lowest_entry() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut launched = LaunchTable::new();
    launched.record(MONITOR_PID - 1, "Switchboard", SWITCHBOARD_RUN_PATH);
    launched.record(MONITOR_PID, "Switchboard", SWITCHBOARD_RUN_PATH);
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

    let Ok(SwitchboardOutcome::Published { publisher, .. }) = outcome else {
        panic!("the attested monitor's publish is accepted");
    };
    assert_eq!(publisher, MONITOR_PID);
    assert_ne!(
        Some(publisher),
        launched.running_from(SWITCHBOARD_RUN_PATH),
        "the lowest entry is deliberately not the answer"
    );
}

/// A caller with a launch record for some *other* bundle is refused: a
/// recorded launch is not authority in itself, only a recorded launch of
/// the monitor's own bundle is.
#[test]
fn a_caller_launched_from_another_bundle_is_refused() {
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
            owner_windows: &FakeOwnerWindows::none(),
            relaunch: &mut |_: &mut LaunchTable, run_path: &str, label: &str| {
                relaunches
                    .launched
                    .push((String::from(run_path), String::from(label)));
            },
            self_proc_id: session_proc_id(),
        },
        41,
        &SwitchboardRequest::PublishSummary {
            summary: tray_summary(2),
        }
        .to_le_bytes(),
    );

    assert_eq!(refusal, Err(SwitchboardRefusal::Unattested));
    assert_eq!(
        shell.session().taskbar().tray().signal().state().activity,
        tairix_controls::ActivityState::Idle,
        "a refused call publishes nothing"
    );
}

/// The press finds the recorded instance and starts nothing: one monitor
/// per session is enforced at the spawn, not by refusing a running
/// instance's calls.
#[test]
fn ensuring_the_monitor_reuses_the_recorded_instance() {
    let mut launched = monitor_launched();
    let mut spawns = 0;

    let live = ensure_switchboard(&mut launched, |_| {
        spawns += 1;
        Some(MONITOR_PID + 5)
    });

    assert_eq!(
        live,
        Some(MONITOR_PID),
        "the recorded instance is the live one"
    );
    assert_eq!(spawns, 0, "a recorded instance is never respawned");
    assert_eq!(launched.len(), 1);
}

/// With no instance recorded, the answer is the pid the spawn just
/// recorded — never some other entry the table happens to hold.
#[test]
fn ensuring_the_monitor_answers_with_the_instance_it_started() {
    let mut launched = LaunchTable::new();
    launched.record(3, "Files", "/System/Applications/files.app/Run");

    let live = ensure_switchboard(&mut launched, |launched| {
        launched.record(90, "Switchboard", SWITCHBOARD_RUN_PATH);
        Some(90)
    });

    assert_eq!(live, Some(90), "the instance just started is the live one");
}

/// A refused spawn answers with nothing: the desktop runs without its
/// monitor rather than naming an instance that does not exist.
#[test]
fn ensuring_the_monitor_answers_nothing_when_the_spawn_is_refused() {
    let mut launched = LaunchTable::new();
    assert_eq!(ensure_switchboard(&mut launched, |_| None), None);
    assert!(launched.is_empty());
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
        Ok(SwitchboardOutcome::Published {
            session: session_proc_id(),
            publisher: MONITOR_PID,
        })
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
            // No launch record of its own: an orphan, a foreign process,
            // or a copy launched by hand.
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

/// The frame report the mailbox received, or `None` when it received
/// something else.
fn frame_of(command: SwitchboardCommand) -> Option<FrameReport> {
    match command {
        SwitchboardCommand::FrameReport { report } => Some(report),
        _ => None,
    }
}

/// The frame report is sent only to a live instance and only when the
/// counts moved: a desktop redrawing the same rectangles sends nothing.
#[test]
fn the_frame_report_is_sent_only_on_change_and_only_to_a_live_instance() {
    let mut shell = shell();
    let mut comp = compositor();
    let _ = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    comp.composite();
    let mut mailbox = RecordingMailbox::default();
    let mut gate = FrameReportGate::new();

    gate.maybe_send(&comp, None, FrameContent::Foreign, 0, &mut mailbox);
    assert!(
        mailbox.sent.is_empty(),
        "nothing is sent with no instance live"
    );

    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        0,
        &mut mailbox,
    );
    let (pid, command) = mailbox.sent.first().copied().expect("one report");
    assert_eq!(pid, MONITOR_PID);
    let report = frame_of(command).expect("a frame report");
    let mode = comp.mode();
    assert_eq!(
        report.screen_px,
        u64::from(mode.width_px) * u64::from(mode.height_px)
    );
    assert_eq!(report.damaged_px, comp.frame_stats().damaged_px);
    assert!(report.damaged_px > 0, "the composed frame changed pixels");

    // Offered far enough past the first that the rate limit cannot be what
    // silences it, so this tests the change gate alone — and proves the
    // accepted report is what the gate remembers holding.
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        10 * MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );
    assert_eq!(
        mailbox.sent.len(),
        1,
        "the same frame twice over is one report"
    );
}

/// Whatever the compositor really produces, the receiver's own validation
/// accepts it: a rule that refused a legitimate frame would blank the page
/// as surely as a missing check would trust a hostile one.
#[test]
fn every_reported_frame_survives_the_receivers_validation() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    let mut mailbox = RecordingMailbox::default();
    let mut gate = FrameReportGate::new();

    // A busy frame with a window and its furniture, an idle one that
    // recomposed nothing, and one over bare desktop with the window gone —
    // the frame that blends nothing at all.
    // Each offered a full interval after the last, so the rate limit never
    // hides a frame this test is about.
    comp.composite();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        0,
        &mut mailbox,
    );
    comp.composite();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::None,
        MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );
    shell.close_window(&mut comp, window);
    comp.composite();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::None,
        2 * MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );

    assert!(mailbox.sent.len() >= 2, "the frames above differ");
    for (_, command) in &mailbox.sent {
        assert_eq!(
            SwitchboardCommand::from_bytes(&command.to_le_bytes()),
            Ok(*command),
            "the receiver must accept a frame the compositor really composed"
        );
    }
    assert!(
        mailbox
            .sent
            .iter()
            .filter_map(|(_, command)| frame_of(*command))
            .any(|report| report.is_idle()),
        "a desktop that recomposed nothing reports an idle frame"
    );
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

    // A refused frame report is dropped, and not remembered as sent: the
    // panel never saw it, so the next frame must offer it again.
    let mut shell = shell();
    let mut comp = compositor();
    let _ = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    comp.composite();
    let mut gate = FrameReportGate::new();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        0,
        &mut mailbox,
    );
    assert_eq!(mailbox.attempts, 3, "one frame is one attempt");

    // A refused report is not what the panel holds: the very same frame,
    // offered again once the limit allows, is offered again rather than
    // silenced as something the monitor already has.
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );
    assert_eq!(
        mailbox.attempts, 4,
        "a refused report is offered again, never recorded as sent"
    );
}

/// A frame whose only served content is the Switchboard's own paint must
/// not be reported: that is the monitor measuring itself, and reporting it
/// re-excites another paint forever.
#[test]
fn a_switchboard_only_frame_is_not_reported() {
    let mut shell = shell();
    let mut comp = compositor();
    let _ = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    comp.composite();
    let mut mailbox = RecordingMailbox::default();
    let mut gate = FrameReportGate::new();

    // A real desktop frame still reports.
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        0,
        &mut mailbox,
    );
    assert_eq!(mailbox.sent.len(), 1, "real work is reported");

    // The Switchboard rebuilds from that report and presents only itself.
    // Counters differ (its paint is real work for the compositor) but the
    // content gate must drop the report so the loop cannot restart. Offered
    // past the interval, so the content gate is what has to stop it.
    let _ = open_app(&mut shell, &mut comp, Point::new(100, 100), "Switchboard");
    comp.composite();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::SwitchboardOnly,
        MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );
    assert_eq!(
        mailbox.sent.len(),
        1,
        "the monitor's own paint must not re-excite a report"
    );

    // A suppressed report is not remembered either: that very frame, once
    // some other window's content lands in it, is still news to the monitor.
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        2 * MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );
    assert_eq!(
        mailbox.sent.len(),
        2,
        "a report the content gate dropped was never recorded as sent"
    );

    // Chrome-only or idle work with no served present still reports when
    // the counters move — the gate is content, not a blanket silence.
    shell.close_window(&mut comp, shell.router().focused().expect("live"));
    // Close the editor too so the settle is not another app present.
    if let Some(other) = shell.router().focused() {
        shell.close_window(&mut comp, other);
    }
    comp.composite();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::None,
        3 * MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );
    assert!(
        mailbox.sent.len() >= 3,
        "a non-Switchboard frame whose counts moved still reports"
    );
}

/// A desktop whose frame counts move on *every* frame still reports at the
/// limited rate, not at frame rate.
///
/// This is the regression for the pointer-over-wallpaper storm: a pointer
/// crossing bare desktop redamages the rectangle the cursor leaves and the
/// one it arrives in, and those two overlap by a different amount each
/// frame, so `damaged_px` and `dirty_rects` differ from the previous frame
/// even though the desktop did nothing new. Change detection alone cannot
/// see that, so without the rate limit every frame became one command — and
/// the monitor rebuilt its whole overview model for each one, burning half a
/// core with its window closed.
#[test]
fn a_frame_report_storm_is_capped_at_one_report_per_interval() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    let mut mailbox = RecordingMailbox::default();
    let mut gate = FrameReportGate::new();

    // Twenty frames inside a single interval, each damaging a different pair
    // of rectangles — the counters moving every frame, as motion moves them.
    let mut counts = Vec::new();
    for step in 0..20i32 {
        assert!(
            comp.move_window(window, Point::new(300 + step * step, 200 + step)),
            "the window this test moves must exist"
        );
        comp.composite();
        counts.push(comp.frame_stats().damaged_px);
        gate.maybe_send(
            &comp,
            Some(MONITOR_PID),
            FrameContent::Foreign,
            u64::try_from(step).expect("a small step") * (MIN_FRAME_REPORT_INTERVAL_NS / 100),
            &mut mailbox,
        );
    }

    // The premise: these really are frames the change gate would have let
    // through, so the limit is what this test measures.
    assert!(
        counts.windows(2).any(|pair| pair[0] != pair[1]),
        "the frames this test composes must differ from one another"
    );
    assert_eq!(
        mailbox.sent.len(),
        1,
        "counts that move every frame are still one report per interval"
    );
}

/// A change the limit held back goes out once the interval has elapsed, and
/// it carries the counts of the frame on screen *then* — not the stale ones
/// it was holding.
#[test]
fn a_held_back_frame_report_goes_out_with_the_counts_it_has_when_it_can() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    let mut mailbox = RecordingMailbox::default();
    let mut gate = FrameReportGate::new();

    comp.composite();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        0,
        &mut mailbox,
    );
    assert_eq!(mailbox.sent.len(), 1, "the first frame reports at once");

    // A different frame inside the interval is held back.
    assert!(comp.move_window(window, Point::new(320, 220)));
    comp.composite();
    let held = comp.frame_stats().damaged_px;
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        MIN_FRAME_REPORT_INTERVAL_NS / 2,
        &mut mailbox,
    );
    assert_eq!(mailbox.sent.len(), 1, "a change inside the interval waits");

    // A different frame again, now that the interval has elapsed.
    assert!(comp.move_window(window, Point::new(500, 400)));
    comp.composite();
    let fresh = comp.frame_stats().damaged_px;
    assert_ne!(held, fresh, "the two frames this test compares must differ");
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );

    let (_, command) = mailbox.sent.get(1).copied().expect("the held-back report");
    let report = frame_of(command).expect("a frame report");
    assert_eq!(
        report.damaged_px, fresh,
        "a held-back report is re-read, never replayed: the page must show the frame on screen"
    );
}

/// The rate limit costs the desktop no report and no poll: a held-back
/// change tightens the session's park to the moment it may go out, and
/// nothing else ever arms a timer.
#[test]
fn the_frame_report_park_deadline_arms_only_while_a_change_is_held_back() {
    let mut shell = shell();
    let mut comp = compositor();
    let window = open_app(&mut shell, &mut comp, Point::new(300, 200), "Editor");
    let mut mailbox = RecordingMailbox::default();
    let mut gate = FrameReportGate::new();

    assert_eq!(
        gate.park_deadline_ns(0, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "a session that has reported nothing arms no timer"
    );

    comp.composite();
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        0,
        &mut mailbox,
    );
    assert_eq!(mailbox.sent.len(), 1);
    assert_eq!(
        gate.park_deadline_ns(0, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "a report that went out holds nothing back"
    );

    assert!(comp.move_window(window, Point::new(340, 240)));
    comp.composite();
    let held_at = MIN_FRAME_REPORT_INTERVAL_NS / 4;
    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        held_at,
        &mut mailbox,
    );
    assert_eq!(mailbox.sent.len(), 1, "held back inside the interval");
    assert_eq!(
        gate.park_deadline_ns(held_at, NO_DEADLINE_NS),
        MIN_FRAME_REPORT_INTERVAL_NS - held_at,
        "the park tightens to exactly when the held-back report may go"
    );
    assert_eq!(
        gate.park_deadline_ns(held_at, 5),
        5,
        "folding in only ever shortens a caller's own deadline"
    );

    gate.maybe_send(
        &comp,
        Some(MONITOR_PID),
        FrameContent::Foreign,
        MIN_FRAME_REPORT_INTERVAL_NS,
        &mut mailbox,
    );
    assert_eq!(mailbox.sent.len(), 2, "the held-back change goes out");
    assert_eq!(
        gate.park_deadline_ns(MIN_FRAME_REPORT_INTERVAL_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "the flush must not re-arm: a desktop gone quiet stays quiet"
    );
}

/// A dropped frame reading says nothing on the console; every command with
/// a consequence a user can see still does.
///
/// The readings are offered again on the very next frame that differs, so
/// a full mailbox produces one refusal per frame — and it fills precisely
/// when the desktop is busy, which is the load the reading describes.
/// Narrating each would put a synchronous console write on every frame of
/// exactly that load.
#[test]
fn a_dropped_frame_reading_is_not_worth_stating() {
    let report = FrameReport {
        screen_px: 1,
        damaged_px: 1,
        blended_px: 0,
        opaque_px: 1,
        dirty_rects: 1,
        present_calls: 1,
        chrome_hits: 0,
        chrome_misses: 0,
    };
    assert!(
        !drop_is_noteworthy(SwitchboardCommand::FrameReport { report }),
        "a reading the monitor may ignore is not news"
    );

    for command in [
        SwitchboardCommand::OpenPanel {
            section: CommandSection::Tasks,
        },
        SwitchboardCommand::Power {
            action: PowerAction::PowerOff,
        },
        SwitchboardCommand::SeatReport {
            report: SeatReport::HEALTHY,
        },
    ] {
        assert!(
            drop_is_noteworthy(command),
            "a command with a visible consequence is stated when it is dropped: {command:?}"
        );
    }
}

/// Owner classification: only a present whose owner is exactly the live
/// Switchboard is Switchboard-only; anything else is foreign work.
#[test]
fn presented_owners_classify_switchboard_only_versus_foreign() {
    let mut owners = PresentedOwners::default();
    assert_eq!(owners.content(), FrameContent::None);

    owners.note(Some(MONITOR_PID), Some(MONITOR_PID));
    assert_eq!(owners.content(), FrameContent::SwitchboardOnly);

    owners.note(Some(MONITOR_PID + 1), Some(MONITOR_PID));
    assert_eq!(owners.content(), FrameContent::Foreign);

    let mut only_unknown = PresentedOwners::default();
    only_unknown.note(None, Some(MONITOR_PID));
    assert_eq!(
        only_unknown.content(),
        FrameContent::Foreign,
        "an unresolved owner is not assumed to be the Switchboard"
    );

    let mut no_live = PresentedOwners::default();
    no_live.note(Some(MONITOR_PID), None);
    assert_eq!(
        no_live.content(),
        FrameContent::Foreign,
        "without a live Switchboard every present is foreign work"
    );
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

use crate::confirm::{build_dialog, Answer, ConfirmPrompt, CONFIRM_ORIGIN, WIN_HEIGHT, WIN_WIDTH};
use crate::relay_power;
use tairix_abi::PowerAction;

/// The prompt-local centre of the action button at `index`, resolved through
/// the very dialog the prompt draws, so a test presses the button rather
/// than a re-derived guess at where it sits.
fn prompt_action_centre(action: PowerAction, index: usize, shell: &DesktopShell) -> Point {
    let theme = shell.session().active_theme();
    let rects = build_dialog(action).action_rects(
        Rect::new(0, 0, WIN_WIDTH, WIN_HEIGHT),
        Scale::ONE,
        theme,
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

/// A fake [`Verifier`] that answers from a scripted list of verdicts, oldest
/// first, and records every password it was ever offered — so a test can
/// assert both what was typed and what came back for it.
#[derive(Default)]
struct ScriptedUnlocker {
    answers: Vec<Verdict>,
    offered: Vec<String>,
}

impl ScriptedUnlocker {
    /// Answers `answers` in order as it is offered passwords, one verdict
    /// per offer. Offered past the end of the script, it refuses.
    fn scripted(mut answers: Vec<Verdict>) -> Self {
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

impl Verifier for ScriptedUnlocker {
    /// The account name is ignored here as it is in the running desktop: the
    /// lock re-authenticates the caller the kernel attests to.
    fn verify(&mut self, _account: &str, password: &str) -> Verdict {
        self.offered.push(String::from(password));
        self.answers.pop().unwrap_or(Verdict::Refused)
    }
}

/// A key press with no modifiers held.
fn key_press(key: Key) -> InputEvent {
    InputEvent::KeyPressed {
        key,
        modifiers: tairix_wm::Modifiers::default(),
    }
}

/// The instant the lock tests that exercise event handling hand the lock.
///
/// They assert what a key or a click *does*, never how anything moves, so
/// one named instant serves all of them; the motion tests below name their
/// own so the arithmetic reads.
const LOCK_EVENT_NS: u64 = 3_000_000_000;

/// Type `password`'s characters into the lock one key at a time, then press
/// Enter, returning the outcome of that final, submitting event. Every key
/// lands at `now_ns`, as one burst of typing does.
fn submit(
    lock: &mut ScreenLock,
    password: &str,
    now_ns: u64,
    unlocker: &mut dyn Verifier,
    shell: &DesktopShell,
    comp: &mut Compositor,
) -> LockOutcome {
    for ch in password.chars() {
        lock.handle(&key_press(Key::Char(ch)), now_ns, unlocker, shell, comp);
    }
    lock.handle(
        &key_press(Key::Named(NamedKey::Enter)),
        now_ns,
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
    let mut unlocker = ScriptedUnlocker::scripted(vec![Verdict::Refused]);

    let outcome = submit(
        &mut lock,
        "wrong",
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );

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
    let mut unlocker = ScriptedUnlocker::scripted(vec![Verdict::Unreachable]);

    let outcome = submit(
        &mut lock,
        "whatever",
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );

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
    let mut unlocker = ScriptedUnlocker::scripted(vec![Verdict::Verified]);

    let outcome = submit(
        &mut lock,
        "correct horse battery staple",
        LOCK_EVENT_NS,
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
    let mut unlocker = ScriptedUnlocker::scripted(vec![Verdict::Verified]);

    submit(
        &mut lock,
        "Hunter2!",
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );

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
    let mut unlocker = ScriptedUnlocker::scripted(vec![Verdict::Refused, Verdict::Verified]);

    submit(
        &mut lock,
        "wrong",
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );
    submit(
        &mut lock,
        "right",
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );

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
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );
    assert!(lock.is_locked(), "Escape does not unlock the screen");

    lock.handle(
        &key_press(Key::Named(NamedKey::Enter)),
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );
    assert!(lock.is_locked(), "Enter on an empty field does not unlock");

    lock.handle(
        &key_press(Key::Named(NamedKey::Tab)),
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );
    assert!(lock.is_locked(), "Tab does not unlock");

    lock.handle(
        &key_press(Key::Char('x')),
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );
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

    let outcome = lock.handle(
        &PRIMARY_PRESS,
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );

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
    unlocker: &mut dyn Verifier,
    shell: &DesktopShell,
    comp: &mut Compositor,
) -> LockedDrain {
    let mut drain = LockedDrain::new();
    for event in events {
        drain.feed(lock, event, LOCK_EVENT_NS, unlocker, shell, comp);
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
    let mut unlocker = ScriptedUnlocker::scripted(vec![Verdict::Verified]);
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
    let mut unlocker = ScriptedUnlocker::scripted(vec![Verdict::Verified]);

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
        lock.handle(
            &PRIMARY_PRESS,
            LOCK_EVENT_NS,
            &mut unlocker,
            &shell,
            &mut comp
        ),
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

/// The path of a bundle asset the fake reader will serve. The bytes never
/// reach a real decoder here — the rasteriser above is injected — so any
/// non-empty payload stands for the artwork.
fn artwork_source(bundle: &str) -> String {
    format!("{bundle}/Resources/icon.svg")
}

fn artwork_assets(bundles: &[&str]) -> ArtworkFileReader<MemoryAssets> {
    let mut assets = MemoryAssets::default();
    for bundle in bundles {
        assets = assets.with(&artwork_source(bundle), VALID_SVG);
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
    let path = artwork_source("/Apps/One.app");

    assert!(cache
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut rasteriser),
            &path,
            16
        )
        .is_some());
    assert!(cache
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut rasteriser),
            &path,
            16
        )
        .is_some());
    assert_eq!(rasteriser.0.calls, 1, "the second lookup is a cache hit");

    // A different side is a different entry: the artwork is rasterised
    // again at the new geometry rather than scaled from the old one.
    assert!(cache
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut rasteriser),
            &path,
            32
        )
        .is_some());
    assert_eq!(rasteriser.0.calls, 2);
}

#[test]
fn a_refused_pin_icon_is_refused_once_not_on_every_refresh() {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let mut reader = artwork_assets(&["/Apps/Bad.app"]);
    let mut rasteriser = ArtworkSandbox(CountingRasteriser::refusing());
    let path = artwork_source("/Apps/Bad.app");

    assert!(cache
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut rasteriser),
            &path,
            16
        )
        .is_none());
    assert!(cache
        .path_artwork(
            &mut InlineArtwork::new(&mut reader, &mut rasteriser),
            &path,
            16
        )
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
        let path = artwork_source(bundle);
        assert!(
            cache
                .path_artwork(
                    &mut InlineArtwork::new(&mut reader, &mut rasteriser),
                    &path,
                    32
                )
                .is_some(),
            "every slot still gets its artwork, cached or not"
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
fn pin_artwork_survives_tightening_pressure_and_is_wiped_on_teardown() {
    // A gauge of this test's own, so moving the band cannot perturb the
    // shared one other tests hold at normal.
    static PRESSURED: ReportedPressure = ReportedPressure::unknown();
    PRESSURED.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&PRESSURED, TEST_FRAME_BYTES);
    let mut reader = artwork_assets(&["/Apps/One.app"]);
    let mut rasteriser = ArtworkSandbox(CountingRasteriser::working());
    let path = artwork_source("/Apps/One.app");
    let mut drawn = |cache: &mut ArtworkCache| {
        cache
            .path_artwork(
                &mut InlineArtwork::new(&mut reader, &mut rasteriser),
                &path,
                16,
            )
            .is_some()
    };

    assert!(drawn(&mut cache));
    let held = cache.charged_bytes();
    assert!(held > 0);

    // Tightening memory does not take the desktop's pictures away. A
    // decoded icon is not local work the session can repeat at will: it is
    // a capability-gated read plus a parser-sandbox round trip, so dropping
    // it frees a fraction of one screenful and then costs both again, per
    // icon, on the next repaint — with the machine already short. On this
    // output the whole budget is inside the shared UI reserve, so no band
    // takes it: the desktop keeps drawing its real artwork however deep
    // pressure goes, and only the session's own teardown clears it.
    for band in [
        PressureBand::Mild,
        PressureBand::Moderate,
        PressureBand::Severe,
        PressureBand::Critical,
    ] {
        PRESSURED.report(band);
        assert_eq!(cache.trim(), 0, "{band:?} keeps the icon working set");
        assert_eq!(cache.charged_bytes(), held);
        assert!(drawn(&mut cache), "{band:?} still draws real artwork");
    }

    PRESSURED.report(PressureBand::Normal);
    assert!(drawn(&mut cache));
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
    let comp = compositor();
    open_library(session.taskbar_mut());
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
    // Two resolutions of the same strip must cost one read and one decode:
    // the icon bar and the library share the one cache the shell owns, so a
    // re-resolve before a paint is a lookup, not a fresh decode of the same
    // file.
    NORMAL_PRESSURE.report(PressureBand::Normal);
    let mut cache = test_artwork_cache(&NORMAL_PRESSURE, TEST_FRAME_BYTES);
    let bundle = "/Apps/One.app";
    let mut reader = ArtworkFileReader(CountingAssets::new(
        MemoryAssets::default()
            .with(
                &format!("{bundle}/AppInfo"),
                &manifest_fixture("One", Some("icon.svg")),
            )
            .with(&artwork_source(bundle), &[BUNDLE_TINT]),
    ));
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());
    let mut manifests = MemoryAssets::default().with(
        &format!("{bundle}/AppInfo"),
        &manifest_fixture("One", Some("icon.svg")),
    );
    let mut service = AppBarService::new();
    let strip = service.strip(&[(window_owner(1), TaskId(0))], |_| {
        Some(String::from(bundle))
    });

    let mut resolve = || {
        let mut inline = InlineArtwork::new(&mut reader, &mut rasteriser);
        service.slots(&strip, &mut manifests, (&mut inline, &mut cache, 24))
    };
    let first = resolve();
    let again = resolve();

    assert_eq!(reader.0.reads, 2, "the manifest and the asset, each once");
    assert_eq!(rasteriser.0.calls, 1, "and one decode");
    let tint = |slots: &[tairix_taskbar::AppSlot]| {
        slots
            .first()
            .and_then(tairix_taskbar::AppSlot::artwork)
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
        &mut InlineArtwork::new(&mut reader, &mut rasteriser),
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

/// The reported defect: with the decode on a worker thread, the launcher
/// popup and the application strip drew a screenful of built-in glyphs and
/// only replaced them a round trip *per icon* later — so a user opening the
/// launcher saw generic pictures, and a running application wore its
/// fallback.
///
/// The fix is that the desktop asks for the whole set the moment the catalog
/// naming it is known, which is long before either surface is shown. This is
/// that contract: after the warm-up and the decodes it started, the popup's
/// very first resolution has every row's own icon, with nothing left for the
/// worker to do.
#[test]
fn the_launcher_has_its_icons_before_it_is_first_drawn() {
    /// A resolver over the real desk: it answers only what the desk has
    /// already been delivered and records everything else — exactly what the
    /// serve loop's own resolver does.
    struct Deferring(Rc<RefCell<ArtworkDesk>>);

    impl ArtworkResolver for Deferring {
        fn resolve(&mut self, key: &tairix_icon::ArtworkKey, side: u32) -> Resolved {
            self.0.borrow_mut().collect(key, side)
        }

        fn prefetch(&mut self, key: &tairix_icon::ArtworkKey, side: u32) {
            self.0.borrow_mut().want(key, side);
        }
    }

    NORMAL_PRESSURE.report(PressureBand::Normal);
    let assets = shipped_app_bundle_master(
        MemoryAssets::default().with("/Apps/one.app/Resources/icon.svg", &[BUNDLE_TINT]),
    );
    let desk = Rc::new(RefCell::new(ArtworkDesk::new()));
    let mut cat = Catalog::new();
    cat.insert(entry_with_icon("one", "One", Some("icon.svg")))
        .expect("fits");

    let mut comp = compositor();
    let mut shell = shell();
    shell.set_artwork_resolver(alloc::boxed::Box::new(Deferring(Rc::clone(&desk))));
    shell.set_library(&mut comp, cat);
    shell.warm_icon_artwork(&comp);

    // The desktop has the catalog, so the decoder is asked for the whole set
    // now — before the popup is opened or the strip is looked at.
    assert!(
        desk.borrow().has_work(),
        "knowing the catalog must start the decodes"
    );

    // The worker does its work, and the desktop asks again as each batch lands
    // — the serve loop's own round, since a tier that refuses is what makes the
    // next one wanted. It runs dry, which is what makes the wait finite.
    let mut reader = ArtworkFileReader(assets);
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());
    let mut decoded = 0;
    let mut rounds = 0;
    while desk.borrow().has_work() {
        while let Some(job) = {
            let taken = desk.borrow_mut().next_job();
            taken
        } {
            let artwork =
                tairix_icon::render_artwork(&mut reader, &mut rasteriser, &job.key, job.side);
            assert!(desk.borrow_mut().deliver(&job, artwork));
            decoded += 1;
            assert!(decoded < 64, "the warm-up must be a bounded set");
        }
        shell.warm_icon_artwork(&comp);
        rounds += 1;
        assert!(
            rounds < 8,
            "the warm-up must run dry, not chase its own tail"
        );
    }
    assert!(decoded > 0, "the warm-up asked for nothing at all");

    // Now the user opens the launcher. Its very first paint draws the
    // application's own icon, not a glyph it would replace a frame later.
    open_library_on(&mut shell, &mut comp);
    shell.present(&mut comp);

    let rows = shown_entry_rows(shell.session());
    assert_eq!(rows.len(), 1, "one entry, one row");
    assert_eq!(
        row_tint(shell.session(), rows[0]),
        Some(BUNDLE_TINT),
        "the first frame of the popup shows the application's own icon"
    );
    assert!(
        rounds <= 3,
        "one warm-up per tier at most, and there are three tiers"
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
        &mut InlineArtwork::new(&mut reader, &mut rasteriser),
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
        &mut InlineArtwork::new(&mut reader, &mut rasteriser),
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
    for index in 0..MAX_ENTRIES.min(96) {
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
        &mut InlineArtwork::new(&mut reader, &mut rasteriser),
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

    // The application strip: a slot whose bundle declares an icon nothing
    // will serve still exists, so the strip keeps it.
    let mut service = AppBarService::new();
    let strip = service.strip(&[(window_owner(1), TaskId(0))], |_| {
        Some(String::from("/Apps/one.app"))
    });
    let mut manifests = MemoryAssets::default();
    let slots = {
        let mut inline = InlineArtwork::new(&mut reader, &mut rasteriser);
        service.slots(&strip, &mut manifests, (&mut inline, &mut cache, 24))
    };
    assert_eq!(slots.len(), 1, "the slot is still shown");
    assert!(
        slots[0].artwork().is_none(),
        "with nothing to read there is no artwork — the slot draws its glyph"
    );

    session.taskbar_mut().tasks_mut().add(TaskId(1), "Editor");
    resolve_library_icons(
        session.taskbar_mut(),
        Scale::ONE,
        &mut InlineArtwork::new(&mut reader, &mut rasteriser),
        &mut cache,
    );
    for row in shown_entry_rows(&session) {
        assert_eq!(row_tint(&session, row), None, "row {row} has no artwork");
    }

    // Present the whole bar and popup twice: once through the seams that
    // can serve nothing, once through the do-nothing lookup. Identical
    // pixels means the empty-store desktop is exactly the glyph desktop,
    // and a bar drawn from glyphs is not a blank bar.
    session.taskbar_mut().set_apps(slots);
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let mut presenter = TaskbarPresenter::new();
    let mut inline = InlineArtwork::new(&mut reader, &mut rasteriser);
    let mut source = IconArtworkSource::new(&mut cache, &mut inline);
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

// --- The pinboard: the desktop layer and its context menu -----------------

use crate::desktop::Desktop;
use tairix_browse::GridView;
use tairix_wallpaper::{Backdrop, PinboardSettings, Rgb};
use tairix_window::WindowHost;
use tairix_wm::{Region, Window};

/// The row stride, in bytes, of the [`headless_desktop`] frame.
const FRAME_STRIDE: usize = 640 * 4;

/// A point on the headless screen clear of both the icon column and the bar.
const CLEAR_OF_EVERYTHING: (usize, usize) = (600, 200);

/// The desktop's own folder over the in-memory tree, already listed.
fn pinboard_desktop() -> Desktop<TreeSource> {
    let mut desktop = Desktop::new(TreeSource::fixture(), Vec::new());
    desktop.relist(0);
    desktop
}

/// The centre of the icon at `index`, in screen coordinates.
fn centre_of(layout: &GridView, index: usize) -> Point {
    let cell = layout.cell_rect(0, index).expect("a shown icon");
    Point::new(
        cell.left() + i32::try_from(cell.width / 2).unwrap_or(0),
        cell.top() + i32::try_from(cell.height / 2).unwrap_or(0),
    )
}

/// A point on the backdrop that no icon reaches: inside the margin the icon
/// column is inset by.
const EMPTY_BACKDROP: Point = Point::new(2, 2);

/// One desktop gesture, reporting the cells it changed into a damage sink.
/// The lifetime is spelled because each gesture borrows the layout it acts on.
type Gesture<'a> = dyn Fn(&mut Desktop<TreeSource>, &mut Region) + 'a;

/// `desktop` with its backdrop set to the flat colour `rgb`.
fn with_backdrop(desktop: &mut Desktop<TreeSource>, rgb: Rgb) {
    let base = desktop.settings().clone();
    let _ = desktop.apply_settings(PinboardSettings {
        backdrop: Backdrop::Colour(rgb),
        ..base
    });
}

/// The instant every test that is not about the crossfade itself installs a
/// wallpaper at.
const WALLPAPER_AT_NS: u64 = 4_000_000_000;

/// How long the desktop's own theme gives a backdrop change, in nanoseconds:
/// read from the theme so a retimed dissolve cannot leave a test measuring a
/// frame part-way through one.
fn backdrop_span_ns(shell: &DesktopShell) -> u64 {
    u64::from(
        shell
            .session()
            .active_theme()
            .motion()
            .duration(tairix_theme::MotionInteraction::BackdropChange),
    ) * 1_000_000
}

/// Install `paper` as the desktop's ground over `backdrop` and let the
/// crossfade arrive, so what follows measures the ground itself rather than a
/// frame part-way through the dissolve.
fn install_wallpaper(
    shell: &mut DesktopShell,
    backdrop: Backdrop,
    paper: Option<tairix_wm::Surface>,
) {
    shell.set_wallpaper(paper, backdrop, WALLPAPER_AT_NS);
    let span = backdrop_span_ns(shell);
    shell.advance_backdrop(WALLPAPER_AT_NS + span);
}

/// The composited pixel at (`x`, `y`) as its four raw frame bytes.
fn frame_pixel(comp: &Compositor, x: usize, y: usize) -> [u8; 4] {
    let start = y * FRAME_STRIDE + x * 4;
    let bytes = comp
        .frame()
        .get(start..start.saturating_add(4))
        .expect("a pixel inside the frame");
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

/// Whether any composited pixel inside `area` differs from `colour`.
fn any_pixel_differs(comp: &Compositor, area: Rect, colour: [u8; 4]) -> bool {
    let left = usize::try_from(area.left().max(0)).unwrap_or(0);
    let top = usize::try_from(area.top().max(0)).unwrap_or(0);
    let right = usize::try_from(area.right().max(0)).unwrap_or(0);
    let bottom = usize::try_from(area.bottom().max(0)).unwrap_or(0);
    for y in top..bottom {
        for x in left..right {
            if frame_pixel(comp, x, y) != colour {
                return true;
            }
        }
    }
    false
}

#[test]
fn the_desktop_layer_paints_the_backdrop_colour_the_settings_name_under_the_icons() {
    let (mut shell, mut comp) = headless_desktop();
    let mut desktop = pinboard_desktop();
    with_backdrop(&mut desktop, Rgb::new(10, 20, 30));

    shell.present_desktop(&mut comp, &desktop);
    comp.composite();

    let (x, y) = CLEAR_OF_EVERYTHING;
    let empty = frame_pixel(&comp, x, y);
    assert_eq!(empty[3], 255, "the desktop layer is opaque");
    assert!(
        empty.contains(&10) && empty.contains(&20) && empty.contains(&30),
        "the layer shows the colour the settings name, got {empty:?}"
    );

    let layout = shell.desktop_layout(&comp, &desktop);
    let cell = layout.cell_rect(0, 0).expect("a shown icon");
    assert!(
        any_pixel_differs(&comp, cell, empty),
        "the icons are drawn over the backdrop"
    );
}

#[test]
fn the_desktop_layer_paints_the_wallpaper_when_one_is_set() {
    let (mut shell, mut comp) = headless_desktop();
    let mut desktop = pinboard_desktop();
    with_backdrop(&mut desktop, Rgb::new(10, 20, 30));
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    let (x, y) = CLEAR_OF_EVERYTHING;
    let without = frame_pixel(&comp, x, y);

    let mut paper = Surface::new(640, 480).expect("a screen-sized wallpaper");
    paper.fill_rect(0, 0, 640, 480, Color::rgb(200, 100, 50));
    install_wallpaper(&mut shell, desktop.settings().backdrop, Some(paper));
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();

    let with = frame_pixel(&comp, x, y);
    assert_ne!(
        with, without,
        "the wallpaper is the layer's base, not the backdrop colour"
    );
    assert!(
        with.contains(&200) && with.contains(&100) && with.contains(&50),
        "the layer shows the wallpaper's own pixels, got {with:?}"
    );
    let layout = shell.desktop_layout(&comp, &desktop);
    let cell = layout.cell_rect(0, 0).expect("a shown icon");
    assert!(
        any_pixel_differs(&comp, cell, with),
        "the icons are drawn over the wallpaper"
    );

    install_wallpaper(&mut shell, desktop.settings().backdrop, None);
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    assert_eq!(
        frame_pixel(&comp, x, y),
        without,
        "taking the wallpaper away brings the backdrop colour back"
    );
}

/// A wallpaper arriving over the plain backdrop colour dissolves into it: the
/// frame at the instant it is installed still shows the colour, a frame
/// part-way through is the mix of the two, and the picture only stands alone
/// once the fade has arrived.
///
/// The whole screen changing between two frames is the one change on a desktop
/// nobody can miss, which is why the login wallpaper fades up rather than
/// appearing the moment the worker hands it over.
#[test]
fn a_wallpaper_arriving_over_the_backdrop_colour_fades_up_into_it() {
    let (mut shell, mut comp) = headless_desktop();
    let mut desktop = pinboard_desktop();
    with_backdrop(&mut desktop, Rgb::new(10, 20, 30));
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    let (x, y) = CLEAR_OF_EVERYTHING;
    let colour_alone = frame_pixel(&comp, x, y);

    let mut paper = Surface::new(640, 480).expect("a screen-sized wallpaper");
    paper.fill_rect(0, 0, 640, 480, Color::rgb(200, 100, 50));
    shell.set_wallpaper(Some(paper), desktop.settings().backdrop, WALLPAPER_AT_NS);
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    assert_eq!(
        frame_pixel(&comp, x, y),
        colour_alone,
        "the picture is invisible at the instant it is installed, so nothing jumps"
    );

    let span = backdrop_span_ns(&shell);
    assert!(shell.advance_backdrop(WALLPAPER_AT_NS + span / 2));
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    let midway = frame_pixel(&comp, x, y);
    for channel in 0..3 {
        let (from, to) = (colour_alone[channel], [200, 100, 50][channel]);
        let between = midway[channel] > from.min(to) && midway[channel] < from.max(to);
        assert!(
            between,
            "channel {channel} reads {} part-way from {from} to {to}",
            midway[channel]
        );
    }

    assert!(shell.advance_backdrop(WALLPAPER_AT_NS + span));
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    let arrived = frame_pixel(&comp, x, y);
    assert!(
        arrived.contains(&200) && arrived.contains(&100) && arrived.contains(&50),
        "the picture stands alone once the fade has arrived, got {arrived:?}"
    );
    assert!(shell.backdrop_settled(), "and nothing more is owed");
}

/// One wallpaper replacing another crossfades: a frame part-way through is the
/// mix of the two pictures, and neither is what is on screen on its own.
///
/// Including in the margins of the outgoing picture, where the ground being
/// left was the backdrop colour — that is what the flattened copy of the
/// outgoing ground is for, and a fade that skipped it would snap those margins
/// to the arriving picture while the rest dissolved.
#[test]
fn one_wallpaper_replacing_another_crossfades_margins_and_all() {
    let (mut shell, mut comp) = headless_desktop();
    let mut desktop = pinboard_desktop();
    with_backdrop(&mut desktop, Rgb::new(10, 20, 30));

    // The outgoing picture is pillarboxed: it covers the screen as far as the
    // column below and leaves the backdrop colour showing beyond it, which is
    // where the margin is sampled.
    let mut leaving = Surface::new(640, 480).expect("a screen-sized wallpaper");
    leaving.fill_rect(0, 0, 500, 480, Color::rgb(200, 0, 0));
    install_wallpaper(&mut shell, desktop.settings().backdrop, Some(leaving));
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    let (x, y) = CLEAR_OF_EVERYTHING;
    let margin_before = frame_pixel(&comp, x, y);
    assert!(
        margin_before.contains(&10) && margin_before.contains(&20) && margin_before.contains(&30),
        "the margin starts on the backdrop colour, got {margin_before:?}"
    );

    // The arriving one covers the whole screen, so both the picture and its
    // margins have somewhere to dissolve to.
    let mut arriving = Surface::new(640, 480).expect("a screen-sized wallpaper");
    arriving.fill_rect(0, 0, 640, 480, Color::rgb(0, 0, 200));
    shell.set_wallpaper(Some(arriving), desktop.settings().backdrop, WALLPAPER_AT_NS);
    let span = backdrop_span_ns(&shell);
    shell.advance_backdrop(WALLPAPER_AT_NS + span / 2);
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();

    let covered = frame_pixel(&comp, 300, 200);
    assert!(
        covered[0] > 0 && covered[2] > 0,
        "where both pictures reach is the mix of the two, got {covered:?}"
    );
    let margin = frame_pixel(&comp, x, y);
    assert!(
        margin[2] > margin_before[2] && margin[2] < 200,
        "the margin dissolves from the colour to the arriving picture, got {margin:?}"
    );

    shell.advance_backdrop(WALLPAPER_AT_NS + span);
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();
    let arrived = frame_pixel(&comp, x, y);
    assert!(
        arrived[2] > 190 && arrived[0] < 10,
        "the arriving picture stands alone everywhere, got {arrived:?}"
    );
}

/// A wallpaper that does not cover the screen — a centred or letterboxed
/// picture, delivered as a screen-sized surface with transparent margins —
/// shows the backdrop colour the settings name in the margins it leaves.
///
/// The layer is painted into the same buffer every frame, so a margin that
/// took no paint would keep whatever the previous frame drew there: an icon
/// that has since moved, or nothing at all.
#[test]
fn a_wallpaper_that_does_not_cover_the_screen_shows_the_backdrop_in_its_margins() {
    let (mut shell, mut comp) = headless_desktop();
    let mut desktop = pinboard_desktop();
    with_backdrop(&mut desktop, Rgb::new(10, 20, 30));

    // The sandbox draws an under-sized placement into a screen-sized canvas
    // and leaves every pixel outside it fully transparent.
    let mut paper = Surface::new(640, 480).expect("a screen-sized wallpaper");
    paper.fill_rect(220, 140, 200, 200, Color::rgb(200, 100, 50));
    install_wallpaper(&mut shell, desktop.settings().backdrop, Some(paper));
    shell.present_desktop(&mut comp, &desktop);
    comp.composite();

    let inside = frame_pixel(&comp, 300, 200);
    assert!(
        inside.contains(&200) && inside.contains(&100) && inside.contains(&50),
        "the picture itself is drawn, got {inside:?}"
    );
    let (x, y) = CLEAR_OF_EVERYTHING;
    let margin = frame_pixel(&comp, x, y);
    assert_eq!(margin[3], 255, "the layer is opaque everywhere");
    assert!(
        margin.contains(&10) && margin.contains(&20) && margin.contains(&30),
        "the margin shows the chosen backdrop, not the root fill, got {margin:?}"
    );
}

/// A window over the desktop, translucent and backdrop-blurred: a terminal on
/// frosted glass, which is what makes a needless desktop repaint expensive.
fn frosted_window(shell: &mut DesktopShell, comp: &mut Compositor) -> WindowId {
    let window = shell
        .open_window(comp, Point::new(200, 60), app_surface(), "Terminal")
        .expect("opens");
    assert!(comp.set_opacity(window, 128));
    assert!(comp.set_backdrop_blur(window, 8));
    window
}

/// Clicking between a window and the wallpaper moves the keyboard, which
/// moves the desktop's Focus Ring — one icon's worth of pixels.
///
/// It must cost that. The desktop is the bottom layer, so repainting it whole
/// marks the whole screen: every window above it recomposites and every
/// frosted backdrop over it is thrown away and blurred again. On a 1080p
/// screen that is most of a megapixel of blur per click, which is felt as the
/// pointer freezing.
#[test]
fn moving_focus_between_a_window_and_the_desktop_repaints_one_icon() {
    let (mut shell, mut comp) = headless_desktop();
    let mut desktop = pinboard_desktop();
    with_backdrop(&mut desktop, Rgb::new(10, 20, 30));
    let window = frosted_window(&mut shell, &mut comp);
    shell.present_desktop(&mut comp, &desktop);
    let layout = shell.desktop_layout(&comp, &desktop);
    let cell = layout.cell_rect(0, 1).expect("a shown icon");

    // An icon is selected and the desktop holds the keyboard, exactly as it
    // does before the user clicks into the terminal.
    let mut damage = Region::new();
    desktop.press(centre_of(&layout, 1), &layout, 0, &[], &mut damage);
    shell.present_desktop_area(&mut comp, &desktop, &damage);
    comp.composite();
    assert!(!comp.has_damage(), "the opening frames have been drained");
    assert_eq!(desktop.selected(), Some(1));

    // The click into the terminal: the window takes the keyboard, so the ring
    // leaves the selected icon.
    damage.clear();
    desktop.set_focused(false, &layout, &mut damage);
    assert_eq!(damage.rects(), [cell], "the ring's own cell, nothing more");
    shell.present_desktop_area(&mut comp, &desktop, &damage);
    let composed = comp.composite();
    assert_eq!(composed.rects(), [cell], "the frame recomposed one cell");
    assert_eq!(
        comp.frame_stats().blur_px,
        0,
        "the window's frosted backdrop was kept, not blurred again"
    );
    assert!(
        comp.frame_stats().damaged_px < 640 * 480 / 10,
        "a focus click must not cost a screen, got {}",
        comp.frame_stats().damaged_px
    );

    // And the click back onto the wallpaper, which brings the ring back.
    damage.clear();
    desktop.set_focused(true, &layout, &mut damage);
    assert_eq!(damage.rects(), [cell]);
    shell.present_desktop_area(&mut comp, &desktop, &damage);
    comp.composite();
    assert_eq!(comp.frame_stats().blur_px, 0, "still no re-blur");

    // With nothing selected there is no ring to move, so the same click
    // changes no pixel at all and asks for no frame.
    damage.clear();
    desktop.press(EMPTY_BACKDROP, &layout, 1, &[], &mut damage);
    shell.present_desktop_area(&mut comp, &desktop, &damage);
    comp.composite();
    assert_eq!(desktop.selected(), None);
    damage.clear();
    desktop.set_focused(false, &layout, &mut damage);
    assert!(damage.is_empty(), "no selection, no ring, no repaint");
    shell.present_desktop_area(&mut comp, &desktop, &damage);
    assert!(!comp.has_damage(), "and no frame to compose");
    assert_eq!(comp.window(window).map(Window::opacity), Some(128));
}

/// Repainting part of the desktop layer must produce the very pixels a whole
/// repaint would have: the same backdrop, the same wallpaper over it, and the
/// same icons over that.
///
/// Two identical screens are driven through the same gestures, one presenting
/// the cells the model reported and the other presenting the whole layer, and
/// their scan-out is compared byte for byte — so a partial paint that forgot
/// the wallpaper, mis-clipped a tile, or left a stale highlight behind cannot
/// pass as the cheaper path.
#[test]
fn a_partial_desktop_repaint_draws_what_a_whole_one_would() {
    let mut cheap = headless_desktop();
    let mut whole = headless_desktop();
    // Striped on both axes, at periods no cell origin is a multiple of: the
    // partial paint draws the wallpaper at its true screen position and
    // writes only inside the cell, so a picture shifted by even a few pixels
    // would show here where a flat one could not.
    let mut paper = Surface::new(640, 480).expect("a screen-sized wallpaper");
    for row in 0..480 / 8 {
        let shade = u8::try_from(row * 3 % 200).unwrap_or(0);
        paper.fill_rect(0, row * 8, 640, 4, Color::rgb(shade, 100, 200 - shade));
    }
    for column in 0..640 / 16 {
        let shade = u8::try_from(column * 5 % 200).unwrap_or(0);
        paper.fill_rect(column * 16, 0, 8, 480, Color::rgb(40, shade, 120));
    }
    for (shell, comp) in [&mut cheap, &mut whole] {
        install_wallpaper(
            shell,
            Backdrop::Colour(Rgb::new(10, 20, 30)),
            Some(paper.clone()),
        );
        frosted_window(shell, comp);
    }
    let mut desktops = [pinboard_desktop(), pinboard_desktop()];
    for desktop in &mut desktops {
        with_backdrop(desktop, Rgb::new(10, 20, 30));
    }
    let layout = cheap.0.desktop_layout(&cheap.1, &desktops[0]);

    // Every gesture the pointer and keyboard produce over the icon column,
    // each of which reports its own cells.
    let first = centre_of(&layout, 0);
    let second = centre_of(&layout, 1);
    let gestures: [&Gesture<'_>; 6] = [
        &|d, dmg| {
            d.pointer_moved(first, &layout, 0, dmg);
        },
        &|d, dmg| {
            d.pointer_moved(second, &layout, 1, dmg);
        },
        &|d, dmg| {
            d.press(second, &layout, 2, &[], dmg);
        },
        &|d, dmg| d.set_focused(false, &layout, dmg),
        &|d, dmg| d.set_focused(true, &layout, dmg),
        &|d, dmg| {
            d.pointer_left(&layout, dmg);
        },
    ];
    for (step, gesture) in gestures.iter().enumerate() {
        let mut damage = Region::new();
        gesture(&mut desktops[0], &mut damage);
        cheap
            .0
            .present_desktop_area(&mut cheap.1, &desktops[0], &damage);
        cheap.1.composite();

        gesture(&mut desktops[1], &mut Region::new());
        whole.0.present_desktop(&mut whole.1, &desktops[1]);
        whole.1.composite();

        assert_eq!(
            cheap.1.frame(),
            whole.1.frame(),
            "the cheap path drew a different screen at step {step}"
        );
    }
}

/// The desktop's own menu is a chain like every other, so the shell draws it
/// as compositor windows it reconciles against the chain's own list — and the
/// backdrop menu keeps no window of its own to be left behind.
#[test]
fn the_backdrop_menu_is_drawn_as_chain_plates_and_taken_down_when_it_closes() {
    let (mut shell, mut comp) = headless_desktop();
    shell.present(&mut comp);
    let before = comp.window_count();
    let mut chain = MenuChain::new();
    let theme = shell.session().floating_theme().clone();
    let (screen, scale, epoch) = (comp.screen_rect(), comp.scale(), comp.chrome_epoch());
    let geom = ChainGeometry {
        screen,
        scale,
        theme: &theme,
        epoch,
    };

    assert!(shell.present_menu_chain(&mut comp, &chain, None));
    assert_eq!(
        comp.window_count(),
        before,
        "a seat with no chain places nothing"
    );

    let at = Point::new(100, 100);
    chain
        .open(
            ChainOwner::Backdrop,
            crate::pinboard::model(true, &PinboardSettings::default()),
            crate::windows::window_menu_placement(Rect::new(at.x, at.y, 0, 0)),
            &geom,
        )
        .expect("the backdrop model opens");
    assert!(shell.present_menu_chain(&mut comp, &chain, None));
    assert_eq!(comp.window_count(), before + 1, "one plate, one window");
    let plate = chain.row_rect(0, 0, &geom).expect("the plate has a row");
    assert!(
        plate.left() >= at.x && plate.top() >= at.y,
        "the plate opens at the pointer, got {plate:?}"
    );

    assert!(shell.present_menu_chain(&mut comp, &chain, None));
    assert_eq!(
        comp.window_count(),
        before + 1,
        "re-presenting reuses the plate's window"
    );

    // A menu opened in the far corner is placed wholly on screen.
    chain
        .open(
            ChainOwner::Backdrop,
            crate::pinboard::model(false, &PinboardSettings::default()),
            crate::windows::window_menu_placement(Rect::new(
                screen.right() - 1,
                screen.bottom() - 1,
                0,
                0,
            )),
            &geom,
        )
        .expect("the backdrop model opens");
    assert!(shell.present_menu_chain(&mut comp, &chain, None));
    assert_eq!(comp.window_count(), before + 1);
    let corner = chain.surfaces().first().expect("the root plate").rect;
    assert!(
        corner.right() <= screen.right() && corner.bottom() <= screen.bottom(),
        "a menu opened in the corner is clamped wholly on screen, got {corner:?}"
    );

    assert!(chain.dismiss());
    assert!(shell.present_menu_chain(&mut comp, &chain, None));
    assert_eq!(
        comp.window_count(),
        before,
        "dismissing the chain takes its plates down"
    );
}

/// The backdrop blur `window` asks the compositor for.
fn blur_of(comp: &Compositor, window: Option<WindowId>, what: &str) -> u16 {
    let id = window.unwrap_or_else(|| panic!("{what} is on screen"));
    comp.window(id)
        .expect("a placed window is live")
        .blur_radius()
}

/// The blur the active theme asks for behind its floating chrome — read through
/// the production rule, so a test cannot assert a frosting the desktop does not
/// ask for, and guarded against zero so no assertion below is vacuous.
fn chrome_blur(shell: &DesktopShell) -> u16 {
    let blur = crate::presenter::chrome_blur(shell.session().floating_theme());
    assert!(blur > 0, "the theme asks for no frosting at all");
    blur
}

/// Every surface the taskbar puts on screen is floating chrome: it is drawn
/// on the theme's translucent fill, which reads as frosted glass only if the
/// compositor blurs what is behind it, so each asks for that as it is placed.
#[test]
fn the_bar_and_its_library_popup_frost_what_is_behind_them() {
    let mut shell = shell();
    let mut comp = compositor();
    shell.present(&mut comp);
    let blur = chrome_blur(&shell);
    assert_eq!(
        blur_of(&comp, shell.presenter().bar_window(), "the bar"),
        blur
    );

    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(office_and_games());
    let library = centre(shell.session().taskbar().layout(Scale::ONE).library);
    shell.handle(moved(library.x, library.y), &mut comp, 0);
    shell.handle(PRIMARY_PRESS, &mut comp, 0);
    assert_eq!(
        blur_of(&comp, shell.presenter().popup_window(), "the library popup"),
        blur
    );
}

/// The bar's popover surfaces are the same chrome. Each is opened the way the
/// desktop opens it, on its own shell: the ones that are modal would otherwise
/// swallow the input that raises the next.
#[test]
fn the_bars_popover_and_readout_frost_what_is_behind_them() {
    let mut hovered = shell();
    let mut comp = compositor();
    let blur = chrome_blur(&hovered);
    hovered.present(&mut comp);
    let capsule = capsule_point(&hovered);
    hovered.handle(moved(capsule.x, capsule.y), &mut comp, 0);
    assert_eq!(
        blur_of(&comp, hovered.presenter().readout_window(), "the readout"),
        blur
    );

    let mut notified = shell();
    let mut comp = compositor();
    notified.apply_notify(
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
        blur_of(
            &comp,
            notified.presenter().notifications_window(),
            "the notification popover"
        ),
        blur
    );
}

/// A menu plate is the desktop's floating chrome like the bar and its popups:
/// it is translucent, so it asks the compositor to frost what it opens over —
/// every surface of the chain, the information panel included.
///
/// The chain is reconciled surface by surface and its windows are obtained
/// three ways (created standalone, re-surfaced on a later present, and — in the
/// sibling test — created as an owner's transient), so a look applied on only
/// one of those paths would leave a surface sharp for as long as it lived.
#[test]
fn every_menu_chain_surface_frosts_what_is_behind_it() {
    let (mut shell, mut comp) = headless_desktop();
    let blur = chrome_blur(&shell);
    let theme = shell.session().floating_theme().clone();
    let geom = ChainGeometry {
        screen: comp.screen_rect(),
        scale: comp.scale(),
        theme: &theme,
        epoch: comp.chrome_epoch(),
    };
    let facts = FactList::new(vec![Fact::new("Name", "App")]);
    let mut wire = app_bar(false).menu;
    wire.push(AppMenuRow::Info)
        .expect("an information row fits");
    let mut chain = MenuChain::new();
    chain
        .open(
            ChainOwner::Backdrop,
            ChainModel::from_app_menu("App", &wire, Some(&facts)),
            crate::windows::window_menu_placement(Rect::new(100, 100, 0, 0)),
            &geom,
        )
        .expect("an attested model opens");
    // Hover the information row so the panel hangs: the chain then lists a
    // plate and a panel, which are painted by two different arms.
    let info = chain.row_rect(0, 1, &geom).expect("the information row");
    let at = Point::new(
        info.left() + i32::try_from(info.width / 2).expect("small"),
        info.top() + i32::try_from(info.height / 2).expect("small"),
    );
    chain.handle(&moved(at.x, at.y), at, &geom);
    assert!(chain.info_panel().is_some(), "the panel hangs");

    // Once to create the windows, again to re-surface the ones it kept.
    for round in ["created", "re-used"] {
        assert!(shell.present_menu_chain(&mut comp, &chain, None));
        let surfaces = chain.surfaces();
        assert!(
            surfaces
                .iter()
                .any(|placed| placed.kind == SurfaceKind::Info),
            "the panel is one of the surfaces presented"
        );
        for placed in surfaces {
            let id = comp
                .window_at(centre(placed.rect))
                .expect("a chain surface is the window over its own centre");
            assert_eq!(
                comp.window(id).expect("live").blur_radius(),
                blur,
                "{round}: a translucent {:?} over a sharp backdrop has its text on detail",
                placed.kind
            );
        }
    }
}

/// A plate the chain hangs on an owner window is composited as that window's
/// transient, which is a different path to a compositor window — and it is the
/// same chrome.
#[test]
fn an_owned_menu_plate_frosts_what_is_behind_it() {
    let (mut shell, mut comp) = headless_desktop();
    let blur = chrome_blur(&shell);
    let owner = comp.add_window(
        Point::new(0, 0),
        Surface::new(400, 300).expect("an owner surface"),
    );
    let mut chain = MenuChain::new();
    let geom = crate::windows::chain_geometry(shell.session(), &comp);
    chain
        .open(
            ChainOwner::Backdrop,
            crate::pinboard::model(true, &PinboardSettings::default()),
            crate::windows::window_menu_placement(Rect::new(100, 100, 0, 0)),
            &geom,
        )
        .expect("the backdrop model opens");
    assert!(shell.present_menu_chain(&mut comp, &chain, Some(owner)));
    let plate = chain.surfaces().first().expect("the root plate").rect;
    let id = comp
        .window_at(centre(plate))
        .expect("the plate is the window over its own centre");
    assert_ne!(id, owner, "the plate is its own window over the owner");
    assert_eq!(comp.window(id).expect("live").blur_radius(), blur);
}

/// The bar and every menu plate ground themselves in **one** floating theme the
/// session derives, and a theme switch moves that one value — so no surface can
/// be left on the ground it had before, and a plate's pixels and the row
/// rectangles it is hit-tested against cannot come from two themes.
#[test]
fn one_floating_theme_grounds_the_bar_and_every_plate() {
    let mut shell = shell();
    let comp = compositor();
    for appearance in [Appearance::Light, Appearance::Dark] {
        shell.session_mut().set_appearance(appearance);
        let session = shell.session();
        let floating = session.floating_theme();
        assert_eq!(
            floating.ground(),
            SurfaceGround::Floating,
            "the desktop's chrome theme is the floating one"
        );
        assert_eq!(
            floating.appearance(),
            appearance,
            "and it followed the switch"
        );
        assert_eq!(
            session.taskbar().theme(),
            floating,
            "the bar grounds itself in it"
        );
        assert_eq!(
            crate::windows::chain_geometry(session, &comp).theme,
            floating,
            "and so does every plate"
        );
    }
}

/// The bar stands clear of the screen edges it faces, and the band it
/// reserves runs from its own edge to its inner side. A maximized window
/// therefore never covers the bar, and never claims the wallpaper gap behind
/// it either — that gap can only be reached through the bar.
#[test]
fn a_floating_bar_reserves_the_whole_band_from_its_screen_edge() {
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let shell = shell_for(TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1920, 1080)
        });
        let comp = compositor();
        let screen = comp.screen_rect();
        let bar = shell.session().taskbar().layout(comp.scale()).bar;
        let area = shell.work_area(&comp);

        let gap = match edge {
            Edge::Top => bar.top() - screen.top(),
            Edge::Bottom => screen.bottom() - bar.bottom(),
            Edge::Left => bar.left() - screen.left(),
            Edge::Right => screen.right() - bar.right(),
        };
        assert!(
            gap > 0,
            "{edge:?}: the bar hugs its screen edge, so nothing below tests a gap"
        );

        // Toward the screen edge from the bar is the wallpaper gap; away from
        // it is the first row or column a window may have.
        let (in_gap, in_bar, past_bar) = match edge {
            Edge::Top => (
                Point::new(bar.left(), screen.top()),
                Point::new(bar.left(), bar.top()),
                Point::new(bar.left(), bar.bottom()),
            ),
            Edge::Bottom => (
                Point::new(bar.left(), screen.bottom() - 1),
                Point::new(bar.left(), bar.bottom() - 1),
                Point::new(bar.left(), bar.top() - 1),
            ),
            Edge::Left => (
                Point::new(screen.left(), bar.top()),
                Point::new(bar.left(), bar.top()),
                Point::new(bar.right(), bar.top()),
            ),
            Edge::Right => (
                Point::new(screen.right() - 1, bar.top()),
                Point::new(bar.right() - 1, bar.top()),
                Point::new(bar.left() - 1, bar.top()),
            ),
        };
        assert!(
            !area.contains(in_gap),
            "{edge:?}: the wallpaper gap {in_gap:?} was handed to a window ({area:?})"
        );
        assert!(
            !area.contains(in_bar),
            "{edge:?}: a maximized window would cover the bar ({area:?} over {bar:?})"
        );
        assert!(
            area.contains(past_bar),
            "{edge:?}: the row past the bar {past_bar:?} is usable ({area:?})"
        );
        assert!(
            area.width <= screen.width && area.height <= screen.height,
            "{edge:?}: the work area left the screen"
        );
    }
}

/// Every session-owned surface reads the compositor's density rather than
/// hard-coding its own, so a change to it is reflected in what each one
/// lays out at its next repaint — not just that a repaint ran.
#[test]
fn session_surfaces_adapt_to_display_scale() {
    let (mut shell, mut comp) = headless_desktop();
    let scale_200 = Scale::from_percent(200).expect("200% is in range");

    // 1. The confirmation prompt's window grows with the density: its
    // fixed logical extents are scaled at paint time.
    let mut confirm = ConfirmPrompt::new();
    assert!(confirm.ask(PowerAction::Restart, &mut shell, &mut comp));
    let confirm_id = confirm.wm_id().expect("confirm window exists");
    let size_100 = window_rect(&comp, confirm_id);

    assert!(comp.set_scale(scale_200));
    confirm.repaint(&mut shell, &mut comp);
    let size_200 = window_rect(&comp, confirm_id);
    assert_eq!(
        size_200.width,
        scale_200.scale_length(crate::confirm::WIN_WIDTH)
    );
    assert_eq!(
        size_200.height,
        scale_200.scale_length(crate::confirm::WIN_HEIGHT)
    );
    assert!(size_200.width > size_100.width);

    // 2. The picker's window grows the same way, at the browser-view
    // extent it shares with the file manager.
    let (mut shell2, mut comp2) = headless_desktop();
    assert!(comp2.set_scale(scale_200));
    let mut picker = SessionPicker::new(TreeSource::fixture);
    assert!(picker.begin(1, &mut shell2, &mut comp2).is_ok());
    let picker_id = picker.wm_id().expect("picker window exists");
    let p_size_200 = window_rect(&comp2, picker_id);
    assert_eq!(
        p_size_200.width,
        scale_200.scale_length(tairix_browse::WIN_WIDTH)
    );
    assert_eq!(
        p_size_200.height,
        scale_200.scale_length(tairix_browse::WIN_HEIGHT)
    );

    // 3. The lock's window is always the whole screen, so the density
    // shows up in what gets laid out *inside* it instead. Asserted on the
    // laid-out extent and on the whole rendered surface rather than on one
    // chosen coordinate, which a later layout change could silently make
    // meaningless.
    let mut lock = ScreenLock::new();
    assert!(lock.engage("user", &shell, &mut comp));
    let lock_id = locked_window(&comp);
    let screen = comp.screen_rect();

    let block_100 = tairix_greeter::panel_rect(screen, Scale::ONE);
    let block_200 = tairix_greeter::panel_rect(screen, scale_200);
    assert!(
        block_200.width > block_100.width && block_200.height > block_100.height,
        "the prompt block is authored in logical pixels, so it grows with the density"
    );

    assert!(comp.set_scale(Scale::ONE));
    lock.repaint(&shell, &mut comp);
    let frame_100 = lock_surface(&comp, lock_id);

    assert!(comp.set_scale(scale_200));
    lock.repaint(&shell, &mut comp);
    let frame_200 = lock_surface(&comp, lock_id);

    assert_ne!(
        frame_100, frame_200,
        "the density reaches the pixels the lock actually paints"
    );
}

/// Every pixel `id`'s retained content holds, so a test can compare two
/// renders whole instead of guessing a coordinate that discriminates them.
fn lock_surface(comp: &Compositor, id: WindowId) -> Vec<tairix_raster::Pixel> {
    let content = comp
        .window(id)
        .expect("the window is presented")
        .content()
        .expect("content is retained");
    let width = content.width();
    (0..content.height())
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| content.get(x, y).expect("the walk stays on-surface"))
        .collect()
}

/// `desktop_info` answers with the compositor's own screen rectangle,
/// density, and active appearance — the same three facts
/// [`tairix_window::WindowHost::desktop`] hands back to an application,
/// since [`ShellWindowHost`] answers it by delegating to this very
/// function.
#[test]
fn desktop_info_reports_compositor_state() {
    let mut comp = compositor();
    let scale_200 = Scale::from_percent(200).expect("200% is in range");
    assert!(comp.set_scale(scale_200));

    let info = desktop_info(&comp).expect("info exists");
    assert_eq!(info.scale_percent(), 200);
    assert_eq!(info.screen_width_px(), 1920);
    assert_eq!(info.screen_height_px(), 1080);
    assert_eq!(info.appearance(), Appearance::Dark);

    let mut shell = shell();
    let mut picker = SessionPicker::new(TreeSource::fixture);
    let mut apps = AppBarService::new();
    let mut windows = SessionWindows::new();
    let mut host = ShellWindowHost {
        shell: &mut shell,
        compositor: &mut comp,
        windows: &mut windows,
        picker: &mut picker,
        apps: &mut apps,
        menu: &mut MenuChain::new(),
        seat_held: false,
    };

    // What an application is actually handed, whole: the record is one
    // value, so comparing it field by field could miss a field added
    // later.
    assert_eq!(WindowHost::desktop(&mut host), Ok(info));
}

// --- App-owned popup surfaces ---------------------------------------------

/// A resizable window declaring no minimum client extent of its own, so a
/// drag is bounded by the window manager's furniture floor alone.
fn resizable_sizing() -> WindowSizing {
    WindowSizing::Resizable {
        min_width_px: 0,
        min_height_px: 0,
    }
}

/// A frame geometry of `width`×`height` in the compositor's own format —
/// what the window engine hands the host for a create or a popup.
fn served_mode(width: u32, height: u32) -> DisplayMode {
    DisplayMode {
        width_px: width,
        height_px: height,
        stride_bytes: width * 4,
        format: DisplayFormat::Rgba8888,
    }
}

/// Run `body` against a [`ShellWindowHost`] built over `shell`, `comp` and
/// `windows`, so a test drives the very bridge the serve loop drives.
fn with_window_host<R>(
    shell: &mut DesktopShell,
    comp: &mut Compositor,
    windows: &mut SessionWindows,
    body: impl FnOnce(&mut ShellWindowHost<'_>) -> R,
) -> R {
    let mut picker = SessionPicker::new(TreeSource::fixture);
    let mut apps = AppBarService::new();
    let mut host = ShellWindowHost {
        shell,
        compositor: comp,
        windows,
        picker: &mut picker,
        apps: &mut apps,
        menu: &mut MenuChain::new(),
        seat_held: false,
    };
    body(&mut host)
}

/// A served parent window (channel id 1) plus a popup (channel id 2) at
/// `offset` from the parent's client origin, opened exactly as the serve
/// loop opens them.
fn open_parent_and_popup(
    shell: &mut DesktopShell,
    comp: &mut Compositor,
    windows: &mut SessionWindows,
    offset: (i32, i32),
    popup: (u32, u32),
) -> (WindowId, WindowId) {
    with_window_host(shell, comp, windows, |host| {
        assert_eq!(
            host.window_opened(
                window_owner(1),
                1,
                &served_mode(320, 240),
                "Terminal",
                resizable_sizing(),
            ),
            Ok(())
        );
        assert_eq!(
            host.popup_opened(2, 1, offset.0, offset.1, &served_mode(popup.0, popup.1)),
            Ok(())
        );
    });
    let parent = windows.wm_id(1).expect("the parent window is live");
    let popup = windows.wm_id(2).expect("the popup window is live");
    (parent, popup)
}

#[test]
fn a_popup_opens_undecorated_over_its_parent_and_off_the_taskbar() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut windows = SessionWindows::new();

    let (parent, popup) =
        open_parent_and_popup(&mut shell, &mut comp, &mut windows, (10, 20), (100, 80));

    // Placed against the parent's *client* origin, which is what the app's
    // own coordinates are relative to — never the decorated outer bounds.
    let client = comp.window_client_rect(parent).expect("parent client");
    let bounds = comp.window(popup).expect("popup is live").bounds();
    assert_eq!(
        (bounds.left(), bounds.top()),
        (client.left() + 10, client.top() + 20)
    );
    assert_eq!((bounds.width, bounds.height), (100, 80));

    // Undecorated: no frame furniture at all, so the whole window is client.
    assert!(comp.window_frame(popup).is_none(), "a popup wears no frame");
    assert_eq!(
        comp.window_client_rect(popup),
        Some(bounds),
        "an undecorated popup reserves no band around its content"
    );

    // Not a task: the bar lists the parent alone.
    assert_eq!(shell.session().taskbar().tasks().len(), 1);
    assert!(shell.tasks().task_for(popup).is_none());

    // Above its parent, and routed back to the app under its own id.
    assert_eq!(
        comp.window_at(Point::new(bounds.left() + 1, bounds.top() + 1)),
        Some(popup)
    );
    assert_eq!(windows.ipc_id(popup), Some(2));
}

#[test]
fn a_popup_is_clamped_wholly_onto_the_screen() {
    let (mut shell, mut comp) = headless_desktop();
    let mut windows = SessionWindows::new();
    let screen = comp.screen_rect();

    // An offset that would hang the popup off the bottom-right corner.
    let (_, popup) = open_parent_and_popup(
        &mut shell,
        &mut comp,
        &mut windows,
        (screen.width.cast_signed(), screen.height.cast_signed()),
        (200, 100),
    );

    let bounds = comp.window(popup).expect("popup is live").bounds();
    assert_eq!(
        (bounds.left(), bounds.top()),
        (screen.right() - 200, screen.bottom() - 100),
        "the whole popup is pulled back onto the screen"
    );
}

#[test]
fn a_popup_for_an_unknown_parent_is_refused() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut windows = SessionWindows::new();

    let before = comp.window_count();
    with_window_host(&mut shell, &mut comp, &mut windows, |host| {
        assert_eq!(
            host.popup_opened(2, 99, 0, 0, &served_mode(100, 80)),
            Err(Errno::NotFound),
            "a parent the session has no window for places nothing"
        );
    });

    assert!(windows.wm_id(2).is_none(), "no record was committed");
    assert_eq!(comp.window_count(), before, "no window was opened");
}

#[test]
fn closing_a_popup_leaves_its_parent_and_its_task_alone() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut windows = SessionWindows::new();
    let (parent, popup) =
        open_parent_and_popup(&mut shell, &mut comp, &mut windows, (10, 20), (100, 80));

    with_window_host(&mut shell, &mut comp, &mut windows, |host| {
        host.window_closed(2);
    });

    assert!(comp.window(popup).is_none(), "the popup surface is gone");
    assert!(windows.wm_id(2).is_none());
    assert!(comp.window(parent).is_some(), "the parent stands");
    assert_eq!(
        shell.session().taskbar().tasks().len(),
        1,
        "the parent's task is untouched"
    );

    // And the parent then leaves through the task path, taking its entry.
    with_window_host(&mut shell, &mut comp, &mut windows, |host| {
        host.window_closed(1);
    });
    assert!(comp.window(parent).is_none());
    assert!(shell.session().taskbar().tasks().is_empty());
}

/// A popup is opened as its parent's transient, so the window manager keeps
/// the pair together by itself. The session re-asserts nothing per frame: it
/// is the restack that holds the arrangement, wherever the raise comes from.
#[test]
fn a_popup_is_glued_above_its_parent_by_every_restack() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut windows = SessionWindows::new();
    let (parent, popup) =
        open_parent_and_popup(&mut shell, &mut comp, &mut windows, (10, 20), (200, 160));
    let bounds = comp.window(popup).expect("popup is live").bounds();
    let over_popup = Point::new(bounds.left() + 1, bounds.top() + 1);
    let client = comp.window_client_rect(parent).expect("parent client");

    // A third window opened over the pair is simply in front of both: the
    // popup is buried *with* its parent, never separated from it.
    let intruder = shell
        .open_window(&mut comp, bounds.origin, app_surface(), "Intruder")
        .expect("opens");
    assert_eq!(comp.window_at(over_popup), Some(intruder));

    // Raising the parent — a click on the terminal, the taskbar activating
    // its task — brings its popup back with it, still directly above it.
    assert!(comp.raise(parent));
    assert_eq!(
        comp.window_at(over_popup),
        Some(popup),
        "the pair rose as a unit, popup on top"
    );
    // Directly above: a point the parent covers but the popup does not is
    // the parent's, so nothing landed between them.
    assert_eq!(comp.window_at(client.origin), Some(parent));

    // And the intruder raised again takes the front from both of them
    // without ever slipping between them.
    assert!(comp.raise(intruder));
    assert_eq!(comp.window_at(over_popup), Some(intruder));
    assert!(comp.raise(popup), "a raise of the popup arranges the pair");
    assert_eq!(comp.window_at(over_popup), Some(popup));
    assert_eq!(comp.window_at(client.origin), Some(parent));
}

/// Hovering an open menu is the frame the desktop draws most often: the app
/// repaints its popup, and *nothing else about the screen changes*. With a
/// translucent, backdrop-blurred parent — a terminal set to frosted glass —
/// that frame must cost the popup's rectangle and no more, or every pointer
/// sample re-blurs a whole window.
#[test]
fn hovering_an_open_popup_repaints_only_the_popup() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut windows = SessionWindows::new();
    let (parent, popup) =
        open_parent_and_popup(&mut shell, &mut comp, &mut windows, (10, 20), (200, 160));
    assert!(comp.set_opacity(parent, 128));
    assert!(comp.set_backdrop_blur(parent, 8));
    comp.composite();
    assert!(!comp.has_damage(), "the opening frame has been drained");

    // The app redraws its menu row and presents it; the session serves that
    // present exactly as the serve loop does.
    let popup_bounds = comp.window(popup).expect("popup is live").bounds();
    let content = comp
        .window_client_rect(popup)
        .expect("the popup has a client");
    with_window_host(&mut shell, &mut comp, &mut windows, |host| {
        assert_eq!(
            host.window_presented(
                2,
                &served_mode(popup_bounds.width, popup_bounds.height),
                &vec![0x40; (popup_bounds.width * popup_bounds.height * 4) as usize],
                DamageRect {
                    x: 0,
                    y: 0,
                    width_px: popup_bounds.width,
                    height_px: popup_bounds.height,
                },
            ),
            Ok(())
        );
    });

    let damage = comp.composite();
    assert_eq!(
        damage.rects(),
        &[content],
        "the hover repainted more than the popup itself"
    );
    assert_eq!(
        comp.frame_stats().blur_px,
        0,
        "the parent's frosted backdrop was thrown away and blurred again"
    );
}

/// The whole point of the frame deadline, driven exactly as the serve loop
/// drives it: a hand on the mouse delivers samples several times faster than
/// any screen shows a frame, and each one moves the cursor and so damages the
/// screen. Compositing per sample would spend a whole frame's blending on
/// pixels the next sample overwrites.
#[test]
fn a_flood_of_pointer_samples_inside_one_period_is_one_composite() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut pacer = FramePacer::new();

    // The bar coming up is the desktop's first frame, and a first frame is
    // admitted at once.
    shell.present(&mut comp);
    assert!(pacer.admit(0, comp.has_damage()));
    comp.composite();
    assert!(!comp.has_damage(), "the opening frame has been drained");

    // Sixteen samples crossing the bar, all inside one frame period.
    let step = Timeline::FRAME_NS / 16;
    let mut composites = 0u32;
    for sample in 1..=16i32 {
        let at = u64::try_from(sample).expect("a positive sample") * step;
        shell
            .pump(
                &mut MemoryInput::new(&[moved(24 + sample * 8, 1060)]),
                &mut comp,
                at,
            )
            .expect("the in-memory source never faults");
        assert!(comp.has_damage(), "sample {sample} changed nothing");
        if pacer.admit(at, comp.has_damage()) {
            comp.composite();
            composites += 1;
        }
    }
    assert_eq!(
        composites, 0,
        "a sample inside the period composited a frame of its own"
    );

    // What the flood accumulated reaches the screen on the deadline the pacer
    // armed for it, not on some later unrelated wake — and the park folds back
    // to indefinite once it has.
    let last_ns = 16 * step;
    let due = pacer.park_deadline_ns(last_ns, NO_DEADLINE_NS);
    assert!(due > 0, "a deadline of nothing would be a busy poll");
    assert!(due < Timeline::FRAME_NS, "one period at most, not {due}ns");
    assert!(pacer.admit(last_ns + due, comp.has_damage()));
    assert!(
        !comp.composite().is_empty(),
        "the held frame reached the screen having drawn nothing"
    );
    assert_eq!(
        pacer.park_deadline_ns(last_ns + due, NO_DEADLINE_NS),
        NO_DEADLINE_NS
    );
}

// ---- the desktop's reveal from black ----

/// The instant a test session starts. Well clear of zero, so anything that
/// silently read an unset clock instead of this one could not pass by
/// accident.
const SESSION_START_NS: u64 = 9_000_000_000;

/// A finite park some other part of the loop already asked for, so a test can
/// show that a settled animation neither shortens it nor replaces it.
const OTHER_PARK_NS: u64 = 7_000_000;

/// A dark theme that animates nothing, for the reduced-motion path.
fn still_dark() -> Theme {
    let base = Theme::dark();
    Theme::new(
        ThemeId(101),
        "Still Dark",
        Appearance::Dark,
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion().with_reduced_motion(true),
        base.density(),
        base.contrast(),
    )
}

/// Walk `timeline` exactly as the run loop does — waking only when it asks to
/// — and collect the instants it wakes at. The last is its end, so a test
/// reads its span off the theme instead of spelling a duration of its own.
///
/// A running timeline asks for its terminal frame the moment the span runs
/// out; the loop draws that frame and settles, so the walk ends on it rather
/// than asking again from the instant it already stands at.
fn motion_wakes(timeline: Timeline, start_ns: u64) -> Vec<u64> {
    let mut at = start_ns;
    let mut wakes = Vec::new();
    while let Some(delta) = timeline.next_frame_in(at).filter(|delta| *delta > 0) {
        at = at.saturating_add(delta);
        wakes.push(at);
    }
    wakes
}

/// The session fade `comp`'s own theme asks for, and the instants a loop
/// driving it from [`SESSION_START_NS`] would wake at.
fn session_fade(comp: &Compositor) -> (Timeline, Vec<u64>) {
    let span = comp
        .theme()
        .motion()
        .duration(MotionInteraction::SessionFade);
    assert!(span > 0, "the shipped theme fades a session in");
    let timeline = Timeline::start(SESSION_START_NS, span);
    let wakes = motion_wakes(timeline, SESSION_START_NS);
    assert!(!wakes.is_empty(), "a running fade wakes at least once");
    (timeline, wakes)
}

/// A display that refuses every frame it is handed, so a test can prove a
/// fade nobody could put on screen still ends fully revealed.
struct RefusingDisplay {
    mode: DisplayMode,
    refusals: u32,
}

impl Display for RefusingDisplay {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }

    fn present(&mut self, _frame: &[u8]) -> Result<(), DriverError> {
        self.refusals += 1;
        Err(DriverError::Busy)
    }

    fn present_rects(&mut self, _frame: &[u8], _damage: &[DamageRect]) -> Result<(), DriverError> {
        self.refusals += 1;
        Err(DriverError::Busy)
    }
}

/// Keeps every record it is handed, so a test can count the desktop's
/// one-shot reveal witness and check the exact text a log consumer keys on.
struct RecordingSink {
    events: RefCell<Vec<(u32, String)>>,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            events: RefCell::new(Vec::new()),
        }
    }

    /// How many reveal witnesses have been recorded, by id *and* rendered
    /// text — the vertical keys on the text, so a test that ignored it could
    /// pass while the gate it stands for never fired.
    fn witnesses(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|(id, message)| {
                *id == DESKTOP_REVEALED.0 && message.as_str() == DESKTOP_REVEALED_MESSAGE
            })
            .count()
    }
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.events
            .borrow_mut()
            .push((event.id.0, String::from(event.message)));
    }
}

#[test]
fn the_desktop_reveals_from_black_over_the_themes_session_fade() {
    let mut comp = compositor();
    let (reference, wakes) = session_fade(&comp);

    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);

    assert_eq!(
        comp.reveal(),
        0,
        "the desktop starts from the black the login screen left behind"
    );
    assert_eq!(
        fade.park_deadline_ns(SESSION_START_NS, NO_DEADLINE_NS),
        Timeline::FRAME_NS,
        "an indefinite park is bounded to the fade's next frame"
    );

    let mut shown = 0u8;
    for at in &wakes {
        let expected = reference.progress(*at);
        assert_eq!(
            fade.advance(*at, &mut comp),
            expected != shown,
            "a step repaints exactly when the strength moved"
        );
        assert_eq!(comp.reveal(), expected, "at {at}");
        shown = expected;
    }

    let end = wakes[wakes.len() - 1];
    assert_eq!(
        comp.reveal(),
        u8::MAX,
        "the fade ends on the fully composed desktop"
    );
    assert!(
        !fade.advance(end, &mut comp),
        "a settled fade is no further work"
    );
    assert_eq!(
        fade.park_deadline_ns(end, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "and arms no further timer"
    );
}

/// The stall this guards: the loop steps the reveal, spends real time
/// presenting that frame, and only then works out its park. A span that ended
/// in between still owes the frame that completes the reveal, or the desktop
/// The reveal witness must not fire while the user's own backdrop is still being
/// prepared: the wallpaper is read and decoded on a worker thread, and the frame
/// on screen until it lands carries the fallback colour in its place. The
/// desktop QEMU verticals gate their scan-out readback on this witness and check
/// a wallpaper pixel, so a witness that fired early would read the wrong screen.
#[test]
fn the_reveal_witness_waits_for_the_first_backdrop() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let end = wakes[wakes.len() - 1];
    let sink = RecordingSink::new();
    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);

    fade.set_awaiting_backdrop(true);
    fade.advance(end + 1, &mut comp);
    assert_eq!(comp.reveal(), u8::MAX, "the fade itself is unaffected");
    fade.presented(&sink);
    assert_eq!(
        sink.witnesses(),
        0,
        "the desktop was announced visible before its wallpaper arrived"
    );

    // The preparation resolves — installed, refused, or never wanted — and the
    // frame that carries it is the one the witness follows.
    fade.set_awaiting_backdrop(false);
    fade.presented(&sink);
    assert_eq!(sink.witnesses(), 1);
    fade.presented(&sink);
    assert_eq!(sink.witnesses(), 1, "the witness is still one-shot");
}

/// parks indefinitely one step short of visible.
#[test]
fn a_fade_that_ended_while_its_frame_was_presented_still_tightens_the_park() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let end = wakes[wakes.len() - 1];
    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);

    fade.advance(end - 1, &mut comp);
    assert!(comp.reveal() < u8::MAX, "a step short of the desktop");

    let asked = end + 1;
    for park in [NO_DEADLINE_NS, OTHER_PARK_NS] {
        assert_eq!(
            fade.park_deadline_ns(asked, park),
            0,
            "the frame that completes the reveal is owed now"
        );
    }

    assert!(fade.advance(asked, &mut comp), "and drawing it finishes");
    assert_eq!(comp.reveal(), u8::MAX);
    for park in [NO_DEADLINE_NS, OTHER_PARK_NS] {
        assert_eq!(
            fade.park_deadline_ns(asked, park),
            park,
            "only then is the park left alone"
        );
    }
}

/// The whole point of folding rather than replacing: with nothing animating,
/// the park is byte-for-byte the value the loop already carried.
#[test]
fn an_idle_desktop_parks_exactly_as_it_would_without_a_fade() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let end = wakes[wakes.len() - 1];
    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    let lock = ScreenLock::new();

    fade.advance(end, &mut comp);

    for park in [NO_DEADLINE_NS, OTHER_PARK_NS] {
        assert_eq!(
            fade.park_deadline_ns(end, park),
            park,
            "a settled fade arms no timer"
        );
        assert_eq!(
            lock.park_deadline_ns(end, park),
            park,
            "nor does an unengaged lock"
        );
    }
}

#[test]
fn reduced_motion_shows_the_desktop_at_once_with_no_frame_and_no_timer() {
    let mut comp = compositor();
    assert!(comp.set_theme(still_dark()));
    comp.composite();
    assert!(!comp.has_damage(), "the theme switch is already drawn");

    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);

    assert_eq!(comp.reveal(), u8::MAX, "the desktop is simply there");
    assert!(
        !comp.has_damage(),
        "nothing was dimmed, so nothing owes a repaint"
    );
    assert!(
        !fade.advance(SESSION_START_NS, &mut comp),
        "and no frame is owed"
    );
    assert_eq!(
        fade.park_deadline_ns(SESSION_START_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "nor a timer"
    );
}

/// The fade is driven by the clock, not by the screen: a display that refuses
/// the frames cannot leave the desktop stranded dark.
#[test]
fn a_refused_present_mid_fade_still_reaches_a_fully_revealed_desktop() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let mid = wakes[wakes.len() / 2];
    let end = wakes[wakes.len() - 1];
    let mut display = RefusingDisplay {
        mode: comp.mode(),
        refusals: 0,
    };

    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    fade.advance(mid, &mut comp);
    assert!(comp.reveal() < u8::MAX, "the fade is still in flight");
    assert!(
        comp.present(&mut display).is_err(),
        "the display refuses the frame"
    );
    assert!(display.refusals > 0, "and really was asked for it");

    assert!(fade.advance(end, &mut comp));

    assert_eq!(
        comp.reveal(),
        u8::MAX,
        "time finishes the fade, never a present that succeeded"
    );
    assert_eq!(
        fade.park_deadline_ns(end, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "and it stops asking for frames"
    );
}

/// The witness is the desktop being *visible*, not merely presented: every
/// frame of the fade is black to a degree, so nothing is announced until the
/// screen stands at full strength — and then only once.
#[test]
fn the_desktop_announces_itself_visible_once_the_fade_has_completed() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let sink = RecordingSink::new();

    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    fade.presented(&sink);
    assert_eq!(
        comp.reveal(),
        0,
        "the first frame is the black it starts on"
    );
    assert_eq!(sink.witnesses(), 0, "which nobody could see the desktop in");

    for at in &wakes {
        fade.advance(*at, &mut comp);
        fade.presented(&sink);
        assert_eq!(
            sink.witnesses(),
            usize::from(comp.reveal() == u8::MAX),
            "announced exactly when the screen first stands at full strength, at {at}"
        );
    }

    let end = wakes[wakes.len() - 1];
    fade.advance(end, &mut comp);
    fade.presented(&sink);
    fade.presented(&sink);
    assert_eq!(
        sink.witnesses(),
        1,
        "once per session, never once per frame"
    );
}

/// A theme that reports a zero duration is fully revealed from its first
/// frame, so its witness lands there rather than never — a consumer waiting
/// on it would otherwise wait for ever.
#[test]
fn reduced_motion_announces_the_desktop_visible_on_its_first_frame() {
    let mut comp = compositor();
    assert!(comp.set_theme(still_dark()));
    let sink = RecordingSink::new();

    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    assert_eq!(comp.reveal(), u8::MAX, "nothing was dimmed");

    fade.presented(&sink);
    assert_eq!(sink.witnesses(), 1, "so the desktop is visible at once");

    fade.presented(&sink);
    assert_eq!(sink.witnesses(), 1, "and is still announced only once");
}

/// The witness's id sits inside the block this crate reserves, so it cannot
/// collide with another subsystem's.
#[test]
fn the_reveal_witness_id_is_inside_the_sessions_reserved_range() {
    assert!((DESKTOP_SESSION_RANGE_START..DESKTOP_SESSION_RANGE_END).contains(&DESKTOP_REVEALED.0));
}

/// Logging out and stepping aside hand the seat on cleared, so the desktop
/// dims into that black over the same span it arrived on rather than
/// vanishing from a lit screen.
#[test]
fn the_desktop_dissolves_back_to_black_when_it_gives_the_screen_up() {
    let mut comp = compositor();
    let (reference, wakes) = session_fade(&comp);
    let arrived = wakes[wakes.len() - 1];
    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    fade.advance(arrived, &mut comp);
    assert_eq!(comp.reveal(), u8::MAX, "a fully revealed desktop");

    fade.depart(arrived, &mut comp);

    assert_eq!(comp.reveal(), u8::MAX, "the departure starts from the lit");
    assert!(!fade.settled(), "and has a span to run");
    assert_eq!(
        fade.park_deadline_ns(arrived, NO_DEADLINE_NS),
        Timeline::FRAME_NS,
        "which the park is bounded to"
    );

    for at in &wakes {
        let elapsed = at - SESSION_START_NS;
        fade.advance(arrived + elapsed, &mut comp);
        assert_eq!(
            u32::from(comp.reveal()),
            u32::from(u8::MAX) - u32::from(reference.progress(*at)),
            "the same span, run the other way, at {at}"
        );
    }

    assert_eq!(comp.reveal(), 0, "and it ends on black");
    assert!(fade.settled(), "with nothing further owed");
    assert_eq!(
        fade.park_deadline_ns(arrived + (arrived - SESSION_START_NS), NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "and no timer armed"
    );
}

/// A log-out chosen while the desktop is still appearing must not flash it
/// bright first: the departure begins from the strength actually on screen.
#[test]
fn a_departure_mid_reveal_dims_from_what_is_on_screen() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let mid = wakes[wakes.len() / 2];
    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    fade.advance(mid, &mut comp);
    let part_way = comp.reveal();
    assert!(part_way > 0 && part_way < u8::MAX, "part-way through");

    fade.depart(mid, &mut comp);

    assert_eq!(comp.reveal(), part_way, "no jump either way");
    let (_, from_mid) = session_fade(&comp);
    let span = from_mid[from_mid.len() - 1] - SESSION_START_NS;
    fade.advance(mid + span, &mut comp);
    assert_eq!(comp.reveal(), 0, "and it still finishes on black");
}

/// The witness says the desktop became *visible*. A session on its way out
/// reaches black, not visibility, so it must never announce — including one
/// that departs before it ever finished arriving.
#[test]
fn a_departing_desktop_never_announces_itself_visible() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let mid = wakes[wakes.len() / 2];
    let end = wakes[wakes.len() - 1];
    let span = end - SESSION_START_NS;
    let sink = RecordingSink::new();

    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    fade.advance(mid, &mut comp);
    fade.depart(mid, &mut comp);
    for at in [mid, mid + span / 2, mid + span] {
        fade.advance(at, &mut comp);
        fade.presented(&sink);
    }

    assert_eq!(comp.reveal(), 0, "the screen is black");
    assert_eq!(
        sink.witnesses(),
        0,
        "and nothing claimed the desktop was ever visible"
    );
}

/// A session resumed from the background comes back to a seat the login
/// screen handed over cleared, so it fades in exactly as a fresh one does
/// rather than reappearing on black or snapping on.
#[test]
fn a_resumed_desktop_fades_in_rather_than_returning_to_black() {
    let mut comp = compositor();
    let (_, wakes) = session_fade(&comp);
    let end = wakes[wakes.len() - 1];
    let span = end - SESSION_START_NS;
    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    fade.advance(end, &mut comp);
    fade.depart(end, &mut comp);
    fade.advance(end + span, &mut comp);
    assert_eq!(comp.reveal(), 0, "stepped aside, screen black");

    let resumed = end + span;
    fade.arrive(resumed, &mut comp);

    assert_eq!(comp.reveal(), 0, "the first frame back is still the black");
    assert!(!fade.settled(), "with the reveal now in flight");
    fade.advance(resumed + span / 2, &mut comp);
    assert!(comp.reveal() > 0, "and it lifts");
    fade.advance(resumed + span, &mut comp);
    assert_eq!(comp.reveal(), u8::MAX, "to the whole desktop");
}

/// Under a reduced-motion theme a departure is black from its first frame
/// and asks for nothing, exactly as its arrival is fully revealed at once.
#[test]
fn reduced_motion_blacks_the_screen_at_once_with_no_frame_and_no_timer() {
    let mut comp = compositor();
    assert!(comp.set_theme(still_dark()));
    let mut fade = ScreenFade::begin(SESSION_START_NS, &mut comp);
    assert_eq!(comp.reveal(), u8::MAX);

    fade.depart(SESSION_START_NS, &mut comp);

    assert_eq!(comp.reveal(), 0, "black immediately");
    assert!(fade.settled(), "with no frame owed");
    assert_eq!(
        fade.park_deadline_ns(SESSION_START_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "nor a timer"
    );
}

/// The lock screen is the login screen's engine, so a refused unlock shakes
/// the question and the lock folds that into the session's park.
///
/// This is also what proves the clock is really threaded: a surface handed a
/// frozen zero would start the shake at zero, and by the instant the test
/// asks it would already be long over and ask for nothing at all.
#[test]
fn a_refused_unlock_animates_on_the_sessions_clock() {
    let shell = shell();
    let mut comp = compositor();
    let mut lock = ScreenLock::new();
    assert!(lock.engage("ann", &shell, &mut comp));
    let mut unlocker = ScriptedUnlocker::refusing();
    assert_eq!(
        lock.park_deadline_ns(LOCK_EVENT_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "an idle lock screen arms no timer"
    );

    submit(
        &mut lock,
        "wrong",
        LOCK_EVENT_NS,
        &mut unlocker,
        &shell,
        &mut comp,
    );

    assert!(
        lock.park_deadline_ns(LOCK_EVENT_NS, NO_DEADLINE_NS) < NO_DEADLINE_NS,
        "the refusal asks for a frame"
    );
    comp.composite();
    let mut at = LOCK_EVENT_NS;
    let mut frames = 0u32;
    let mut repainted = false;
    loop {
        let due = lock.park_deadline_ns(at, NO_DEADLINE_NS);
        if due == NO_DEADLINE_NS {
            break;
        }
        assert!(due > 0, "a frame that is due now would spin the loop");
        assert_eq!(
            lock.park_deadline_ns(at, OTHER_PARK_NS),
            OTHER_PARK_NS.min(due),
            "an animating lock shortens a park it already has, never lengthens it"
        );
        at = at.saturating_add(due);
        lock.advance(at, &shell, &mut comp);
        repainted |= comp.has_damage();
        frames += 1;
        assert!(frames < 1_000, "the shake must end");
    }

    assert!(frames > 0, "the refusal really animated");
    assert!(repainted, "and redrew the lock while it did");
    assert!(lock.is_locked(), "a refusal never unlocks");
    assert_eq!(
        lock.park_deadline_ns(at, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "a settled lock screen is back to arming no timer"
    );
}

// --- The owning application's identity icon -------------------------------

/// Lends one counting double to the shell's boxed artwork seam while the
/// test keeps reading its counts — the shell owns its seams outright, so a
/// shared handle is the only way to ask afterwards what was read or decoded.
struct Shared<T>(Rc<RefCell<T>>);

impl<T: SessionFileReader> SessionFileReader for Shared<T> {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        self.0.borrow_mut().read(path)
    }
}

impl<T: IconRasteriser> IconRasteriser for Shared<T> {
    fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<Vec<u8>> {
        self.0.borrow_mut().rasterise(side, icon)
    }
}

/// The bundle each identity test's window owner was launched from.
const EDITOR_BUNDLE: &str = "/Apps/editor.app";
const CHESS_BUNDLE: &str = "/Apps/chess.app";

/// The task ids the launch table records those bundles under.
const EDITOR_PID: u64 = 41;
const CHESS_PID: u64 = 42;

/// The tint each bundle's own icon rasterises to, so a drawn slot names the
/// bundle it came from.
const EDITOR_TINT: u8 = 0xE1;
const CHESS_TINT: u8 = 0xC5;

/// An asset table holding `bundle`'s manifest (declaring its own icon) and
/// that icon's bytes, whose first byte is the tint it rasterises to. Empty
/// `icon` bytes stand for an asset the decoder refuses.
fn identity_bundle(assets: MemoryAssets, bundle: &str, name: &str, icon: &[u8]) -> MemoryAssets {
    assets
        .with(
            &format!("{bundle}/AppInfo"),
            &manifest_fixture(name, Some("icon.svg")),
        )
        .with(&format!("{bundle}/Resources/icon.svg"), icon)
}

/// A desktop whose artwork seams read `assets` and rasterise by tint, with
/// the reader and rasteriser shared back so a test can count what the
/// resolution actually cost.
#[allow(clippy::type_complexity)] // The two shared doubles are the point.
fn identity_desktop(
    assets: MemoryAssets,
) -> (
    DesktopShell,
    Compositor,
    Rc<RefCell<CountingAssets>>,
    Rc<RefCell<TaggedRasteriser>>,
) {
    let reader = Rc::new(RefCell::new(CountingAssets::new(assets)));
    let rasteriser = Rc::new(RefCell::new(TaggedRasteriser::new()));
    let mut shell = shell();
    shell.set_artwork_resolver(alloc::boxed::Box::new(InlineArtwork::new(
        ArtworkFileReader(Shared(Rc::clone(&reader))),
        ArtworkSandbox(Shared(Rc::clone(&rasteriser))),
    )));
    (shell, compositor(), reader, rasteriser)
}

/// Open served window `window_id` exactly as the serve loop does, for the
/// attested `owner`.
fn open_owned_window(
    shell: &mut DesktopShell,
    comp: &mut Compositor,
    windows: &mut SessionWindows,
    owner: ProcId,
    window_id: u64,
) {
    with_window_host(shell, comp, windows, |host| {
        host.window_opened(
            owner,
            window_id,
            &served_mode(320, 240),
            "App",
            resizable_sizing(),
        )
        .expect("the window opens");
    });
}

/// A launch table recording each `(pid, bundle)` the desktop started, and
/// the attestation map from that pid's owner to it.
fn launched_bundles(records: &[(u64, &str)]) -> LaunchTable {
    let mut launched = LaunchTable::new();
    for (pid, bundle) in records {
        launched.record(*pid, "App", &format!("{bundle}{BUNDLE_RUN_SUFFIX}"));
    }
    launched
}

/// The identity the decorated window `wm` wears, and the opaque tint of the
/// artwork drawn in its slot when it has any.
fn window_identity(comp: &Compositor, wm: WindowId) -> (Option<IconKind>, Option<u8>) {
    let window = comp.window(wm).expect("the window is live");
    let identity = window
        .frame()
        .expect("a served window is decorated")
        .title_bar()
        .identity();
    let tint = window
        .identity_artwork()
        .and_then(|art| art.pixels().first().map(|pixel| pixel.r));
    (identity, tint)
}

#[test]
fn a_windows_identity_comes_from_the_bundle_the_desktop_launched() {
    let (mut shell, mut comp, _reader, _rasteriser) = identity_desktop(identity_bundle(
        MemoryAssets::default(),
        EDITOR_BUNDLE,
        "Editor",
        &[EDITOR_TINT],
    ));
    let mut windows = SessionWindows::new();
    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(1), 1);
    let wm = windows.wm_id(1).expect("the window is live");
    let side = comp
        .window_title_icon_side(wm)
        .expect("a decorated window draws an identity slot");

    resolve_window_identities(
        &mut shell,
        &mut comp,
        &mut windows,
        &launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE)]),
        |owner| (owner == window_owner(1)).then_some(EDITOR_PID),
    );

    assert_eq!(
        window_identity(&comp, wm),
        (Some(IconKind::AppBundle), Some(EDITOR_TINT)),
        "the bundle the desktop launched supplies the icon"
    );
    let artwork = comp
        .window(wm)
        .expect("live")
        .identity_artwork()
        .expect("the bundle's icon was drawn");
    assert_eq!(
        artwork.width(),
        side,
        "rasterised at exactly the slot the title bar draws"
    );
}

#[test]
fn a_window_whose_owner_the_desktop_did_not_launch_gets_no_identity() {
    let (mut shell, mut comp, _reader, _rasteriser) = identity_desktop(identity_bundle(
        MemoryAssets::default(),
        EDITOR_BUNDLE,
        "Editor",
        &[EDITOR_TINT],
    ));
    let mut windows = SessionWindows::new();
    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(9), 1);
    let wm = windows.wm_id(1).expect("the window is live");

    // A shell-spawned program: attested, but this desktop never launched it,
    // so there is no bundle to name it by.
    resolve_window_identities(
        &mut shell,
        &mut comp,
        &mut windows,
        &launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE)]),
        |_| None,
    );

    assert_eq!(
        window_identity(&comp, wm),
        (None, None),
        "an application that cannot be named wears no badge"
    );
}

#[test]
fn one_owners_pid_cannot_yield_another_bundles_icon() {
    let assets = identity_bundle(
        identity_bundle(
            MemoryAssets::default(),
            EDITOR_BUNDLE,
            "Editor",
            &[EDITOR_TINT],
        ),
        CHESS_BUNDLE,
        "Chess",
        &[CHESS_TINT],
    );
    let (mut shell, mut comp, _reader, _rasteriser) = identity_desktop(assets);
    let mut windows = SessionWindows::new();
    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(1), 1);
    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(2), 2);

    resolve_window_identities(
        &mut shell,
        &mut comp,
        &mut windows,
        &launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE), (CHESS_PID, CHESS_BUNDLE)]),
        |owner| match owner {
            owner if owner == window_owner(1) => Some(EDITOR_PID),
            owner if owner == window_owner(2) => Some(CHESS_PID),
            _ => None,
        },
    );

    let editor = windows.wm_id(1).expect("live");
    let chess = windows.wm_id(2).expect("live");
    assert_eq!(window_identity(&comp, editor).1, Some(EDITOR_TINT));
    assert_eq!(
        window_identity(&comp, chess).1,
        Some(CHESS_TINT),
        "each window wears the icon of the bundle its own attested owner runs"
    );
}

#[test]
fn a_window_opens_and_keeps_its_identity_when_the_icon_cannot_be_resolved() {
    // The bundle declares an icon whose bytes the decoder refuses, and no
    // shipped application-bundle master stands behind it.
    let (mut shell, mut comp, _reader, _rasteriser) = identity_desktop(identity_bundle(
        MemoryAssets::default(),
        EDITOR_BUNDLE,
        "Editor",
        &[],
    ));
    let mut windows = SessionWindows::new();
    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(1), 1);
    let wm = windows.wm_id(1).expect("the window opened regardless");

    resolve_window_identities(
        &mut shell,
        &mut comp,
        &mut windows,
        &launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE)]),
        |_| Some(EDITOR_PID),
    );

    assert_eq!(windows.len(), 1, "the window is live");
    assert_eq!(
        window_identity(&comp, wm),
        (Some(IconKind::AppBundle), None),
        "a refused picture leaves the identity on its built-in glyph"
    );
}

#[test]
fn a_second_window_of_the_same_application_reuses_the_resolved_icon() {
    let (mut shell, mut comp, reader, rasteriser) = identity_desktop(identity_bundle(
        MemoryAssets::default(),
        EDITOR_BUNDLE,
        "Editor",
        &[EDITOR_TINT],
    ));
    let mut windows = SessionWindows::new();
    let launched = launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE)]);

    let mut costs = Vec::new();
    for window_id in 1..=2 {
        let before = (reader.borrow().reads, rasteriser.borrow().calls);
        open_owned_window(
            &mut shell,
            &mut comp,
            &mut windows,
            window_owner(1),
            window_id,
        );
        resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
            Some(EDITOR_PID)
        });
        costs.push((
            reader.borrow().reads - before.0,
            rasteriser.borrow().calls - before.1,
        ));
    }

    let second = windows.wm_id(2).expect("live");
    assert_eq!(
        window_identity(&comp, second).1,
        Some(EDITOR_TINT),
        "the second window wears the same icon"
    );
    assert_eq!(
        costs[0].1, 1,
        "the first window decodes the bundle's own icon once, at its slot side"
    );
    assert_eq!(
        costs[1],
        (0, 0),
        "and the second is served out of the one shared cache — the bundle's \
         manifest is not read again either, because the cache is keyed by the \
         bundle directory rather than by the asset it names"
    );
}

/// The desktop's production resolver decodes on a worker thread, so a paint
/// that misses is answered "not yet" and draws the built-in glyph. A window's
/// title-bar and taskbar identity *store* the picture rather than re-resolving
/// as they paint, so they have to be offered it again when it lands — this is
/// that contract, exercised through the real resolution path with the landing
/// under the test's control instead of a thread's.
#[test]
fn a_window_identity_pending_at_open_is_pictured_when_the_decode_lands() {
    /// Refuses every decode until `landed` is set, exactly as the desk does
    /// while its worker is still reading and rasterising.
    struct Deferring {
        inner: InlineArtwork<ArtworkFileReader<MemoryAssets>, ArtworkSandbox<TaggedRasteriser>>,
        landed: Rc<Cell<bool>>,
    }

    impl ArtworkResolver for Deferring {
        fn resolve(&mut self, key: &tairix_icon::ArtworkKey, side: u32) -> Resolved {
            if self.landed.get() {
                self.inner.resolve(key, side)
            } else {
                Resolved::Pending
            }
        }
    }

    let landed = Rc::new(Cell::new(false));
    let mut shell = shell();
    shell.set_artwork_resolver(alloc::boxed::Box::new(Deferring {
        inner: InlineArtwork::new(
            ArtworkFileReader(identity_bundle(
                MemoryAssets::default(),
                EDITOR_BUNDLE,
                "Editor",
                &[EDITOR_TINT],
            )),
            ArtworkSandbox(TaggedRasteriser::new()),
        ),
        landed: Rc::clone(&landed),
    }));
    let mut comp = compositor();
    let mut windows = SessionWindows::new();
    let launched = launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE)]);

    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(1), 1);
    resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
        Some(EDITOR_PID)
    });
    let wm = windows.wm_id(1).expect("live");
    assert_eq!(
        window_identity(&comp, wm),
        (Some(IconKind::AppBundle), None),
        "a decode still in flight leaves the slot on its built-in glyph"
    );

    // The decode lands. Nothing else about the desktop changed, so the window
    // is pictured only because it kept its place on the identification list.
    landed.set(true);
    resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
        Some(EDITOR_PID)
    });
    assert_eq!(
        window_identity(&comp, wm).1,
        Some(EDITOR_TINT),
        "the landing pictures the title bar"
    );

    // Pictured, so it is off the list: a later landing has nothing to redo.
    resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
        panic!("a window already wearing its picture was offered identification again")
    });
}

/// A window opens a spawn, a load, and an application's own bring-up after the
/// launch that started it, so its icon can be decoded in between. This is that:
/// warming from the launch table leaves the window wearing its own picture on
/// the frame it first appears in, rather than the shared application glyph.
#[test]
fn a_window_wears_its_own_icon_on_the_frame_it_opens_in() {
    /// A resolver over the real desk, as the serve loop's own is.
    struct Deferring(Rc<RefCell<ArtworkDesk>>);

    impl ArtworkResolver for Deferring {
        fn resolve(&mut self, key: &tairix_icon::ArtworkKey, side: u32) -> Resolved {
            self.0.borrow_mut().collect(key, side)
        }

        fn prefetch(&mut self, key: &tairix_icon::ArtworkKey, side: u32) {
            self.0.borrow_mut().want(key, side);
        }
    }

    NORMAL_PRESSURE.report(PressureBand::Normal);
    let desk = Rc::new(RefCell::new(ArtworkDesk::new()));
    let mut comp = compositor();
    let mut shell = shell();
    shell.set_artwork_resolver(alloc::boxed::Box::new(Deferring(Rc::clone(&desk))));
    let mut windows = SessionWindows::new();
    let launched = launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE)]);

    // The launch is recorded; no window exists yet. The desktop asks for the
    // application's picture at both the bar's and the title band's slot
    // sides all the same.
    shell.warm_launched_artwork(&comp, launched.bundles());
    assert!(
        desk.borrow().has_work(),
        "a recorded launch must start its icon"
    );

    let mut reader = ArtworkFileReader(identity_bundle(
        MemoryAssets::default(),
        EDITOR_BUNDLE,
        "Editor",
        &[EDITOR_TINT],
    ));
    let mut rasteriser = ArtworkSandbox(TaggedRasteriser::new());
    let mut rounds = 0;
    while desk.borrow().has_work() {
        while let Some(job) = {
            let taken = desk.borrow_mut().next_job();
            taken
        } {
            let artwork =
                tairix_icon::render_artwork(&mut reader, &mut rasteriser, &job.key, job.side);
            assert!(desk.borrow_mut().deliver(&job, artwork));
        }
        shell.warm_launched_artwork(&comp, launched.bundles());
        rounds += 1;
        assert!(rounds < 8, "the warm-up must run dry");
    }

    // Only now does the window open. Its first identification wears the
    // picture, with nothing deferred for a later pass to finish.
    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(1), 1);
    resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
        Some(EDITOR_PID)
    });
    let wm = windows.wm_id(1).expect("live");
    assert_eq!(
        window_identity(&comp, wm).1,
        Some(EDITOR_TINT),
        "the window opens wearing its application's own icon"
    );
    resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
        panic!("a window pictured on its first pass was offered identification again")
    });
}

/// A window whose application has no picture at all must not stay on the
/// identification list: a refusal is an answer, and re-offering it on every
/// landing for as long as the window is open would be work with no end.
#[test]
fn a_window_whose_application_has_no_picture_leaves_the_identification_list() {
    let (mut shell, mut comp, _reader, _rasteriser) = identity_desktop(MemoryAssets::default());
    let mut windows = SessionWindows::new();
    let launched = launched_bundles(&[(EDITOR_PID, EDITOR_BUNDLE)]);

    open_owned_window(&mut shell, &mut comp, &mut windows, window_owner(1), 1);
    resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
        Some(EDITOR_PID)
    });
    let wm = windows.wm_id(1).expect("live");
    assert_eq!(
        window_identity(&comp, wm),
        (Some(IconKind::AppBundle), None),
        "no manifest and no class master: the built-in glyph, for good"
    );

    resolve_window_identities(&mut shell, &mut comp, &mut windows, &launched, |_| {
        panic!("a refused identity was offered again")
    });
}

/// The reported stall, through the real window host: right-clicking a frosted,
/// translucent terminal and dismissing the menu must not re-blur the window.
///
/// The hand-built compositor scenes prove the restack rule; this proves the
/// *session's* own sequence obeys it — a popup opened above a window that sits
/// under the taskbar, painted, closed, and the owner refocused.
#[test]
fn a_popup_over_a_frosted_window_never_reblurs_it() {
    let mut shell = shell();
    let mut comp = compositor();
    let mut windows = SessionWindows::new();

    comp.set_desktop(
        Surface::filled(1920, 1080, Color::rgb(40, 60, 90).premultiply()).expect("desktop"),
    );
    // The bar window exists before any app window, as in a live session.
    shell.present(&mut comp);
    with_window_host(&mut shell, &mut comp, &mut windows, |host| {
        assert_eq!(
            host.window_opened(
                window_owner(1),
                1,
                &served_mode(1000, 700),
                "Terminal",
                resizable_sizing(),
            ),
            Ok(())
        );
    });
    let parent = windows.wm_id(1).expect("parent live");
    assert!(comp.set_opacity(parent, 128));
    assert!(comp.set_backdrop_blur(parent, 12));
    // Compose until nothing is dirty, so the backdrop is retained.
    for _ in 0..4 {
        comp.composite();
    }
    // The bar is frosted too, so the count is the session's, not a constant;
    // what matters is that none of the gestures below drops one.
    let retained = comp.frost_cache_len();
    assert!(retained >= 1, "the window retained no backdrop");
    let window_px = 1000 * 700;

    // The right-click: the app opens its menu as a popup.
    with_window_host(&mut shell, &mut comp, &mut windows, |host| {
        assert_eq!(
            host.popup_opened(2, 1, 100, 100, &served_mode(220, 180)),
            Ok(())
        );
    });
    comp.composite();
    let opened = comp.frame_stats();
    assert_eq!(opened.blur_px, 0, "opening the menu re-blurred the window");
    assert!(
        opened.damaged_px < window_px / 8,
        "opening the menu repainted most of the window: {opened:?}"
    );
    assert_eq!(
        comp.frost_cache_len(),
        retained,
        "a backdrop was thrown away"
    );

    // The app paints the menu into its popup.
    let frame = vec![0xffu8; 220 * 180 * 4];
    with_window_host(&mut shell, &mut comp, &mut windows, |host| {
        assert_eq!(
            host.window_presented(
                2,
                &served_mode(220, 180),
                &frame,
                DamageRect::full(&served_mode(220, 180))
            ),
            Ok(())
        );
    });
    comp.composite();
    let painted = comp.frame_stats();
    assert_eq!(
        painted.blur_px, 0,
        "painting the menu re-blurred the window"
    );
    assert_eq!(painted.damaged_px, 220 * 180, "{painted:?}");

    // The dismissing click: the popup goes and the terminal takes focus back.
    let popup = windows.wm_id(2).expect("popup live");
    assert!(shell.close_popup_window(&mut comp, popup));
    assert!(comp.set_active_frame(parent, true));
    comp.composite();
    let closed = comp.frame_stats();
    assert_eq!(
        closed.blur_px, 0,
        "dismissing the menu re-blurred the window"
    );
    assert!(
        closed.damaged_px < window_px / 8,
        "dismissing the menu repainted most of the window: {closed:?}"
    );
    assert_eq!(
        comp.frost_cache_len(),
        retained,
        "a backdrop was thrown away"
    );
}

// --- The credential prompt for a command the session may not perform ----

use crate::config::DATETIME_RUN_PATH;
use crate::elevate::{
    ElevatePrompt, Elevator, PromptOutcome, ELEVATE_ORIGIN, NOT_STARTED_REASON, REFUSED_REASON,
};
use alloc::string::ToString;

/// One sentence explaining the command, as the session words it.
const ELEVATE_PURPOSE: &str = "Setting the clock needs an account that may.";

/// A broker stand-in recording every offer and answering a scripted verdict.
///
/// It records the plaintext deliberately: a test has to be able to prove the
/// prompt offered exactly what was typed, once.
#[derive(Default)]
struct ScriptedElevator {
    outcome: Option<Result<i32, Errno>>,
    offers: Vec<(String, String, String)>,
}

impl ScriptedElevator {
    fn accepting(pid: i32) -> Self {
        Self {
            outcome: Some(Ok(pid)),
            offers: Vec::new(),
        }
    }

    fn refusing(err: Errno) -> Self {
        Self {
            outcome: Some(Err(err)),
            offers: Vec::new(),
        }
    }
}

impl Elevator for ScriptedElevator {
    fn launch(&mut self, username: &str, password: &str, program: &str) -> Result<i32, Errno> {
        self.offers.push((
            username.to_string(),
            password.to_string(),
            program.to_string(),
        ));
        self.outcome.unwrap_or(Err(Errno::PermissionDenied))
    }
}

/// Type `text` into whichever field holds the keyboard.
fn type_text(
    prompt: &mut ElevatePrompt,
    text: &str,
    elevator: &mut dyn Elevator,
    shell: &mut DesktopShell,
    comp: &mut Compositor,
) {
    for ch in text.chars() {
        prompt.handle(&key_press(Key::Char(ch)), elevator, shell, comp);
    }
}

/// Fill both fields: the account name, `Tab`, then the password.
fn fill_credentials(
    prompt: &mut ElevatePrompt,
    account: &str,
    password: &str,
    elevator: &mut dyn Elevator,
    shell: &mut DesktopShell,
    comp: &mut Compositor,
) {
    type_text(prompt, account, elevator, shell, comp);
    prompt.handle(&key_press(Key::Named(NamedKey::Tab)), elevator, shell, comp);
    type_text(prompt, password, elevator, shell, comp);
}

#[test]
fn the_credential_prompt_opens_once_and_refuses_a_second() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();

    assert!(prompt.ask(DATETIME_RUN_PATH, ELEVATE_PURPOSE, &mut shell, &mut comp));
    let wm = prompt.wm_id().expect("a prompt window is showing");
    assert_eq!(comp.window(wm).expect("live").origin(), ELEVATE_ORIGIN);
    assert_eq!(prompt.pending(), Some(DATETIME_RUN_PATH));

    assert!(
        !prompt.ask(
            "/System/Applications/other.app/Run",
            ELEVATE_PURPOSE,
            &mut shell,
            &mut comp
        ),
        "one prompt at a time"
    );
    assert_eq!(
        prompt.pending(),
        Some(DATETIME_RUN_PATH),
        "the question already asked stands"
    );
}

#[test]
fn escape_cancels_the_prompt_and_offers_nothing() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();
    let mut elevator = ScriptedElevator::accepting(4210);
    assert!(prompt.ask(DATETIME_RUN_PATH, ELEVATE_PURPOSE, &mut shell, &mut comp));
    let wm = prompt.wm_id().expect("showing");
    fill_credentials(
        &mut prompt,
        "root",
        "hunter2",
        &mut elevator,
        &mut shell,
        &mut comp,
    );

    assert_eq!(
        prompt.handle(
            &key_press(Key::Named(NamedKey::Escape)),
            &mut elevator,
            &mut shell,
            &mut comp
        ),
        PromptOutcome::Cancelled
    );
    assert_eq!(prompt.wm_id(), None, "the prompt window is closed");
    assert!(comp.window(wm).is_none(), "and gone from the compositor");
    assert!(
        elevator.offers.is_empty(),
        "a cancelled prompt never spends an attempt against the account"
    );
}

#[test]
fn enter_offers_exactly_what_was_typed_and_reports_the_started_pid() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();
    let mut elevator = ScriptedElevator::accepting(4210);
    assert!(prompt.ask(DATETIME_RUN_PATH, ELEVATE_PURPOSE, &mut shell, &mut comp));
    fill_credentials(
        &mut prompt,
        "root",
        "hunter2",
        &mut elevator,
        &mut shell,
        &mut comp,
    );

    assert_eq!(
        prompt.handle(
            &key_press(Key::Named(NamedKey::Enter)),
            &mut elevator,
            &mut shell,
            &mut comp
        ),
        PromptOutcome::Started { pid: 4210 }
    );
    assert_eq!(
        elevator.offers.as_slice(),
        &[(
            "root".to_string(),
            "hunter2".to_string(),
            DATETIME_RUN_PATH.to_string()
        )],
        "offered once, verbatim, for the program the prompt named"
    );
    assert_eq!(prompt.wm_id(), None, "an accepted prompt is already down");
}

#[test]
fn an_incomplete_prompt_is_never_offered() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();
    let mut elevator = ScriptedElevator::accepting(4210);
    assert!(prompt.ask(DATETIME_RUN_PATH, ELEVATE_PURPOSE, &mut shell, &mut comp));

    // An account with no password: there is nothing to check, and asking
    // would spend an audited attempt against the account.
    type_text(&mut prompt, "root", &mut elevator, &mut shell, &mut comp);
    assert_eq!(
        prompt.handle(
            &key_press(Key::Named(NamedKey::Enter)),
            &mut elevator,
            &mut shell,
            &mut comp
        ),
        PromptOutcome::Pending
    );
    assert!(elevator.offers.is_empty());
    assert!(
        prompt.wm_id().is_some(),
        "the prompt stays up to be finished"
    );

    // The keyboard moved to the empty field, so typing continues there.
    type_text(&mut prompt, "hunter2", &mut elevator, &mut shell, &mut comp);
    assert_eq!(
        prompt.handle(
            &key_press(Key::Named(NamedKey::Enter)),
            &mut elevator,
            &mut shell,
            &mut comp
        ),
        PromptOutcome::Started { pid: 4210 }
    );
    assert_eq!(elevator.offers.len(), 1);
}

#[test]
fn a_refusal_keeps_the_prompt_up_states_it_and_clears_the_password() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();
    let mut elevator = ScriptedElevator::refusing(Errno::PermissionDenied);
    assert!(prompt.ask(DATETIME_RUN_PATH, ELEVATE_PURPOSE, &mut shell, &mut comp));
    fill_credentials(
        &mut prompt,
        "root",
        "wrong",
        &mut elevator,
        &mut shell,
        &mut comp,
    );

    assert_eq!(
        prompt.handle(
            &key_press(Key::Named(NamedKey::Enter)),
            &mut elevator,
            &mut shell,
            &mut comp
        ),
        PromptOutcome::Pending,
        "a refusal is not a conclusion"
    );
    assert!(prompt.wm_id().is_some(), "the prompt is still up to retry");
    assert_eq!(prompt.stated_reason(), Some(REFUSED_REASON));
    assert_eq!(
        prompt.secret_len(),
        0,
        "the refused password was cleared for another attempt"
    );
    assert_eq!(
        prompt.account_text(),
        Some("root"),
        "the account name is not the secret and is kept"
    );

    // The retry types only a password, into the field the refusal focused.
    type_text(&mut prompt, "hunter2", &mut elevator, &mut shell, &mut comp);
    prompt.handle(
        &key_press(Key::Named(NamedKey::Enter)),
        &mut elevator,
        &mut shell,
        &mut comp,
    );
    assert_eq!(
        elevator.offers,
        vec![
            (
                "root".to_string(),
                "wrong".to_string(),
                DATETIME_RUN_PATH.to_string()
            ),
            (
                "root".to_string(),
                "hunter2".to_string(),
                DATETIME_RUN_PATH.to_string()
            ),
        ],
        "the retry offers the new password under the same account"
    );
}

#[test]
fn a_launch_failure_reads_differently_from_a_refused_password() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();
    let mut elevator = ScriptedElevator::refusing(Errno::NotFound);
    assert!(prompt.ask(DATETIME_RUN_PATH, ELEVATE_PURPOSE, &mut shell, &mut comp));
    fill_credentials(
        &mut prompt,
        "root",
        "hunter2",
        &mut elevator,
        &mut shell,
        &mut comp,
    );

    assert_eq!(
        prompt.handle(
            &key_press(Key::Named(NamedKey::Enter)),
            &mut elevator,
            &mut shell,
            &mut comp
        ),
        PromptOutcome::Pending
    );
    assert_eq!(
        prompt.stated_reason(),
        Some(NOT_STARTED_REASON),
        "an accepted account is never blamed on the password"
    );
}

#[test]
fn abandoning_the_prompt_offers_nothing_and_closes_it() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();
    let mut elevator = ScriptedElevator::accepting(4210);
    assert!(prompt.ask(DATETIME_RUN_PATH, ELEVATE_PURPOSE, &mut shell, &mut comp));
    let wm = prompt.wm_id().expect("showing");
    fill_credentials(
        &mut prompt,
        "root",
        "hunter2",
        &mut elevator,
        &mut shell,
        &mut comp,
    );

    prompt.abandon(&mut shell, &mut comp);
    assert_eq!(prompt.wm_id(), None);
    assert!(comp.window(wm).is_none());
    assert!(elevator.offers.is_empty());
}

#[test]
fn an_idle_prompt_ignores_every_event() {
    let (mut shell, mut comp) = headless_desktop();
    let mut prompt = ElevatePrompt::new();
    let mut elevator = ScriptedElevator::accepting(4210);

    for event in [
        key_press(Key::Named(NamedKey::Enter)),
        key_press(Key::Named(NamedKey::Escape)),
        key_press(Key::Char('a')),
        PRIMARY_PRESS,
    ] {
        assert_eq!(
            prompt.handle(&event, &mut elevator, &mut shell, &mut comp),
            PromptOutcome::Pending
        );
    }
    assert!(elevator.offers.is_empty());
}
