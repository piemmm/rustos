//! TAIRiX **wallpaper chooser** — the graphical application that edits the
//! desktop pinboard settings (`plans/PINBOARD.md` §8, deliverable P9).
//!
//! The chooser lets the user pick the desktop backdrop image (or none), how
//! it is fitted to the screen, the flat colour shown wherever the wallpaper
//! does not reach, where the desktop icon grid grows from, and how the
//! `Desktop` folder is sorted, then asks the desktop session to adopt the
//! change (`plans/PINBOARD.md` §6). It never applies a change itself: the
//! session decides, applies, and persists.
//!
//! # What this crate is
//!
//! The host-testable chooser engine the `Run` binary composes:
//!
//! * [`Chooser`] — the model: the candidate list (built from a directory
//!   listing through [`candidates_from_catalog`]), the current selection,
//!   the fit / backdrop / icon-flow / sort choices, and which region holds
//!   keyboard focus ([`Focus`]). It performs no I/O and holds no authority:
//!   every thumbnail arrives already rendered ([`Chooser::set_thumbnail`])
//!   or refused ([`Chooser::mark_thumbnail_refused`]) by the caller, which
//!   is the only thing that may talk to the parser sandbox.
//! * [`Layout`] — the pure window-geometry function every render and every
//!   hit-test agrees on, so a resize can never leave two regions
//!   overlapping or a control drawn outside the window.
//! * [`Chooser::render`] — the themed painter over the shared `lib/font`
//!   face, `lib/raster` [`Surface`], and the `lib/controls` selector/button
//!   family — no new control family is defined here.
//! * [`Chooser::settings_document`] — the exact rendered settings document
//!   (`lib/wallpaper`'s own grammar) the current UI state means, ready to
//!   post to the desktop session.
//!
//! # Keyboard, not pointer
//!
//! Like the file viewer, the chooser is driven by the keyboard
//! ([`Chooser::handle_key`]): arrows move within the thumbnail grid or
//! cycle the focused option group, Tab/Shift-Tab move focus between the
//! grid, the four option groups, and the Apply/Close actions, Enter
//! applies, and Escape closes. Pointer events are accepted by the `Run`
//! binary but drive nothing, exactly as the viewer's are — a deliberate,
//! charter-consistent scope, not an oversight.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); depends only on the audited `lib/abi` crate and
//! the shared `lib/*` desktop libraries — never a kernel, driver, or
//! window-manager crate. No `unsafe` in this engine, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_controls::button::Button;
use tairix_controls::scroll::{ScrollModel, ScrollRange};
use tairix_controls::selector::Radio;
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;
use tairix_wallpaper::{
    Backdrop, CatalogEntry, IconFlow, IconSort, PinboardSettings, Rgb, WallpaperChoice,
    WallpaperFit, WallpaperPath, WALLPAPER_STORE,
};

/// Initial window content width of the chooser window, in pixels. The
/// chooser re-lays its grid and rows out to whatever client size the window
/// manager reports ([`Layout::compute`]), so this is a starting size, not a
/// fixed one.
pub const WIN_WIDTH: u32 = 640;

/// Initial window content height of the chooser window, in pixels (see
/// [`WIN_WIDTH`]).
pub const WIN_HEIGHT: u32 = 520;

/// The smallest client width the chooser draws into, in pixels — a floor so
/// a resize-to-nothing still shows a usable (if cramped) window rather than
/// a zero-sized surface.
pub const MIN_WIN_WIDTH: u32 = 280;

/// The smallest client height the chooser draws into, in pixels: the fixed
/// rows the layout claims from the bottom, plus a whole thumbnail row above
/// them, so the floor still shows one candidate rather than a sliver of one
/// (see [`MIN_WIN_WIDTH`]).
pub const MIN_WIN_HEIGHT: u32 = 300;

/// The label shown for the "no wallpaper" candidate.
pub const NONE_LABEL: &str = "No wallpaper";

/// Every [`WallpaperFit`] value, in the order the fit option row offers
/// them.
pub const FIT_ALL: [WallpaperFit; 5] = [
    WallpaperFit::Fill,
    WallpaperFit::Fit,
    WallpaperFit::Stretch,
    WallpaperFit::Centre,
    WallpaperFit::Tile,
];

/// Every [`IconFlow`] value, in the order the arrangement option row offers
/// them.
pub const ICON_FLOW_ALL: [IconFlow; 2] = [IconFlow::Leading, IconFlow::Trailing];

/// Every [`IconSort`] value, in the order the sort option row offers them.
pub const SORT_ALL: [IconSort; 4] = [
    IconSort::Name,
    IconSort::Kind,
    IconSort::Size,
    IconSort::Date,
];

/// The backdrop colours the backdrop option row offers: the active theme's
/// own desktop colour first, then a small fixed palette of named flat
/// colours.
///
/// A named palette rather than a free-form colour entry: the settings
/// document's backdrop is one opaque `rrggbb` value, and a closed set is a
/// complete keyboard-driven choice with no text field to validate. A
/// backdrop already in effect that this palette does not carry is still
/// offered — [`backdrop_options`] appends it under its own bare `rrggbb`
/// spelling — so opening the chooser never quietly changes the colour that
/// is already on screen.
pub const BACKDROP_PALETTE: [(&str, Backdrop); 6] = [
    ("Theme", Backdrop::Theme),
    ("Black", Backdrop::Colour(Rgb::new(0x00, 0x00, 0x00))),
    ("Slate", Backdrop::Colour(Rgb::new(0x2e, 0x34, 0x40))),
    ("Ocean", Backdrop::Colour(Rgb::new(0x1b, 0x3a, 0x5c))),
    ("Moss", Backdrop::Colour(Rgb::new(0x2c, 0x40, 0x2c))),
    ("Linen", Backdrop::Colour(Rgb::new(0xe8, 0xe0, 0xd8))),
];

