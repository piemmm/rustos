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
use tairix_font::ELLIPSIS;

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

/// Spell root-first `components` as the location text a window title carries,
/// fitted to `budget` bytes by dropping whole leading components behind a
/// leading [`ELLIPSIS`].
///
/// A window title is a bounded field ([`WINDOW_TITLE_MAX`]) and a path is not,
/// so something has to give. Whole components go, oldest first: the directory
/// the window is *in* is the part the reader needs, and a title cut at the tail
/// would hide exactly that. The mark is the one the text engine elides with, so
/// a title the window manager shortens further reads the same way.
///
/// The result never exceeds `budget` bytes and always breaks on a `char`
/// boundary. A `budget` too small even for the mark and the leaf yields the
/// longest prefix of the leaf that fits — a shortened name rather than nothing.
///
/// A control character in a name — which the kernel refuses to *create* but a
/// foreign volume can still carry — is shown as [`char::REPLACEMENT_CHARACTER`],
/// so the text is always one the bounded title field accepts and a hostile name
/// cannot leave a window permanently unable to state where it is. Only the title
/// is spelled this way: [`spell_absolute_path`] stays byte-exact, because it
/// names a real path to open.
///
/// [`WINDOW_TITLE_MAX`]: tairix_abi::window_ipc::WINDOW_TITLE_MAX
#[must_use]
pub fn spell_title_location(components: &[String], budget: usize) -> String {
    let shown: Option<Vec<String>> = components
        .iter()
        .any(|name| name.chars().any(char::is_control))
        .then(|| components.iter().map(|name| shown_name(name)).collect());
    let components = shown.as_deref().unwrap_or(components);
    let full = spell_absolute_path(components);
    if full.len() <= budget {
        return full;
    }
    for skip in 1..components.len() {
        let tail = spell_absolute_path(&components[skip..]);
        if ELLIPSIS.len().saturating_add(tail.len()) <= budget {
            let mut fitted = String::from(ELLIPSIS);
            fitted.push_str(&tail);
            return fitted;
        }
    }
    let leaf = components.last().map_or("", String::as_str);
    let Some(room) = budget.checked_sub(ELLIPSIS.len().saturating_add(1)) else {
        return String::from(cut_to_bytes(leaf, budget));
    };
    let mut fitted = String::from(ELLIPSIS);
    fitted.push('/');
    fitted.push_str(cut_to_bytes(leaf, room));
    fitted
}

/// `name` with every control character shown as
/// [`char::REPLACEMENT_CHARACTER`] — what a reader is shown for a byte a
/// display cannot render, rather than the byte itself.
fn shown_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                ch
            }
        })
        .collect()
}

/// The longest prefix of `text` that fits in `budget` bytes, cut on a `char`
/// boundary.
fn cut_to_bytes(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index.saturating_add(ch.len_utf8());
        if next > budget {
            break;
        }
        end = next;
    }
    &text[..end]
}

/// Append the leaf `name` to the directory path already spelled in `path`.
///
/// The one place a directory path and a leaf are joined, so the root case
/// (`/` plus a name, never `//name`) is handled once. Appending in place lets
/// a caller that spells many children of one directory — a grid of tiles —
/// reuse a single buffer instead of allocating a path each time.
pub fn push_child(path: &mut String, name: &str) {
    if !path.ends_with('/') {
        path.push('/');
    }
    path.push_str(name);
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
///
/// A source built with [`new`](VfsDirectorySource::new) offers no occupancy
/// probe: [`has_children`](DirectorySource::has_children) answers
/// [`Errno::NotImplemented`], the browser records the child as
/// [`Occupancy::Indeterminate`](crate::Occupancy::Indeterminate), and it draws
/// the plain folder. That is the trusted file picker's deliberate choice — a
/// read-only chooser gains nothing from the cue, so it reads no directory it
/// is not showing. [`probing`](VfsDirectorySource::probing) opts in.
pub struct VfsDirectorySource<F, P = NoProbe> {
    fetch: F,
    probe: Option<P>,
}

/// The probe type of a source that does not probe — the default `P`, so
/// [`VfsDirectorySource::new`] infers a concrete type without a caller naming
/// one.
pub type NoProbe = fn(&str, &mut [u8]) -> Result<usize, Errno>;

/// Bytes handed to a probe: one maximal `DirEntry` record.
///
/// `fs_readdir` is all-or-nothing — it packs the *whole* listing or refuses
/// with [`Errno::BufferTooSmall`] — so a buffer this size answers "is there at
/// least one child?" without ever copying a directory out: an empty directory
/// fits (zero bytes), a one-child directory fits, and anything larger refuses.
/// All three answers are decisive, and none of them scales with the child
/// count.
const PROBE_BUF_LEN: usize = tairix_abi::fs::DirEntry::HEADER_LEN + tairix_abi::fs::FS_NAME_MAX;

impl<F> VfsDirectorySource<F>
where
    F: FnMut(&str) -> Result<Vec<u8>, Errno>,
{
    /// Build the source over `fetch`, the capability-checked directory read.
    pub fn new(fetch: F) -> Self {
        Self { fetch, probe: None }
    }
}

impl<F, P> VfsDirectorySource<F, P>
where
    F: FnMut(&str) -> Result<Vec<u8>, Errno>,
    P: FnMut(&str, &mut [u8]) -> Result<usize, Errno>,
{
    /// Build the source over `fetch` and an occupancy `probe`.
    ///
    /// `probe` opens the directory named by the spelled path, reads at most
    /// the given buffer, and closes it, returning the bytes the packed stream
    /// occupies — on a running system `tairix_rt::open_dir` plus one
    /// `Dir::read`. It is handed a one-record buffer, never a listing-sized
    /// one, so the call costs the same on an empty directory and on one with a
    /// hundred thousand children.
    pub fn probing(fetch: F, probe: P) -> Self {
        Self {
            fetch,
            probe: Some(probe),
        }
    }
}

impl<F, P> DirectorySource for VfsDirectorySource<F, P>
where
    F: FnMut(&str) -> Result<Vec<u8>, Errno>,
    P: FnMut(&str, &mut [u8]) -> Result<usize, Errno>,
{
    fn list(&mut self, components: &[String]) -> Result<Vec<Entry>, Errno> {
        let path = absolute_path(components)?;
        let stream = (self.fetch)(&path)?;
        entries_from_dir_stream(&stream)
    }

    fn has_children(&mut self, components: &[String]) -> Result<bool, Errno> {
        let probe = self.probe.as_mut().ok_or(Errno::NotImplemented)?;
        let path = absolute_path(components)?;
        let mut buf = [0u8; PROBE_BUF_LEN];
        match probe(&path, &mut buf) {
            Ok(0) => Ok(false),
            Ok(_) | Err(Errno::BufferTooSmall) => Ok(true),
            Err(errno) => Err(errno),
        }
    }
}
