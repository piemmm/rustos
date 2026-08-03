//! The one content-type registry the browser classifies entries through.
//!
//! A listed entry has exactly one [`MediaType`], named by its IANA (or TAIRiX
//! vendor) media-type spelling. That one classification drives *both* the
//! file-type glyph the grid tile draws ([`MediaType::icon`]) and the "Open
//! With…" association vocabulary a bundle declares in its `AppInfo`
//! ([`MediaType::as_str`] / [`MediaType::from_media_str`]), so the icon a name
//! gets and the applications offered for it can never drift apart: they read
//! the same closed table.
//!
//! This is the one classifier the windowed file manager and the trusted file
//! picker share (`plans/NEW-FILEMANAGER.md`), so the two can never disagree
//! about what a name *is*. It is a **display and offer hint only**: it decides
//! an icon and which applications are *offered*, never an operation. What a
//! file is for the purposes of reading, launching, or permission is the VFS's
//! and the launcher's job; and the signed load gate still verifies and
//! capability-checks whichever bundle the user picks.
//!
//! # Two independent facts
//!
//! An entry's *type* and the *icon* drawn for it are separate: the type is the
//! association vocabulary a bundle's `AppInfo` declares, the icon is only how
//! the type is pictured. Many types deliberately share one glyph — every
//! textual and structured-config type draws [`IconKind::Text`] — but they stay
//! distinct types, because merging two would silently stop an application that
//! declares one of them from matching its own files.
//!
//! # Subclassing
//!
//! A concrete textual format *is* plain text as well as being itself, so
//! [`MediaType::parent`] names the broader type each one specialises — the
//! same subclass relation the freedesktop.org shared-mime-info database uses
//! (`text/x-csrc` is a subclass of `text/plain`). Association matching walks
//! that chain, so a text editor declaring `text/plain` opens a `.rs` file
//! while a Rust IDE declaring `text/x-rust` still claims it more
//! specifically. Naming a format precisely therefore costs nothing: the
//! broader declaration keeps matching.
//!
//! # Closed by design
//!
//! [`MediaType`] is a closed enum. An unrecognised extension classifies as
//! [`MediaType::ApplicationOctetStream`] (drawn with the generic file glyph)
//! rather than becoming a free-form string at a draw or association site, and
//! an unrecognised media-type spelling is simply not one this registry knows
//! ([`from_media_str`](MediaType::from_media_str) returns `None`) — fail
//! closed, never a guess.

use alloc::string::String;

use tairix_abi::SYSTEM_SERVICE_STORE;
use tairix_icon::{IconKind, IconRequest};

use crate::entry::{Entry, EntryKind};

