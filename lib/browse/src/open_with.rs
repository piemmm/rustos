//! The "Open With…" type→bundle association model (`plans/NEW-FILEMANAGER.md`
//! `FM6b`).
//!
//! When the user asks to open a regular file with a chosen application, the
//! file manager offers the installed bundles whose signed `AppInfo` claims the
//! file's type. This module is the **pure model** behind that offer, host-proven
//! without a kernel exactly as the [`Activation`](crate::activate) decision is:
//!
//! * [`mime_for_name`] derives a file's content type from its filename
//!   extension — the one bridge from a name (all the VFS listing gives us) to
//!   the MIME vocabulary a bundle declares its associations in. It is a display
//!   *hint* like the [`icon`](crate::icon) classifier, never authority: it
//!   decides which applications are *offered*, and the load gate still verifies
//!   and capability-checks whichever one the user picks.
//! * [`BundleSource`] is the injected enumeration seam — the installed-bundle
//!   analogue of [`DirectorySource`](crate::source). On a running system it is
//!   backed by the app store (each bundle's `AppInfo` MIME table); in tests it
//!   is an in-memory list, so the matching logic is exercised without a kernel.
//! * [`applications_for`] selects the bundles that handle a file's type, in the
//!   source's order. No match is an **honest empty answer** — the caller shows a
//!   "no application" notice, never a crash and never a fabricated default.
//!
//! The engine holds no launch authority: it *names* the candidate bundles and
//! *what should happen*; spawning the chosen bundle through the signed load gate
//! stays in the file manager's own capability-checked tail under the user's
//! identity (so the read-only picker, which composes the same engine, never
//! launches). Deciding a file's type here never opens it.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;

use crate::icon::extension;

/// One installed application and the file types its signed `AppInfo` claims to
/// open — a single "Open With…" candidate.
///
/// The [`mime_types`](Self::mime_types) are the bundle's *own* declared
/// associations (`AppInfo`'s MIME table), never a registry the file manager
/// invents: the manager reads what each bundle claims and offers only those.
/// [`bundle_path`](Self::bundle_path) is the absolute path of the `<Name>.app`
/// directory the caller launches through the ordinary signed load gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppAssociation {
    name: String,
    bundle_path: String,
    mime_types: Vec<String>,
}

impl AppAssociation {
    /// Construct an association from a bundle's display name, the absolute path
    /// of its `<Name>.app` directory, and the MIME types its `AppInfo` declares.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        bundle_path: impl Into<String>,
        mime_types: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            bundle_path: bundle_path.into(),
            mime_types,
        }
    }

    /// The bundle's human-readable name — the "Open With…" menu label.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The absolute path of the `<Name>.app` bundle to launch through the
    /// signed load gate.
    #[must_use]
    pub fn bundle_path(&self) -> &str {
        &self.bundle_path
    }

    /// The MIME types the bundle's `AppInfo` declares it can open.
    #[must_use]
    pub fn mime_types(&self) -> &[String] {
        &self.mime_types
    }

    /// Whether this bundle declares an association with `mime`, matched
    /// ASCII-case-insensitively so a type reads the same however it was cased.
    #[must_use]
    pub fn handles(&self, mime: &str) -> bool {
        self.mime_types
            .iter()
            .any(|declared| declared.eq_ignore_ascii_case(mime))
    }
}

/// The installed-application enumeration seam — the "Open With…" analogue of
/// [`DirectorySource`](crate::source).
///
/// It is the one thing the association model needs from the outside world: the
/// installed bundles and the file types each declares. Keeping it a trait means
/// the matching logic is exhaustively testable against an in-memory list without
/// a kernel, exactly as the browser's directory reads are.
///
/// On a running system the source is backed by the app store, reading each
/// bundle's signed `AppInfo` MIME table under the caller's own identity — the
/// permission decision stays in the store behind the seam, never here.
pub trait BundleSource {
    /// Enumerate the installed applications and their declared file-type
    /// associations.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the app store cannot be
    /// enumerated (for example [`Errno::PermissionDenied`]).
    fn installed_bundles(&mut self) -> Result<Vec<AppAssociation>, Errno>;
}

/// The MIME content type of a regular file named `name`, from its lowercased
/// filename extension, or `None` when the extension is unrecognised (or the
/// name has none — including a leading-dot "dotfile").
///
/// The recognised set is exactly the one the [`icon`](crate::icon) classifier
/// draws a specific glyph for — text, image, archive, and executable formats —
/// so the manager offers "Open With…" for precisely the files it shows a typed
/// icon for. It is a fail-closed *hint*: an unknown type yields `None`, which
/// [`applications_for`] turns into an honest empty candidate list rather than a
/// guess.
#[must_use]
pub fn mime_for_name(name: &str) -> Option<&'static str> {
    extension(name).and_then(mime_for_extension)
}

/// The MIME type for a bare filename extension (without the dot), matched
/// ASCII-case-insensitively.
fn mime_for_extension(ext: &str) -> Option<&'static str> {
    MIME_TABLE
        .iter()
        .find(|(_, exts)| {
            exts.iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
        .map(|(mime, _)| *mime)
}

/// Extension → MIME table, grouped by content type. The extension set matches
/// the [`icon`](crate::icon) classifier's; source and structured-config formats
/// map to their honest concrete type (`text/plain`, `application/json`, …)
/// rather than an invented one. The TAIRiX-native executable envelope has no
/// registered IANA type, so it carries the vendor `application/x-tairix-rxe`.
const MIME_TABLE: &[(&str, &[&str])] = &[
    (
        "text/plain",
        &[
            "txt", "log", "rst", "rs", "toml", "ini", "cfg", "conf", "sh", "c", "h",
        ],
    ),
    ("text/markdown", &["md", "markdown"]),
    ("text/csv", &["csv"]),
    ("application/json", &["json"]),
    ("application/yaml", &["yaml", "yml"]),
    ("application/xml", &["xml"]),
    ("image/png", &["png"]),
    ("image/jpeg", &["jpg", "jpeg"]),
    ("image/gif", &["gif"]),
    ("image/bmp", &["bmp"]),
    ("image/svg+xml", &["svg"]),
    ("image/vnd.microsoft.icon", &["ico"]),
    ("image/webp", &["webp"]),
    ("image/tiff", &["tiff", "tif"]),
    ("application/zip", &["zip"]),
    ("application/x-tar", &["tar"]),
    ("application/gzip", &["gz", "tgz"]),
    ("application/x-xz", &["xz"]),
    ("application/x-bzip2", &["bz2"]),
    ("application/zstd", &["zst"]),
    ("application/x-7z-compressed", &["7z"]),
    ("application/vnd.rar", &["rar"]),
    ("application/x-tairix-rxe", &["rxe"]),
    ("application/wasm", &["wasm"]),
    ("application/x-elf", &["elf"]),
];

/// The installed applications that can open a file named `name`, in `bundles`'
/// enumeration order — the "Open With…" candidate list.
///
/// The file's type is derived by [`mime_for_name`] and each bundle is offered
/// when it [`handles`](AppAssociation::handles) that type. The result is empty —
/// an honest "no application" answer — when the file's type is unrecognised or
/// no installed bundle claims it; it never falls back to a guessed default.
#[must_use]
pub fn applications_for<'a>(name: &str, bundles: &'a [AppAssociation]) -> Vec<&'a AppAssociation> {
    match mime_for_name(name) {
        Some(mime) => bundles.iter().filter(|b| b.handles(mime)).collect(),
        None => Vec::new(),
    }
}
