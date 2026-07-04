//! The `ls` bundle's own `Help/` tree, embedded from the crate's `Help/`
//! source directory.
//!
//! An installed image carries these documents on the read-only `/System`
//! volume at `/System/Apps/ls.app/Help/<locale>/ls.md`. The bytes are
//! embedded **here**, beside the source documents, so the image builder
//! (`tools/mkimage`) and the QEMU image fixtures plant exactly what this
//! crate ships — one source of truth, never a second copy of the tree to
//! drift.

/// The file name every locale directory holds: one document, the bundle's
/// primary command.
pub const HELP_DOC_NAME: &str = "ls.md";

/// The bundle's `Help/` documents as `(locale_dir, bytes)` rows: the
/// canonical `default/` (en-US) document plus the standing required locale
/// set (plans/APPS.md §8.1).
pub const HELP_DOCS: &[(&str, &[u8])] = &[
    ("default", include_bytes!("../Help/default/ls.md")),
    ("fr-FR", include_bytes!("../Help/fr-FR/ls.md")),
    ("de-DE", include_bytes!("../Help/de-DE/ls.md")),
    ("es-ES", include_bytes!("../Help/es-ES/ls.md")),
    ("uk-UA", include_bytes!("../Help/uk-UA/ls.md")),
    ("it-IT", include_bytes!("../Help/it-IT/ls.md")),
];

#[cfg(test)]
mod tests {
    use rustos_help::{load, DocumentName, HelpSource, Loaded, Locale, SourceError};

    use alloc::string::String;
    use alloc::vec::Vec;

    use super::{HELP_DOCS, HELP_DOC_NAME};

    /// The embedded tree itself as a [`HelpSource`], so the engine parses
    /// the real shipped documents.
    struct EmbeddedTree;

    impl HelpSource for EmbeddedTree {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(HELP_DOCS
                .iter()
                .map(|(locale, _)| String::from(*locale))
                .collect())
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if file_name != HELP_DOC_NAME {
                return Ok(None);
            }
            Ok(HELP_DOCS
                .iter()
                .find(|(locale, _)| *locale == locale_dir)
                .map(|(_, bytes)| bytes.to_vec()))
        }
    }

    /// Every shipped document — the canonical `default/` and each required
    /// translation — parses under the engine's fail-closed bounds, so a
    /// malformed page can never reach an image.
    #[test]
    fn every_shipped_document_parses() {
        let name = DocumentName::parse("ls").expect("valid name");
        for (locale_dir, _) in HELP_DOCS {
            let locale = Locale::parse(locale_dir).expect("valid locale dir");
            let Loaded { selection, .. } =
                load(&EmbeddedTree, &locale, &name).expect("document parses");
            assert_eq!(&selection.locale_dir, locale_dir);
        }
    }

    /// The tree ships the mandatory `default/` canonical document and the
    /// standing required locale set, exactly once each.
    #[test]
    fn the_required_locale_set_is_complete() {
        let expected = ["default", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        assert_eq!(HELP_DOCS.len(), expected.len());
        for locale in expected {
            assert_eq!(
                HELP_DOCS
                    .iter()
                    .filter(|(candidate, _)| *candidate == locale)
                    .count(),
                1,
                "{locale} must appear exactly once"
            );
        }
    }

    /// Every locale's `OPTIONS` section records exactly the switches the
    /// argument parser accepts (plans/APPS.md §3.1) — the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one.
    #[test]
    fn every_document_records_the_parser_switches() {
        for (locale_dir, bytes) in HELP_DOCS {
            let text = core::str::from_utf8(bytes).expect("utf8 document");
            for switch in ["`-a, --all`", "`-l, --long`", "`-h, -?`"] {
                assert!(
                    text.contains(switch),
                    "{locale_dir}/ls.md must document {switch}"
                );
            }
        }
    }
}
