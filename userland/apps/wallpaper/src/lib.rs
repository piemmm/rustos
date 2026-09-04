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
//! * [`Chooser`] — the model: the candidate gallery (built per store
//!   category through [`candidates_from_catalog`]), the category rail that
//!   filters it, the current selection, the four settings drop-downs, the
//!   live preview, and the pointer and
//!   keyboard state that drives them. It performs no I/O and holds no
//!   authority: every thumbnail and every preview arrives already rendered
//!   ([`Chooser::set_thumbnail`], [`Chooser::set_preview`]) or refused, by
//!   the caller, which is the only thing that may talk to the parser
//!   sandbox.
//! * [`Layout`] — the pure window-geometry function every paint and every
//!   hit-test agrees on, so a resize can never leave a control drawn
//!   somewhere a click does not land.
//! * [`Chooser::render_into`] — the painter over the shared `lib/font` face,
//!   `lib/raster` [`Surface`] and the `lib/controls` family. Every
//!   interactive thing on screen is a shared control — the drop-downs, the
//!   buttons, the gallery scrollbar, and the gallery's own tiles — so this
//!   app defines no control of its own and inherits the whole desktop's
//!   hover, press, selection and focus vocabulary.
//! * [`Chooser::settings_document`] — the exact rendered settings document
//!   (`lib/wallpaper`'s own grammar) the current state means, ready to post
//!   to the desktop session.
//!
//! # Pointer first
//!
//! The chooser is driven by the mouse: click a category in the rail beside
//! the gallery to narrow it, click a wallpaper to select it and see it in
//! the preview, click a drop-down to change how it is fitted or what the
//! desktop icons do, drag or wheel the gallery's scrollbar, and click Apply.
//! Every control shows the shared hover and pressed states, and a press
//! released away from the control it started on does nothing.
//!
//! The keyboard is the secondary path, not the primary one: Tab and
//! Shift-Tab move focus, the arrows move within the rail or the gallery or
//! open a drop-down, Enter applies, and Escape closes (or dismisses an open
//! drop-down first).
//!
//! # Categories
//!
//! The shipped store files its masters one directory level deep, and each
//! category directory's own name is the label the rail draws — so the rail
//! is discovered from the store, never a list this app carries. `All` is the
//! rail's leading entry, and a candidate belonging to no category (the "no
//! wallpaper" choice, or a wallpaper in effect from outside the store) shows
//! under every entry, so narrowing the gallery can never hide the choice
//! that is actually applied.
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

use tairix_font::BitmapFont;
use tairix_geometry::Scale;
use tairix_raster::Surface;
use tairix_theme::{TextRole, Theme};
use tairix_wallpaper::{
    wallpaper_path, Backdrop, CatalogEntry, IconFlow, IconSort, Rgb, WallpaperChoice, WallpaperFit,
    WallpaperPath,
};

mod chooser;
pub mod events;
mod layout;
mod paint;

pub use chooser::{Chooser, PreviewRequest, ThumbnailRequest};
pub use layout::Layout;

#[cfg(test)]
mod tests;

/// Initial window content width of the chooser window, in pixels. The
/// chooser re-lays everything out to whatever client size the window
/// manager reports ([`Layout::compute`]), so this is a starting size, not a
/// fixed one.
pub const WIN_WIDTH: u32 = 880;

/// Initial window content height of the chooser window, in pixels (see
/// [`WIN_WIDTH`]).
pub const WIN_HEIGHT: u32 = 640;

/// The smallest client width the chooser lays out into, in pixels — below
/// this the gallery and its options stop being usable. It is declared to
/// the window manager when the window opens, which is what holds a resize
/// to it; the app itself adopts whatever size it is given.
pub const MIN_WIN_WIDTH: u32 = 420;

/// The smallest client height the chooser declares (see
/// [`MIN_WIN_WIDTH`]).
pub const MIN_WIN_HEIGHT: u32 = 360;

/// The label shown for the "no wallpaper" candidate.
pub const NONE_LABEL: &str = "No wallpaper";

/// The gallery's section heading.
pub const GALLERY_HEADING: &str = "Wallpapers";

/// The category rail's leading entry: every candidate, whatever category it
/// is filed under.
///
/// A first-class entry rather than a mode: the rail is the one place the
/// gallery's contents are chosen from, so "show me everything" is a rail
/// entry like any other and needs no second control to reach.
pub const ALL_CATEGORIES_LABEL: &str = "All";