/// The window margin, in pixels, around the whole layout.
const MARGIN: u32 = 8;

/// Gap, in pixels, left between stacked layout regions.
const ROW_GAP: u32 = 4;

/// One rendered thumbnail's width, in pixels — also the destination width
/// the `Run` binary asks the sandbox to render at.
pub const THUMB_WIDTH: u32 = 96;

/// One rendered thumbnail's height, in pixels (see [`THUMB_WIDTH`]).
pub const THUMB_HEIGHT: u32 = 64;

/// Padding, in pixels, between a grid cell's border and its thumbnail.
const CELL_PADDING: u32 = 6;

/// Height, in pixels, reserved below a thumbnail for its label.
const LABEL_HEIGHT: u32 = 14;

/// One grid cell's width, in pixels: the thumbnail plus padding on both
/// sides.
const CELL_WIDTH: u32 = THUMB_WIDTH + CELL_PADDING * 2;

/// One grid cell's height, in pixels: the thumbnail, its label, and padding.
const CELL_HEIGHT: u32 = THUMB_HEIGHT + LABEL_HEIGHT + CELL_PADDING * 2;

/// Thickness, in pixels, of the selection border drawn around a selected
/// grid cell.
const SELECTION_BORDER: u32 = 2;

/// The word drawn across a candidate the sandbox refused, so a refused tile
/// says why it is not artwork instead of looking like one still loading.
const REFUSED_MARKER: &str = "unreadable";

/// Height, in pixels, of one option row (a group of [`Radio`] selectors).
const OPTION_ROW_HEIGHT: u32 = 24;

/// Height, in pixels, of the apply-outcome status line.
const STATUS_HEIGHT: u32 = 16;

/// Height, in pixels, of the Apply/Close button row.
const BUTTON_ROW_HEIGHT: u32 = 28;

/// The preferred width, in pixels, of the Apply and Close buttons, clamped
/// to whatever the window can actually offer without the two overlapping.
const BUTTON_WIDTH: u32 = 84;

/// One candidate a chooser may offer: the shipped "no wallpaper" backdrop
/// entry, or one wallpaper image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// The wallpaper choice this candidate selects.
    pub choice: WallpaperChoice,
    /// The display label (the catalog file name, or [`NONE_LABEL`]).
    pub label: String,
    /// The candidate's current thumbnail lifecycle state.
    pub thumbnail: Thumbnail,
}

/// A candidate thumbnail's lifecycle.
///
/// A candidate whose thumbnail will not render shows a placeholder and its
/// name — never a blank tile, never a crash — and a refusal is remembered
/// ([`Thumbnail::Refused`]) so a bad file costs one attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Thumbnail {
    /// The "no wallpaper" entry: painted from the current backdrop colour,
    /// resolved at render time (the active theme is needed when the
    /// backdrop is [`Backdrop::Theme`]) — never sandboxed, since it decodes
    /// nothing.
    Backdrop,
    /// Not yet requested from the sandbox.
    Pending,
    /// Rendered successfully by the sandbox at [`THUMB_WIDTH`] x
    /// [`THUMB_HEIGHT`].
    Ready(Surface),
    /// The sandbox refused this wallpaper once; it will not be retried this
    /// session.
    Refused,
}

impl Candidate {
    /// The "no wallpaper" entry: always first, always present.
    #[must_use]
    fn none_entry() -> Self {
        Self {
            choice: WallpaperChoice::None,
            label: String::from(NONE_LABEL),
            thumbnail: Thumbnail::Backdrop,
        }
    }

    /// A pending image candidate at the shipped store's `path`.
    #[must_use]
    fn image(path: WallpaperPath, label: String) -> Self {
        Self {
            choice: WallpaperChoice::Image(path),
            label,
            thumbnail: Thumbnail::Pending,
        }
    }
}

/// Build the image candidates a chooser may offer from a wallpaper store
/// listing, discovered by [`tairix_wallpaper::catalog::catalog_entries`].
///
/// Every entry becomes a [`Candidate`] naming the wallpaper at
/// `<WALLPAPER_STORE>/<entry.name>`, with a [`Thumbnail::Pending`] state:
/// the caller renders each one through the sandbox and reports the result
/// with [`Chooser::set_thumbnail`] / [`Chooser::mark_thumbnail_refused`]. An
/// entry whose name somehow fails to parse as a wallpaper path (impossible
/// for anything [`tairix_wallpaper::catalog::catalog_entries`] itself
/// already validated, but never assumed here) is silently dropped rather
/// than fabricating a candidate that could not be applied.
#[must_use]
pub fn candidates_from_catalog(entries: &[CatalogEntry]) -> Vec<Candidate> {
    entries
        .iter()
        .filter_map(|entry| {
            let full_path = alloc::format!("{WALLPAPER_STORE}/{}", entry.name);
            let path = WallpaperPath::new(&full_path).ok()?;
            Some(Candidate::image(path, entry.name.clone()))
        })
        .collect()
}

/// One backdrop the backdrop option row offers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackdropOption {
    /// The display label.
    pub label: String,
    /// The backdrop this option selects.
    pub backdrop: Backdrop,
}

