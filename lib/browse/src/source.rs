//! The directory-read seam the browser is built on.
//!
//! [`DirectorySource`] is the one thing the browser needs from the outside
//! world: the children of an absolute path. Keeping it a trait means the
//! navigation and rendering logic is exhaustively testable against an
//! in-memory tree without a kernel, exactly as `appmgr`'s `BundleStore` and
//! `ps`'s transport are injected seams.
//!
//! On a running system the source is backed by the VFS: a `list` call is a
//! capability-checked directory read, so the permission decision and the
//! path policy live in the VFS, not here. The browser shows exactly the
//! entries the source returns — it never fabricates a `/proc`/`/sys`-style
//! synthetic entry.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;

use crate::entry::Entry;

/// A read-only view of the filesystem's directory structure.
pub trait DirectorySource {
    /// List the children of the directory named by `components`
    /// (root-first; an empty slice is the root directory `/`).
    ///
    /// The returned entries are taken as authoritative and shown verbatim;
    /// iteration order is the source's own stable order. The browser does not
    /// sort, filter, or add to them.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the directory cannot be
    /// listed — for example [`Errno::PermissionDenied`] when the caller lacks
    /// the capability to read it or [`Errno::NotFound`]
    /// when it does not exist.
    fn list(&mut self, components: &[String]) -> Result<Vec<Entry>, Errno>;

    /// Whether the directory named by `components` holds at least one child.
    ///
    /// This answers the one question a listing cannot: no VFS surface reports
    /// a child count, so the empty/non-empty cue the browser draws is only
    /// knowable by reading the directory. The answer is a `bool`, never a
    /// listing — an implementation must read the *cheapest* thing that decides
    /// it (one record) and must never build, copy, or walk the children.
    ///
    /// Probing exercises the caller's directory-read authority on a child the
    /// caller is only *displaying*, so a source is free not to offer it: the
    /// default answers [`Errno::NotImplemented`], which the browser records as
    /// [`Occupancy::Indeterminate`](crate::Occupancy::Indeterminate) and draws
    /// as a plain folder. The trusted file picker takes that default
    /// deliberately — the cue adds nothing to choosing a file, so the picker
    /// exercises no authority it does not need.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the directory cannot be
    /// read — for example [`Errno::PermissionDenied`] when the caller lacks
    /// the capability, or [`Errno::NotImplemented`] when the source does not
    /// probe at all.
    fn has_children(&mut self, components: &[String]) -> Result<bool, Errno> {
        let _ = components;
        Err(Errno::NotImplemented)
    }
}
