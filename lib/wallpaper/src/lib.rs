//! The desktop pinboard wallpaper engine.
//!
//! TAIRiX keeps the user's pinboard settings — the wallpaper, its fit, the
//! backdrop colour, and the `Desktop` folder's icon flow and sort order —
//! in the desktop session's **published** app-data scope
//! (`plans/APPDATA.md` §3.11). This crate is the **single definition** of
//! that document's closed registry ([`settings`]), of the shipped default
//! wallpaper set and the bounded listing model a chooser draws its
//! thumbnail grid from ([`catalog`]), and of the one pure
//! wallpaper-placement geometry the desktop renderer and the chooser's
//! preview both draw through ([`fit`]) — so no two consumers can ever
//! disagree about what the settings say, which wallpapers exist, or how a
//! fit places one.
//!
//! # Who may write it, and who may read it
//!
//! The session is the store's only writer, by construction rather than by
//! convention: an application publishes only *its own* scope, so no other
//! program the user launches — including the wallpaper chooser — can write
//! the desktop's document at all. A chooser asks the session to adopt a
//! change over the pinboard channel, and the session decides.
//!
//! Reading is the sanctioned channel the same store provides: any
//! application may read what the desktop publishes about itself by naming
//! [`PINBOARD_PUBLISHER`] on a request shape that carries no scope field, so
//! "read the desktop's private settings" is not a request that exists. That
//! replaces the hand-rolled `~/Settings/Pinboard/pinboard.conf` path the
//! chooser used to open directly, which every application of that user could
//! also read *and rewrite*.
//!
//! # Security
//!
//! The settings document is **untrusted input** to every consumer, and this
//! crate has two readings of it, deliberately different
//! ([`PinboardSettings::load`] tolerant, [`decode`] strict — [`settings`]
//! records why). Both are bounded: the format engine bounds the document,
//! the line, the key and the value, and [`MAX_WALLPAPER_PATH_LEN`] bounds
//! the one value that carries a path. Neither ever half-applies a document:
//! a reader that cannot use a value runs on [`PinboardSettings::default`]
//! for that field rather than guessing at a partial intent. A wallpaper path
//! surviving validation still names untrusted image content; this crate
//! performs no decode of its own.
//!
//! The crate performs no I/O and holds no authority: reading and writing the
//! document go through the app-data service under the caller's own
//! kernel-attested identity, and listing a wallpaper directory goes through
//! the secured VFS. Pinboard settings are per-user state only; there is no
//! machine-wide store, and the published scope deliberately has no layer
//! beneath it, so nobody can make the desktop appear to say something it
//! never said.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod catalog;
pub mod fit;
pub mod settings;

pub use catalog::{
    catalog_categories, catalog_entries, category_path, default_wallpaper_path,
    is_wallpaper_category_name, is_wallpaper_file_name, wallpaper_path, CatalogEntry,
    DEFAULT_WALLPAPER, DEFAULT_WALLPAPER_CATEGORY, MAX_WALLPAPER_BYTES,
    MAX_WALLPAPER_CATALOG_ENTRIES, MAX_WALLPAPER_CATEGORIES, WALLPAPER_STORE,
};
pub use fit::{decode_request, nominal_source_size, place, Placement};
pub use settings::{
    decode, Backdrop, DocumentRefusal, IconFlow, IconSort, PinboardSettings, Rgb, SettingsKey,
    WallpaperChoice, WallpaperFit, WallpaperPath, WallpaperPathError, MAX_WALLPAPER_PATH_LEN,
};

/// The signed bundle identifier of the desktop session — the application
/// that owns the pinboard settings and publishes them.
///
/// The one place the identifier is spelled, because two principals need it
/// and they must agree: the session names nothing at all (an application
/// never names its own store — the app-data service derives it from the
/// identity the kernel attests), and a reader hands exactly this to
/// `tairix_appdata::read_published` to obtain the desktop's published
/// document. Getting it wrong reaches a store that publishes nothing, never
/// another application's private one, because a foreign read is a request
/// shape with no scope field at all (`plans/APPDATA.md` §3.6).
pub const PINBOARD_PUBLISHER: &str = "os.tairix.desktop";