/// The backdrop options to offer a user whose settings currently name
/// `current`: [`BACKDROP_PALETTE`], plus `current` itself under its bare
/// `rrggbb` spelling when the palette does not carry it.
///
/// The returned list therefore always contains `current`, so a chooser
/// opened on any settings document can show the colour that is in effect.
#[must_use]
pub fn backdrop_options(current: Backdrop) -> Vec<BackdropOption> {
    let mut options: Vec<BackdropOption> = BACKDROP_PALETTE
        .iter()
        .map(|(label, backdrop)| BackdropOption {
            label: String::from(*label),
            backdrop: *backdrop,
        })
        .collect();
    if let Some(label) = unlisted_backdrop_label(current, &options) {
        options.push(BackdropOption {
            label,
            backdrop: current,
        });
    }
    options
}

/// The bare `rrggbb` label a backdrop in effect needs when `offered` does
/// not already carry it, or `None` when it is already on the list.
fn unlisted_backdrop_label(current: Backdrop, offered: &[BackdropOption]) -> Option<String> {
    match current {
        Backdrop::Theme => None,
        Backdrop::Colour(rgb) => offered
            .iter()
            .all(|option| option.backdrop != current)
            .then(|| rgb.to_hex()),
    }
}

/// The last path segment of `path`, for a candidate label.
fn leaf_name(path: &WallpaperPath) -> String {
    path.as_str()
        .rsplit('/')
        .next()
        .unwrap_or_else(|| path.as_str())
        .to_string()
}

/// Which region of the chooser window holds keyboard focus.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Focus {
    /// The thumbnail grid.
    Grid,
    /// The fit option row.
    Fit,
    /// The backdrop-colour option row.
    Backdrop,
    /// The icon-arrangement option row.
    Icons,
    /// The sort-order option row.
    Sort,
    /// The Apply button.
    Apply,
    /// The Close button.
    Close,
}

impl Focus {
    /// The fixed tab order every [`Chooser::handle_key`] Tab/Shift-Tab moves
    /// through.
    const ORDER: [Self; 7] = [
        Self::Grid,
        Self::Fit,
        Self::Backdrop,
        Self::Icons,
        Self::Sort,
        Self::Apply,
        Self::Close,
    ];

    /// This region's position in [`Self::ORDER`].
    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|region| *region == self)
            .unwrap_or(0)
    }

    /// The next region, wrapping from [`Self::Close`] back to
    /// [`Self::Grid`].
    #[must_use]
    fn next(self) -> Self {
        Self::ORDER[(self.index() + 1) % Self::ORDER.len()]
    }

    /// The previous region, wrapping from [`Self::Grid`] back to
    /// [`Self::Close`].
    #[must_use]
    fn prev(self) -> Self {
        let len = Self::ORDER.len();
        Self::ORDER[(self.index() + len - 1) % len]
    }
}

/// A named arrow-key direction, decoupled from the four
/// [`tairix_abi::input::NamedKeyCode`] arrow variants so cycling an option
/// group treats Left/Up and Right/Down alike.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Direction {
    /// Toward the previous item.
    Backward,
    /// Toward the next item.
    Forward,
}

impl Direction {
    /// The direction an arrow key names, or `None` for any other key.
    fn from_key(key: tairix_abi::input::NamedKeyCode) -> Option<Self> {
        use tairix_abi::input::NamedKeyCode;
        match key {
            NamedKeyCode::Left | NamedKeyCode::Up => Some(Self::Backward),
            NamedKeyCode::Right | NamedKeyCode::Down => Some(Self::Forward),
            _ => None,
        }
    }

    /// The next index in a cyclic group of `len` items, stepping from
    /// `index` in this direction. `len == 0` steps nowhere.
    fn step(self, index: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        match self {
            Self::Backward => (index + len - 1) % len,
            Self::Forward => (index + 1) % len,
        }
    }
}

/// The outcome of asking the desktop session to adopt a rendered settings
/// document (`plans/PINBOARD.md` §6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    /// The session adopted and persisted the change.
    Applied,
    /// The session refused the request, with the reason it gave.
    Refused(String),
    /// No desktop session answered the pinboard rendezvous at all (the
    /// [`PINBOARD_ENDPOINT`](tairix_abi::pinboard_ipc::PINBOARD_ENDPOINT)
    /// call itself failed) — distinct from an authenticated refusal.
    NoDesktop,
}

/// The result of feeding a key to [`Chooser::handle_key`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChooserAction {
    /// Nothing changed; no repaint is needed.
    None,
    /// State changed; the caller should repaint.
    Changed,
    /// The user asked to apply: render [`Chooser::settings_document`] and
    /// send it to the desktop session.
    Apply,
    /// The user asked to close the window.
    Close,
}

/// The pure window-geometry the chooser draws into and hit-tests against.
///
/// Every region is computed from the bottom of the window upward so that,
/// however small the window, the fixed-height rows never draw outside the
/// window bounds — only the thumbnail grid at the top shrinks (down to
/// empty) to make room.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    grid: Rect,
    fit_row: Rect,
    backdrop_row: Rect,
    icons_row: Rect,
    sort_row: Rect,
    status: Rect,
    apply_button: Rect,
    close_button: Rect,
}

