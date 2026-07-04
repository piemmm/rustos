//! Build-discovered system app-store Help payload for image authoring.
//!
//! RustOS ships each command app's internationalised command help as a
//! structured-Markdown `Help/` tree on the read-only `/System` volume, at
//! `/System/Apps/<name>.app/Help/<locale>/<doc>.md` (`plans/APPS.md`). The
//! image builder (`tools/mkimage`) and the QEMU image fixture must plant that
//! tree onto the volume they author.
//!
//! The source of truth for the help is the bundle's own on-disk `Help/`
//! directory. This crate's build script walks the command-app source roots
//! (`userland/apps`, `userland/shell`), finds every help document, and
//! embeds each as a row in [`HELP_FILES`], so
//! the image builder plants help by iterating discovered data — **never** a
//! hand-maintained per-bundle list that a new bundle would force an edit to
//! (the duplication the charter forbids). Adding a bundle's help is dropping
//! files under `<root>/<name>/Help/<locale>/`; the next build
//! rediscovers them. Help documents are therefore authored in exactly one
//! place — the bundle — and never hardcoded into an app binary or copied into
//! the image builder.
//!
//! The payload is `&'static [u8]` document bytes embedded at build time, so
//! this crate is `no_std` and depends on no app crate: both the host image
//! builder and the freestanding QEMU fixture (which also links into the
//! aarch64 guest tail) consume it unchanged.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// One shipped Help document, ready to plant at
/// `/System/Apps/<bundle>/Help/<locale>/<file>` on the read-only `/System`
/// volume.
///
/// The fields are the volume-relative path components under the app store's
/// `Help/` tree plus the document's embedded bytes; the image builder writes
/// `bytes` at `Apps/<bundle>/Help/<locale>/<file>`.
#[derive(Clone, Copy, Debug)]
pub struct HelpFile {
    /// The bundle directory name, including the `.app` suffix (e.g. `ls.app`).
    pub bundle: &'static str,
    /// The BCP-47 locale directory (or the `default` sentinel, always en-US).
    pub locale: &'static str,
    /// The document file name (e.g. `ls.md`).
    pub file: &'static str,
    /// The document's bytes, embedded from the bundle's source `Help/` tree.
    pub bytes: &'static [u8],
}

/// Every command app's Help documents, discovered from the source tree at
/// build time.
///
/// Rows are ordered deterministically (by bundle, then locale, then file
/// name), so the planted store and any reproducible image are stable across
/// builds and hosts.
pub const HELP_FILES: &[HelpFile] = &include!(concat!(env!("OUT_DIR"), "/help_files.rs"));

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeSet;
    use std::vec::Vec;

    use rustos_help::{DocumentName, HelpDoc, Locale};

    use super::HELP_FILES;

    /// The standing required locale set every OS command app must ship
    /// (`plans/APPS.md` §8.1): the mandatory `default/` canonical (en-US)
    /// document plus the standing translations.
    const REQUIRED_LOCALES: &[&str] = &["default", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];

    /// Discovery finds the command apps that ship help. This anchors the
    /// scan: if the roots or the tree layout regress, at least the known
    /// command apps must still be found.
    #[test]
    fn discovers_the_shipped_command_apps() {
        let bundles: BTreeSet<&str> = HELP_FILES.iter().map(|doc| doc.bundle).collect();
        assert!(bundles.contains("ls.app"), "ls.app help must be discovered");
        assert!(
            bundles.contains("man.app"),
            "man.app help must be discovered"
        );
    }

    /// Every discovered document parses under the engine's fail-closed
    /// bounds, so a malformed page can never reach an image; its locale and
    /// document-name spellings are the ones the running engine validates.
    #[test]
    fn every_discovered_document_is_valid() {
        assert!(!HELP_FILES.is_empty(), "at least one help tree must exist");
        for doc in HELP_FILES {
            Locale::parse(doc.locale)
                .unwrap_or_else(|_| panic!("{}/{} bad locale", doc.bundle, doc.locale));
            let stem = doc
                .file
                .strip_suffix(".md")
                .unwrap_or_else(|| panic!("{} is not a .md document", doc.file));
            DocumentName::parse(stem).unwrap_or_else(|_| panic!("{} bad document name", doc.file));
            HelpDoc::parse(doc.bytes).unwrap_or_else(|_| {
                panic!("{}/{}/{} does not parse", doc.bundle, doc.locale, doc.file)
            });
        }
    }

    /// Each bundle that ships help ships the complete required locale set for
    /// each of its documents (`plans/APPS.md` §8.1), so an image never carries
    /// a partially-translated command.
    #[test]
    fn every_bundle_ships_the_required_locale_set() {
        let bundles: BTreeSet<&str> = HELP_FILES.iter().map(|doc| doc.bundle).collect();
        for bundle in bundles {
            // The document names this bundle ships (by file name).
            let files: BTreeSet<&str> = HELP_FILES
                .iter()
                .filter(|doc| doc.bundle == bundle && doc.locale == "default")
                .map(|doc| doc.file)
                .collect();
            assert!(
                !files.is_empty(),
                "{bundle} must ship a default/ (en-US) document"
            );
            for file in files {
                let locales: BTreeSet<&str> = HELP_FILES
                    .iter()
                    .filter(|doc| doc.bundle == bundle && doc.file == file)
                    .map(|doc| doc.locale)
                    .collect();
                let missing: Vec<&str> = REQUIRED_LOCALES
                    .iter()
                    .copied()
                    .filter(|locale| !locales.contains(locale))
                    .collect();
                assert!(
                    missing.is_empty(),
                    "{bundle} {file} missing locales: {}",
                    missing.join(", ")
                );
            }
        }
    }

    /// No bundle carries a document under a translated locale that is absent
    /// from `default/` (`plans/APPS.md` §2.1): there would be nothing to fall
    /// back *from*, and it signals a drifted tree.
    #[test]
    fn translations_never_introduce_a_document_absent_from_default() {
        let bundles: BTreeSet<&str> = HELP_FILES.iter().map(|doc| doc.bundle).collect();
        for bundle in bundles {
            let default_docs: BTreeSet<&str> = HELP_FILES
                .iter()
                .filter(|doc| doc.bundle == bundle && doc.locale == "default")
                .map(|doc| doc.file)
                .collect();
            for doc in HELP_FILES.iter().filter(|doc| doc.bundle == bundle) {
                assert!(
                    default_docs.contains(doc.file),
                    "{bundle} {}/{} has no default/ counterpart",
                    doc.locale,
                    doc.file
                );
            }
        }
    }
}
