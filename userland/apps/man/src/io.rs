//! The seams through which `man` touches the outside world.
//!
//! Keeping the app-store filesystem and the terminal behind object-safe
//! traits is what lets the resolve/render logic in [`crate::client`] run
//! against in-memory fixtures with no kernel, mirroring the seam design of
//! the sibling tools (`cat`'s `FileSource`, `ps`'s `Transport`/`Output`).
//! The production implementations live in the `Run` binary and are thin
//! wrappers over the `fs_*` and standard-stream syscalls; every capability
//! check stays kernel-side.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;

/// Read-only access to installed application bundles and their `Help/`
/// trees.
///
/// `bundle_dir` values are the candidate spellings the shared
/// `tairix_cmdres::bundle_candidates` policy computed; `locale_dir` and
/// `file_name` values are spellings the `lib/help` engine validated, so an
/// implementation may join them onto the bundle path without re-parsing —
/// a hostile name can never traverse outside a bundle's `Help/` tree.
pub trait BundleStore {
    /// Whether `bundle_dir` names an installed bundle.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the store raises *other than* absence (absence is the
    /// `Ok(false)` answer): e.g. [`Errno::PermissionDenied`] when the caller
    /// may not search the store. Such a refusal is final — the caller does
    /// not probe further candidates past it.
    fn bundle_exists(&self, bundle_dir: &str) -> Result<bool, Errno>;

    /// The directory names at the top of `bundle_dir`'s `Help/` tree, in any
    /// order. A bundle with no `Help/` directory has no locales: `Ok(vec![])`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the store raises reading the tree.
    fn locale_dirs(&self, bundle_dir: &str) -> Result<Vec<String>, Errno>;

    /// The names of the directories directly inside `dir`, in any order —
    /// the listing step of the recursive app-store search. A missing `dir`
    /// has no children: `Ok(vec![])`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the store raises *other than* absence (absence is the
    /// empty answer). Such a refusal is final — the caller does not search
    /// past it.
    fn subdirs(&self, dir: &str) -> Result<Vec<String>, Errno>;

    /// The bytes of `<bundle_dir>/Help/<locale_dir>/<file_name>`, or `None`
    /// if that document does not exist in that locale.
    ///
    /// An implementation reads at most `limit` bytes and reports a longer
    /// file by returning `limit + 1` bytes, so the engine's own document
    /// bound — not the reader — decides the failure and an oversized
    /// document cannot exhaust memory first.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the store raises reading the document.
    fn read_doc(
        &self,
        bundle_dir: &str,
        locale_dir: &str,
        file_name: &str,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, Errno>;
}

/// The terminal the rendered page is written to.
pub trait Console {
    /// Write every byte of `bytes` to standard output.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;

    /// Emit one framed `stdinfo` line (fd 3), best-effort: advisory metadata
    /// must never affect correctness or exit status, so there is no error to
    /// return and an unattached fd 3 is a no-op.
    fn info(&self, record: &[u8]);

    /// The terminal's row count, or `None` when standard output is not an
    /// interactive terminal of known height (a redirection, a pipe, a serial
    /// line) — the page is then streamed without pagination.
    fn rows(&self) -> Option<u16>;

    /// Block until one key of input arrives and return it, or `None` when
    /// input has ended (the pager then stops prompting and streams the
    /// rest).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the input stream raises.
    fn read_key(&self) -> Result<Option<u8>, Errno>;
}