/// A content type TAIRiX recognises, named by its media type.
///
/// Closed by design: an unrecognised spelling classifies as
/// [`ApplicationOctetStream`](Self::ApplicationOctetStream) rather than
/// becoming free-form text at a draw or association site.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MediaType {
    /// A directory the browser can descend into (`inode/directory`).
    InodeDirectory,
    /// A `<Name>.app` application bundle (`application/x-tairix-app`).
    TairixApp,
    /// A `<Name>.app` bundle listed from the system service store
    /// (`application/x-tairix-service`).
    TairixService,
    /// The TAIRiX executable envelope (`application/x-tairix-rxe`).
    TairixRxe,
    /// A bare WebAssembly module (`application/wasm`).
    Wasm,
    /// A raw ELF image (`application/x-elf`).
    Elf,
    /// Plain text and the config formats with no type of their own
    /// (`text/plain`).
    TextPlain,
    /// A Markdown document (`text/markdown`).
    TextMarkdown,
    /// Comma-separated values (`text/csv`).
    TextCsv,
    /// A JSON document (`application/json`).
    Json,
    /// A YAML document (`application/yaml`).
    Yaml,
    /// An XML document (`application/xml`).
    Xml,
    /// An HTML document (`text/html`).
    TextHtml,
    /// Rust source (`text/x-rust`).
    TextRust,
    /// Java source (`text/x-java`).
    TextJava,
    /// C source or a C header (`text/x-c`).
    TextC,
    /// A shell script (`application/x-shellscript`).
    ShellScript,
    /// A PDF document (`application/pdf`).
    Pdf,
    /// A PNG image (`image/png`).
    ImagePng,
    /// A JPEG image (`image/jpeg`).
    ImageJpeg,
    /// A GIF image (`image/gif`).
    ImageGif,
    /// An SVG image (`image/svg+xml`).
    ImageSvg,
    /// A RISC OS sprite image (`image/x-riscos-sprite`).
    ImageSprite,
    /// A BMP image (`image/bmp`).
    ImageBmp,
    /// A Windows icon image (`image/vnd.microsoft.icon`).
    ImageIcon,
    /// A WebP image (`image/webp`).
    ImageWebp,
    /// A TIFF image (`image/tiff`).
    ImageTiff,
    /// A ZIP archive (`application/zip`).
    ArchiveZip,
    /// A tar archive (`application/x-tar`).
    ArchiveTar,
    /// A gzip archive (`application/gzip`).
    ArchiveGzip,
    /// An xz archive (`application/x-xz`).
    ArchiveXz,
    /// A bzip2 archive (`application/x-bzip2`).
    ArchiveBzip2,
    /// A Zstandard archive (`application/zstd`).
    ArchiveZstd,
    /// A 7z archive (`application/x-7z-compressed`).
    Archive7z,
    /// A RAR archive (`application/vnd.rar`).
    ArchiveRar,
    /// Any content of no recognised type (`application/octet-stream`).
    ApplicationOctetStream,
}