impl Layout {
    /// Compute the layout for a `width` x `height` client area.
    #[must_use]
    pub fn compute(width: u32, height: u32) -> Self {
        let content_w = width.saturating_sub(MARGIN * 2);

        // Claim height from the bottom of the window upward: each row takes
        // only what is left above the rows already claimed, so however
        // small the window, no fixed row is ever placed outside it and only
        // the grid at the top shrinks.
        let mut top = height;
        let (buttons_y, button_h) = claim_up(&mut top, BUTTON_ROW_HEIGHT);
        claim_up(&mut top, ROW_GAP);
        let (status_y, status_h) = claim_up(&mut top, STATUS_HEIGHT);
        claim_up(&mut top, ROW_GAP);
        let (sort_y, sort_h) = claim_up(&mut top, OPTION_ROW_HEIGHT);
        claim_up(&mut top, ROW_GAP);
        let (icons_y, icons_h) = claim_up(&mut top, OPTION_ROW_HEIGHT);
        claim_up(&mut top, ROW_GAP);
        let (backdrop_y, backdrop_h) = claim_up(&mut top, OPTION_ROW_HEIGHT);
        claim_up(&mut top, ROW_GAP);
        let (fit_y, fit_h) = claim_up(&mut top, OPTION_ROW_HEIGHT);
        claim_up(&mut top, ROW_GAP);

        let grid_bottom = top;
        let grid_y = MARGIN.min(grid_bottom);
        let grid_h = grid_bottom.saturating_sub(grid_y);

        let row = |y: u32, h: u32| Rect::new(to_i32(MARGIN), to_i32(y), content_w, h);

        let button_w = BUTTON_WIDTH.min(content_w.saturating_sub(ROW_GAP) / 2);
        let buttons_row = row(buttons_y, button_h);
        let close_button = Rect::new(
            buttons_row.right() - to_i32(button_w),
            to_i32(buttons_y),
            button_w,
            button_h,
        );
        let apply_button = Rect::new(
            close_button.left() - to_i32(button_w.saturating_add(ROW_GAP)),
            to_i32(buttons_y),
            button_w,
            button_h,
        );

        Self {
            grid: row(grid_y, grid_h),
            fit_row: row(fit_y, fit_h),
            backdrop_row: row(backdrop_y, backdrop_h),
            icons_row: row(icons_y, icons_h),
            sort_row: row(sort_y, sort_h),
            status: row(status_y, status_h),
            apply_button,
            close_button,
        }
    }

    /// The thumbnail grid region.
    #[must_use]
    pub fn grid(&self) -> Rect {
        self.grid
    }

    /// The fit option row.
    #[must_use]
    pub fn fit_row(&self) -> Rect {
        self.fit_row
    }

    /// The backdrop-colour option row.
    #[must_use]
    pub fn backdrop_row(&self) -> Rect {
        self.backdrop_row
    }

    /// The icon-arrangement option row.
    #[must_use]
    pub fn icons_row(&self) -> Rect {
        self.icons_row
    }

    /// The sort-order option row.
    #[must_use]
    pub fn sort_row(&self) -> Rect {
        self.sort_row
    }

    /// The apply-outcome status line.
    #[must_use]
    pub fn status(&self) -> Rect {
        self.status
    }

    /// The Apply button.
    #[must_use]
    pub fn apply_button(&self) -> Rect {
        self.apply_button
    }

    /// The Close button.
    #[must_use]
    pub fn close_button(&self) -> Rect {
        self.close_button
    }

    /// Every named region, for a test to check non-overlap and containment
    /// against.
    #[cfg(test)]
    fn regions(&self) -> [Rect; 8] {
        [
            self.grid,
            self.fit_row,
            self.backdrop_row,
            self.icons_row,
            self.sort_row,
            self.status,
            self.apply_button,
            self.close_button,
        ]
    }
}

/// Claim a `wanted`-tall row immediately above `top`, moving `top` up to
/// the row's own top edge.
///
/// Returns the row's y coordinate and the height it actually got, which is
/// clamped to the room left above it — so a row is never placed outside the
/// window and the claims can never sum past the window height.
fn claim_up(top: &mut u32, wanted: u32) -> (u32, u32) {
    let taken = wanted.min(*top);
    *top = top.saturating_sub(taken);
    (*top, taken)
}

/// The wallpaper chooser's engine: the candidate list, the current
/// selection and option choices, and which region holds keyboard focus.
#[derive(Clone, Debug, PartialEq)]
pub struct Chooser {
    candidates: Vec<Candidate>,
    selected: usize,
    columns: usize,
    scroll: ScrollModel,
    fit: WallpaperFit,
    icons: IconFlow,
    sort: IconSort,
    backdrops: Vec<BackdropOption>,
    backdrop: usize,
    focus: Focus,
    apply_outcome: Option<ApplyOutcome>,
}

impl Chooser {
    /// Build a chooser from a catalog listing and the settings the user
    /// currently has in effect, so the UI opens on what is actually
    /// applied.
    ///
    /// The "no wallpaper" entry is always first. When the current
    /// wallpaper is one of `catalog`'s candidates, it starts selected; when
    /// it names a file outside the discovered listing (e.g. one applied by
    /// an older or foreign tool), a synthetic pending candidate for it is
    /// appended and selected, so the chooser never silently disagrees with
    /// what is on screen.
    #[must_use]
    pub fn new(catalog: Vec<Candidate>, settings: &PinboardSettings) -> Self {
        let mut candidates = Vec::with_capacity(catalog.len() + 2);
        candidates.push(Candidate::none_entry());

        let selected = match &settings.wallpaper {
            WallpaperChoice::None => {
                candidates.extend(catalog);
                0
            }
            WallpaperChoice::Image(path) => {
                let found = catalog.iter().position(
                    |candidate| matches!(&candidate.choice, WallpaperChoice::Image(p) if p == path),
                );
                candidates.extend(catalog);
                if let Some(index) = found {
                    index + 1
                } else {
                    candidates.push(Candidate::image(path.clone(), leaf_name(path)));
                    candidates.len() - 1
                }
            }
        };

        let backdrops = backdrop_options(settings.backdrop);
        let backdrop = backdrops
            .iter()
            .position(|option| option.backdrop == settings.backdrop)
            .unwrap_or(0);
        let mut chooser = Self {
            candidates,
            selected,
            columns: 1,
            scroll: ScrollModel::new(ScrollRange::new(1, 1, 0), 1, 1),
            fit: settings.fit,
            icons: settings.icons,
            sort: settings.sort,
            backdrops,
            backdrop,
            focus: Focus::Grid,
            apply_outcome: None,
        };
        chooser.relayout(WIN_WIDTH, WIN_HEIGHT);
        chooser
    }