/// The Apply button's label.
pub const APPLY_LABEL: &str = "Apply";

/// The Close button's label.
pub const CLOSE_LABEL: &str = "Close";

/// The word drawn across a candidate the sandbox refused, so a refused tile
/// says why it is not artwork instead of looking like one still loading.
pub const REFUSED_MARKER: &str = "unreadable";

/// Every [`WallpaperFit`] value, in the order the fit drop-down offers them.
pub const FIT_ALL: [WallpaperFit; 5] = [
    WallpaperFit::Fill,
    WallpaperFit::Fit,
    WallpaperFit::Stretch,
    WallpaperFit::Centre,
    WallpaperFit::Tile,
];

/// The human names of [`FIT_ALL`], in the same order.
///
/// The settings document spells a fit as its own bare keyword
/// ([`WallpaperFit::as_str`]); a person reading a drop-down is owed a phrase
/// that says what will happen to their picture, so the two vocabularies are
/// deliberately separate and this one belongs to the surface that shows it.
pub const FIT_LABELS: [&str; FIT_ALL.len()] =
    ["Fill screen", "Fit to screen", "Stretch", "Centre", "Tile"];

/// Every [`IconFlow`] value, in the order the arrangement drop-down offers
/// them.
pub const ICON_FLOW_ALL: [IconFlow; 2] = [IconFlow::Leading, IconFlow::Trailing];

/// The human names of [`ICON_FLOW_ALL`], in the same order: the corner the
/// desktop's first icon takes (see [`FIT_LABELS`] on why these are not the
/// document's own keywords).
pub const ICON_FLOW_LABELS: [&str; ICON_FLOW_ALL.len()] = ["Top left", "Top right"];

/// Every [`IconSort`] value, in the order the sort drop-down offers them.
pub const SORT_ALL: [IconSort; 4] = [
    IconSort::Name,
    IconSort::Kind,
    IconSort::Size,
    IconSort::Date,
];

/// The human names of [`SORT_ALL`], in the same order.
pub const SORT_LABELS: [&str; SORT_ALL.len()] = ["Name", "Kind", "Size", "Date"];

/// The backdrop colours the backdrop drop-down offers: the active theme's
/// own desktop colour first, then a small fixed palette of named flat
/// colours.
///
/// A named palette rather than a free-form colour entry: the settings
/// document's backdrop is one opaque `rrggbb` value, and a closed set is a
/// complete choice with no text field to validate. A backdrop already in
/// effect that this palette does not carry is still offered —
/// [`backdrop_options`] appends it under its own bare `rrggbb` spelling —
/// so opening the chooser never quietly changes the colour that is already
/// on screen.
pub const BACKDROP_PALETTE: [(&str, Backdrop); 6] = [
    ("Theme", Backdrop::Theme),
    ("Black", Backdrop::Colour(Rgb::new(0x00, 0x00, 0x00))),
    ("Slate", Backdrop::Colour(Rgb::new(0x2e, 0x34, 0x40))),
    ("Ocean", Backdrop::Colour(Rgb::new(0x1b, 0x3a, 0x5c))),
    ("Moss", Backdrop::Colour(Rgb::new(0x2c, 0x40, 0x2c))),
    ("Linen", Backdrop::Colour(Rgb::new(0xe8, 0xe0, 0xd8))),
];

/// How many settings drop-downs the option column carries.
pub const OPTION_GROUP_COUNT: usize = 4;

/// One of the chooser's four settings drop-downs.
///
/// A closed set rather than a loose index: the layout, the painter, the
/// pointer hit-test, and the keyboard focus order all name a group by this
/// type, so adding a fifth setting forces every one of them to say what it
/// means for the new group instead of silently mis-indexing an array.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum OptionGroup {
    /// How the wallpaper is fitted to the screen.
    Fit,
    /// The flat colour shown wherever the wallpaper does not reach.
    Backdrop,
    /// The corner the desktop's icon grid grows from.
    Icons,
    /// The order the `Desktop` folder's icons are sorted in.
    Sort,
}

impl OptionGroup {
    /// Every group, in the order the option column shows them.
    pub const ALL: [Self; OPTION_GROUP_COUNT] =
        [Self::Fit, Self::Backdrop, Self::Icons, Self::Sort];