impl MediaType {
    /// The IANA (or TAIRiX vendor) media-type spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InodeDirectory => "inode/directory",
            Self::TairixApp => "application/x-tairix-app",
            Self::TairixService => "application/x-tairix-service",
            Self::TairixRxe => "application/x-tairix-rxe",
            Self::Wasm => "application/wasm",
            Self::Elf => "application/x-elf",
            Self::TextPlain => "text/plain",
            Self::TextMarkdown => "text/markdown",
            Self::TextCsv => "text/csv",
            Self::Json => "application/json",
            Self::Yaml => "application/yaml",
            Self::Xml => "application/xml",
            Self::TextHtml => "text/html",
            Self::TextRust => "text/x-rust",
            Self::TextJava => "text/x-java",
            Self::TextC => "text/x-c",
            Self::ShellScript => "application/x-shellscript",
            Self::Pdf => "application/pdf",
            Self::ImagePng => "image/png",
            Self::ImageJpeg => "image/jpeg",
            Self::ImageGif => "image/gif",
            Self::ImageSvg => "image/svg+xml",
            Self::ImageSprite => "image/x-riscos-sprite",
            Self::ImageBmp => "image/bmp",
            Self::ImageIcon => "image/vnd.microsoft.icon",
            Self::ImageWebp => "image/webp",
            Self::ImageTiff => "image/tiff",
            Self::ArchiveZip => "application/zip",
            Self::ArchiveTar => "application/x-tar",
            Self::ArchiveGzip => "application/gzip",
            Self::ArchiveXz => "application/x-xz",
            Self::ArchiveBzip2 => "application/x-bzip2",
            Self::ArchiveZstd => "application/zstd",
            Self::Archive7z => "application/x-7z-compressed",
            Self::ArchiveRar => "application/vnd.rar",
            Self::ApplicationOctetStream => "application/octet-stream",
        }
    }

    /// The registry entry for a media-type spelling, if it is one we know.
    ///
    /// The match is ASCII-case-insensitive so a type reads the same however it
    /// was cased, mirroring the association matching. An unknown spelling is
    /// not one this closed registry knows: `None`, never a guess.
    #[must_use]
    pub fn from_media_str(text: &str) -> Option<Self> {
        ALL.iter()
            .copied()
            .find(|media| media.as_str().eq_ignore_ascii_case(text))
    }

    /// The broader type this one is a subclass of, if any.
    ///
    /// A concrete textual format is *also* plain text — a `.rs` file is Rust
    /// source and readable text both — so every textual type names
    /// [`TextPlain`](Self::TextPlain) (directly, or through an intermediate
    /// such as SVG's `application/xml`) and everything binary names `None`.
    /// The relation exists so naming a format precisely never narrows what can
    /// open it: association matching walks this chain, offering an application
    /// that declares an ancestor type while ranking one that declares the exact
    /// type ahead of it.
    ///
    /// The chain is **finite and acyclic**: each step names a strictly broader
    /// type and the broadest ones return `None`, so a walk from any variant
    /// reaches `None` in a bounded number of steps.
    #[must_use]
    pub const fn parent(self) -> Option<Self> {
        match self {
            Self::TextMarkdown
            | Self::TextCsv
            | Self::Json
            | Self::Yaml
            | Self::Xml
            | Self::TextHtml
            | Self::TextRust
            | Self::TextJava
            | Self::TextC
            | Self::ShellScript => Some(Self::TextPlain),
            Self::ImageSvg => Some(Self::Xml),
            Self::InodeDirectory
            | Self::TairixApp
            | Self::TairixService
            | Self::TairixRxe
            | Self::Wasm
            | Self::Elf
            | Self::TextPlain
            | Self::Pdf
            | Self::ImagePng
            | Self::ImageJpeg
            | Self::ImageGif
            | Self::ImageSprite
            | Self::ImageBmp
            | Self::ImageIcon
            | Self::ImageWebp
            | Self::ImageTiff
            | Self::ArchiveZip
            | Self::ArchiveTar
            | Self::ArchiveGzip
            | Self::ArchiveXz
            | Self::ArchiveBzip2
            | Self::ArchiveZstd
            | Self::Archive7z
            | Self::ArchiveRar
            | Self::ApplicationOctetStream => None,
        }
    }

    /// The icon that represents this content type.
    ///
    /// **This mapping is deliberately many-to-one, and it is the only part of
    /// the registry allowed to be.** A type's identity (its spelling, the
    /// vocabulary bundles declare associations in) and the picture drawn for it
    /// are two independent facts: several distinct types share one glyph — every
    /// textual and structured-config type draws [`IconKind::Text`], every
    /// archive draws [`IconKind::Archive`] — and that is correct. Never
    /// "simplify" two types into one because they draw the same icon: an
    /// application whose manifest declares the type that disappeared would
    /// silently stop matching its own files.
    ///
    /// A fine-grained kind shares its broad family's glyph when the system
    /// ships no distinct raster artwork for it, so the returned kind is always
    /// drawable (`lib/icon`).
    #[must_use]
    pub const fn icon(self) -> IconKind {
        match self {
            Self::InodeDirectory => IconKind::Folder,
            Self::TairixApp => IconKind::AppBundle,
            Self::TairixService => IconKind::ServiceBundle,
            Self::TairixRxe | Self::Wasm | Self::Elf => IconKind::Executable,
            Self::TextPlain
            | Self::TextMarkdown
            | Self::TextCsv
            | Self::Json
            | Self::Yaml
            | Self::Xml
            | Self::TextC => IconKind::Text,
            Self::TextHtml => IconKind::TextHtml,
            Self::TextRust => IconKind::TextRust,
            Self::TextJava => IconKind::TextJava,
            Self::ShellScript => IconKind::ShellScript,
            Self::Pdf => IconKind::Pdf,
            Self::ImagePng => IconKind::ImagePng,
            Self::ImageJpeg => IconKind::ImageJpeg,
            Self::ImageGif => IconKind::ImageGif,
            Self::ImageSvg => IconKind::ImageSvg,
            Self::ImageSprite => IconKind::ImageSprite,
            Self::ImageBmp | Self::ImageIcon | Self::ImageWebp | Self::ImageTiff => IconKind::Image,
            Self::ArchiveZip
            | Self::ArchiveTar
            | Self::ArchiveGzip
            | Self::ArchiveXz
            | Self::ArchiveBzip2
            | Self::ArchiveZstd
            | Self::Archive7z
            | Self::ArchiveRar => IconKind::Archive,
            Self::ApplicationOctetStream => IconKind::File,
        }
    }
}

