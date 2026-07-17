//! A command app's **own** short help (`plans/APPS.md` §4), in one place.
//!
//! Every command app answers its reserved `-h`/`-?` switches with the same
//! render: load its own Help document in the active locale, render the
//! short view (`NAME` + `SYNOPSIS` + compact `OPTIONS`), and encode it as
//! `lib/vt` operations. That sequence must not be re-derived per tool —
//! this module is its single definition.
//!
//! The pure [`own_short_help`] helper works over any injected
//! [`HelpSource`]; the syscall-backed source that reads the running
//! bundle's own `/System/Apps/<word>.app/Help/` tree is `BundleHelp`
//! (compiled under the `rt` feature), which a freestanding `Run` binary
//! constructs with its own command word.

use alloc::vec::Vec;

use tairix_vt::encode_all_into;

use crate::locale::{load, DocumentName, HelpSource, Locale};
use crate::render::render_short;

/// Render `word`'s own short help — the `NAME`, `SYNOPSIS`, and compact
/// `OPTIONS` of its Help document — as encoded `lib/vt` output bytes.
///
/// `locale` is the user's raw `LANG` preference, if any: a well-formed tag
/// selects that locale through the engine's fallback chain, and a missing
/// or malformed one degrades to the canonical `en-US/` documents rather
/// than making the short help unreadable.
///
/// Returns `None` when no document can be served (an invalid `word`
/// spelling, an absent or unreadable `Help/` tree, a document that does
/// not parse). The caller then prints its own usage banner — its own
/// text, never fabricated help content — so `-h` never fails.
pub fn own_short_help(
    source: &dyn HelpSource,
    locale: Option<&str>,
    word: &str,
) -> Option<Vec<u8>> {
    let name = DocumentName::parse(word).ok()?;
    let requested = locale
        .and_then(|tag| Locale::parse(tag).ok())
        .unwrap_or_default();
    let loaded = load(source, &requested, &name).ok()?;
    let mut bytes = Vec::new();
    encode_all_into(&render_short(&loaded.doc), &mut bytes);
    Some(bytes)
}

#[cfg(feature = "rt")]
mod rt_source {
    //! The production own-bundle [`HelpSource`]: the running command app's
    //! `/System/Apps/<word>.app/Help/` tree, read through the
    //! kernel-authorised `fs_*` syscall wrappers. It adds no authority —
    //! every path resolution, per-inode permission, and mount-flag check
    //! happens kernel-side under the caller's attested identity — and the
    //! bundle directory is spelled from the one shared `lib/abi`
    //! definition, so it cannot drift from where the image builder plants
    //! the documents.

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    use tairix_abi::fs::{DirEntry, FileKind, FS_IO_MAX};
    use tairix_abi::{Errno, BUNDLE_SUFFIX, SYSTEM_APP_STORE};

    use crate::doc::MAX_DOC_LEN;
    use crate::locale::{HelpSource, SourceError};

    /// Initial byte size of the locale-directory listing buffer: one page
    /// covers a bundle's handful of locale directories; `BufferTooSmall`
    /// grows it (below).
    const DIR_BUF_INITIAL: usize = 4096;

    /// Ceiling for the directory-listing buffer: the kernel's own per-call
    /// staging cap ([`FS_IO_MAX`]), so the buffer grows exactly as far as
    /// one `fs_readdir` transfer can ever fill and no further.
    const DIR_BUF_MAX: usize = FS_IO_MAX;

    /// A command app's own bundle `Help/` tree in the system app store,
    /// scoped to one command word at construction.
    ///
    /// A build without the bundle's documents simply has no locales: the
    /// engine then reports "not found" and the caller falls back to its
    /// usage banner, so `-h` never fails.
    pub struct BundleHelp {
        /// The app's own command word (e.g. `ls`), naming its bundle
        /// directory `<word>.app` in the system app store.
        word: &'static str,
    }

    impl BundleHelp {
        /// The `Help/` tree of `word`'s own bundle,
        /// `/System/Apps/<word>.app/Help/`.
        #[must_use]
        pub const fn new(word: &'static str) -> Self {
            Self { word }
        }

        /// `/System/Apps/<word>.app/Help`.
        fn help_root(&self) -> String {
            format!("{SYSTEM_APP_STORE}/{}{BUNDLE_SUFFIX}/Help", self.word)
        }

        /// Read a directory's raw entry stream, growing the buffer up to
        /// the kernel's per-call cap.
        fn read_dir_bytes(dir: &tairix_rt::Dir) -> Result<Vec<u8>, SourceError> {
            let mut buf = alloc::vec![0u8; DIR_BUF_INITIAL];
            let used = loop {
                match dir.read(&mut buf) {
                    Ok(used) => break used,
                    Err(ret) => match Errno::from_syscall(ret) {
                        Errno::BufferTooSmall if buf.len() < DIR_BUF_MAX => {
                            buf.resize((buf.len() * 2).min(DIR_BUF_MAX), 0);
                        }
                        _ => return Err(SourceError),
                    },
                }
            };
            buf.truncate(used);
            Ok(buf)
        }
    }

    impl HelpSource for BundleHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            let path = self.help_root();
            // No tree, no locales: the engine reports "not found" and the
            // caller falls back to its usage banner.
            let Ok(dir) = tairix_rt::open_dir(path.as_bytes()) else {
                return Ok(Vec::new());
            };
            let bytes = Self::read_dir_bytes(&dir)?;
            let mut dirs = Vec::new();
            let mut rest = bytes.as_slice();
            while !rest.is_empty() {
                let (entry, consumed) = DirEntry::decode(rest).map_err(|_| SourceError)?;
                rest = &rest[consumed..];
                if entry.kind != FileKind::Directory {
                    continue;
                }
                // A non-UTF-8 name can never be a locale directory the
                // engine validated a spelling for; skipping it loses
                // nothing and fabricates nothing.
                if let Ok(name) = core::str::from_utf8(entry.name) {
                    dirs.push(String::from(name));
                }
            }
            Ok(dirs)
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            let path = format!("{}/{locale_dir}/{file_name}", self.help_root());
            let file = match tairix_rt::open(path.as_bytes()) {
                Ok(file) => file,
                Err(ret) => {
                    return match Errno::from_syscall(ret) {
                        Errno::NotFound => Ok(None),
                        _ => Err(SourceError),
                    };
                }
            };
            // Read at most one byte past the engine's limit: the engine's
            // own document bound then rejects the oversized file, and a
            // hostile huge file cannot exhaust memory here first.
            let cap = MAX_DOC_LEN.saturating_add(1);
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            while bytes.len() < cap {
                let want = chunk.len().min(cap - bytes.len());
                let read = file
                    .read_at(bytes.len() as u64, &mut chunk[..want])
                    .map_err(|_| SourceError)?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Ok(Some(bytes))
        }
    }
}

#[cfg(feature = "rt")]
pub use rt_source::BundleHelp;
