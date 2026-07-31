//! The closed set of built-in icon glyphs.
//!
//! [`IconKind`] is the vocabulary of glyphs the desktop draws: the taskbar's
//! status/notification area (network, volume, battery, bell), the file
//! manager's file-type icons (folder, document, application bundle, and the
//! broad content classes text/image/archive/executable), and the file
//! manager's toolbar command icons (back/forward/up navigation, refresh, the
//! view toggle, sort, and new folder). A theme asset id
//! resolves to a kind through [`IconKind::for_asset`]; an unrecognised id
//! falls back to [`IconKind::Generic`] rather than failing, so an unknown
//! asset still shows a placeholder instead of nothing. [`builtin_icon`] turns
//! a kind plus a single theme colour into a [`VectorIcon`]; the glyphs are
//! monochrome silhouettes tinted by the caller, so re-theming is data, not
//! new code.

use alloc::vec;

use tairix_raster::Color;

use crate::vector::{IconLayer, VectorIcon};

/// The design-grid side every built-in glyph is authored on.
const DESIGN: u32 = 24;

/// A desktop icon glyph — a taskbar status/notification icon or a file
/// manager file-type icon.
///
/// A closed set: adding a glyph is a new variant plus its coordinate table
/// (and its [`index`](Self::index) slot), never an open-ended string lookup
/// at the draw site.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum IconKind {
    /// Network / signal-strength bars.
    Network,
    /// A speaker, for volume / audio status.
    Volume,
    /// A battery body, for power status.
    Battery,
    /// A bell, for pending notifications.
    Bell,
    /// A closed folder, for a directory the browser can descend into.
    Folder,
    /// An open folder, for a directory being entered or a drop target.
    FolderOpen,
    /// A generic document, for a regular file of no recognised class.
    File,
    /// An application tile, for a `<Name>.app` bundle.
    AppBundle,
    /// Lines of text, for a text/document file.
    Text,
    /// A picture, for an image file.
    Image,
    /// A package, for an archive file.
    Archive,
    /// A run/bolt mark, for an executable.
    Executable,
    /// A left arrow, for the file manager's Back navigation command.
    NavBack,
    /// A right arrow, for the file manager's Forward navigation command.
    NavForward,
    /// An up arrow, for the file manager's Up (climb-to-parent) command.
    NavUp,
    /// A circular arrow, for the file manager's Refresh command.
    Refresh,
    /// A grid of tiles, for the file manager's list/grid view toggle.
    ViewToggle,
    /// Descending horizontal bars, for the file manager's sort command.
    Sort,
    /// A folder with a plus badge, for the file manager's New Folder command.
    NewFolder,
    /// A waste bin, for the file manager's Trash location (the "go to Trash"
    /// command that navigates to the user's Trash directory).
    Trash,
    /// An open, tipped-out waste bin, for the file manager's Empty Trash
    /// command (the permanent removal of the Trash's contents).
    EmptyTrash,
    /// A three-by-three grid of application tiles, for the taskbar's
    /// program-library launcher.
    Library,
    /// The fallback glyph for an unrecognised asset id: a filled diamond.
    Generic,
}

impl IconKind {
    /// Resolve a theme asset identifier to a glyph, falling back to
    /// [`Generic`](Self::Generic) for an unknown id so an unexpected
    /// notification still draws a placeholder.
    #[must_use]
    pub fn for_asset(asset: &str) -> Self {
        match asset {
            "network" => Self::Network,
            "volume" => Self::Volume,
            "battery" => Self::Battery,
            "bell" => Self::Bell,
            "folder" => Self::Folder,
            "folder-open" => Self::FolderOpen,
            "file" => Self::File,
            "app-bundle" => Self::AppBundle,
            "text" => Self::Text,
            "image" => Self::Image,
            "archive" => Self::Archive,
            "executable" => Self::Executable,
            "nav-back" => Self::NavBack,
            "nav-forward" => Self::NavForward,
            "nav-up" => Self::NavUp,
            "refresh" => Self::Refresh,
            "view-toggle" => Self::ViewToggle,
            "sort" => Self::Sort,
            "new-folder" => Self::NewFolder,
            "trash" => Self::Trash,
            "empty-trash" => Self::EmptyTrash,
            "library" => Self::Library,
            _ => Self::Generic,
        }
    }

