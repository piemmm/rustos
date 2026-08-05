//! The desktop session's **trusted file picker** (`plans/APPWIN.md` AW5,
//! `plans/CAPABILITY_USE.md` CU6).
//!
//! When an app asks the window channel to pick a file
//! (`WindowRequest::PickFile`), the *session* — not the app — browses the
//! filesystem: the picker is a session-owned window driven by the one
//! shared `lib/browse` engine (the same model and renderer the files app
//! composes), listing directories under the session's own identity and
//! authority. The app never sees a path it was not handed and never
//! browses anything itself; it receives exactly one conclusion — a
//! one-shot `fd_grant` delegation for the chosen file, or a cancellation
//! — delivered over its ordinary event channel.
//!
//! [`SessionPicker`] is the host-testable engine: the single picker slot
//! (one pick UI at a time, the session's modality policy), the browser
//! state, and the key/click navigation that concludes in a
//! [`PickConclusion`]. The privileged tail — opening the chosen file and
//! minting the delegation — stays in the session's `Run` binary, which
//! holds the syscalls; the engine only ever reports *what* was chosen.
//!
//! [`PickerSlot`] is the narrow face the window-channel bridge
//! ([`ShellWindowHost`](crate::ShellWindowHost)) drives: accepting a
//! validated pick request, and aborting a pick whose requesting window
//! died. Keeping the trait object-safe keeps the bridge non-generic.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode};
use tairix_abi::Errno;
use tairix_browse::render::{entry_index_at, render, reveal_selection, toolbar_command_at};
use tairix_browse::ManagerChrome;
use tairix_browse::{apply_command, vfs, Browser, DirectorySource, WIN_HEIGHT, WIN_WIDTH};
use tairix_font::BitmapFont;
use tairix_geometry::Scale;
use tairix_icon::NoArtwork;
use tairix_theme::{TextRole, Theme};
use tairix_wm::{Compositor, Point, Rect, WindowId};

use crate::shell::DesktopShell;

/// Title of the picker window — on the taskbar and in the window chrome,
/// so the user always sees which UI is asking on an app's behalf.
pub const PICKER_TITLE: &str = "Choose a file";

/// Top-left of the picker window, in screen pixels. One deterministic
/// spot (clear of the first cascade slots), exported so a host-side
/// observer (the AW5 QEMU vertical's click script) drives the picker
/// where the session actually places it — never a re-derived guess.
pub const PICKER_ORIGIN: Point = Point::new(120, 90);

/// How the user concluded a pick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickConclusion {
    /// The user chose the regular file at this absolute path. The path is
    /// the session's to open — it is never disclosed to the requesting
    /// app, which receives only the delegation handle.
    Chosen(String),
    /// The user dismissed the picker without choosing.
    Cancelled,
}

/// A concluded pick: which window asked, and how it ended. Returned by
/// the navigation handlers once the picker window is already closed, so
/// the embedder only has to deliver the outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConcludedPick {
    /// The window-channel id of the requesting app's window.
    pub for_window: u64,
    /// How the user concluded.
    pub conclusion: PickConclusion,
}