    /// Every candidate, in display order (the "no wallpaper" entry first).
    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// The index of the currently selected candidate.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The currently focused region.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// The currently chosen fit.
    #[must_use]
    pub fn fit(&self) -> WallpaperFit {
        self.fit
    }

    /// The currently chosen icon arrangement.
    #[must_use]
    pub fn icons(&self) -> IconFlow {
        self.icons
    }

    /// The currently chosen sort order.
    #[must_use]
    pub fn sort(&self) -> IconSort {
        self.sort
    }

    /// Every backdrop the backdrop option row offers, in display order.
    #[must_use]
    pub fn backdrops(&self) -> &[BackdropOption] {
        &self.backdrops
    }

    /// The currently chosen backdrop.
    #[must_use]
    pub fn backdrop(&self) -> Backdrop {
        self.backdrops
            .get(self.backdrop)
            .map_or(Backdrop::Theme, |option| option.backdrop)
    }

    /// The most recent apply outcome, or `None` before any apply attempt.
    #[must_use]
    pub fn apply_outcome(&self) -> Option<&ApplyOutcome> {
        self.apply_outcome.as_ref()
    }

    /// Record the outcome of an apply attempt, so it is reported in the
    /// window rather than silently dropped.
    pub fn set_apply_outcome(&mut self, outcome: ApplyOutcome) {
        self.apply_outcome = Some(outcome);
    }

    /// The index of the first candidate still awaiting a thumbnail, or
    /// `None` when every candidate is resolved (ready or refused) — the
    /// `Run` binary drives the sandbox one candidate at a time from this.
    #[must_use]
    pub fn next_pending(&self) -> Option<usize> {
        self.candidates
            .iter()
            .position(|candidate| candidate.thumbnail == Thumbnail::Pending)
    }

    /// The path of the candidate at `index`, for the caller to read and
    /// hand to the sandbox — `None` for the "no wallpaper" entry or an
    /// out-of-range index.
    #[must_use]
    pub fn candidate_path(&self, index: usize) -> Option<&WallpaperPath> {
        match self.candidates.get(index).map(|c| &c.choice) {
            Some(WallpaperChoice::Image(path)) => Some(path),
            _ => None,
        }
    }

    /// Record a successfully rendered thumbnail for the candidate at
    /// `index`. Out of range is a no-op (the candidate list cannot have
    /// shrunk beneath the caller, but this stays total rather than
    /// assuming it).
    pub fn set_thumbnail(&mut self, index: usize, surface: Surface) {
        if let Some(candidate) = self.candidates.get_mut(index) {
            candidate.thumbnail = Thumbnail::Ready(surface);
        }
    }

    /// Record that the candidate at `index` will not decode, so it is not
    /// retried this session.
    pub fn mark_thumbnail_refused(&mut self, index: usize) {
        if let Some(candidate) = self.candidates.get_mut(index) {
            candidate.thumbnail = Thumbnail::Refused;
        }
    }

    /// Return every rendered thumbnail to [`Thumbnail::Pending`] so the
    /// caller re-renders it under the newly chosen fit — the previews show
    /// what the desktop will actually do, so they cannot outlive the fit
    /// they were rendered for.
    ///
    /// A candidate the sandbox already refused stays [`Thumbnail::Refused`]:
    /// a file that will not decode under one fit will not decode under
    /// another, so a bad file still costs exactly one attempt.
    fn invalidate_thumbnails(&mut self) {
        for candidate in &mut self.candidates {
            if matches!(candidate.thumbnail, Thumbnail::Ready(_)) {
                candidate.thumbnail = Thumbnail::Pending;
            }
        }
    }

    /// Re-lay the grid out for a `width` x `height` client area, updating
    /// the column count and the scroll viewport and keeping the selection
    /// on screen. Called once at construction and again on every resize.
    pub fn relayout(&mut self, width: u32, height: u32) {
        let grid = Layout::compute(width, height).grid();
        self.columns = usize::try_from(grid.width / CELL_WIDTH).unwrap_or(0).max(1);
        let visible_rows = usize::try_from(grid.height / CELL_HEIGHT)
            .unwrap_or(0)
            .max(1);
        let rows = row_count(self.candidates.len(), self.columns);
        let offset = self.scroll.offset().min(rows.saturating_sub(1) as u64);
        let page = visible_rows.saturating_sub(1).max(1) as u64;
        self.scroll = ScrollModel::new(
            ScrollRange::new(rows as u64, visible_rows as u64, offset),
            1,
            page,
        );
        self.ensure_selected_visible();
    }

    /// The visible row window's first row and row count, for a renderer.
    fn visible_rows(&self) -> (usize, usize) {
        let first = usize::try_from(self.scroll.offset()).unwrap_or(0);
        let count = usize::try_from(self.scroll.range().viewport_extent()).unwrap_or(0);
        (first, count)
    }