    /// This kind's stable index into the closed [`ICON_KINDS`] table, so an
    /// [`IconSet`] can store one slot per kind by position rather than a field
    /// per kind. The identity `ICON_KINDS[kind.index()] == kind` holds for
    /// every kind.
    ///
    /// [`ICON_KINDS`]: crate::load::ICON_KINDS
    /// [`IconSet`]: crate::load::IconSet
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Network => 0,
            Self::Volume => 1,
            Self::Battery => 2,
            Self::Bell => 3,
            Self::Folder => 4,
            Self::FolderOpen => 5,
            Self::File => 6,
            Self::AppBundle => 7,
            Self::Text => 8,
            Self::Image => 9,
            Self::Archive => 10,
            Self::Executable => 11,
            Self::NavBack => 12,
            Self::NavForward => 13,
            Self::NavUp => 14,
            Self::Refresh => 15,
            Self::ViewToggle => 16,
            Self::Sort => 17,
            Self::NewFolder => 18,
            Self::Generic => 19,
            Self::Trash => 20,
            Self::EmptyTrash => 21,
            Self::Library => 22,
        }
    }

    /// The canonical asset identifier for this kind — the inverse of
    /// [`for_asset`](Self::for_asset).
    ///
    /// A desktop loader names a kind's on-disk SVG asset by this id, so the
    /// id↔kind mapping lives in one place rather than being restated at the
    /// load site. The round trip holds for every kind:
    /// `IconKind::for_asset(kind.asset_id()) == kind`.
    #[must_use]
    pub fn asset_id(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Volume => "volume",
            Self::Battery => "battery",
            Self::Bell => "bell",
            Self::Folder => "folder",
            Self::FolderOpen => "folder-open",
            Self::File => "file",
            Self::AppBundle => "app-bundle",
            Self::Text => "text",
            Self::Image => "image",
            Self::Archive => "archive",
            Self::Executable => "executable",
            Self::NavBack => "nav-back",
            Self::NavForward => "nav-forward",
            Self::NavUp => "nav-up",
            Self::Refresh => "refresh",
            Self::ViewToggle => "view-toggle",
            Self::Sort => "sort",
            Self::NewFolder => "new-folder",
            Self::Generic => "generic",
            Self::Trash => "trash",
            Self::EmptyTrash => "empty-trash",
            Self::Library => "library",
        }
    }
}

/// Build the built-in glyph for `kind`, tinted with `color`.
///
/// The returned [`VectorIcon`] is authored on a fixed square design grid; the
/// caller rasterises it to whatever pixel size the notification slot needs.
#[must_use]
pub fn builtin_icon(kind: IconKind, color: Color) -> VectorIcon {
    let layers = match kind {
        IconKind::Network => network(color),
        IconKind::Volume => volume(color),
        IconKind::Battery => battery(color),
        IconKind::Bell => bell(color),
        IconKind::Folder => folder(color),
        IconKind::FolderOpen => folder_open(color),
        IconKind::File => file(color),
        IconKind::AppBundle => app_bundle(color),
        IconKind::Text => text(color),
        IconKind::Image => image(color),
        IconKind::Archive => archive(color),
        IconKind::Executable => executable(color),
        IconKind::NavBack => nav_back(color),
        IconKind::NavForward => nav_forward(color),
        IconKind::NavUp => nav_up(color),
        IconKind::Refresh => refresh(color),
        IconKind::ViewToggle => view_toggle(color),
        IconKind::Sort => sort(color),
        IconKind::NewFolder => new_folder(color),
        IconKind::Trash => trash(color),
        IconKind::EmptyTrash => empty_trash(color),
        IconKind::Library => library(color),
        IconKind::Generic => generic(color),
    };
    VectorIcon::new(DESIGN, layers)
}