    /// The label drawn beside this group's drop-down.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            Self::Backdrop => "Backdrop",
            Self::Icons => "Icons",
            Self::Sort => "Sort",
        }
    }

    /// This group's slot in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Fit => 0,
            Self::Backdrop => 1,
            Self::Icons => 2,
            Self::Sort => 3,
        }
    }
}

/// What every painter and hit-test in this engine resolves its geometry
/// from: the active theme, the desktop UI scale, the one shared text face,
/// and the desktop's own screen extent (what the preview panel's true-scale
/// model represents).
///
/// Carried as one value so the layout a click is tested against and the
/// layout that was painted are resolved from identical inputs, rather than
/// from four arguments that could be threaded inconsistently.
#[derive(Copy, Clone)]
pub struct Style<'a> {
    theme: &'a Theme,
    scale: Scale,
    font: BitmapFont,
    screen: (u32, u32),
}

impl<'a> Style<'a> {
    /// The style drawing with `font` from `theme` at `scale`, for a desktop
    /// whose screen is `screen` pixels (see [`Self::screen`]).
    #[must_use]
    pub const fn new(theme: &'a Theme, scale: Scale, font: BitmapFont, screen: (u32, u32)) -> Self {
        Self {
            theme,
            scale,
            font,
            screen,
        }
    }

    /// The active theme.
    #[must_use]
    pub const fn theme(&self) -> &'a Theme {
        self.theme
    }

    /// The desktop UI scale.
    #[must_use]
    pub const fn scale(&self) -> Scale {
        self.scale
    }

    /// The desktop's screen extent, in physical pixels: what the preview
    /// panel's true-scale model represents.
    #[must_use]
    pub const fn screen(&self) -> (u32, u32) {
        self.screen
    }

    /// The interface text face the chooser sets its own text in: the option
    /// labels, the preview's placeholder, and the footer's status line.
    /// Shared controls resolve their own face from the theme.
    #[must_use]
    pub const fn font(&self) -> BitmapFont {
        self.font
    }

    /// The face the gallery's section heading is set in.
    #[must_use]
    pub fn heading_font(&self) -> BitmapFont {
        BitmapFont::for_role(self.theme.fonts(), TextRole::SectionHeader, self.scale)
    }

    /// The face the de-emphasised caption under the option column is set in.
    #[must_use]
    pub fn caption_font(&self) -> BitmapFont {
        BitmapFont::for_role(self.theme.fonts(), TextRole::Caption, self.scale)
    }

    /// The face a shared control sets its own body text in — what the
    /// category rail draws its labels with, so its owner can measure the
    /// width they need in the very face they will be drawn in.
    #[must_use]
    pub fn body_font(&self) -> BitmapFont {
        BitmapFont::for_role(self.theme.fonts(), TextRole::Body, self.scale)
    }
}

/// One candidate the gallery may offer: the shipped "no wallpaper" backdrop
/// entry, or one wallpaper image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// The wallpaper choice this candidate selects.
    pub choice: WallpaperChoice,
    /// The display label (the catalog file name, or [`NONE_LABEL`]).
    pub label: String,
    /// The store category this candidate is filed under, or `None` for one
    /// that belongs to no category.
    ///
    /// A category-less candidate is shown under *every* rail entry: the
    /// "no wallpaper" choice must stay reachable whichever category is being
    /// browsed, and a wallpaper already in effect from outside the shipped
    /// store must never be the one thing the chooser hides.
    pub category: Option<String>,
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
    /// resolved at paint time (the active theme is needed when the backdrop
    /// is [`Backdrop::Theme`]) — never sandboxed, since it decodes nothing.
    Backdrop,
    /// Not yet requested from the sandbox.
    Pending,
    /// Rendered successfully by the sandbox, at the square side the tile
    /// asked for ([`Chooser::next_thumbnail`]).
    Ready(Surface),
    /// The sandbox refused this wallpaper once; it will not be retried this
    /// session.
    Refused,
}

impl Candidate {
    /// The "no wallpaper" entry: always first, always present, and in no
    /// category, so browsing one never takes it away.
    #[must_use]
    fn none_entry() -> Self {
        Self {
            choice: WallpaperChoice::None,
            label: String::from(NONE_LABEL),
            category: None,
            thumbnail: Thumbnail::Backdrop,
        }
    }

