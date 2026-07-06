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
    /// The BCP-47 locale directory (`en-US/` is the mandatory canonical one).
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

    use rustos_help::{lint_help_trees, LintDoc};

    use super::HELP_FILES;

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

    /// Every discovered tree passes the one shared help-tree lint
    /// (`plans/APPS.md` §8.1) — the same judgement `cargo xtask help-lint`
    /// gates on: spellings and fail-closed parse bounds, canonical `en-US/`
    /// presence, required-locale completeness, no translation-only
    /// documents, cross-locale `OPTIONS` switch-key drift, and the content
    /// policy. A tree this rejects can never reach an image.
    #[test]
    fn every_discovered_tree_passes_the_shared_lint() {
        assert!(!HELP_FILES.is_empty(), "at least one help tree must exist");
        let docs: Vec<LintDoc<'_>> = HELP_FILES
            .iter()
            .map(|doc| LintDoc {
                bundle: doc.bundle,
                locale: doc.locale,
                file: doc.file,
                bytes: doc.bytes,
            })
            .collect();
        let violations = lint_help_trees(&docs);
        assert!(violations.is_empty(), "{}", violations.join("\n"));
    }
}
