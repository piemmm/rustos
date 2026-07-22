//! File-type icon classification.
//!
//! [`icon_for`] maps an [`Entry`] to the [`IconKind`] the browser draws for it:
//! by [`EntryKind`] first (a directory is a folder, a `<Name>.app` bundle is an
//! application tile), then — for a regular file — by a small, documented
//! filename-extension table into the broad content classes text / image /
//! archive / executable, with the [generic file](IconKind::File) glyph as the
//! fail-closed fallback for anything unrecognised.
//!
//! This is the one classifier the windowed file manager and the trusted file
//! picker share, so the two can never disagree about what glyph a name gets.
//! It is a **display hint only**: it decides an icon, never an operation. What
//! a file *is* for the purposes of reading, launching, or permission is the
//! VFS's and the launcher's job, never this table's.

use tairix_icon::IconKind;

use crate::entry::{Entry, EntryKind};

/// The icon glyph for `entry`: by kind first, then — for a regular file — by
/// its filename extension (see [`icon_for_name`]).
#[must_use]
pub fn icon_for(entry: &Entry) -> IconKind {
    match entry.kind() {
        EntryKind::Directory => IconKind::Folder,
        EntryKind::Bundle => IconKind::AppBundle,
        EntryKind::File => icon_for_name(entry.name()),
    }
}

/// The icon glyph for a regular file named `name`, from its lowercased
/// filename extension: a recognised text / image / archive / executable
/// extension maps to that content class; anything else — including a name with
/// no extension or a leading-dot "dotfile" whose only dot starts the name —
/// falls back to the [generic file](IconKind::File) glyph.
#[must_use]
pub fn icon_for_name(name: &str) -> IconKind {
    match extension(name) {
        Some(ext) if in_table(TEXT_EXTENSIONS, ext) => IconKind::Text,
        Some(ext) if in_table(IMAGE_EXTENSIONS, ext) => IconKind::Image,
        Some(ext) if in_table(ARCHIVE_EXTENSIONS, ext) => IconKind::Archive,
        Some(ext) if in_table(EXECUTABLE_EXTENSIONS, ext) => IconKind::Executable,
        _ => IconKind::File,
    }
}

/// The filename extension of `name`: the text after the final `.`, or `None`
/// when there is no such dot, the dot is the first byte (a dotfile with no
/// further extension, e.g. `.profile`), or nothing follows it (`archive.`).
///
/// Shared with the [`open_with`](crate::open_with) MIME classifier so the two
/// file-type views split a name's extension off identically, never each its own
/// copy.
pub(crate) fn extension(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    let ext = name.get(dot + 1..)?;
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

/// Whether `table` lists `ext`, matched ASCII-case-insensitively so an
/// extension reads the same however the volume cased it.
fn in_table(table: &[&str], ext: &str) -> bool {
    table.iter().any(|entry| entry.eq_ignore_ascii_case(ext))
}

/// Extensions shown with the text/document glyph: plain text, markup, source,
/// and common structured-config formats.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "rst", "log", "rs", "toml", "json", "yaml", "yml", "xml", "csv",
    "ini", "cfg", "conf", "sh", "c", "h",
];

/// Extensions shown with the image glyph, raster and vector alike.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "svg", "ico", "webp", "tiff", "tif",
];

/// Extensions shown with the archive/package glyph.
const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "tar", "gz", "tgz", "xz", "bz2", "zst", "7z", "rar"];

/// Extensions shown with the executable glyph: the TAIRiX `rxe` envelope, a
/// bare wasm module, and a raw ELF image.
const EXECUTABLE_EXTENSIONS: &[&str] = &["rxe", "wasm", "elf"];