/// Three rising signal bars.
fn network(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SHORT: &[(i32, i32)] = &[(3, 15), (7, 15), (7, 20), (3, 20)];
    const MID: &[(i32, i32)] = &[(10, 10), (14, 10), (14, 20), (10, 20)];
    const TALL: &[(i32, i32)] = &[(17, 5), (21, 5), (21, 20), (17, 20)];
    vec![
        IconLayer::from_points(color, SHORT),
        IconLayer::from_points(color, MID),
        IconLayer::from_points(color, TALL),
    ]
}

/// A speaker cone (a rectangle joined to a triangular horn).
fn volume(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SPEAKER: &[(i32, i32)] = &[(3, 9), (7, 9), (12, 4), (12, 20), (7, 15), (3, 15)];
    vec![IconLayer::from_points(color, SPEAKER)]
}

/// A battery body with a small terminal nub on the right.
fn battery(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BODY: &[(i32, i32)] = &[(3, 8), (18, 8), (18, 17), (3, 17)];
    const TERMINAL: &[(i32, i32)] = &[(18, 11), (21, 11), (21, 14), (18, 14)];
    vec![
        IconLayer::from_points(color, BODY),
        IconLayer::from_points(color, TERMINAL),
    ]
}

/// A bell with a clapper beneath it.
fn bell(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BODY: &[(i32, i32)] = &[
        (12, 2),
        (16, 5),
        (17, 15),
        (20, 18),
        (4, 18),
        (7, 15),
        (8, 5),
    ];
    const CLAPPER: &[(i32, i32)] = &[(10, 18), (14, 18), (12, 22)];
    vec![
        IconLayer::from_points(color, BODY),
        IconLayer::from_points(color, CLAPPER),
    ]
}

/// A closed folder: a body with a raised tab on its leading edge.
fn folder(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BODY: &[(i32, i32)] = &[(3, 6), (9, 6), (11, 8), (21, 8), (21, 20), (3, 20)];
    vec![IconLayer::from_points(color, BODY)]
}

/// An open folder: a back panel with a splayed front flap, so it reads as
/// distinct from the closed [`folder`] silhouette.
fn folder_open(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BACK: &[(i32, i32)] = &[(3, 6), (9, 6), (11, 8), (21, 8), (21, 12), (3, 12)];
    const FRONT: &[(i32, i32)] = &[(1, 13), (23, 13), (20, 20), (4, 20)];
    vec![
        IconLayer::from_points(color, BACK),
        IconLayer::from_points(color, FRONT),
    ]
}

/// A generic document: a page with a folded top-trailing corner.
fn file(color: Color) -> alloc::vec::Vec<IconLayer> {
    const PAGE: &[(i32, i32)] = &[(6, 3), (15, 3), (19, 7), (19, 21), (6, 21)];
    const FOLD: &[(i32, i32)] = &[(15, 3), (15, 7), (19, 7)];
    vec![
        IconLayer::from_points(color, PAGE),
        IconLayer::from_points(color, FOLD),
    ]
}

/// An application bundle: a hexagonal tile, unlike any folder or document.
fn app_bundle(color: Color) -> alloc::vec::Vec<IconLayer> {
    const TILE: &[(i32, i32)] = &[(12, 3), (20, 8), (20, 16), (12, 21), (4, 16), (4, 8)];
    vec![IconLayer::from_points(color, TILE)]
}

/// A text document: three horizontal lines suggesting lines of text, spaced
/// so the gaps between them read at small sizes.
fn text(color: Color) -> alloc::vec::Vec<IconLayer> {
    const LINE1: &[(i32, i32)] = &[(5, 6), (19, 6), (19, 8), (5, 8)];
    const LINE2: &[(i32, i32)] = &[(5, 11), (19, 11), (19, 13), (5, 13)];
    const LINE3: &[(i32, i32)] = &[(5, 16), (15, 16), (15, 18), (5, 18)];
    vec![
        IconLayer::from_points(color, LINE1),
        IconLayer::from_points(color, LINE2),
        IconLayer::from_points(color, LINE3),
    ]
}