    /// Scroll the grid so the selected candidate's row is on screen.
    fn ensure_selected_visible(&mut self) {
        let columns = self.columns.max(1);
        let selected_row = (self.selected / columns) as u64;
        let offset = self.scroll.offset();
        let viewport = self.scroll.range().viewport_extent();
        if selected_row < offset {
            let delta =
                i64::try_from(selected_row).unwrap_or(0) - i64::try_from(offset).unwrap_or(0);
            self.scroll = self.scroll.scroll_by(delta);
        } else if viewport > 0 && selected_row >= offset.saturating_add(viewport) {
            let target = selected_row + 1 - viewport;
            let delta = i64::try_from(target).unwrap_or(0) - i64::try_from(offset).unwrap_or(0);
            self.scroll = self.scroll.scroll_by(delta);
        }
    }

    /// Feed one key press to the chooser.
    ///
    /// Tab/Shift-Tab move focus between the grid, the four option groups,
    /// and the Apply/Close actions; arrows move within the focused grid or
    /// cycle the focused option group. Enter activates the focused action —
    /// a close when the Close button holds focus, an apply from anywhere
    /// else — and Escape closes from anywhere. Any other key is a no-op.
    #[must_use]
    pub fn handle_key(
        &mut self,
        key: tairix_abi::input::NamedKeyCode,
        shift: bool,
    ) -> ChooserAction {
        use tairix_abi::input::NamedKeyCode;
        match key {
            NamedKeyCode::Tab => {
                self.focus = if shift {
                    self.focus.prev()
                } else {
                    self.focus.next()
                };
                ChooserAction::Changed
            }
            NamedKeyCode::Enter => match self.focus {
                Focus::Close => ChooserAction::Close,
                _ => ChooserAction::Apply,
            },
            NamedKeyCode::Escape => ChooserAction::Close,
            NamedKeyCode::Left | NamedKeyCode::Right | NamedKeyCode::Up | NamedKeyCode::Down => {
                if self.navigate(key) {
                    ChooserAction::Changed
                } else {
                    ChooserAction::None
                }
            }
            _ => ChooserAction::None,
        }
    }

    /// Apply an arrow key according to the currently focused region.
    /// Returns whether anything actually changed.
    fn navigate(&mut self, key: tairix_abi::input::NamedKeyCode) -> bool {
        match self.focus {
            Focus::Grid => self.navigate_grid(key),
            Focus::Fit => self.cycle_fit(key),
            Focus::Backdrop => self.cycle_backdrop(key),
            Focus::Icons => self.cycle_icons(key),
            Focus::Sort => self.cycle_sort(key),
            Focus::Apply | Focus::Close => false,
        }
    }

    /// Move the grid selection one cell in the arrow key's direction,
    /// clamped to the candidate list (a `Down` past an incomplete final row
    /// lands on the last candidate rather than doing nothing).
    fn navigate_grid(&mut self, key: tairix_abi::input::NamedKeyCode) -> bool {
        use tairix_abi::input::NamedKeyCode;
        let len = self.candidates.len();
        if len == 0 {
            return false;
        }
        let columns = self.columns.max(1);
        let old = self.selected;
        let new = match key {
            NamedKeyCode::Left => old.checked_sub(1),
            NamedKeyCode::Right => (old + 1 < len).then_some(old + 1),
            NamedKeyCode::Up => old.checked_sub(columns),
            NamedKeyCode::Down => {
                let candidate = old.saturating_add(columns);
                if candidate < len {
                    Some(candidate)
                } else {
                    let last_row = (len - 1) / columns;
                    let old_row = old / columns;
                    (old_row < last_row).then_some(len - 1)
                }
            }
            _ => None,
        };
        match new {
            Some(index) if index != old && index < len => {
                self.selected = index;
                self.ensure_selected_visible();
                true
            }
            _ => false,
        }
    }

    /// Cycle the fit option in the arrow key's direction. Returns `false`
    /// for a non-arrow key.
    fn cycle_fit(&mut self, key: tairix_abi::input::NamedKeyCode) -> bool {
        let Some(direction) = Direction::from_key(key) else {
            return false;
        };
        let index = FIT_ALL.iter().position(|f| *f == self.fit).unwrap_or(0);
        self.fit = FIT_ALL[direction.step(index, FIT_ALL.len())];
        self.invalidate_thumbnails();
        true
    }

    /// Cycle the backdrop option; see [`Self::cycle_fit`].
    fn cycle_backdrop(&mut self, key: tairix_abi::input::NamedKeyCode) -> bool {
        let Some(direction) = Direction::from_key(key) else {
            return false;
        };
        self.backdrop = direction.step(self.backdrop, self.backdrops.len());
        true
    }

    /// Cycle the icon-arrangement option; see [`Self::cycle_fit`].
    fn cycle_icons(&mut self, key: tairix_abi::input::NamedKeyCode) -> bool {
        let Some(direction) = Direction::from_key(key) else {
            return false;
        };
        let index = ICON_FLOW_ALL
            .iter()
            .position(|f| *f == self.icons)
            .unwrap_or(0);
        self.icons = ICON_FLOW_ALL[direction.step(index, ICON_FLOW_ALL.len())];
        true
    }

    /// Cycle the sort-order option; see [`Self::cycle_fit`].
    fn cycle_sort(&mut self, key: tairix_abi::input::NamedKeyCode) -> bool {
        let Some(direction) = Direction::from_key(key) else {
            return false;
        };
        let index = SORT_ALL.iter().position(|s| *s == self.sort).unwrap_or(0);
        self.sort = SORT_ALL[direction.step(index, SORT_ALL.len())];
        true
    }