/// The narrow face the window-channel bridge drives — object-safe so
/// [`ShellWindowHost`](crate::ShellWindowHost) stays non-generic.
pub trait PickerSlot {
    /// A validated `PickFile` for `for_window` was accepted by the window
    /// engine; open the picker UI.
    ///
    /// # Errors
    ///
    /// * [`Errno::AlreadyExists`] — the single picker slot is taken by
    ///   another window's pick (the session shows one picker at a time).
    /// * Any [`Errno`] the initial root listing surfaces (the session's
    ///   filesystem reach refused) or the UI cannot come up; nothing is
    ///   recorded and the refusal is relayed to the requesting app.
    fn begin(
        &mut self,
        for_window: u64,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Result<(), Errno>;

    /// The window-channel window `window_id` is gone (closed by its owner
    /// or torn down after the owner exited): if its pick is showing, take
    /// the picker down. No conclusion is delivered — the engine already
    /// dropped the window's pending pick with its record.
    fn abort_for(&mut self, window_id: u64, shell: &mut DesktopShell, compositor: &mut Compositor);
}

/// One live pick: the requesting window, the picker's compositor window,
/// and the browser state behind it.
struct ActivePick<S: DirectorySource> {
    for_window: u64,
    wm: WindowId,
    browser: Browser<S>,
}

/// The session's picker engine over an injected directory-source factory
/// (`F` builds the session-authority source each pick starts from — the
/// live VFS listing calls in production, an in-memory tree in tests).
pub struct SessionPicker<S: DirectorySource, F: FnMut() -> S> {
    source: F,
    /// Root-first components of the directory each pick opens at — the
    /// user's home in production, so the picker starts among the user's own
    /// files rather than at the storage-forest root. Empty means the root
    /// `/`, which is also the fallback when the start directory cannot be
    /// listed.
    start: Vec<String>,
    active: Option<ActivePick<S>>,
}

impl<S: DirectorySource, F: FnMut() -> S> SessionPicker<S, F> {
    /// An idle picker over `source`, opening each pick at the root `/`.
    pub const fn new(source: F) -> Self {
        Self {
            source,
            start: Vec::new(),
            active: None,
        }
    }

    /// Open each pick at the directory named by root-first `start` instead of
    /// the root — the session points its picker at the logged-in user's home
    /// so the user lands among their own files. A start directory that cannot
    /// be listed when a pick begins falls back to the root rather than
    /// refusing the pick (see [`begin`](PickerSlot::begin)).
    #[must_use]
    pub fn starting_at(mut self, start: Vec<String>) -> Self {
        self.start = start;
        self
    }

    /// The compositor window of the showing picker, if one is active.
    /// The embedder routes this window's key and click input into
    /// [`handle_key`](Self::handle_key) / [`handle_click`](Self::handle_click)
    /// instead of the served-window channel.
    #[must_use]
    pub fn wm_id(&self) -> Option<WindowId> {
        self.active.as_ref().map(|active| active.wm)
    }

