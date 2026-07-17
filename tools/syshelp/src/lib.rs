//! Build-discovered system app-store bundle payload (Help documents and
//! `Resources/` files) for image authoring.
//!
//! TAIRiX ships each command app's internationalised command help as a
//! structured-Markdown `Help/` tree on the read-only `/System` volume, at
//! `/System/Apps/<name>.app/Help/<locale>/<doc>.md` (`plans/APPS.md`), and
//! each app's bundle resources (e.g. `lspci`'s compiled ID-database table,
//! `plans/DEVICES.md`) at `/System/Apps/<name>.app/Resources/<file>`. The
//! image builder (`tools/mkimage`) and the QEMU image fixture must plant
//! both onto the volume they author.
//!
//! The source of truth for the payload is the bundle's own on-disk `Help/`
//! and `Resources/` directories. This crate's build script walks the
//! command-app source roots (`userland/apps`, `userland/gui`,
//! `userland/shell`; each bundle named by its crate's `AppInfo.toml`,
//! never the crate directory), finds every
//! help document and resource file, and embeds each as a row in
//! [`HELP_FILES`] / [`RESOURCE_FILES`], so the image builder plants the
//! payload by iterating discovered data — **never** a hand-maintained
//! per-bundle list that a new bundle would force an edit to (the duplication
//! the charter forbids). Adding a bundle's payload is dropping files under
//! `<root>/<name>/Help/<locale>/` or `<root>/<name>/Resources/`; the next
//! build rediscovers them. Bundle payload is therefore authored in exactly
//! one place — the bundle — and never hardcoded into an app binary or copied
//! into the image builder.
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

/// One shipped bundle resource, ready to plant at
/// `/System/Apps/<bundle>/Resources/<file>` on the read-only `/System`
/// volume.
///
/// A resource is bundle data the program reads at runtime through the
/// secured VFS (never `include_bytes!` into its binary): e.g. `lspci`'s
/// compiled `pci.ids.bin` lookup table. The image builder writes `bytes` at
/// `Apps/<bundle>/Resources/<file>`, and the bundle's signed `AppInfo`
/// content hash covers it, so a tampered resource fails the load gate
/// closed.
#[derive(Clone, Copy, Debug)]
pub struct ResourceFile {
    /// The bundle directory name, including the `.app` suffix
    /// (e.g. `lspci.app`).
    pub bundle: &'static str,
    /// The resource file name (e.g. `pci.ids.bin`).
    pub file: &'static str,
    /// The file's bytes, embedded from the bundle's source `Resources/`
    /// directory.
    pub bytes: &'static [u8],
}

/// Every command app's `Resources/` files, discovered from the source tree
/// at build time.
///
/// Rows are ordered deterministically (by bundle, then file name), so the
/// planted store and any reproducible image are stable across builds and
/// hosts.
pub const RESOURCE_FILES: &[ResourceFile] =
    &include!(concat!(env!("OUT_DIR"), "/resource_files.rs"));

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeSet;
    use std::vec::Vec;

    use tairix_help::{lint_help_trees, LintDoc};

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

    /// Discovery finds the bundle resources the shipped command apps carry.
    /// This anchors the resource scan exactly as the help scan above: if
    /// the roots or the `Resources/` layout regress, at least the known
    /// resource-carrying apps must still be found, with non-empty bytes.
    #[test]
    fn discovers_the_shipped_bundle_resources() {
        let lspci_table = super::RESOURCE_FILES
            .iter()
            .find(|r| r.bundle == "lspci.app" && r.file == "pci.ids.bin")
            .expect("lspci.app's pci.ids.bin resource must be discovered");
        assert!(
            !lspci_table.bytes.is_empty(),
            "a discovered resource carries its file bytes"
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