/// Every [`MediaType`], for the spelling round-trip
/// ([`from_media_str`](MediaType::from_media_str)). The order is irrelevant;
/// the spellings are distinct, so at most one matches.
const ALL: &[MediaType] = &[
    MediaType::InodeDirectory,
    MediaType::TairixApp,
    MediaType::TairixService,
    MediaType::TairixRxe,
    MediaType::Wasm,
    MediaType::Elf,
    MediaType::TextPlain,
    MediaType::TextMarkdown,
    MediaType::TextCsv,
    MediaType::Json,
    MediaType::Yaml,
    MediaType::Xml,
    MediaType::TextHtml,
    MediaType::TextRust,
    MediaType::TextJava,
    MediaType::TextC,
    MediaType::ShellScript,
    MediaType::Pdf,
    MediaType::ImagePng,
    MediaType::ImageJpeg,
    MediaType::ImageGif,
    MediaType::ImageSvg,
    MediaType::ImageSprite,
    MediaType::ImageBmp,
    MediaType::ImageIcon,
    MediaType::ImageWebp,
    MediaType::ImageTiff,
    MediaType::ArchiveZip,
    MediaType::ArchiveTar,
    MediaType::ArchiveGzip,
    MediaType::ArchiveXz,
    MediaType::ArchiveBzip2,
    MediaType::ArchiveZstd,
    MediaType::Archive7z,
    MediaType::ArchiveRar,
    MediaType::ApplicationOctetStream,
];

/// Extension → [`MediaType`] table, the one source for the mapping a file
/// name's extension implies. Directory / app / service types are kind-implied,
/// not extension-implied, so they are absent here (they are reached only
/// through [`media_for_entry`]).
///
/// Every extension the browser recognises appears exactly once. Source and
/// structured-config formats map to their honest concrete type — a `.json`
/// file is `application/json`, not "text that happens to draw a text glyph" —
/// so an application that declares one of them keeps matching its own files;
/// only [`MediaType::icon`] is coarse. `text/plain` is left for the formats
/// that genuinely have no type of their own. The two TAIRiX-native forms (the
/// executable envelope, a service bundle) carry a vendor
/// `application/x-tairix-*` spelling.
const EXTENSION_TABLE: &[(MediaType, &[&str])] = &[
    (MediaType::TairixRxe, &["rxe"]),
    (MediaType::Wasm, &["wasm"]),
    (MediaType::Elf, &["elf"]),
    (
        MediaType::TextPlain,
        &["txt", "rst", "log", "toml", "ini", "cfg", "conf"],
    ),
    (MediaType::TextMarkdown, &["md", "markdown"]),
    (MediaType::TextCsv, &["csv"]),
    (MediaType::Json, &["json"]),
    (MediaType::Yaml, &["yaml", "yml"]),
    (MediaType::Xml, &["xml"]),
    (MediaType::TextHtml, &["html", "htm"]),
    (MediaType::TextRust, &["rs"]),
    (MediaType::TextJava, &["java"]),
    (MediaType::TextC, &["c", "h"]),
    (MediaType::ShellScript, &["sh"]),
    (MediaType::Pdf, &["pdf"]),
    (MediaType::ImagePng, &["png"]),
    (MediaType::ImageJpeg, &["jpg", "jpeg"]),
    (MediaType::ImageGif, &["gif"]),
    (MediaType::ImageSvg, &["svg"]),
    (MediaType::ImageSprite, &["spr"]),
    (MediaType::ImageBmp, &["bmp"]),
    (MediaType::ImageIcon, &["ico"]),
    (MediaType::ImageWebp, &["webp"]),
    (MediaType::ImageTiff, &["tiff", "tif"]),
    (MediaType::ArchiveZip, &["zip"]),
    (MediaType::ArchiveTar, &["tar"]),
    (MediaType::ArchiveGzip, &["gz", "tgz"]),
    (MediaType::ArchiveXz, &["xz"]),
    (MediaType::ArchiveBzip2, &["bz2"]),
    (MediaType::ArchiveZstd, &["zst"]),
    (MediaType::Archive7z, &["7z"]),
    (MediaType::ArchiveRar, &["rar"]),
];

