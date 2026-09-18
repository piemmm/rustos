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
//!
//! [`disk_icon`] maps the storage medium a mounted volume reports onto the
//! drive kind that represents it, so the file manager and the desktop draw
//! the same icon for the same medium rather than each guessing.

use alloc::vec;

use tairix_abi::blkio::BlkDeviceClass;
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
///
/// A fine-grained file-class kind (an HTML or Rust text file, a PNG or SVG
/// image, a specific disk medium) deliberately shares its broad family's
/// built-in glyph: [`builtin_icon`] draws a `TextHtml` as the plain text
/// glyph, an `ImagePng` as the plain image glyph, and every `Disk*` as the
/// one disk glyph. The distinction still names a distinct on-disk asset id,
/// so a system that ships the raster artwork resolves the precise icon while
/// one that does not still shows a meaningful family glyph rather than the
/// bare [`Generic`](Self::Generic) placeholder — fallback stays total.
///
/// `Ord` orders the cache-invalidation candidates a reclaim cache indexes,
/// not a meaningful glyph ordering — the taskbar's icon cache
/// (`plans/SMARTRAM.md` section 6.4) needs `IconKind` as a `BTreeMap` key.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
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
    /// A folder with pages stacked behind it, for a directory that holds at
    /// least one entry — the occupancy cue beside the empty
    /// [`Folder`](Self::Folder).
    FolderFilled,
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
    /// A head-and-shoulders bust, for a user account.
    ///
    /// The last-resort mark for the desktop's own account capsule at the
    /// trailing end of the icon bar, and for anywhere else an account is
    /// shown with no picture behind it. An account that has a name resolves
    /// to its circular identity disc ([`monogram_disc`](crate::monogram_disc))
    /// instead, so this is reached only when no picture can be produced at
    /// all.
    User,
    /// A long-running system service bundle; shares the app-bundle glyph.
    ServiceBundle,
    /// An HTML document; shares the text glyph.
    TextHtml,
    /// A Rust source file; shares the text glyph.
    TextRust,
    /// A Java source file; shares the text glyph.
    TextJava,
    /// A shell script; shares the text glyph.
    ShellScript,
    /// A PDF document; shares the text glyph.
    Pdf,
    /// A PNG image; shares the image glyph.
    ImagePng,
    /// A JPEG image; shares the image glyph.
    ImageJpeg,
    /// A GIF image; shares the image glyph.
    ImageGif,
    /// An SVG image; shares the image glyph.
    ImageSvg,
    /// A RISC OS sprite image; shares the image glyph.
    ImageSprite,
    /// A drive whose medium is unknown or paravirtual: the generic disk
    /// glyph, drawn when nothing better can be said honestly.
    Disk,
    /// A rotational hard disk; shares the disk glyph.
    DiskHard,
    /// A solid-state disk; shares the disk glyph.
    DiskSolidState,
    /// A USB mass-storage disk; shares the disk glyph.
    DiskUsb,
    /// The fallback glyph for an unrecognised asset id: a filled diamond.
    Generic,
    /// Three bulleted horizontal bars, for opening a menu that lists a
    /// screen's own sections (a Switchboard-style location breadcrumb's
    /// trailing list button).
    ListMenu,
    /// An hourglass, for a unit of queued or background work.
    Job,
    /// An arrow entering a window body, for switching to a task's own window.
    TaskSwitch,
    /// A window frame with its title bar, for showing where a window is
    /// without switching to it.
    Reveal,
    /// Two upright bars, for suspending a running task.
    Pause,
    /// A right-pointing triangle, for continuing a suspended task.
    Resume,
    /// A downward arrow, for lowering a task's scheduling priority.
    Priority,
    /// A cross, for ending a task outright.
    Quit,
    /// A magnifier bearing a plus, for magnifying what is displayed.
    ZoomIn,
    /// A magnifier bearing a minus, for reducing what is displayed.
    ZoomOut,
    /// Four corner brackets, for scaling what is displayed to its window.
    ZoomFit,
    /// Callipers around a fixed box, for displaying at the true pixel size.
    ZoomActual,
    /// A clockwise half-turn arrow, for turning what is displayed right.
    RotateRight,
    /// An anticlockwise half-turn arrow, for turning what is displayed left.
    RotateLeft,
    /// Two arrowheads facing away from an axis, for mirroring what is
    /// displayed.
    Mirror,
    /// An `i` in a ring, for showing what is known about the thing on
    /// display.
    Info,
    /// A cog, for the desktop's settings surface and its General category.
    Settings,
    /// A half-filled ring, for the appearance (light/dark, contrast) category.
    Appearance,
    /// A framed landscape, for the desktop-backdrop category.
    Wallpaper,
    /// A monitor on a stand, for the attached-displays category.
    Display,
    /// A padlock, for the screen-lock category.
    LockScreen,
    /// A crescent moon, for the screensaver category.
    Screensaver,
    /// The power symbol, for the machine's power category.
    Power,
    /// The Bluetooth rune, for the short-range-radio category.
    Bluetooth,
    /// A speaker, for the audio category; shares the volume glyph.
    Sound,
    /// A bell, for the notification-policy category; shares the bell glyph.
    Notifications,
    /// A key bank, for the keyboard category.
    Keyboard,
    /// A mouse body and wheel, for the pointing-device category.
    Mouse,
    /// A pad under a fingertip, for the trackpad category.
    Trackpad,
    /// A finger on an upright screen, for the touch-input category.
    Touchscreen,
    /// A printer with a sheet through it, for the print and scan category.
    Printer,
    /// A figure with arms out in a ring, for the accessibility category.
    Accessibility,
    /// A globe, for the language and region category.
    Language,
    /// Joined nodes, for the file- and screen-sharing category.
    Sharing,
    /// Two busts, for the accounts and groups category, beside the single
    /// [`User`](Self::User) bust that stands for one account.
    Users,
    /// Stacked media, for the storage category, beside the
    /// [`Disk`](Self::Disk) family that stands for one drive.
    Storage,
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
            "folder-filled" => Self::FolderFilled,
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
            "user" => Self::User,
            "service-bundle" => Self::ServiceBundle,
            "text-html" => Self::TextHtml,
            "text-x-rust" => Self::TextRust,
            "text-x-java" => Self::TextJava,
            "application-x-shellscript" => Self::ShellScript,
            "application-pdf" => Self::Pdf,
            "image-png" => Self::ImagePng,
            "image-jpeg" => Self::ImageJpeg,
            "image-gif" => Self::ImageGif,
            "image-svg-xml" => Self::ImageSvg,
            "image-x-riscos-sprite" => Self::ImageSprite,
            "disk" => Self::Disk,
            "disk-hard" => Self::DiskHard,
            "disk-solid-state" => Self::DiskSolidState,
            "disk-usb" => Self::DiskUsb,
            "list-menu" => Self::ListMenu,
            "job" => Self::Job,
            "task-switch" => Self::TaskSwitch,
            "reveal" => Self::Reveal,
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "priority" => Self::Priority,
            "quit" => Self::Quit,
            "zoom-in" => Self::ZoomIn,
            "zoom-out" => Self::ZoomOut,
            "zoom-fit" => Self::ZoomFit,
            "zoom-actual" => Self::ZoomActual,
            "rotate-right" => Self::RotateRight,
            "rotate-left" => Self::RotateLeft,
            "mirror" => Self::Mirror,
            "info" => Self::Info,
            "settings" => Self::Settings,
            "appearance" => Self::Appearance,
            "wallpaper" => Self::Wallpaper,
            "display" => Self::Display,
            "lock-screen" => Self::LockScreen,
            "screensaver" => Self::Screensaver,
            "power" => Self::Power,
            "bluetooth" => Self::Bluetooth,
            "sound" => Self::Sound,
            "notifications" => Self::Notifications,
            "keyboard" => Self::Keyboard,
            "mouse" => Self::Mouse,
            "trackpad" => Self::Trackpad,
            "touchscreen" => Self::Touchscreen,
            "printer" => Self::Printer,
            "accessibility" => Self::Accessibility,
            "language" => Self::Language,
            "sharing" => Self::Sharing,
            "users" => Self::Users,
            "storage" => Self::Storage,
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
            Self::User => 23,
            Self::ServiceBundle => 24,
            Self::TextHtml => 25,
            Self::TextRust => 26,
            Self::TextJava => 27,
            Self::ShellScript => 28,
            Self::Pdf => 29,
            Self::ImagePng => 30,
            Self::ImageJpeg => 31,
            Self::ImageGif => 32,
            Self::ImageSvg => 33,
            Self::ImageSprite => 34,
            Self::Disk => 35,
            Self::DiskHard => 36,
            Self::DiskSolidState => 37,
            Self::DiskUsb => 38,
            Self::ListMenu => 39,
            Self::Job => 40,
            Self::TaskSwitch => 41,
            Self::Reveal => 42,
            Self::Pause => 43,
            Self::Resume => 44,
            Self::Priority => 45,
            Self::Quit => 46,
            Self::FolderFilled => 47,
            Self::ZoomIn => 48,
            Self::ZoomOut => 49,
            Self::ZoomFit => 50,
            Self::ZoomActual => 51,
            Self::RotateRight => 52,
            Self::RotateLeft => 53,
            Self::Mirror => 54,
            Self::Info => 55,
            Self::Settings => 56,
            Self::Appearance => 57,
            Self::Wallpaper => 58,
            Self::Display => 59,
            Self::LockScreen => 60,
            Self::Screensaver => 61,
            Self::Power => 62,
            Self::Bluetooth => 63,
            Self::Sound => 64,
            Self::Notifications => 65,
            Self::Keyboard => 66,
            Self::Mouse => 67,
            Self::Trackpad => 68,
            Self::Touchscreen => 69,
            Self::Printer => 70,
            Self::Accessibility => 71,
            Self::Language => 72,
            Self::Sharing => 73,
            Self::Users => 74,
            Self::Storage => 75,
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
            Self::User => "user",
            Self::ServiceBundle => "service-bundle",
            Self::TextHtml => "text-html",
            Self::TextRust => "text-x-rust",
            Self::TextJava => "text-x-java",
            Self::ShellScript => "application-x-shellscript",
            Self::Pdf => "application-pdf",
            Self::ImagePng => "image-png",
            Self::ImageJpeg => "image-jpeg",
            Self::ImageGif => "image-gif",
            Self::ImageSvg => "image-svg-xml",
            Self::ImageSprite => "image-x-riscos-sprite",
            Self::Disk => "disk",
            Self::DiskHard => "disk-hard",
            Self::DiskSolidState => "disk-solid-state",
            Self::DiskUsb => "disk-usb",
            Self::ListMenu => "list-menu",
            Self::Job => "job",
            Self::TaskSwitch => "task-switch",
            Self::Reveal => "reveal",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Priority => "priority",
            Self::Quit => "quit",
            Self::FolderFilled => "folder-filled",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
            Self::ZoomFit => "zoom-fit",
            Self::ZoomActual => "zoom-actual",
            Self::RotateRight => "rotate-right",
            Self::RotateLeft => "rotate-left",
            Self::Mirror => "mirror",
            Self::Info => "info",
            Self::Settings => "settings",
            Self::Appearance => "appearance",
            Self::Wallpaper => "wallpaper",
            Self::Display => "display",
            Self::LockScreen => "lock-screen",
            Self::Screensaver => "screensaver",
            Self::Power => "power",
            Self::Bluetooth => "bluetooth",
            Self::Sound => "sound",
            Self::Notifications => "notifications",
            Self::Keyboard => "keyboard",
            Self::Mouse => "mouse",
            Self::Trackpad => "trackpad",
            Self::Touchscreen => "touchscreen",
            Self::Printer => "printer",
            Self::Accessibility => "accessibility",
            Self::Language => "language",
            Self::Sharing => "sharing",
            Self::Users => "users",
            Self::Storage => "storage",
        }
    }
}