    /// A pending image candidate at `path`, filed under `category`.
    #[must_use]
    fn image(path: WallpaperPath, label: String, category: Option<String>) -> Self {
        Self {
            choice: WallpaperChoice::Image(path),
            label,
            category,
            thumbnail: Thumbnail::Pending,
        }
    }
}

/// Build the image candidates a chooser may offer for one store category,
/// from that category's listing as
/// [`tairix_wallpaper::catalog::catalog_entries`] discovered it.
///
/// Every entry becomes a [`Candidate`] naming the wallpaper at
/// `<WALLPAPER_STORE>/<category>/<entry.name>`, filed under `category` so
/// the rail can offer it, with a [`Thumbnail::Pending`] state: the caller
/// renders each one through the sandbox and reports the result with
/// [`Chooser::set_thumbnail`] / [`Chooser::mark_thumbnail_refused`]. An
/// entry whose name somehow fails to parse as a wallpaper path (impossible
/// for anything [`tairix_wallpaper::catalog::catalog_entries`] itself
/// already validated, but never assumed here) is silently dropped rather
/// than fabricating a candidate that could not be applied.
#[must_use]
pub fn candidates_from_catalog(category: &str, entries: &[CatalogEntry]) -> Vec<Candidate> {
    entries
        .iter()
        .filter_map(|entry| {
            let path = WallpaperPath::new(&wallpaper_path(category, &entry.name)).ok()?;
            Some(Candidate::image(
                path,
                entry.name.clone(),
                Some(category.to_string()),
            ))
        })
        .collect()
}

/// One backdrop the backdrop drop-down offers.
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
///
/// The pointer needs no such state — a click lands where it lands — so this
/// is the keyboard's cursor through the window, and a click moves it to
/// whatever was clicked so the two paths never disagree about where the user
/// is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Focus {
    /// The gallery's category rail.
    Categories,
    /// The wallpaper gallery.
    Gallery,
    /// One of the four settings drop-downs.
    Setting(OptionGroup),
    /// The Apply button.
    Apply,
    /// The Close button.
    Close,
}

impl Focus {
    /// The fixed tab order Tab and Shift-Tab move through.
    const ORDER: [Self; OPTION_GROUP_COUNT + 4] = [
        Self::Categories,
        Self::Gallery,
        Self::Setting(OptionGroup::Fit),
        Self::Setting(OptionGroup::Backdrop),
        Self::Setting(OptionGroup::Icons),
        Self::Setting(OptionGroup::Sort),
        Self::Close,
        Self::Apply,
    ];

    /// This region's position in [`Self::ORDER`].
    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|region| *region == self)
            .unwrap_or(0)
    }

    /// The next region, wrapping round.
    #[must_use]
    fn next(self) -> Self {
        Self::ORDER[(self.index() + 1) % Self::ORDER.len()]
    }

    /// The previous region, wrapping round.
    #[must_use]
    fn prev(self) -> Self {
        let len = Self::ORDER.len();
        Self::ORDER[(self.index() + len - 1) % len]
    }
}

/// The outcome of asking the desktop session to adopt a rendered settings
/// document (`plans/PINBOARD.md` §6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    /// The request is with the session and no answer has come back yet.
    ///
    /// The chooser shows this rather than the previous attempt's answer, so a
    /// footer can never report a result the store has not given: the apply is
    /// carried out on a worker and the window keeps drawing meanwhile.
    Applying,
    /// The session adopted and persisted the change.
    Applied,
    /// The session refused the request, with the reason it gave.
    Refused(String),
    /// No desktop session answered the pinboard rendezvous at all (the
    /// [`PINBOARD_ENDPOINT`](tairix_abi::pinboard_ipc::PINBOARD_ENDPOINT)
    /// call itself failed) — distinct from an authenticated refusal.
    NoDesktop,
}

/// The result of feeding one pointer or key event to the chooser.
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

impl ChooserAction {
    /// [`Self::Changed`] when `changed`, otherwise [`Self::None`].
    const fn changed(changed: bool) -> Self {
        if changed {
            Self::Changed
        } else {
            Self::None
        }
    }
}

/// Saturating `i32` → `u32` (never negative in this engine's own geometry).
pub(crate) fn to_u32(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

/// Saturating `u32` → `i32`.
pub(crate) fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