    /// Apply one key press to the showing picker.
    ///
    /// `Down`/`Up` move the selection, `Enter` descends into a selected
    /// directory or chooses a selected regular file, `Backspace` climbs
    /// to the parent, and `Escape` cancels. A refused navigation (an
    /// unreadable directory, an empty listing) changes nothing — the
    /// engine fails closed and the picker stays where it was.
    ///
    /// Returns the concluded pick once the user chose or cancelled; the
    /// picker window is already closed when it is returned.
    pub fn handle_key(
        &mut self,
        key: &KeyInput,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Option<ConcludedPick> {
        let KeyInput::Pressed { key, .. } = key else {
            return None;
        };
        match key {
            KeyValue::Named(NamedKeyCode::Down) => self.navigate(shell, compositor, |browser| {
                browser.select_next();
                NavOutcome::Redraw
            }),
            KeyValue::Named(NamedKeyCode::Up) => self.navigate(shell, compositor, |browser| {
                browser.select_previous();
                NavOutcome::Redraw
            }),
            KeyValue::Named(NamedKeyCode::Enter) => self.navigate(shell, compositor, |browser| {
                match browser.selected_index() {
                    Some(index) => open_or_choose(browser, index),
                    None => NavOutcome::None,
                }
            }),
            KeyValue::Named(NamedKeyCode::Backspace) => {
                self.navigate(shell, compositor, |browser| {
                    if browser.go_up().unwrap_or(false) {
                        NavOutcome::Redraw
                    } else {
                        NavOutcome::None
                    }
                })
            }
            KeyValue::Named(NamedKeyCode::Escape) => {
                self.conclude(shell, compositor, PickConclusion::Cancelled)
            }
            _ => None,
        }
    }

    /// Apply one primary-button press at the picker-window-local position
    /// `local`.
    ///
    /// A click on a toolbar command runs it (the read-only navigation the
    /// picker shares with the file manager — Back/Forward/Up/Refresh, the view
    /// toggle, and sort — through the one shared
    /// `tairix_browse::apply_command`); a click on an entry row resolves
    /// through the shared hit-test
    /// (`tairix_browse::render::entry_index_at` — exactly the rows the
    /// renderer drew): a directory row descends, a regular-file row
    /// chooses that file. A click on the path bar, a disabled tool, past the
    /// listing, or on an unresolvable coordinate changes nothing.
    pub fn handle_click(
        &mut self,
        local: Point,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Option<ConcludedPick> {
        // Hit-test with the same font and theme the picker renders with, so a
        // click resolves to exactly the item the user saw (list row or grid
        // tile), and a click on the path bar or the scrollbar gutter resolves
        // to nothing.
        let scale = compositor.scale();
        let theme = shell.session().active_theme();
        let font = picker_font(theme, scale);
        let viewport = Rect::new(
            0,
            0,
            scale.scale_length(WIN_WIDTH),
            scale.scale_length(WIN_HEIGHT),
        );
        // A toolbar command takes priority over the item area it sits above;
        // an enabled command runs, a disabled one resolves to nothing.
        if let Some(command) = self
            .active
            .as_ref()
            .and_then(|active| toolbar_command_at(&active.browser, theme, viewport, local))
        {
            return self.navigate(shell, compositor, move |browser| {
                match apply_command(browser, command) {
                    Ok(true) => NavOutcome::Redraw,
                    Ok(false) | Err(_) => NavOutcome::None,
                }
            });
        }
        let index = self
            .active
            .as_ref()
            .and_then(|active| entry_index_at(&active.browser, font, theme, viewport, local))?;
        self.navigate(shell, compositor, move |browser| {
            open_or_choose(browser, index)
        })
    }

    /// Run one navigation step against the active browser, repaint on a
    /// change, and conclude when the step chose a file.
    fn navigate(
        &mut self,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        step: impl FnOnce(&mut Browser<S>) -> NavOutcome,
    ) -> Option<ConcludedPick> {
        let active = self.active.as_mut()?;
        let scale = compositor.scale();
        match step(&mut active.browser) {
            NavOutcome::None => None,
            NavOutcome::Redraw => {
                // Keep the (possibly moved) selection on screen before the
                // repaint, scrolling the shared view the least it can.
                {
                    let theme = shell.session().active_theme();
                    reveal_selection(
                        &mut active.browser,
                        picker_font(theme, scale),
                        theme,
                        Rect::new(
                            0,
                            0,
                            scale.scale_length(WIN_WIDTH),
                            scale.scale_length(WIN_HEIGHT),
                        ),
                    );
                }
                redraw(&active.browser, active.wm, shell, compositor);
                None
            }
            NavOutcome::Chosen(path) => {
                self.conclude(shell, compositor, PickConclusion::Chosen(path))
            }
        }
    }

    /// Close the picker window and hand the conclusion to the embedder.
    fn conclude(
        &mut self,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        conclusion: PickConclusion,
    ) -> Option<ConcludedPick> {
        let active = self.active.take()?;
        let _ = shell.close_window(compositor, active.wm);
        Some(ConcludedPick {
            for_window: active.for_window,
            conclusion,
        })
    }
}

impl<S: DirectorySource, F: FnMut() -> S> PickerSlot for SessionPicker<S, F> {
    fn begin(
        &mut self,
        for_window: u64,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Result<(), Errno> {
        if self.active.is_some() {
            return Err(Errno::AlreadyExists);
        }
        // List the start directory under the session's own authority before
        // any UI state exists. The picker opens at the user's home; a home
        // that cannot be listed (missing, or its capability refused) falls
        // back to the root rather than refusing the pick, so the user can
        // still choose a file. Only when the root itself cannot be listed is
        // the whole pick refused (fail closed, nothing half-open).
        let browser = match Browser::open_at((self.source)(), self.start.clone()) {
            Ok(browser) => browser,
            Err(_) if !self.start.is_empty() => Browser::open_root((self.source)())
                .map_err(|err| err.source_errno().unwrap_or(Errno::PermissionDenied))?,
            Err(err) => {
                return Err(err.source_errno().unwrap_or(Errno::PermissionDenied));
            }
        };
        let surface =
            render_surface(&browser, compositor.scale(), shell).ok_or(Errno::LengthOutOfRange)?;
        let wm = shell
            .open_window(compositor, PICKER_ORIGIN, surface, PICKER_TITLE)
            .ok_or(Errno::NoSpace)?;
        self.active = Some(ActivePick {
            for_window,
            wm,
            browser,
        });
        Ok(())
    }

    fn abort_for(&mut self, window_id: u64, shell: &mut DesktopShell, compositor: &mut Compositor) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.for_window == window_id)
        {
            let _ = self.conclude(shell, compositor, PickConclusion::Cancelled);
        }
    }
}

/// What one navigation step did.
enum NavOutcome {
    /// Nothing changed (a refused move, an unresolvable click).
    None,
    /// The view changed; repaint the picker window.
    Redraw,
    /// The user chose the regular file at this absolute path.
    Chosen(String),
}

/// Descend into the entry at `index` when it is a directory, or choose it
/// when it is a regular file — the one open-or-choose rule the Enter key
/// and the row click share.
fn open_or_choose<S: DirectorySource>(browser: &mut Browser<S>, index: usize) -> NavOutcome {
    let Some(entry) = browser.entries().get(index) else {
        return NavOutcome::None;
    };
    if entry.is_directory() {
        return match browser.open_index(index) {
            Ok(()) => NavOutcome::Redraw,
            // A refused descent (unreadable directory) changes nothing.
            Err(_) => NavOutcome::None,
        };
    }
    // Spell the chosen file's absolute path through the one shared
    // spelling; a malformed name refuses the choice rather than guessing.
    let mut components: Vec<String> = browser.components().to_vec();
    components.push(String::from(entry.name()));
    match vfs::absolute_path(&components) {
        Ok(path) => NavOutcome::Chosen(path),
        Err(_) => NavOutcome::None,
    }
}

/// Paint the picker's current listing at the shared browser-view
/// physical geometry through the active theme.
fn render_surface<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    shell: &DesktopShell,
) -> Option<tairix_wm::Surface> {
    let theme = shell.session().active_theme();
    // The picker is strictly read-only, so it draws no manager chrome at all:
    // no write tools (New Folder, the Trash location, and Empty Trash are the
    // file manager's alone — no write authority here) and no places rail (a
    // pick is bounded to the tree the requesting application was authorised to
    // be shown, and one-click jumps to arbitrary volumes would widen it).
    // The picker has no per-entry artwork cache yet, so it resolves every grid
    // tile to its built-in glyph through the always-empty artwork lookup; a
    // later change gives it a real cache.
    let w = scale.scale_length(WIN_WIDTH);
    let h = scale.scale_length(WIN_HEIGHT);
    render(
        browser,
        theme,
        picker_font(theme, scale),
        Rect::new(0, 0, w, h),
        &ManagerChrome::none(),
        &mut NoArtwork,
    )
}

/// The picker's text font: the theme's ordinary interface-text role resolved
/// through the one shared role-to-font conversion, so the picker's rows are
/// sized and weighted exactly like every other list of interface text.
///
/// It is the one place the render and hit-test paths agree on a font,
/// resolved at the density of the output it is drawn to.
pub(crate) fn picker_font(theme: &Theme, scale: Scale) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
}

/// Repaint the picker window after a navigation change. A surface that
/// cannot be allocated leaves the previous frame on screen (fail closed,
/// never a panic).
fn redraw<S: DirectorySource>(
    browser: &Browser<S>,
    wm: WindowId,
    shell: &mut DesktopShell,
    compositor: &mut Compositor,
) {
    if let Some(surface) = render_surface(browser, compositor.scale(), shell) {
        let _ = compositor.set_surface(wm, surface);
    }
}
