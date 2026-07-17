//! The adapter that scopes a [`BundleStore`] to one bundle's `Help/` tree
//! and hands it to the `lib/help` engine as its injected read seam.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_help::{HelpSource, SourceError, MAX_DOC_LEN};

use crate::io::BundleStore;

/// One bundle's `Help/` tree, viewed through the store.
///
/// The engine only ever asks for spellings it validated (`Locale`,
/// `DocumentName` file names), and this adapter only ever prefixes them
/// with the one bundle directory it was constructed for, so the seam can
/// never be steered outside that tree. Reads are bounded to the engine's
/// own document limit, so the limit — not memory exhaustion — decides an
/// oversized document's fate.
pub struct ScopedHelp<'a> {
    store: &'a dyn BundleStore,
    bundle_dir: &'a str,
}

impl<'a> ScopedHelp<'a> {
    /// Scope `store` to `bundle_dir`'s `Help/` tree.
    #[must_use]
    pub fn new(store: &'a dyn BundleStore, bundle_dir: &'a str) -> Self {
        Self { store, bundle_dir }
    }
}

impl HelpSource for ScopedHelp<'_> {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        self.store
            .locale_dirs(self.bundle_dir)
            .map_err(|_| SourceError)
    }

    fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        self.store
            .read_doc(self.bundle_dir, locale_dir, file_name, MAX_DOC_LEN)
            .map_err(|_| SourceError)
    }
}
