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
//!
//! # A listing may not be ready yet
//!
//! A source that reads the directory on the calling thread answers
//! [`Listing::Ready`] and is the simple case. A source that reads it
//! *elsewhere* — the desktop session, whose event loop must not stall on a
//! directory a slow disk is still walking — answers [`Listing::Pending`], and
//! the embedder asks again when its own wake says the answer has landed
//! (`Browser::resume`). Nothing here ever polls or waits: the trait reports
//! what it has, and the party that owns the wake decides when to ask again.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;

use crate::entry::Entry;

/// What a source has for a directory right now.
///
/// Two answers, both of them normal: the children, or "not yet". A refusal is
/// the `Err` half of the enclosing [`Result`] and is a third thing entirely —
/// pending is never an error, and an error is never retried by waiting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Listing {
    /// The children, in the source's own stable order.
    Ready(Vec<Entry>),
    /// The read is under way somewhere else and the answer will arrive later.
    ///
    /// The source has taken note of the request; the *embedder* is what asks
    /// again, when whatever it parks on says the answer has landed. A source
    /// that returns this and never becomes ready simply leaves the view where
    /// it was — it can never make the browser spin.
    Pending,
}

/// A read-only view of the filesystem's directory structure.
pub trait DirectorySource {
    /// List the children of the directory named by `components`
    /// (root-first; an empty slice is the root directory `/`).
    ///
    /// The returned entries are taken as authoritative and shown verbatim;
    /// iteration order is the source's own stable order. The browser does not
    /// sort, filter, or add to them.
    ///
    /// A source that reads the directory itself always answers
    /// [`Listing::Ready`]. One that reads it elsewhere answers
    /// [`Listing::Pending`] and is asked again later; asking with the same
    /// `components` must not start a second read.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the directory cannot be
    /// listed — for example [`Errno::PermissionDenied`] when the caller lacks
    /// the capability to read it or [`Errno::NotFound`]
    /// when it does not exist.
    fn list(&mut self, components: &[String]) -> Result<Listing, Errno>;

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
