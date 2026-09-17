//! The one bounded walk of the installed program stores.
//!
//! Three consumers need the same answer to "which bundles are installed, and
//! what does each one's own signed manifest say?": the program-library
//! `rescan`, the file manager's open-with table, and the desktop session's
//! icon-bar identity index. Each had its own copy of the walk, its own depth
//! and entry bounds, and its own manifest decode; this crate is that walk,
//! defined once.
//!
//! Reading is injected through [`StoreReader`], so the crate performs no I/O
//! and holds no authority — the consumer's own capability-checked filesystem
//! access does. It verifies no signature either: a manifest read here is a
//! *claim* the walk decodes and bounds, and only the load gate turns a claim
//! into an attested identity.
//!
//! # What the walk guarantees
//!
//! * **Fixed precedence.** [`store_roots`] spells the roots in the order a
//!   program word resolves against them, so a duplicate resolves to the
//!   shipped bundle deterministically rather than to whichever listing came
//!   back first. Each visited bundle carries the index of the root it was
//!   found under, so a consumer resolving a collision reads the precedence
//!   rather than re-deriving it from path prefixes.
//! * **Contained.** Bundles may be filed in nested plain subdirectories, so
//!   the walk descends — to [`MAX_WALK_DEPTH`], and across no more than
//!   [`MAX_WALK_ENTRIES`] directory entries in total. A tree that exhausts
//!   either fails the whole scan closed rather than walking on. A `.app`
//!   directory is a sealed unit and is never descended into.
//! * **Fail-closed per bundle.** A bundle whose manifest is absent,
//!   unreadable, over-long, or undecodable contributes nothing and is
//!   counted, so one broken bundle costs only itself.
//! * **Deterministic.** Listings are consumed in sorted order, so the same
//!   tree always yields the same sequence.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{
    AppInfoHeader, BundleEntry, Errno, APPINFO_WIRE_MAX, BUNDLE_SUFFIX, HOME_APPLICATION_STORE_DIR,
    HOME_COMMAND_STORE_DIR, INSTALLED_APP_STORE, SYSTEM_APPLICATION_STORE, SYSTEM_COMMAND_STORE,
};

#[cfg(test)]
mod tests;

/// Depth bound on the store walk.
///
/// Bundles may be filed in nested plain subdirectories, so the walk descends;
/// a pathological tree must not recurse without limit, so a directory deeper
/// than this is not descended into. Ample for the stores' real nesting.
pub const MAX_WALK_DEPTH: usize = 8;

/// Bound on directory entries one scan examines across all of its roots.
///
/// A **containment bound** on an untrusted directory tree, not a capacity: an
/// installed program is one `.app` entry the walk never descends into, so a
/// real program store — the two system stores, the machine-wide installed
/// store, and a user's own pair — is hundreds of entries. A tree presenting
/// thousands is not a believable program store, and exhausting this fails the
/// whole scan closed rather than walking on.
pub const MAX_WALK_ENTRIES: usize = 4096;

/// The machine-wide store roots, in the precedence a program name resolves
/// against them: the system command store, the system application store, then
/// the machine-wide installed store.
pub const MACHINE_ROOTS: [&str; 3] = [
    SYSTEM_COMMAND_STORE,
    SYSTEM_APPLICATION_STORE,
    INSTALLED_APP_STORE,
];

/// One entry of a listed directory, as [`StoreReader`] reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// The entry's name within its directory.
    pub name: String,
    /// Whether the entry is itself a directory.
    pub directory: bool,
}

/// Reads the program stores: directory listings for the walk, and one
/// bundle's own `AppInfo` manifest.
///
/// A running system backs this with the secured VFS, so every path
/// resolution and per-inode permission decision is the kernel's under the
/// caller's attested identity; tests back it with an in-memory tree. The
/// seam is why this crate needs no capability of its own.
pub trait StoreReader {
    /// List one directory, or `None` when the path does not exist — an
    /// absent store root is the ordinary state of a machine without one.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the backing raises other than absence.
    fn list_dir(&self, path: &str) -> Result<Option<Vec<DirEntry>>, Errno>;

    /// Read the `AppInfo` manifest inside the bundle directory `bundle`, or
    /// `None` when no manifest file exists there — a plain directory that is
    /// simply not a bundle.
    ///
    /// The read is **bounded**, so no file at a bundle's manifest path can
    /// make a caller slurp an unbounded number of bytes. A file longer than
    /// [`APPINFO_WIRE_MAX`] cannot be a manifest, and an implementation may
    /// either report that as an error or hand the over-long bytes back —
    /// [`decode_manifest`] refuses them either way. What it must never do is
    /// truncate them to the ceiling, which would turn an over-long file into a
    /// shorter, decodable-looking manifest.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the backing raises other than absence.
    fn read_appinfo(&self, bundle: &str) -> Result<Option<Vec<u8>>, Errno>;
}

/// One installed bundle the walk found, with its manifest already decoded.
#[derive(Copy, Clone, Debug)]
pub struct Bundle<'a> {
    /// The bundle *directory*, absolute and with no trailing separator.
    pub path: &'a str,
    /// Index in the `roots` slice the walk was given of the store this
    /// bundle was found under — its precedence, which is what a consumer
    /// resolving two bundles claiming one identity decides by.
    pub root: usize,
    /// What the bundle's own manifest declares.
    pub header: &'a AppInfoHeader,
    /// The whole manifest, for the bounded body fields that sit past the
    /// header (the capability request, the declared MIME types).
    pub manifest: &'a [u8],
}