/// The media type a file name's extension implies, if any.
///
/// The lookup is ASCII-case-insensitive and allocates no `String`: it splits
/// the extension off in place and compares it against the static table.
/// `None` for a name with no extension, a leading-dot dotfile whose only dot
/// starts the name, or an extension the registry does not recognise.
#[must_use]
pub fn media_for_name(name: &str) -> Option<MediaType> {
    let ext = extension(name)?;
    EXTENSION_TABLE
        .iter()
        .find(|(_, exts)| {
            exts.iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
        .map(|(media, _)| *media)
}

/// The media type of a listed entry, given the components of the directory it
/// was listed from.
///
/// A directory classifies as [`InodeDirectory`](MediaType::InodeDirectory)
/// regardless of its name; a bundle classifies as
/// [`TairixService`](MediaType::TairixService) when `parent` is the system
/// service store and [`TairixApp`](MediaType::TairixApp) otherwise; a regular
/// file takes its extension's type, or
/// [`ApplicationOctetStream`](MediaType::ApplicationOctetStream) when the
/// extension is unrecognised (or absent) — fail closed to the generic type.
#[must_use]
pub fn media_for_entry(entry: &Entry, parent: &[String]) -> MediaType {
    match entry.kind() {
        EntryKind::Directory => MediaType::InodeDirectory,
        EntryKind::Bundle => {
            if is_system_service_store(parent) {
                MediaType::TairixService
            } else {
                MediaType::TairixApp
            }
        }
        EntryKind::File => {
            media_for_name(entry.name()).unwrap_or(MediaType::ApplicationOctetStream)
        }
    }
}

/// The icon request for one listed entry drawn out of the directory `dir`.
///
/// An application bundle names *itself* in the request, so the artwork layer
/// can prefer the icon the bundle carries in its own `Resources/` over the
/// generic artwork for its class; every other entry resolves by class alone.
/// `scratch` is a buffer the caller reuses across the entries of one frame, so
/// a grid of tiles spells its bundle paths without allocating one per tile.
///
/// Both surfaces that draw directory entries — the file manager's grid and the
/// desktop's icons — build their request here, so an application cannot be
/// pictured one way on the desktop and another in the manager.
#[must_use]
pub fn entry_icon_request<'a>(
    dir: &str,
    entry: &Entry,
    kind: IconKind,
    scratch: &'a mut String,
) -> IconRequest<'a> {
    if !entry.is_bundle() {
        return IconRequest::kind(kind);
    }
    scratch.clear();
    scratch.push_str(dir);
    crate::vfs::push_child(scratch, entry.name());
    IconRequest::bundle(kind, scratch)
}

/// Whether `parent`'s root-first components are exactly the system service
/// store ([`SYSTEM_SERVICE_STORE`]).
///
/// The store path is spelled once in `lib/abi`; this compares against that one
/// definition component-wise rather than carrying a second literal.
fn is_system_service_store(parent: &[String]) -> bool {
    let mut expected = SYSTEM_SERVICE_STORE
        .split('/')
        .filter(|part| !part.is_empty());
    let mut got = parent.iter();
    loop {
        match (expected.next(), got.next()) {
            (Some(want), Some(have)) if want == have => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// `media` followed by every broader type it is a subclass of, most specific
/// first — the chain application association matches along.
///
/// The walk is bounded by the number of registry entries: the relation is
/// acyclic by construction, and a chain longer than the registry itself could
/// only mean a mis-edited [`MediaType::parent`] had introduced a cycle, so the
/// bound stops rather than spins.
pub(crate) fn ancestry(media: MediaType) -> impl Iterator<Item = MediaType> {
    let mut step = Some(media);
    core::iter::from_fn(move || {
        let current = step?;
        step = current.parent();
        Some(current)
    })
    .take(ALL.len())
}

/// The filename extension of `name`: the text after the final `.`, or `None`
/// when there is no such dot, the dot is the first byte (a dotfile with no
/// further extension, e.g. `.profile`), or nothing follows it (`archive.`).
///
/// Shared by [`media_for_name`] and the "Open With…" association model so the
/// two split a name's extension off identically, never each its own copy.
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

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