    /// The settings the current UI state means: the selected candidate's
    /// choice and the chosen fit, backdrop, icon flow, and sort order.
    #[must_use]
    pub fn to_settings(&self) -> PinboardSettings {
        let wallpaper = self
            .candidates
            .get(self.selected)
            .map_or(WallpaperChoice::None, |candidate| candidate.choice.clone());
        PinboardSettings {
            wallpaper,
            fit: self.fit,
            backdrop: self.backdrop(),
            icons: self.icons,
            sort: self.sort,
        }
    }

    /// Render [`Self::to_settings`] as the canonical document text, ready
    /// to post to the desktop session (`plans/PINBOARD.md` §6).
    #[must_use]
    pub fn settings_document(&self) -> String {
        tairix_wallpaper::settings::render(&self.to_settings())
    }

    /// Paint the chooser into a `width` x `height` surface for the active
    /// theme. Returns `None` only when the surface cannot be allocated.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: u32, height: u32) -> Option<Surface> {
        let layout = Layout::compute(width, height);
        let painter = Painter {
            theme,
            scale: Scale::default(),
            font: BitmapFont::console(),
        };
        let mut surface = Surface::new(width, height)?;
        surface.fill(theme.palette().surface.into());

        self.render_grid(&mut surface, layout.grid(), painter);

        let fit_index = FIT_ALL.iter().position(|f| *f == self.fit).unwrap_or(0);
        render_option_row(
            &mut surface,
            layout.fit_row(),
            painter,
            &FIT_ALL.map(WallpaperFit::as_str),
            fit_index,
            self.focus == Focus::Fit,
        );
        let backdrop_labels: Vec<&str> = self
            .backdrops
            .iter()
            .map(|option| option.label.as_str())
            .collect();
        render_option_row(
            &mut surface,
            layout.backdrop_row(),
            painter,
            &backdrop_labels,
            self.backdrop,
            self.focus == Focus::Backdrop,
        );
        let icons_index = ICON_FLOW_ALL
            .iter()
            .position(|f| *f == self.icons)
            .unwrap_or(0);
        render_option_row(
            &mut surface,
            layout.icons_row(),
            painter,
            &ICON_FLOW_ALL.map(IconFlow::as_str),
            icons_index,
            self.focus == Focus::Icons,
        );
        let sort_index = SORT_ALL.iter().position(|s| *s == self.sort).unwrap_or(0);
        render_option_row(
            &mut surface,
            layout.sort_row(),
            painter,
            &SORT_ALL.map(IconSort::as_str),
            sort_index,
            self.focus == Focus::Sort,
        );

        render_status(
            &mut surface,
            layout.status(),
            painter,
            self.apply_outcome.as_ref(),
        );
        render_buttons(
            &mut surface,
            layout.apply_button(),
            layout.close_button(),
            painter,
            self.focus,
        );

        Some(surface)
    }

    /// Paint the thumbnail grid's visible rows and columns.
    ///
    /// Painting is confined to `grid`, so a window too short for a whole
    /// cell row shows the top of that row and nothing spills into the
    /// option rows below it.
    fn render_grid(&self, surface: &mut Surface, grid: Rect, painter: Painter<'_>) {
        if grid.is_empty() || self.columns == 0 {
            return;
        }
        let (first_row, visible_rows) = self.visible_rows();
        let gx = to_u32(grid.left());
        let gy = to_u32(grid.top());
        let backdrop = self.backdrop();
        surface.with_clip(gx, gy, grid.width, grid.height, |clipped| {
            for row in 0..visible_rows {
                for col in 0..self.columns {
                    let index = (first_row + row) * self.columns + col;
                    let Some(candidate) = self.candidates.get(index) else {
                        continue;
                    };
                    let x = gx + CELL_WIDTH * u32::try_from(col).unwrap_or(0);
                    let y = gy + CELL_HEIGHT * u32::try_from(row).unwrap_or(0);
                    render_cell(
                        clipped,
                        x,
                        y,
                        candidate,
                        index == self.selected,
                        backdrop,
                        painter,
                    );
                }
            }
        });
    }
}

/// What every painter in this engine draws with: the active theme, the
/// desktop UI scale, and the one shared text face. Carried as one value so
/// each renderer takes the paint context rather than three parallel
/// arguments that could be threaded inconsistently.
#[derive(Copy, Clone)]
struct Painter<'a> {
    theme: &'a Theme,
    scale: Scale,
    font: BitmapFont,
}

/// Candidates-per-row of `columns` needed to show `total` candidates.
fn row_count(total: usize, columns: usize) -> usize {
    if columns == 0 {
        0
    } else {
        total.div_ceil(columns)
    }
}