/// What a visitor made of one [`Bundle`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The visitor read the bundle, or deliberately had no use for it.
    Accepted,
    /// The visitor refused it — a field it could not accept. Counted among
    /// the scan's skipped bundles, exactly as an undecodable manifest is.
    Refused,
}

/// Why a whole scan was abandoned.
///
/// Both are fail-closed: the scan produced no trustworthy answer, so the
/// caller acts on none of it rather than on a partial tree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WalkError {
    /// A store root, or a directory inside one, could not be listed for a
    /// reason other than absence.
    Listing(Errno),
    /// The tree presented more than [`MAX_WALK_ENTRIES`] entries.
    TreeTooLarge,
}

/// What one completed scan examined.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Scan {
    /// Bundles whose manifest decoded and whose visitor accepted them.
    pub accepted: usize,
    /// Bundles skipped fail-closed: an unreadable, over-long, or undecodable
    /// manifest, or one the visitor refused.
    pub skipped: usize,
}

/// The store roots to walk, in the precedence a program name resolves
/// against them: the machine-wide stores, then the account's own pair.
///
/// A `home` that is absent or empty contributes no per-user root, so a
/// session without one still finds every installed program. The system
/// stores come first because they are read-only and system-signed: a
/// user-writable store can never claim an identity a shipped bundle already
/// declares.
#[must_use]
pub fn store_roots(home: Option<&str>) -> Vec<String> {
    let mut roots: Vec<String> = MACHINE_ROOTS
        .iter()
        .map(|root| String::from(*root))
        .collect();
    roots.extend(user_roots(home));
    roots
}

/// The account's own two store roots under `home`, or nothing when `home` is
/// absent or empty.
#[must_use]
pub fn user_roots(home: Option<&str>) -> Vec<String> {
    let Some(home) = home
        .map(|home| home.strip_suffix('/').unwrap_or(home))
        .filter(|home| !home.is_empty())
    else {
        return Vec::new();
    };
    alloc::vec![
        format!("{home}/{HOME_COMMAND_STORE_DIR}"),
        format!("{home}/{HOME_APPLICATION_STORE_DIR}"),
    ]
}

/// The path of the signed manifest inside the bundle directory `bundle`.
#[must_use]
pub fn manifest_path(bundle: &str) -> String {
    format!("{bundle}/{}", BundleEntry::AppInfo.as_str())
}

/// Decode a bundle's `AppInfo` bytes, bounded by the shared manifest ceiling.
///
/// `None` for an over-long or malformed manifest, so every consumer degrades
/// to what it can honestly say about such a bundle rather than handling an
/// error. The decode itself validates the bundle-identifier grammar, so a
/// header that comes back names a directory that cannot traverse out of a
/// store.
#[must_use]
pub fn decode_manifest(manifest: &[u8]) -> Option<AppInfoHeader> {
    if manifest.len() > APPINFO_WIRE_MAX {
        return None;
    }
    AppInfoHeader::from_bytes(manifest).ok()
}

/// Walk `roots` for installed bundles, offering each one's decoded manifest
/// to `visit`.
///
/// Breadth-first with listings consumed in sorted order, so the sequence is
/// deterministic; a `.app` directory is never descended into; an absent root
/// contributes nothing. A bundle whose manifest cannot be read or decoded is
/// counted among [`Scan::skipped`] and the scan carries on.
///
/// # Errors
///
/// [`WalkError::Listing`] for a directory that exists but could not be
/// listed, and [`WalkError::TreeTooLarge`] once the tree exceeds
/// [`MAX_WALK_ENTRIES`] entries. Either abandons the scan, so the caller acts
/// on nothing rather than on a partial tree.
pub fn walk<R, S, V>(reader: &R, roots: &[S], mut visit: V) -> Result<Scan, WalkError>
where
    R: StoreReader + ?Sized,
    S: AsRef<str>,
    V: FnMut(Bundle<'_>) -> Verdict,
{
    let mut queue: VecDeque<(String, usize, usize)> = roots
        .iter()
        .enumerate()
        .map(|(root, dir)| (String::from(dir.as_ref()), root, 0))
        .collect();
    let mut scan = Scan::default();
    let mut visited = 0usize;

    while let Some((dir, root, depth)) = queue.pop_front() {
        let Some(mut entries) = reader.list_dir(&dir).map_err(WalkError::Listing)? else {
            continue;
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for item in entries {
            visited += 1;
            if visited > MAX_WALK_ENTRIES {
                return Err(WalkError::TreeTooLarge);
            }
            if !item.directory {
                continue;
            }
            let path = format!("{dir}/{}", item.name);
            if !item.name.ends_with(BUNDLE_SUFFIX) {
                if depth + 1 < MAX_WALK_DEPTH {
                    queue.push_back((path, root, depth + 1));
                }
                continue;
            }
            match offer(reader, &path, root, &mut visit) {
                Some(Verdict::Accepted) => scan.accepted += 1,
                Some(Verdict::Refused) | None => scan.skipped += 1,
            }
        }
    }
    Ok(scan)
}

/// Read and decode one bundle's manifest and offer it to `visit`, or `None`
/// when the bundle is skipped fail-closed.
///
/// A directory with no manifest is not a bundle at all and is reported the
/// same way as a broken one: the walk only claims a `.app` directory it could
/// read a manifest out of.
fn offer<R, V>(reader: &R, path: &str, root: usize, visit: &mut V) -> Option<Verdict>
where
    R: StoreReader + ?Sized,
    V: FnMut(Bundle<'_>) -> Verdict,
{
    let manifest = reader.read_appinfo(path).ok().flatten()?;
    let header = decode_manifest(&manifest)?;
    Some(visit(Bundle {
        path,
        root,
        header: &header,
        manifest: &manifest,
    }))
}