/// An image: a small sun above a mountain ridge, the classic picture cue.
fn image(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SUN: &[(i32, i32)] = &[(16, 5), (18, 7), (16, 9), (14, 7)];
    const RIDGE: &[(i32, i32)] = &[(4, 20), (10, 11), (14, 16), (17, 12), (20, 20)];
    vec![
        IconLayer::from_points(color, SUN),
        IconLayer::from_points(color, RIDGE),
    ]
}

/// An archive: a lidded package (a knob, a lid, and a body, with seams between
/// them so the parts read even in one tint).
fn archive(color: Color) -> alloc::vec::Vec<IconLayer> {
    const KNOB: &[(i32, i32)] = &[(10, 4), (14, 4), (14, 6), (10, 6)];
    const LID: &[(i32, i32)] = &[(4, 7), (20, 7), (20, 10), (4, 10)];
    const BODY: &[(i32, i32)] = &[(4, 11), (20, 11), (20, 20), (4, 20)];
    vec![
        IconLayer::from_points(color, KNOB),
        IconLayer::from_points(color, LID),
        IconLayer::from_points(color, BODY),
    ]
}

/// An executable: a lightning bolt, the run/launch cue.
fn executable(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BOLT: &[(i32, i32)] = &[(13, 3), (7, 13), (11, 13), (9, 21), (17, 10), (12, 10)];
    vec![IconLayer::from_points(color, BOLT)]
}

/// A left-pointing arrow (a shaft ending in a head), for Back.
fn nav_back(color: Color) -> alloc::vec::Vec<IconLayer> {
    const ARROW: &[(i32, i32)] = &[
        (4, 12),
        (11, 5),
        (11, 9),
        (20, 9),
        (20, 15),
        (11, 15),
        (11, 19),
    ];
    vec![IconLayer::from_points(color, ARROW)]
}

/// A right-pointing arrow, the mirror of [`nav_back`], for Forward.
fn nav_forward(color: Color) -> alloc::vec::Vec<IconLayer> {
    const ARROW: &[(i32, i32)] = &[
        (20, 12),
        (13, 5),
        (13, 9),
        (4, 9),
        (4, 15),
        (13, 15),
        (13, 19),
    ];
    vec![IconLayer::from_points(color, ARROW)]
}

/// An up-pointing arrow, for Up (climb to parent).
fn nav_up(color: Color) -> alloc::vec::Vec<IconLayer> {
    const ARROW: &[(i32, i32)] = &[
        (12, 4),
        (19, 11),
        (15, 11),
        (15, 20),
        (9, 20),
        (9, 11),
        (5, 11),
    ];
    vec![IconLayer::from_points(color, ARROW)]
}

/// A circular arrow, for Refresh: an annular sector (a ring with a gap at the
/// top) plus an arrowhead at the gap so it reads as a rotation.
fn refresh(color: Color) -> alloc::vec::Vec<IconLayer> {
    const RING: &[(i32, i32)] = &[
        (18, 6),
        (21, 12),
        (18, 18),
        (12, 21),
        (6, 18),
        (3, 12),
        (6, 6),
        (8, 8),
        (7, 12),
        (8, 16),
        (12, 17),
        (16, 16),
        (17, 12),
        (16, 8),
    ];
    const HEAD: &[(i32, i32)] = &[(18, 2), (22, 8), (14, 8)];
    vec![
        IconLayer::from_points(color, RING),
        IconLayer::from_points(color, HEAD),
    ]
}

/// A two-by-two grid of tiles, for the list/grid view toggle.
fn view_toggle(color: Color) -> alloc::vec::Vec<IconLayer> {
    const TL: &[(i32, i32)] = &[(4, 4), (10, 4), (10, 10), (4, 10)];
    const TR: &[(i32, i32)] = &[(14, 4), (20, 4), (20, 10), (14, 10)];
    const BL: &[(i32, i32)] = &[(4, 14), (10, 14), (10, 20), (4, 20)];
    const BR: &[(i32, i32)] = &[(14, 14), (20, 14), (20, 20), (14, 20)];
    vec![
        IconLayer::from_points(color, TL),
        IconLayer::from_points(color, TR),
        IconLayer::from_points(color, BL),
        IconLayer::from_points(color, BR),
    ]
}

