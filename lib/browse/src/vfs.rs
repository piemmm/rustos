//! The production, VFS-backed [`DirectorySource`] engine.
//!
//! [`VfsDirectorySource`] is what the shipping file-manager program wires
//! behind the [`Browser`](crate::Browser): it spells the browser's
//! root-first components into one bounded, validated absolute path, hands
//! that path to an injected *fetch* primitive (on a running system,
//! `tairix_rt::read_dir_all` — the kernel-authorised `fs_open` +
//! `fs_readdir` transfer under the caller's own attested identity), and maps
//! the returned packed `DirEntry` stream onto the browser's [`Entry`]
//! vocabulary through the shared `lib/abi` stream walker.
//!
//! Keeping the fetch injected keeps the whole engine host-provable: tests
//! drive a `Browser` end to end over an in-memory tree of *encoded* streams,
//! so every path-spelling, decode, and refusal branch runs in `cargo test`
//! with no kernel. The engine adds no authority of its own — every
//! permission decision stays kernel-side behind the fetch — and it fails
//! closed: a malformed component, an over-long path, or one bad stream
//! record refuses the whole listing rather than showing a partial or guessed
//! one.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::fs::{DirEntries, FileKind, FS_PATH_MAX};
use tairix_abi::Errno;

use crate::entry::{Entry, EntryKind};
use crate::source::DirectorySource;

// The listing returned here is in the source's own stream order; the
// `Browser` applies the shared sort. This function only decodes and maps,
// so a caller that wants raw stream order (a test, a `du`-style walk) sees
// exactly what the kernel produced.

/// Spell root-first `components` as an absolute path (an empty slice is the
/// root `/`).
///
/// The one path spelling in this app: the browser's displayed path, the
/// tests' tree keys, and the VFS fetch all go through it, so the three can
/// never disagree about what a component list names.
#[must_use]
pub fn spell_absolute_path(components: &[String]) -> String {
    if components.is_empty() {
        return String::from("/");
    }
    let mut path = String::new();
    for component in components {
        path.push('/');
        path.push_str(component);
    }
    path
}

/// Parse an absolute path string into validated root-first components — the
/// inverse of [`spell_absolute_path`].
///
/// This is how a consumer turns a path it was *handed* (the `HOME`
/// environment the desktop session reads to open its picker at the user's
/// home) into the component list the [`Browser`](crate::Browser) navigates,
/// using the *same* per-component rule [`absolute_path`] spells with (§2.2):
/// every segment between `/`s must be a real single filesystem leaf name
/// ([`tairix_path::validate_file_name`]). Leading, trailing, and repeated
/// `/`s collapse (so `/Users/root/` and `/Users/root` parse alike); the bare
/// root `/` (and the empty string) parse to no components.
///
/// # Errors
///
/// * [`Errno::OutOfRange`] if any segment is not a valid leaf name (`.`,
///   `..`, a control character, `:`, or over the name bound), so a caller
///   falls back to a directory it can name rather than guessing.
/// * [`Errno::LengthOutOfRange`] if the re-spelled path exceeds
///   [`FS_PATH_MAX`].
pub fn components_from_absolute_path(path: &str) -> Result<Vec<String>, Errno> {
    let mut components = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        tairix_path::validate_file_name(segment).map_err(|_| Errno::OutOfRange)?;
        components.push(String::from(segment));
    }
    if spell_absolute_path(&components).len() > FS_PATH_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(components)
}

/// Validate `components` and spell them as the absolute path handed to the
/// kernel.
///
/// Every component must be a real single filesystem leaf name — the shared
/// [`tairix_path::validate_file_name`] rule (non-empty, not `.` or `..`, no
/// `/`, no control character or NUL, no `:`, within the name bound) — so a
/// component can never name a different directory than the browser shows and
/// the check is the *same* one the rename editor spells a new name through
/// (§2.2). The spelled path is bounded by the kernel's own [`FS_PATH_MAX`].
///
/// # Errors
///
/// * [`Errno::OutOfRange`] for a malformed component.
/// * [`Errno::LengthOutOfRange`] for a path longer than [`FS_PATH_MAX`].
pub fn absolute_path(components: &[String]) -> Result<String, Errno> {
    for component in components {
        tairix_path::validate_file_name(component).map_err(|_| Errno::OutOfRange)?;
    }
    let path = spell_absolute_path(components);
    if path.len() > FS_PATH_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(path)
}

/// Map one packed `fs_readdir` stream onto the browser's [`Entry`] list.
///
/// The walk is the shared [`DirEntries`] iterator; the first malformed
/// record (or a non-UTF-8 name, refused with the decoder's own domain
/// errno) refuses the whole listing — the browser never shows a partial
/// directory as complete.
///
/// # Errors
///
/// Any [`Errno`] the stream walker surfaces, or [`Errno::OutOfRange`] for a
/// name that is not UTF-8.
pub fn entries_from_dir_stream(stream: &[u8]) -> Result<Vec<Entry>, Errno> {
    let mut entries = Vec::new();
    for item in DirEntries::new(stream) {
        let entry = item?;
        let name = core::str::from_utf8(entry.name).map_err(|_| Errno::OutOfRange)?;
        let kind = EntryKind::for_listing(matches!(entry.kind, FileKind::Directory), name);
        entries.push(Entry::new(name, kind, entry.size, entry.modified));
    }
    Ok(entries)
}

/// The production [`DirectorySource`]: validated path spelling composed
/// with an injected directory fetch and the shared stream decode.
///
/// `fetch` receives the spelled absolute path and returns the raw packed
/// `DirEntry` stream for that directory; the shipping program passes the
/// `lib/rt` listing call, tests pass an in-memory tree. The fetch carries
/// the only authority involved — the engine itself makes no permission
/// decision and fabricates no entry.
pub struct VfsDirectorySource<F> {
    fetch: F,
}

impl<F> VfsDirectorySource<F>
where
    F: FnMut(&str) -> Result<Vec<u8>, Errno>,
{
    /// Build the source over `fetch`, the capability-checked directory read.
    pub fn new(fetch: F) -> Self {
        Self { fetch }
    }
}

impl<F> DirectorySource for VfsDirectorySource<F>
where
    F: FnMut(&str) -> Result<Vec<u8>, Errno>,
{
    fn list(&mut self, components: &[String]) -> Result<Vec<Entry>, Errno> {
        let path = absolute_path(components)?;
        let stream = (self.fetch)(&path)?;
        entries_from_dir_stream(&stream)
    }
}