/// Paint one grid cell: its background, selection border, thumbnail (or
/// placeholder), and label.
fn render_cell(
    surface: &mut Surface,
    x: u32,
    y: u32,
    candidate: &Candidate,
    selected: bool,
    backdrop: Backdrop,
    painter: Painter<'_>,
) {
    let palette = painter.theme.palette();
    let font = painter.font;
    surface.fill_rect(x, y, CELL_WIDTH, CELL_HEIGHT, palette.surface_raised.into());
    if selected {
        let accent = Color::from(palette.accent);
        let t = SELECTION_BORDER;
        surface.fill_rect(x, y, CELL_WIDTH, t, accent);
        surface.fill_rect(x, y + CELL_HEIGHT.saturating_sub(t), CELL_WIDTH, t, accent);
        surface.fill_rect(x, y, t, CELL_HEIGHT, accent);
        surface.fill_rect(x + CELL_WIDTH.saturating_sub(t), y, t, CELL_HEIGHT, accent);
    }

    let thumb_x = x + CELL_PADDING;
    let thumb_y = y + CELL_PADDING;
    match &candidate.thumbnail {
        Thumbnail::Ready(thumb) => {
            // The sandbox leaves every destination pixel its placement does
            // not cover transparent, so painting the chosen backdrop first
            // and compositing the thumbnail over it previews exactly what
            // the desktop will show: the colour fills the letterbox bars of
            // a contained fit and the margins of a centred one.
            surface.fill_rect(
                thumb_x,
                thumb_y,
                THUMB_WIDTH,
                THUMB_HEIGHT,
                backdrop_color(painter.theme, backdrop),
            );
            surface.blit(to_i32(thumb_x), to_i32(thumb_y), thumb);
        }
        Thumbnail::Backdrop => {
            surface.fill_rect(
                thumb_x,
                thumb_y,
                THUMB_WIDTH,
                THUMB_HEIGHT,
                backdrop_color(painter.theme, backdrop),
            );
        }
        Thumbnail::Pending => {
            surface.fill_rect(
                thumb_x,
                thumb_y,
                THUMB_WIDTH,
                THUMB_HEIGHT,
                Color::from(palette.surface_hover),
            );
        }
        Thumbnail::Refused => {
            surface.fill_rect(
                thumb_x,
                thumb_y,
                THUMB_WIDTH,
                THUMB_HEIGHT,
                Color::from(palette.surface_hover),
            );
            let marker = font.truncate_to_width(REFUSED_MARKER, THUMB_WIDTH);
            let marker_y = thumb_y + THUMB_HEIGHT.saturating_sub(font.line_height()) / 2;
            font.draw_text(
                surface,
                to_i32(thumb_x),
                to_i32(marker_y),
                marker,
                palette.danger.into(),
            );
        }
    }

    let label_y = thumb_y + THUMB_HEIGHT + 2;
    let fitted = font.truncate_to_width(
        &candidate.label,
        CELL_WIDTH.saturating_sub(CELL_PADDING * 2),
    );
    font.draw_text(
        surface,
        to_i32(thumb_x),
        to_i32(label_y),
        fitted,
        palette.on_surface.into(),
    );
}

/// The flat colour a backdrop names, resolving [`Backdrop::Theme`] against
/// the active theme's own desktop colour.
fn backdrop_color(theme: &Theme, backdrop: Backdrop) -> Color {
    match backdrop {
        Backdrop::Theme => theme.palette().desktop.into(),
        Backdrop::Colour(rgb) => Color::rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// Paint one option row: `labels.len()` [`Radio`]s dividing the row evenly,
/// the one at `selected` marked on, and — when `group_focused` — carrying
/// the keyboard focus ring.
fn render_option_row(
    surface: &mut Surface,
    row: Rect,
    painter: Painter<'_>,
    labels: &[&str],
    selected: usize,
    group_focused: bool,
) {
    if row.is_empty() || labels.is_empty() {
        return;
    }
    let count = u32::try_from(labels.len()).unwrap_or(1).max(1);
    let item_w = row.width / count;
    // Clipped to the row, so a label longer than the share of the row its
    // radio was given is cut at the row's edge rather than written over a
    // neighbouring region.
    surface.with_clip(
        to_u32(row.left()),
        to_u32(row.top()),
        row.width,
        row.height,
        |clipped| {
            for (index, label) in labels.iter().enumerate() {
                let offset = item_w.saturating_mul(u32::try_from(index).unwrap_or(0));
                let item_rect =
                    Rect::new(row.left() + to_i32(offset), row.top(), item_w, row.height);
                let mut radio = Radio::new(*label, index == selected);
                radio.set_focused(group_focused && index == selected);
                radio.render(
                    clipped,
                    item_rect,
                    painter.scale,
                    painter.theme,
                    painter.font,
                );
            }
        },
    );
}

/// Paint the apply-outcome status line, or nothing before any attempt.
fn render_status(
    surface: &mut Surface,
    rect: Rect,
    painter: Painter<'_>,
    outcome: Option<&ApplyOutcome>,
) {
    let Some(outcome) = outcome else {
        return;
    };
    if rect.is_empty() {
        return;
    }
    let palette = painter.theme.palette();
    let font = painter.font;
    let (text, color): (String, Color) = match outcome {
        ApplyOutcome::Applied => (String::from("Applied."), palette.success.into()),
        ApplyOutcome::Refused(reason) => {
            (alloc::format!("Refused: {reason}"), palette.danger.into())
        }
        ApplyOutcome::NoDesktop => (
            String::from("No desktop session is listening."),
            palette.warning.into(),
        ),
    };
    let fitted = font.truncate_to_width(&text, rect.width);
    font.draw_text(surface, rect.left(), rect.top(), fitted, color);
}

/// Paint the Apply and Close buttons, focused according to `focus`.
fn render_buttons(
    surface: &mut Surface,
    apply_rect: Rect,
    close_rect: Rect,
    painter: Painter<'_>,
    focus: Focus,
) {
    let mut apply = Button::labelled("Apply");
    apply.set_focused(focus == Focus::Apply);
    apply.render(
        surface,
        apply_rect,
        painter.scale,
        painter.theme,
        painter.font,
    );

    let mut close = Button::labelled("Close");
    close.set_focused(focus == Focus::Close);
    close.render(
        surface,
        close_rect,
        painter.scale,
        painter.theme,
        painter.font,
    );
}

/// Saturating `i32` → `u32` (never negative in this engine's own geometry).
fn to_u32(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests;
