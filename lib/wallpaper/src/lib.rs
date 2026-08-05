//! The desktop pinboard wallpaper engine.
//!
//! TAIRiX keeps the user's pinboard settings — the wallpaper, its fit, the
//! backdrop colour, and the `Desktop` folder's icon flow and sort order —
//! in a text document on the volume: the per-user store at
//! [`user_settings_path`]. This crate is the **single definition** of that
//! document ([`settings`]), of the shipped default wallpaper set and the
//! bounded listing model a chooser draws its thumbnail grid from
//! ([`catalog`]), and of the one pure wallpaper-placement geometry the
//! desktop renderer and the chooser's preview both draw through
//! ([`fit`]) — so no two consumers can ever disagree about what the
//! settings say, which wallpapers exist, or how a fit places one.
//!
//! # Security
//!
//! The settings document is **untrusted input** to every consumer: the
//! parser is bounded ([`MAX_SETTINGS_LEN`], [`MAX_WALLPAPER_PATH_LEN`]) and
//! fails closed ([`SettingsError`]) on anything it does not fully
//! understand — an unknown key, a duplicate key, a missing or malformed
//! value, or an over-long document. A reader that cannot fully parse a
//! store runs on [`PinboardSettings::default`] rather than guessing at a
//! partial intent, and a writer refuses the edit outright. A wallpaper
//! path surviving validation still names untrusted image content; this
//! crate performs no decode of its own.
//!
//! The crate performs no I/O and holds no authority: reading and writing
//! the document, and listing a wallpaper directory, go through the secured
//! VFS under the caller's own kernel-attested identity. Pinboard settings
//! are per-user state only; there is no machine-wide store.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;

pub mod catalog;
pub mod fit;
pub mod settings;

pub use catalog::{
    catalog_entries, default_wallpaper_path, is_wallpaper_file_name, CatalogEntry,
    DEFAULT_WALLPAPER, MAX_WALLPAPER_BYTES, MAX_WALLPAPER_CATALOG_ENTRIES, WALLPAPER_STORE,
};
pub use fit::{decode_target, nominal_source_size, place, Placement};
pub use settings::{
    parse, render, Backdrop, IconFlow, IconSort, ParseError, PinboardSettings, Rgb, SettingsError,
    SettingsKey, WallpaperChoice, WallpaperFit, WallpaperPath, WallpaperPathError,
    MAX_SETTINGS_LEN, MAX_WALLPAPER_PATH_LEN,
};

/// The pinboard settings store's own directory name under a `Settings/`
/// tree, spelled once so every path here derives from it.
macro_rules! pinboard_component {
    () => {
        "Pinboard"
    };
}

/// The pinboard settings store's directory name inside a `Settings/` tree —
/// the one component a settings browser creates — shared with
/// [`user_settings_path`] so the spellings cannot drift.
pub const PINBOARD_SETTINGS_SUBDIR: &str = pinboard_component!();

/// The pinboard settings store's document file name.
pub const PINBOARD_FILE: &str = "pinboard.conf";

/// The per-user pinboard settings path inside `home`, the account's home
/// directory exactly as the session inherited it (`HOME`). A trailing `/`
/// is normalised away; an empty or root home yields `None` rather than a
/// guessed rootward path, so a caller with no home fails closed.
#[must_use]
pub fn user_settings_path(home: &str) -> Option<String> {
    let home = home.strip_suffix('/').unwrap_or(home);
    if home.is_empty() || home == "/" {
        return None;
    }
    Some(format!(
        "{home}/Settings/{PINBOARD_SETTINGS_SUBDIR}/{PINBOARD_FILE}"
    ))
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::user_settings_path;

    #[test]
    fn path_is_rooted_under_the_home_settings_tree() {
        assert_eq!(
            user_settings_path("/Users/ada"),
            Some(String::from("/Users/ada/Settings/Pinboard/pinboard.conf"))
        );
    }

    #[test]
    fn a_trailing_slash_on_home_is_normalised_away() {
        assert_eq!(
            user_settings_path("/Users/ada/"),
            user_settings_path("/Users/ada")
        );
    }

    #[test]
    fn an_empty_or_root_home_fails_closed() {
        assert_eq!(user_settings_path(""), None);
        assert_eq!(user_settings_path("/"), None);
    }
}