/// Three left-aligned horizontal bars of decreasing length, for Sort.
fn sort(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BAR1: &[(i32, i32)] = &[(4, 6), (20, 6), (20, 9), (4, 9)];
    const BAR2: &[(i32, i32)] = &[(4, 11), (16, 11), (16, 14), (4, 14)];
    const BAR3: &[(i32, i32)] = &[(4, 16), (11, 16), (11, 19), (4, 19)];
    vec![
        IconLayer::from_points(color, BAR1),
        IconLayer::from_points(color, BAR2),
        IconLayer::from_points(color, BAR3),
    ]
}

/// A closed folder with a plus badge in its top-trailing corner (clear of the
/// folder body so the two read as separate marks in one tint), for New Folder.
fn new_folder(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BODY: &[(i32, i32)] = &[(2, 9), (8, 9), (10, 11), (15, 11), (15, 21), (2, 21)];
    const PLUS_V: &[(i32, i32)] = &[(18, 3), (21, 3), (21, 10), (18, 10)];
    const PLUS_H: &[(i32, i32)] = &[(16, 5), (23, 5), (23, 8), (16, 8)];
    vec![
        IconLayer::from_points(color, BODY),
        IconLayer::from_points(color, PLUS_V),
        IconLayer::from_points(color, PLUS_H),
    ]
}

/// A waste bin, for the Trash location: a handled lid bar over a tapering
/// bin body ribbed with two vertical staves, so the bin reads even in one
/// tint.
fn trash(color: Color) -> alloc::vec::Vec<IconLayer> {
    const HANDLE: &[(i32, i32)] = &[(9, 3), (15, 3), (15, 5), (9, 5)];
    const LID: &[(i32, i32)] = &[(4, 5), (20, 5), (20, 8), (4, 8)];
    const BODY: &[(i32, i32)] = &[(6, 8), (18, 8), (16, 21), (8, 21)];
    const RIB_LEFT: &[(i32, i32)] = &[(10, 10), (11, 10), (11, 19), (10, 19)];
    const RIB_RIGHT: &[(i32, i32)] = &[(13, 10), (14, 10), (14, 19), (13, 19)];
    vec![
        IconLayer::from_points(color, HANDLE),
        IconLayer::from_points(color, LID),
        IconLayer::from_points(color, BODY),
        IconLayer::from_points(color, RIB_LEFT),
        IconLayer::from_points(color, RIB_RIGHT),
    ]
}

/// An emptied waste bin, for Empty Trash: the same tapering bin body with its
/// lid tipped off to one side (a slanted bar clear of the mouth), so it reads
/// as "tipped out" and distinct from the closed [`trash`] bin.
fn empty_trash(color: Color) -> alloc::vec::Vec<IconLayer> {
    const LID: &[(i32, i32)] = &[(14, 2), (22, 5), (21, 8), (13, 5)];
    const BODY: &[(i32, i32)] = &[(5, 9), (17, 9), (15, 21), (7, 21)];
    vec![
        IconLayer::from_points(color, LID),
        IconLayer::from_points(color, BODY),
    ]
}

/// A three-by-three grid of small application tiles — the app-drawer cue for
/// the program-library launcher, denser than the two-by-two [`view_toggle`]
/// grid so the two never read alike.
fn library(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SIDE: i32 = 4;
    const STARTS: [i32; 3] = [4, 10, 16];
    let mut layers = alloc::vec::Vec::with_capacity(9);
    for top in STARTS {
        for left in STARTS {
            layers.push(IconLayer::from_points(
                color,
                &[
                    (left, top),
                    (left + SIDE, top),
                    (left + SIDE, top + SIDE),
                    (left, top + SIDE),
                ],
            ));
        }
    }
    layers
}

/// The fallback placeholder: a filled diamond.
fn generic(color: Color) -> alloc::vec::Vec<IconLayer> {
    const DIAMOND: &[(i32, i32)] = &[(12, 4), (20, 12), (12, 20), (4, 12)];
    vec![IconLayer::from_points(color, DIAMOND)]
}