/// The icon that represents a mounted volume's storage medium.
///
/// An unknown medium and a paravirtual device both resolve to the generic
/// drive icon rather than a guessed one.
#[must_use]
pub const fn disk_icon(medium: Option<BlkDeviceClass>) -> IconKind {
    match medium {
        Some(BlkDeviceClass::Rotational) => IconKind::DiskHard,
        Some(BlkDeviceClass::SolidState) => IconKind::DiskSolidState,
        Some(BlkDeviceClass::Removable) => IconKind::DiskUsb,
        Some(BlkDeviceClass::Virtual) | None => IconKind::Disk,
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
        // The audio category and the tray's volume reading are the same
        // speaker; only the asset slot a theme may override differs.
        IconKind::Volume | IconKind::Sound => volume(color),
        IconKind::Battery => battery(color),
        // As for the notification-policy category and a pending notification.
        IconKind::Bell | IconKind::Notifications => bell(color),
        IconKind::Folder => folder(color),
        IconKind::FolderOpen => folder_open(color),
        IconKind::FolderFilled => folder_filled(color),
        IconKind::File => file(color),
        // The app-bundle, text, and image families each share one built-in
        // glyph: the fine-grained kinds differ only in their shipped raster
        // artwork, so a system without it still shows the broad family glyph.
        IconKind::AppBundle | IconKind::ServiceBundle => app_bundle(color),
        IconKind::Text
        | IconKind::TextHtml
        | IconKind::TextRust
        | IconKind::TextJava
        | IconKind::ShellScript
        | IconKind::Pdf => text(color),
        IconKind::Image
        | IconKind::ImagePng
        | IconKind::ImageJpeg
        | IconKind::ImageGif
        | IconKind::ImageSvg
        | IconKind::ImageSprite => image(color),
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
        IconKind::User => user(color),
        IconKind::Disk | IconKind::DiskHard | IconKind::DiskSolidState | IconKind::DiskUsb => {
            disk(color)
        }
        IconKind::Generic => generic(color),
        IconKind::ListMenu => list_menu(color),
        IconKind::Job => job(color),
        IconKind::TaskSwitch => task_switch(color),
        IconKind::Reveal => reveal(color),
        IconKind::Pause => pause(color),
        IconKind::Resume => resume(color),
        IconKind::Priority => priority(color),
        IconKind::Quit => quit(color),
        IconKind::ZoomIn => magnifier(color, true),
        IconKind::ZoomOut => magnifier(color, false),
        IconKind::ZoomFit => zoom_fit(color),
        IconKind::ZoomActual => zoom_actual(color),
        IconKind::RotateRight => rotate(color, true),
        IconKind::RotateLeft => rotate(color, false),
        IconKind::Mirror => mirror(color),
        IconKind::Info => info(color),
        IconKind::Settings => settings(color),
        IconKind::Appearance => appearance(color),
        IconKind::Wallpaper => wallpaper(color),
        IconKind::Display => display(color),
        IconKind::LockScreen => lock_screen(color),
        IconKind::Screensaver => screensaver(color),
        IconKind::Power => power(color),
        IconKind::Bluetooth => bluetooth(color),
        IconKind::Keyboard => keyboard(color),
        IconKind::Mouse => mouse(color),
        IconKind::Trackpad => trackpad(color),
        IconKind::Touchscreen => touchscreen(color),
        IconKind::Printer => printer(color),
        IconKind::Accessibility => accessibility(color),
        IconKind::Language => language(color),
        IconKind::Sharing => sharing(color),
        IconKind::Users => users(color),
        IconKind::Storage => storage(color),
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

/// A folder holding papers: a low folder body with two offset sheets standing
/// clear above it, so the stack still reads as "holds something" in one tint.
fn folder_filled(color: Color) -> alloc::vec::Vec<IconLayer> {
    const PAGE_BACK: &[(i32, i32)] = &[(10, 2), (19, 2), (19, 10), (10, 10)];
    const PAGE_FRONT: &[(i32, i32)] = &[(6, 4), (15, 4), (15, 10), (6, 10)];
    const BODY: &[(i32, i32)] = &[(2, 12), (8, 12), (10, 14), (22, 14), (22, 21), (2, 21)];
    vec![
        IconLayer::from_points(color, PAGE_BACK),
        IconLayer::from_points(color, PAGE_FRONT),
        IconLayer::from_points(color, BODY),
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

/// Three equal-length bulleted horizontal bars, for opening a list of a
/// screen's own sections — a small leading bullet on each bar keeps it
/// reading as a list rather than [`sort`]'s decreasing ranked bars.
fn list_menu(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BULLET1: &[(i32, i32)] = &[(3, 6), (6, 6), (6, 9), (3, 9)];
    const BAR1: &[(i32, i32)] = &[(9, 6), (21, 6), (21, 9), (9, 9)];
    const BULLET2: &[(i32, i32)] = &[(3, 11), (6, 11), (6, 14), (3, 14)];
    const BAR2: &[(i32, i32)] = &[(9, 11), (21, 11), (21, 14), (9, 14)];
    const BULLET3: &[(i32, i32)] = &[(3, 16), (6, 16), (6, 19), (3, 19)];
    const BAR3: &[(i32, i32)] = &[(9, 16), (21, 16), (21, 19), (9, 19)];
    vec![
        IconLayer::from_points(color, BULLET1),
        IconLayer::from_points(color, BAR1),
        IconLayer::from_points(color, BULLET2),
        IconLayer::from_points(color, BAR2),
        IconLayer::from_points(color, BULLET3),
        IconLayer::from_points(color, BAR3),
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

/// A user account: a head over shoulders, the bust silhouette an account is
/// universally drawn as.
fn user(color: Color) -> alloc::vec::Vec<IconLayer> {
    const HEAD: &[(i32, i32)] = &[
        (12, 4),
        (15, 5),
        (16, 8),
        (15, 11),
        (12, 12),
        (9, 11),
        (8, 8),
        (9, 5),
    ];
    const SHOULDERS: &[(i32, i32)] = &[
        (12, 12),
        (16, 13),
        (19, 16),
        (20, 20),
        (4, 20),
        (5, 16),
        (8, 13),
    ];
    vec![
        IconLayer::from_points(color, HEAD),
        IconLayer::from_points(color, SHOULDERS),
    ]
}

/// A storage drive: a cylinder — a top platter disc over a drum body — the
/// classic disk/storage silhouette, so it reads at 16px and stays distinct
/// from the document and folder shapes. Shared by every fine-grained disk
/// medium (hard, solid-state, floppy, USB), which differ only in their
/// shipped raster artwork, not this fallback.
fn disk(color: Color) -> alloc::vec::Vec<IconLayer> {
    const PLATTER: &[(i32, i32)] = &[
        (4, 7),
        (8, 5),
        (12, 5),
        (16, 5),
        (20, 7),
        (16, 9),
        (12, 9),
        (8, 9),
    ];
    const BODY: &[(i32, i32)] = &[
        (4, 7),
        (8, 9),
        (12, 9),
        (16, 9),
        (20, 7),
        (20, 17),
        (16, 19),
        (12, 19),
        (8, 19),
        (4, 17),
    ];
    vec![
        IconLayer::from_points(color, PLATTER),
        IconLayer::from_points(color, BODY),
    ]
}

/// The fallback placeholder: a filled diamond.
fn generic(color: Color) -> alloc::vec::Vec<IconLayer> {
    const DIAMOND: &[(i32, i32)] = &[(12, 4), (20, 12), (12, 20), (4, 12)];
    vec![IconLayer::from_points(color, DIAMOND)]
}

/// An hourglass: two triangles meeting at the waist, for work that is queued
/// or running in the background rather than in front of the reader.
fn job(color: Color) -> alloc::vec::Vec<IconLayer> {
    const UPPER: &[(i32, i32)] = &[(5, 3), (19, 3), (12, 11)];
    const LOWER: &[(i32, i32)] = &[(12, 13), (19, 21), (5, 21)];
    vec![
        IconLayer::from_points(color, UPPER),
        IconLayer::from_points(color, LOWER),
    ]
}

/// An arrow entering a window body, for switching to a task's own window: the
/// motion is *into* the window, unlike [`reveal`], which only shows where the
/// window is.
fn task_switch(color: Color) -> alloc::vec::Vec<IconLayer> {
    const ARROW: &[(i32, i32)] = &[
        (2, 10),
        (8, 10),
        (8, 7),
        (13, 12),
        (8, 17),
        (8, 14),
        (2, 14),
    ];
    const BODY: &[(i32, i32)] = &[(14, 4), (21, 4), (21, 20), (14, 20)];
    vec![
        IconLayer::from_points(color, ARROW),
        IconLayer::from_points(color, BODY),
    ]
}

/// A window frame with its title bar: the window itself, for showing where it
/// is. Drawn as a filled title band and three thin edge bars, because a single
/// tint cannot punch a hole through a filled rectangle.
fn reveal(color: Color) -> alloc::vec::Vec<IconLayer> {
    const TITLE: &[(i32, i32)] = &[(3, 5), (21, 5), (21, 8), (3, 8)];
    const LEFT: &[(i32, i32)] = &[(3, 8), (5, 8), (5, 19), (3, 19)];
    const RIGHT: &[(i32, i32)] = &[(19, 8), (21, 8), (21, 19), (19, 19)];
    const BOTTOM: &[(i32, i32)] = &[(3, 17), (21, 17), (21, 19), (3, 19)];
    vec![
        IconLayer::from_points(color, TITLE),
        IconLayer::from_points(color, LEFT),
        IconLayer::from_points(color, RIGHT),
        IconLayer::from_points(color, BOTTOM),
    ]
}

/// Two upright bars: the universal pause mark.
fn pause(color: Color) -> alloc::vec::Vec<IconLayer> {
    const LEFT: &[(i32, i32)] = &[(8, 4), (11, 4), (11, 20), (8, 20)];
    const RIGHT: &[(i32, i32)] = &[(13, 4), (16, 4), (16, 20), (13, 20)];
    vec![
        IconLayer::from_points(color, LEFT),
        IconLayer::from_points(color, RIGHT),
    ]
}

/// A right-pointing triangle: the universal play/continue mark.
fn resume(color: Color) -> alloc::vec::Vec<IconLayer> {
    const PLAY: &[(i32, i32)] = &[(8, 4), (19, 12), (8, 20)];
    vec![IconLayer::from_points(color, PLAY)]
}

/// A downward arrow, for lowering a task's scheduling priority: the direction
/// states which way the change goes.
fn priority(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SHAFT: &[(i32, i32)] = &[(10, 3), (14, 3), (14, 13), (10, 13)];
    const HEAD: &[(i32, i32)] = &[(5, 13), (19, 13), (12, 21)];
    vec![
        IconLayer::from_points(color, SHAFT),
        IconLayer::from_points(color, HEAD),
    ]
}

/// A cross, for ending a task outright: two diagonal bars, distinct from the
/// window-close furniture because it is drawn on the design grid at glyph
/// weight.
fn quit(color: Color) -> alloc::vec::Vec<IconLayer> {
    const FALLING: &[(i32, i32)] = &[(5, 7), (7, 5), (19, 17), (17, 19)];
    const RISING: &[(i32, i32)] = &[(17, 5), (19, 7), (7, 19), (5, 17)];
    vec![
        IconLayer::from_points(color, FALLING),
        IconLayer::from_points(color, RISING),
    ]
}

/// A magnifier over a plus or a minus, for magnifying or reducing what a
/// window displays.
///
/// One table for both, because the two glyphs differ only in the bar the
/// lens holds; drawing them as separate coordinate sets would be two lenses
/// to keep identical.
fn magnifier(color: Color, magnify: bool) -> alloc::vec::Vec<IconLayer> {
    // Lens: an octagonal annulus, outer then inner in one even-odd ring.
    const LENS: &[(i32, i32)] = &[
        (10, 3),
        (15, 5),
        (17, 10),
        (15, 15),
        (10, 17),
        (5, 15),
        (3, 10),
        (5, 5),
        (10, 5),
        (13, 7),
        (14, 10),
        (13, 13),
        (10, 14),
        (7, 13),
        (6, 10),
        (7, 7),
    ];
    const HANDLE: &[(i32, i32)] = &[(14, 16), (16, 14), (22, 20), (20, 22)];
    const BAR: &[(i32, i32)] = &[(7, 9), (13, 9), (13, 11), (7, 11)];
    const STEM: &[(i32, i32)] = &[(9, 7), (11, 7), (11, 13), (9, 13)];
    let mut layers = alloc::vec![
        IconLayer::from_points(color, LENS),
        IconLayer::from_points(color, HANDLE),
        IconLayer::from_points(color, BAR),
    ];
    if magnify {
        layers.push(IconLayer::from_points(color, STEM));
    }
    layers
}

/// Four corner brackets facing outward, for scaling a picture to fill the
/// window it is shown in.
fn zoom_fit(color: Color) -> alloc::vec::Vec<IconLayer> {
    const TOP_LEFT: &[(i32, i32)] = &[(3, 3), (11, 3), (11, 6), (6, 6), (6, 11), (3, 11)];
    const TOP_RIGHT: &[(i32, i32)] = &[(21, 3), (21, 11), (18, 11), (18, 6), (13, 6), (13, 3)];
    const BOTTOM_LEFT: &[(i32, i32)] = &[(3, 21), (3, 13), (6, 13), (6, 18), (11, 18), (11, 21)];
    const BOTTOM_RIGHT: &[(i32, i32)] =
        &[(21, 21), (13, 21), (13, 18), (18, 18), (18, 13), (21, 13)];
    vec![
        IconLayer::from_points(color, TOP_LEFT),
        IconLayer::from_points(color, TOP_RIGHT),
        IconLayer::from_points(color, BOTTOM_LEFT),
        IconLayer::from_points(color, BOTTOM_RIGHT),
    ]
}

/// Callipers closed on a fixed box, for showing a picture at its true pixel
/// size: what is measured is the picture, not the window, so the jaws do not
/// move.
fn zoom_actual(color: Color) -> alloc::vec::Vec<IconLayer> {
    const LEFT_JAW: &[(i32, i32)] = &[
        (3, 4),
        (8, 4),
        (8, 7),
        (6, 7),
        (6, 17),
        (8, 17),
        (8, 20),
        (3, 20),
    ];
    const RIGHT_JAW: &[(i32, i32)] = &[
        (21, 4),
        (21, 20),
        (16, 20),
        (16, 17),
        (18, 17),
        (18, 7),
        (16, 7),
        (21, 7),
    ];
    const BOX: &[(i32, i32)] = &[(10, 9), (14, 9), (14, 15), (10, 15)];
    vec![
        IconLayer::from_points(color, LEFT_JAW),
        IconLayer::from_points(color, RIGHT_JAW),
        IconLayer::from_points(color, BOX),
    ]
}

/// A half-turn arrow, for turning what a window displays a quarter turn.
///
/// The arc is shared and only the head moves, so the two directions cannot
/// drift apart. Deliberately a half-turn arc rather than [`refresh`]'s
/// near-complete ring: a toolbar carrying both must not draw them alike.
fn rotate(color: Color, clockwise: bool) -> alloc::vec::Vec<IconLayer> {
    // Outer half-annulus left over the top to right, then back inside.
    const ARC: &[(i32, i32)] = &[
        (3, 12),
        (5, 7),
        (7, 5),
        (12, 3),
        (17, 5),
        (19, 7),
        (21, 12),
        (18, 12),
        (16, 8),
        (12, 6),
        (8, 8),
        (6, 12),
    ];
    const RIGHT_HEAD: &[(i32, i32)] = &[(16, 11), (23, 11), (19, 18)];
    const LEFT_HEAD: &[(i32, i32)] = &[(1, 11), (8, 11), (5, 18)];
    vec![
        IconLayer::from_points(color, ARC),
        IconLayer::from_points(color, if clockwise { RIGHT_HEAD } else { LEFT_HEAD }),
    ]
}

/// Two arrowheads facing away from a central axis, for mirroring what a
/// window displays.
fn mirror(color: Color) -> alloc::vec::Vec<IconLayer> {
    const AXIS: &[(i32, i32)] = &[(11, 2), (13, 2), (13, 22), (11, 22)];
    const LEFT: &[(i32, i32)] = &[(9, 5), (9, 19), (2, 12)];
    const RIGHT: &[(i32, i32)] = &[(15, 5), (15, 19), (22, 12)];
    vec![
        IconLayer::from_points(color, AXIS),
        IconLayer::from_points(color, LEFT),
        IconLayer::from_points(color, RIGHT),
    ]
}

/// An `i` in a ring, for what is known about the thing on display.
fn info(color: Color) -> alloc::vec::Vec<IconLayer> {
    const RING: &[(i32, i32)] = &[
        (12, 2),
        (19, 5),
        (22, 12),
        (19, 19),
        (12, 22),
        (5, 19),
        (2, 12),
        (5, 5),
        (12, 5),
        (17, 7),
        (19, 12),
        (17, 17),
        (12, 19),
        (7, 17),
        (5, 12),
        (7, 7),
    ];
    const DOT: &[(i32, i32)] = &[(10, 7), (14, 7), (14, 10), (10, 10)];
    const STEM: &[(i32, i32)] = &[(10, 12), (14, 12), (14, 18), (10, 18)];
    vec![
        IconLayer::from_points(color, RING),
        IconLayer::from_points(color, DOT),
        IconLayer::from_points(color, STEM),
    ]
}

/// A cog: an eight-toothed rim around a hub bore.
///
/// The rim and the bore are one even-odd ring, which is what carves the bore
/// out rather than a second layer in the background colour a theme may not
/// have.
fn settings(color: Color) -> alloc::vec::Vec<IconLayer> {
    const COG: &[(i32, i32)] = &[
        (23, 12),
        (19, 15),
        (20, 20),
        (15, 19),
        (12, 23),
        (9, 19),
        (4, 20),
        (5, 15),
        (1, 12),
        (5, 9),
        (4, 4),
        (9, 5),
        (12, 1),
        (15, 5),
        (20, 4),
        (19, 9),
        (16, 12),
        (15, 15),
        (12, 16),
        (9, 15),
        (8, 12),
        (9, 9),
        (12, 8),
        (15, 9),
    ];
    vec![IconLayer::from_points(color, COG)]
}

/// A ring with its trailing half filled: the light/dark contrast mark.
fn appearance(color: Color) -> alloc::vec::Vec<IconLayer> {
    const RIM: &[(i32, i32)] = &[
        (22, 12),
        (19, 19),
        (12, 22),
        (5, 19),
        (2, 12),
        (5, 5),
        (12, 2),
        (19, 5),
        (20, 12),
        (18, 18),
        (12, 20),
        (6, 18),
        (4, 12),
        (6, 6),
        (12, 4),
        (18, 6),
    ];
    const FILLED_HALF: &[(i32, i32)] = &[(12, 4), (18, 6), (20, 12), (18, 18), (12, 20)];
    vec![
        IconLayer::from_points(color, RIM),
        IconLayer::from_points(color, FILLED_HALF),
    ]
}

/// A framed landscape with a sun: the desktop backdrop.
fn wallpaper(color: Color) -> alloc::vec::Vec<IconLayer> {
    const FRAME: &[(i32, i32)] = &[
        (2, 4),
        (22, 4),
        (22, 20),
        (2, 20),
        (4, 6),
        (20, 6),
        (20, 18),
        (4, 18),
    ];
    const HILLS: &[(i32, i32)] = &[(4, 18), (10, 11), (14, 15), (20, 9), (20, 18)];
    const SUN: &[(i32, i32)] = &[(10, 9), (8, 11), (6, 9), (8, 7)];
    vec![
        IconLayer::from_points(color, FRAME),
        IconLayer::from_points(color, HILLS),
        IconLayer::from_points(color, SUN),
    ]
}

/// A landscape screen on a pedestal stand.
fn display(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BEZEL: &[(i32, i32)] = &[
        (2, 3),
        (22, 3),
        (22, 17),
        (2, 17),
        (4, 5),
        (20, 5),
        (20, 15),
        (4, 15),
    ];
    const NECK: &[(i32, i32)] = &[(11, 17), (13, 17), (13, 20), (11, 20)];
    const BASE: &[(i32, i32)] = &[(7, 20), (17, 20), (17, 22), (7, 22)];
    vec![
        IconLayer::from_points(color, BEZEL),
        IconLayer::from_points(color, NECK),
        IconLayer::from_points(color, BASE),
    ]
}

/// A padlock: a shackle over a closed body.
fn lock_screen(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SHACKLE: &[(i32, i32)] = &[
        (7, 11),
        (7, 7),
        (9, 4),
        (12, 3),
        (15, 4),
        (17, 7),
        (17, 11),
        (15, 11),
        (15, 8),
        (14, 6),
        (12, 5),
        (10, 6),
        (9, 8),
        (9, 11),
    ];
    const BODY: &[(i32, i32)] = &[(4, 11), (20, 11), (20, 22), (4, 22)];
    vec![
        IconLayer::from_points(color, SHACKLE),
        IconLayer::from_points(color, BODY),
    ]
}

/// A crescent moon: the screen has gone to sleep.
fn screensaver(color: Color) -> alloc::vec::Vec<IconLayer> {
    const CRESCENT: &[(i32, i32)] = &[
        (12, 1),
        (18, 4),
        (21, 10),
        (20, 17),
        (15, 22),
        (8, 23),
        (3, 20),
        (7, 19),
        (13, 18),
        (16, 13),
        (15, 8),
        (11, 4),
    ];
    vec![IconLayer::from_points(color, CRESCENT)]
}

/// The power symbol: a broken ring with a stem through its gap.
fn power(color: Color) -> alloc::vec::Vec<IconLayer> {
    const RING: &[(i32, i32)] = &[
        (17, 5),
        (21, 11),
        (20, 18),
        (14, 22),
        (10, 22),
        (4, 18),
        (3, 11),
        (7, 5),
        (9, 7),
        (6, 11),
        (6, 16),
        (10, 19),
        (14, 19),
        (18, 16),
        (18, 11),
        (15, 7),
    ];
    const STEM: &[(i32, i32)] = &[(11, 2), (13, 2), (13, 10), (11, 10)];
    vec![
        IconLayer::from_points(color, RING),
        IconLayer::from_points(color, STEM),
    ]
}

/// The Bluetooth rune: a stem crossed by two chevrons.
fn bluetooth(color: Color) -> alloc::vec::Vec<IconLayer> {
    const STEM: &[(i32, i32)] = &[(11, 3), (13, 3), (13, 21), (11, 21)];
    const UPPER: &[(i32, i32)] = &[(12, 3), (19, 8), (12, 13), (12, 11), (16, 8), (12, 5)];
    const LOWER: &[(i32, i32)] = &[(12, 21), (19, 16), (12, 11), (12, 13), (16, 16), (12, 19)];
    vec![
        IconLayer::from_points(color, STEM),
        IconLayer::from_points(color, UPPER),
        IconLayer::from_points(color, LOWER),
    ]
}

/// A key bank: a case holding a row of keys and a space bar.
fn keyboard(color: Color) -> alloc::vec::Vec<IconLayer> {
    const CASE: &[(i32, i32)] = &[
        (1, 6),
        (23, 6),
        (23, 19),
        (1, 19),
        (3, 8),
        (21, 8),
        (21, 17),
        (3, 17),
    ];
    const KEYS: &[(i32, i32)] = &[(5, 10), (19, 10), (19, 12), (5, 12)];
    const SPACE: &[(i32, i32)] = &[(8, 14), (16, 14), (16, 16), (8, 16)];
    vec![
        IconLayer::from_points(color, CASE),
        IconLayer::from_points(color, KEYS),
        IconLayer::from_points(color, SPACE),
    ]
}

/// A mouse: an outlined body with its scroll wheel.
fn mouse(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BODY: &[(i32, i32)] = &[
        (12, 2),
        (17, 5),
        (18, 12),
        (17, 19),
        (12, 22),
        (7, 19),
        (6, 12),
        (7, 5),
        (12, 5),
        (15, 7),
        (16, 12),
        (15, 17),
        (12, 19),
        (9, 17),
        (8, 12),
        (9, 7),
    ];
    const WHEEL: &[(i32, i32)] = &[(11, 7), (13, 7), (13, 12), (11, 12)];
    vec![
        IconLayer::from_points(color, BODY),
        IconLayer::from_points(color, WHEEL),
    ]
}

/// A trackpad: a pad under a fingertip.
fn trackpad(color: Color) -> alloc::vec::Vec<IconLayer> {
    const PAD: &[(i32, i32)] = &[
        (3, 4),
        (21, 4),
        (21, 20),
        (3, 20),
        (5, 6),
        (19, 6),
        (19, 18),
        (5, 18),
    ];
    const TIP: &[(i32, i32)] = &[
        (15, 12),
        (14, 14),
        (12, 15),
        (10, 14),
        (9, 12),
        (10, 10),
        (12, 9),
        (14, 10),
    ];
    vec![
        IconLayer::from_points(color, PAD),
        IconLayer::from_points(color, TIP),
    ]
}

/// A finger touching an upright screen.
fn touchscreen(color: Color) -> alloc::vec::Vec<IconLayer> {
    const SCREEN: &[(i32, i32)] = &[
        (3, 2),
        (21, 2),
        (21, 22),
        (3, 22),
        (5, 4),
        (19, 4),
        (19, 20),
        (5, 20),
    ];
    const FINGER: &[(i32, i32)] = &[(10, 12), (14, 8), (21, 14), (17, 18)];
    const CONTACT: &[(i32, i32)] = &[(11, 10), (13, 12), (11, 14), (9, 12)];
    vec![
        IconLayer::from_points(color, SCREEN),
        IconLayer::from_points(color, FINGER),
        IconLayer::from_points(color, CONTACT),
    ]
}

/// A printer: a sheet fed in above the case and a printed sheet below it.
fn printer(color: Color) -> alloc::vec::Vec<IconLayer> {
    const FEED: &[(i32, i32)] = &[(7, 2), (17, 2), (17, 9), (7, 9)];
    const CASE: &[(i32, i32)] = &[
        (2, 9),
        (22, 9),
        (22, 19),
        (2, 19),
        (4, 11),
        (20, 11),
        (20, 17),
        (4, 17),
    ];
    const SHEET: &[(i32, i32)] = &[(7, 17), (17, 17), (17, 22), (7, 22)];
    vec![
        IconLayer::from_points(color, FEED),
        IconLayer::from_points(color, CASE),
        IconLayer::from_points(color, SHEET),
    ]
}

/// A figure with its arms out inside a ring: the universal access mark.
fn accessibility(color: Color) -> alloc::vec::Vec<IconLayer> {
    const RING: &[(i32, i32)] = &[
        (23, 12),
        (20, 20),
        (12, 23),
        (4, 20),
        (1, 12),
        (4, 4),
        (12, 1),
        (20, 4),
        (21, 12),
        (18, 18),
        (12, 21),
        (6, 18),
        (3, 12),
        (6, 6),
        (12, 3),
        (18, 6),
    ];
    const HEAD: &[(i32, i32)] = &[(12, 5), (14, 7), (12, 9), (10, 7)];
    const ARMS: &[(i32, i32)] = &[(5, 10), (19, 10), (19, 12), (5, 12)];
    const TORSO: &[(i32, i32)] = &[
        (11, 12),
        (13, 12),
        (13, 16),
        (15, 19),
        (13, 20),
        (12, 17),
        (11, 20),
        (9, 19),
        (11, 16),
    ];
    vec![
        IconLayer::from_points(color, RING),
        IconLayer::from_points(color, HEAD),
        IconLayer::from_points(color, ARMS),
        IconLayer::from_points(color, TORSO),
    ]
}

/// A globe: a rim crossed by an equator and a meridian.
fn language(color: Color) -> alloc::vec::Vec<IconLayer> {
    const RIM: &[(i32, i32)] = &[
        (22, 12),
        (19, 19),
        (12, 22),
        (5, 19),
        (2, 12),
        (5, 5),
        (12, 2),
        (19, 5),
        (20, 12),
        (18, 18),
        (12, 20),
        (6, 18),
        (4, 12),
        (6, 6),
        (12, 4),
        (18, 6),
    ];
    const EQUATOR: &[(i32, i32)] = &[(3, 11), (21, 11), (21, 13), (3, 13)];
    const MERIDIAN: &[(i32, i32)] = &[
        (12, 2),
        (16, 7),
        (16, 17),
        (12, 22),
        (8, 17),
        (8, 7),
        (12, 4),
        (14, 7),
        (14, 17),
        (12, 20),
        (10, 17),
        (10, 7),
    ];
    vec![
        IconLayer::from_points(color, RIM),
        IconLayer::from_points(color, EQUATOR),
        IconLayer::from_points(color, MERIDIAN),
    ]
}

/// Three joined nodes: what this machine offers to others.
fn sharing(color: Color) -> alloc::vec::Vec<IconLayer> {
    const HUB: &[(i32, i32)] = &[
        (8, 12),
        (7, 15),
        (5, 16),
        (3, 15),
        (2, 12),
        (3, 9),
        (5, 8),
        (7, 9),
    ];
    const UPPER: &[(i32, i32)] = &[
        (21, 6),
        (20, 9),
        (18, 10),
        (16, 9),
        (15, 6),
        (16, 3),
        (18, 2),
        (20, 3),
    ];
    const LOWER: &[(i32, i32)] = &[
        (21, 18),
        (20, 21),
        (18, 22),
        (16, 21),
        (15, 18),
        (16, 15),
        (18, 14),
        (20, 15),
    ];
    const UPPER_LINK: &[(i32, i32)] = &[(6, 11), (17, 6), (18, 8), (7, 13)];
    const LOWER_LINK: &[(i32, i32)] = &[(6, 13), (17, 18), (16, 20), (5, 15)];
    vec![
        IconLayer::from_points(color, UPPER_LINK),
        IconLayer::from_points(color, LOWER_LINK),
        IconLayer::from_points(color, HUB),
        IconLayer::from_points(color, UPPER),
        IconLayer::from_points(color, LOWER),
    ]
}

/// Two busts: the accounts on this machine, beside the one-bust
/// [`user`] glyph that stands for a single account.
fn users(color: Color) -> alloc::vec::Vec<IconLayer> {
    const BEHIND_HEAD: &[(i32, i32)] = &[
        (19, 7),
        (18, 9),
        (16, 10),
        (14, 9),
        (13, 7),
        (14, 5),
        (16, 4),
        (18, 5),
    ];
    const BEHIND_SHOULDERS: &[(i32, i32)] =
        &[(16, 10), (19, 11), (21, 14), (22, 18), (13, 18), (14, 14)];
    const FRONT_HEAD: &[(i32, i32)] = &[
        (13, 8),
        (12, 11),
        (9, 12),
        (6, 11),
        (5, 8),
        (6, 5),
        (9, 4),
        (12, 5),
    ];
    const FRONT_SHOULDERS: &[(i32, i32)] = &[
        (9, 12),
        (13, 13),
        (16, 16),
        (17, 21),
        (1, 21),
        (2, 16),
        (5, 13),
    ];
    vec![
        IconLayer::from_points(color, BEHIND_HEAD),
        IconLayer::from_points(color, BEHIND_SHOULDERS),
        IconLayer::from_points(color, FRONT_HEAD),
        IconLayer::from_points(color, FRONT_SHOULDERS),
    ]
}

/// Stacked media: the machine's storage as a whole, beside the
/// [`disk`] glyph that stands for one drive.
fn storage(color: Color) -> alloc::vec::Vec<IconLayer> {
    vec![platter(color, 5), platter(color, 12), platter(color, 19)]
}

/// One platter of the [`storage`] stack, centred on `cy`: an ellipse rim,
/// outer then inner in one even-odd ring.
///
/// Written once because three identical rims at different heights is what
/// makes the stack read as a stack.
fn platter(color: Color, cy: i32) -> IconLayer {
    IconLayer::from_points(
        color,
        &[
            (12, cy - 4),
            (18, cy - 2),
            (20, cy),
            (18, cy + 2),
            (12, cy + 4),
            (6, cy + 2),
            (4, cy),
            (6, cy - 2),
            (12, cy - 2),
            (16, cy - 1),
            (17, cy),
            (16, cy + 1),
            (12, cy + 2),
            (8, cy + 1),
            (7, cy),
            (8, cy - 1),
        ],
    )
}
