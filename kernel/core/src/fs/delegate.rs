//! Routing VFS resolution to a `drivers/filesystem/*` driver.
//!
//! [`DelegatedFs`] adapts a borrowed
//! [`FilesystemRead`] driver to the VFS's path-resolution and permission
//! model for the subtree below a driver-backed mount point. The driver
//! supplies **structural** I/O only — it mints opaque [`NodeId`]s and
//! reports each node's kind, size, children, and bytes; it makes no
//! permission decision. Ownership, mode bits, ACLs, and
//! the capability gate stay in the VFS, which authorises every node
//! before reading it and every directory it descends into. The metadata it
//! authorises against is chosen by the [`MetaPolicy`] the call site picks:
//! [`Uniform`] applies the mount point's [`Metadata`] to every node (the
//! natural model for a filesystem like FAT that stores no per-file owner),
//! while [`PerInode`] reads each node's own stored record through
//! [`FilesystemSecurity`] (for a driver like `arxfs` that stores full
//! per-inode ownership, mode, ACL, and capability gate).
//!
//! The adapter is constructed per call with a `&mut dyn FilesystemRead`
//! rather than stored in the [`Vfs`](super::Vfs): the VFS tree is
//! `Clone + Eq`, and the live driver — mapped from the mount's
//! [`DriverHandle`](tairix_abi::driver::DriverHandle) by the kernel host —
//! is neither.

use core::marker::PhantomData;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::driver::filesystem::{
    FilesystemAttrs, FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeInfo,
    NodeKind, NodeTimes,
};
use tairix_abi::driver::DriverError;
use tairix_fsmeta::{AttrKey, NamespaceAccess, KEY_MAX};

use super::path::MAX_COMPONENT_LEN;
use super::perm::{Access, Credentials, Metadata};
use super::VfsError;

/// Structural metadata of a delegated node, paired with the VFS
/// [`Metadata`] that governs it.
///
/// `kind` and `size` come from the driver's on-disk layout; `meta` is the
/// metadata the active [`MetaPolicy`] derived for the node — the mount
/// point's template under [`Uniform`], or the node's own stored
/// record under [`PerInode`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedInfo {
    /// Whether the node is a directory or a regular file.
    pub kind: NodeKind,
    /// File length in bytes; `0` for a directory.
    pub size: u64,
    /// Bytes of on-disk storage the node's data occupies, as the driver
    /// reports it from the format's own allocation tracking.
    pub allocated: u64,
    /// The permission metadata applied to the node.
    pub meta: Metadata,
    /// The driver's stable node number for the resolved node (its
    /// [`NodeId`] raw value). Paired with the covering mount's volume id it
    /// forms the node's system-wide [`tairix_abi::FileId`], which
    /// distinguishes "this file grew" from "a different file now sits at
    /// this name" and keys the file-change notification.
    pub node: u64,
    /// The node's four timestamps, as the driver reported them.
    pub times: NodeTimes,
}

/// Maps a [`DriverError`] from the read surface onto the VFS error type.
///
/// Only the errors the [`FilesystemRead`] surface documents can occur here:
/// [`DriverError::NotFound`] for a missing child, [`DriverError::Unsupported`]
/// for a non-directory used as one, and [`DriverError::BufferTooSmall`] /
/// [`DriverError::LengthOutOfRange`] for an over-long on-disk name. Anything
/// else (notably [`DriverError::DeviceFault`]) is an unrecoverable backing
/// fault and surfaces as [`VfsError::Io`].
const fn map_driver_error(error: DriverError) -> VfsError {
    match error {
        DriverError::NotFound => VfsError::NotFound,
        DriverError::Unsupported => VfsError::NotADirectory,
        DriverError::BufferTooSmall | DriverError::LengthOutOfRange => VfsError::InvalidPath,
        _ => VfsError::Io,
    }
}

/// Maps a [`DriverError`] from a rename onto the VFS error type.
///
/// Rename adds [`DriverError::Busy`] to the errors the write surface can
/// report: the driver returns it for a non-empty directory destination and
/// for a refused directory-into-its-own-subtree move. Both are reported as
/// [`VfsError::NotEmpty`] (the closest structural refusal); every other
/// error maps as for any write ([`map_driver_error`]).
const fn map_rename_error(error: DriverError) -> VfsError {
    match error {
        DriverError::Busy => VfsError::NotEmpty,
        other => map_driver_error(other),
    }
}

/// A filesystem driver bound to its mount point, exposing VFS-shaped
/// resolution over the delegated subtree under a [`MetaPolicy`] `P`.
///
/// The driver is borrowed behind the [`FilesystemRead`] surface (and, for
/// the mutating methods, additionally [`FilesystemWrite`]); the
/// [`PerInode`] policy additionally requires [`FilesystemSecurity`].
/// Construct it with [`DelegatedFs::new`] for the [`Uniform`] policy or
/// [`DelegatedFs::new_secured`] for [`PerInode`].
///
/// `components` passed to the methods are the path *relative to the mount
/// point* (the VFS strips the mount prefix); an empty slice names the mount
/// point itself, i.e. the driver's root directory.
pub struct DelegatedFs<'fs, R: FilesystemRead + ?Sized, P = Uniform> {
    fs: &'fs mut R,
    template: Metadata,
    _policy: PhantomData<P>,
}

/// How [`DelegatedFs`] derives the [`Metadata`] it authorises a node
/// against.
///
/// The two implementations are the two sources a delegated subtree
/// can have: the uniform mount-point template ([`Uniform`], for a driver
/// like FAT that stores no per-file owner) and the driver's own stored
/// per-inode record ([`PerInode`], for a driver like `arxfs`). Both feed
/// the *same* [`Metadata::authorize`] decision (the VFS is the single policy point).
///
/// The two implementors [`Uniform`] and [`PerInode`] are the only ones the
/// crate provides; callers select between them with [`DelegatedFs::new`]
/// and [`DelegatedFs::new_secured`] rather than naming this trait.
pub trait MetaPolicy<R: FilesystemRead + ?Sized> {
    /// The permission metadata governing `node`.
    fn metadata(fs: &mut R, node: NodeId, template: &Metadata) -> Result<Metadata, VfsError>;

    /// Stamp the freshly created `node` with its creator's identity.
    ///
    /// A driver's raw `create` mints the node with the format's own
    /// default record (`ARXFS` stamps the system user), which would lock
    /// an ordinary creator out of a file it just made. Under [`PerInode`]
    /// the stored record's ownership is rewritten to the creating
    /// caller's `(uid, gid)` — mode, ACL, and capability gate untouched —
    /// in the same VFS operation that created the node, so ownership is
    /// never observably wrong. Under [`Uniform`] there is no per-node
    /// record to stamp, so the mount template stays the whole story.
    fn stamp_creation(fs: &mut R, node: NodeId, cred: &Credentials<'_>) -> Result<(), VfsError>;
}

/// Apply the mount point's [`Metadata`] uniformly to every delegated node.
pub enum Uniform {}

/// Apply each node's own stored record, read through
/// [`FilesystemSecurity`].
pub enum PerInode {}

impl<R: FilesystemRead + ?Sized> MetaPolicy<R> for Uniform {
    fn metadata(_fs: &mut R, _node: NodeId, template: &Metadata) -> Result<Metadata, VfsError> {
        Ok(template.clone())
    }

    fn stamp_creation(_fs: &mut R, _node: NodeId, _cred: &Credentials<'_>) -> Result<(), VfsError> {
        Ok(())
    }
}

impl<R: FilesystemRead + FilesystemSecurity + ?Sized> MetaPolicy<R> for PerInode {
    fn metadata(fs: &mut R, node: NodeId, _template: &Metadata) -> Result<Metadata, VfsError> {
        let sec = fs.security(node).map_err(map_driver_error)?;
        Ok(Metadata::from_node_security(&sec))
    }

    fn stamp_creation(fs: &mut R, node: NodeId, cred: &Credentials<'_>) -> Result<(), VfsError> {
        let mut sec = fs.security(node).map_err(map_driver_error)?;
        sec.uid = cred.uid.0;
        sec.gid = cred.gid.0;
        fs.set_security(node, sec).map_err(map_driver_error)
    }
}

impl<'fs, R: FilesystemRead + ?Sized> DelegatedFs<'fs, R, Uniform> {
    /// Bind `fs` to the `template` metadata its mount point carries; every
    /// node in the delegated subtree is judged against that one template.
    #[must_use]
    pub fn new(fs: &'fs mut R, template: Metadata) -> Self {
        Self {
            fs,
            template,
            _policy: PhantomData,
        }
    }
}

impl<'fs, R: FilesystemRead + FilesystemSecurity + ?Sized> DelegatedFs<'fs, R, PerInode> {
    /// Bind `fs` so each node is judged against its *own* stored
    /// record (read through [`FilesystemSecurity`]) rather than the mount
    /// template.
    ///
    /// `template` is retained only as the metadata of the mount point in
    /// the in-RAM tree; the delegated walk consults the driver's per-inode
    /// record for every node, including the driver root.
    #[must_use]
    pub fn new_secured(fs: &'fs mut R, template: Metadata) -> Self {
        Self {
            fs,
            template,
            _policy: PhantomData,
        }
    }

    /// Set the permission bits of the node at `components` to `mode` (the
    /// `chmod(2)` shape), leaving its ownership, ACL, and capability gate
    /// untouched.
    ///
    /// Only the node's **owner** may change its mode — mode bits grant no
    /// write-implies-chmod, and holding a capability grants no override —
    /// so a non-owner is refused, and a node carrying a `required_cap`
    /// gate additionally demands that capability (the gate guards *all*
    /// access to the node, this change included). `mode` must already be
    /// validated to the permission mask by the caller; the stored record's
    /// mode field carries only permission bits by contract.
    ///
    /// Implemented only for the per-inode policy: a uniform-template mount
    /// has no stored per-node record for a mode change to land in.
    ///
    /// # Errors
    ///
    /// [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    /// [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn set_mode(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        mode: u32,
    ) -> Result<(), VfsError> {
        let (node, _info, meta) = self.resolve(cred, components)?;
        if let Some(cap) = meta.required_cap {
            if !cred.caps.holds(cap) {
                return Err(VfsError::PermissionDenied);
            }
        }
        if cred.uid != meta.owner {
            return Err(VfsError::PermissionDenied);
        }
        let mut sec = self.fs.security(node).map_err(map_driver_error)?;
        sec.mode = mode;
        self.fs.set_security(node, sec).map_err(map_driver_error)
    }
}

impl<R: FilesystemRead + FilesystemSecurity + FilesystemAttrs + ?Sized>
    DelegatedFs<'_, R, PerInode>
{
    /// Read the extended attribute `key` of the node at `components` into
    /// `value_out`, returning the value's byte count.
    ///
    /// The key is validated against the one shared `lib/fsmeta` grammar and
    /// its namespace's access class decides the gate: the ordinary
    /// namespaces (`user`, the foreign presets, `tairix`) need read
    /// permission on the node itself — the same [`Metadata::authorize`]
    /// decision every delegated read uses, `required_cap` included — while
    /// the privileged namespaces (`system`, `trusted`) are refused outright
    /// (their dedicated capability is introduced with the service that
    /// holds it, and until then the namespaces are reserved, fail closed).
    ///
    /// Implemented only for the per-inode policy: attribute storage is a
    /// per-inode record, which a uniform-template mount does not have.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidKey`] for a key outside the grammar.
    /// * [`VfsError::NoData`] if the node carries no such attribute (a
    ///   value may legitimately be empty, so absence is never an empty
    ///   read).
    /// * [`VfsError::BufferTooSmall`] if the value does not fit
    ///   `value_out` (never truncated).
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    ///   [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn get_attr(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<usize, VfsError> {
        let key = parse_unprivileged_key(key)?;
        let (node, _info, meta) = self.resolve(cred, components)?;
        meta.authorize(cred, Access::Read)?;
        match self
            .fs
            .get_attr(node, key.as_bytes(), value_out)
            .map_err(map_attr_driver_error)?
        {
            Some(len) => Ok(len),
            None => Err(VfsError::NoData),
        }
    }

    /// Set (insert or replace) the extended attribute `key` of the node at
    /// `components` to `value`, in one copy-on-write driver transaction.
    ///
    /// Gated exactly as [`DelegatedFs::get_attr`], with write permission on
    /// the node in place of read. The value is opaque; the driver enforces
    /// the fixed per-inode bounds and fails closed at them.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidKey`] for a key outside the grammar (or an
    ///   over-long key/value an in-kernel caller passed — the dispatcher
    ///   bounds syscall inputs first).
    /// * [`VfsError::NoSpace`] at the per-inode attribute bounds or a full
    ///   volume.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    ///   [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn set_attr(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        key: &[u8],
        value: &[u8],
    ) -> Result<(), VfsError> {
        let key = parse_unprivileged_key(key)?;
        let (node, _info, meta) = self.resolve(cred, components)?;
        meta.authorize(cred, Access::Write)?;
        self.fs
            .set_attr(node, key.as_bytes(), value)
            .map_err(map_attr_driver_error)
    }

    /// Yield the `index`-th *visible* extended-attribute key of the node at
    /// `components` into `key_out`, returning its byte count, or `None`
    /// once `index` is past the last visible attribute.
    ///
    /// Needs read permission on the node. Keys in a privileged namespace
    /// are omitted from the enumeration entirely — `index` addresses the
    /// filtered sequence, so a caller can never learn a `system.*` key
    /// exists, not even as a gap. Iteration order is the driver's stable
    /// on-disk order. Enumeration scans through a fixed [`KEY_MAX`]
    /// scratch, so an over-long `key_out` refusal can only name the key
    /// actually selected, never one that was skipped.
    ///
    /// # Errors
    ///
    /// * [`VfsError::BufferTooSmall`] if the selected key does not fit
    ///   `key_out`.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    ///   [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn list_attr(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, VfsError> {
        let (node, _info, meta) = self.resolve(cred, components)?;
        meta.authorize(cred, Access::Read)?;
        let mut scratch = [0u8; KEY_MAX];
        let mut visible = 0u64;
        let mut raw = 0u64;
        loop {
            let Some(len) = self
                .fs
                .list_attr(node, raw, &mut scratch)
                .map_err(map_attr_driver_error)?
            else {
                return Ok(None);
            };
            raw += 1;
            // A stored key that fails the shared grammar cannot be judged,
            // so it is hidden like a privileged one (fail closed) — a
            // conforming driver never stores such a key.
            let readable = AttrKey::parse(&scratch[..len])
                .is_ok_and(|k| k.access() != NamespaceAccess::Privileged);
            if !readable {
                continue;
            }
            if visible == index {
                let Some(out) = key_out.get_mut(..len) else {
                    return Err(VfsError::BufferTooSmall);
                };
                out.copy_from_slice(&scratch[..len]);
                return Ok(Some(len));
            }
            visible += 1;
        }
    }

    /// Remove the extended attribute `key` from the node at `components`,
    /// in one copy-on-write driver transaction.
    ///
    /// Gated exactly as [`DelegatedFs::set_attr`] (write permission,
    /// privileged namespaces refused).
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidKey`] for a key outside the grammar.
    /// * [`VfsError::NoData`] if the node carries no such attribute (the
    ///   node itself was already resolved, so the driver's not-found can
    ///   only mean the attribute).
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    ///   [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn remove_attr(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        key: &[u8],
    ) -> Result<(), VfsError> {
        let key = parse_unprivileged_key(key)?;
        let (node, _info, meta) = self.resolve(cred, components)?;
        meta.authorize(cred, Access::Write)?;
        self.fs
            .remove_attr(node, key.as_bytes())
            .map_err(|error| match error {
                DriverError::NotFound => VfsError::NoData,
                other => map_attr_driver_error(other),
            })
    }
}

/// Validate `key` against the shared `lib/fsmeta` grammar and refuse the
/// privileged namespaces.
///
/// `system.*` and `trusted.*` guard a security boundary whose dedicated
/// capability is introduced together with the first service that holds and
/// enforces it; until that service exists the namespaces are reserved and
/// every request fails closed as a permission denial — the same answer a
/// capability check would give a holder-less caller.
fn parse_unprivileged_key(key: &[u8]) -> Result<AttrKey, VfsError> {
    let key = AttrKey::parse(key).map_err(|_| VfsError::InvalidKey)?;
    if key.access() == NamespaceAccess::Privileged {
        return Err(VfsError::PermissionDenied);
    }
    Ok(key)
}

/// Map a [`DriverError`] from a [`FilesystemAttrs`] call onto the VFS
/// error surface. Attribute reads and writes carry outcomes the
/// path-resolution mapping ([`map_driver_error`]) would misreport — a
/// too-small *value* buffer is the caller's to grow, not an invalid path,
/// and an exhausted attribute bound is a space refusal — so they map here.
const fn map_attr_driver_error(error: DriverError) -> VfsError {
    match error {
        DriverError::NotFound => VfsError::NotFound,
        DriverError::BufferTooSmall => VfsError::BufferTooSmall,
        DriverError::NoSpace => VfsError::NoSpace,
        DriverError::OutOfRange | DriverError::LengthOutOfRange => VfsError::InvalidKey,
        _ => VfsError::Io,
    }
}

impl<R: FilesystemRead + ?Sized, P: MetaPolicy<R>> DelegatedFs<'_, R, P> {
    /// Resolve `components` from the driver root, enforcing search
    /// (execute) permission on every directory descended into — the same
    /// rule the in-RAM [`Vfs`](super::Vfs) applies. Each directory is
    /// judged against the metadata the active [`MetaPolicy`] derives for
    /// it, and the resolved target's metadata is returned for the caller's
    /// own access check.
    fn resolve(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<(NodeId, NodeInfo, Metadata), VfsError> {
        let mut node = self.fs.root();
        let mut info = self.fs.node_info(node).map_err(map_driver_error)?;
        let mut meta = P::metadata(self.fs, node, &self.template)?;
        for component in components {
            if info.kind != NodeKind::Directory {
                return Err(VfsError::NotADirectory);
            }
            meta.authorize(cred, Access::Execute)?;
            node = self
                .fs
                .lookup(node, component.as_bytes())
                .map_err(map_driver_error)?;
            info = self.fs.node_info(node).map_err(map_driver_error)?;
            meta = P::metadata(self.fs, node, &self.template)?;
        }
        Ok((node, info, meta))
    }

    /// Report the structural metadata of the node at `components`, paired
    /// with the permission metadata that governs it. Like POSIX `stat`,
    /// this needs search permission on every intermediate directory but
    /// none on the target itself.
    ///
    /// # Errors
    ///
    /// [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    /// [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn stat(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<DelegatedInfo, VfsError> {
        let (node, info, meta) = self.resolve(cred, components)?;
        Ok(DelegatedInfo {
            kind: info.kind,
            size: info.size,
            allocated: info.allocated,
            meta,
            node: node.raw(),
            times: info.times,
        })
    }

    /// Read up to `buf.len()` bytes from the file at `components` starting
    /// at `offset`, returning the number of bytes read (short of
    /// `buf.len()` at end-of-file).
    ///
    /// # Errors
    ///
    /// * [`VfsError::IsADirectory`] if `components` names a directory.
    /// * [`VfsError::PermissionDenied`] if the node's metadata denies read.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::Io`].
    pub fn read(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, VfsError> {
        let (node, info, meta) = self.resolve(cred, components)?;
        if info.kind == NodeKind::Directory {
            return Err(VfsError::IsADirectory);
        }
        meta.authorize(cred, Access::Read)?;
        self.fs.read_at(node, offset, buf).map_err(map_driver_error)
    }

    /// List the entries of the directory at `components`, in the driver's
    /// stable on-disk order, each with the structural [`NodeInfo`] (which
    /// carries the node's kind, sizes, and timestamps) the driver reports
    /// for it.
    ///
    /// The kind and sizes come from the listing driver itself, so a caller
    /// never has to re-resolve each child by path — a child whose *path*
    /// is shadowed by another mount would otherwise be judged against the
    /// wrong volume, and on an uncached, authenticated volume every such
    /// re-resolution is a fresh full walk.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotADirectory`] if `components` names a file.
    /// * [`VfsError::PermissionDenied`] if the node's metadata denies read.
    /// * [`VfsError::NotFound`] or [`VfsError::Io`] (the latter also for a
    ///   directory entry whose on-disk name is not valid UTF-8, or for a
    ///   driver cursor that fails to advance).
    pub fn list(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<Vec<(NodeInfo, String)>, VfsError> {
        let (node, info, meta) = self.resolve(cred, components)?;
        if info.kind != NodeKind::Directory {
            return Err(VfsError::NotADirectory);
        }
        meta.authorize(cred, Access::Read)?;

        let mut entries = Vec::new();
        let mut name_buf = [0u8; MAX_COMPONENT_LEN];
        let mut cursor: u64 = 0;
        while let Some(entry) = self
            .fs
            .read_dir(node, cursor, &mut name_buf)
            .map_err(map_driver_error)?
        {
            // A cursor that does not move cannot make progress; fail the
            // listing closed rather than loop on a corrupt directory.
            if entry.next_cursor == cursor {
                return Err(VfsError::Io);
            }
            let name =
                core::str::from_utf8(&name_buf[..entry.name_len]).map_err(|_| VfsError::Io)?;
            entries.push((entry.info, name.to_string()));
            cursor = entry.next_cursor;
        }
        Ok(entries)
    }
}

impl<F: FilesystemRead + FilesystemWrite + ?Sized, P: MetaPolicy<F>> DelegatedFs<'_, F, P> {
    /// Resolve the parent directory of the leaf addressed by `components`,
    /// authorising search on every ancestor and search + write on the
    /// parent itself, judged against the parent's own metadata under the
    /// active [`MetaPolicy`].
    ///
    /// Returns the parent's [`NodeId`]; an empty `components` slice (which
    /// names the mount point itself) is rejected — the driver root cannot
    /// be the target of a mutation.
    fn parent_for_write(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<NodeId, VfsError> {
        let (_, parents) = components.split_last().ok_or(VfsError::InvalidPath)?;
        let (parent, info, meta) = self.resolve(cred, parents)?;
        if info.kind != NodeKind::Directory {
            return Err(VfsError::NotADirectory);
        }
        meta.authorize(cred, Access::Execute)?;
        meta.authorize(cred, Access::Write)?;
        Ok(parent)
    }

    /// Create an empty child of `kind` at `components`.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `components` is empty.
    /// * [`VfsError::AlreadyExists`] if a child of that name already exists.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`],
    ///   [`VfsError::NotFound`] (a missing ancestor), or [`VfsError::Io`].
    pub fn create(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        kind: NodeKind,
    ) -> Result<(), VfsError> {
        let parent = self.parent_for_write(cred, components)?;
        let name = components[components.len() - 1].as_bytes();
        match self.fs.lookup(parent, name) {
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(DriverError::NotFound) => {}
            Err(e) => return Err(map_driver_error(e)),
        }
        let node = self
            .fs
            .create(parent, name, kind)
            .map_err(map_driver_error)?;
        // The driver minted the node with its format's default record;
        // hand it to its creator before the create is observable, so the
        // caller is never locked out of a node it just made.
        P::stamp_creation(self.fs, node, cred)?;
        Ok(())
    }

    /// Write `data` into the file at `components` starting at `offset`,
    /// returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `components` is empty.
    /// * [`VfsError::IsADirectory`] if `components` names a directory.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotFound`],
    ///   [`VfsError::NotADirectory`], or [`VfsError::Io`].
    pub fn write(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, VfsError> {
        let parent = self.parent_for_write(cred, components)?;
        let name = components[components.len() - 1].as_bytes();
        let node = self.fs.lookup(parent, name).map_err(map_driver_error)?;
        if self.fs.node_info(node).map_err(map_driver_error)?.kind == NodeKind::Directory {
            return Err(VfsError::IsADirectory);
        }
        self.fs
            .write_at(parent, name, offset, data)
            .map_err(map_driver_error)
    }

    /// Set the length of the file at `components` to `size`.
    ///
    /// # Errors
    ///
    /// As for [`Self::write`].
    pub fn truncate(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        size: u64,
    ) -> Result<(), VfsError> {
        let parent = self.parent_for_write(cred, components)?;
        let name = components[components.len() - 1].as_bytes();
        let node = self.fs.lookup(parent, name).map_err(map_driver_error)?;
        if self.fs.node_info(node).map_err(map_driver_error)?.kind == NodeKind::Directory {
            return Err(VfsError::IsADirectory);
        }
        self.fs
            .truncate(parent, name, size)
            .map_err(map_driver_error)
    }

    /// Unlink the child at `components`.
    ///
    /// With `dir_only` the removal succeeds only when the child is an
    /// (empty) directory — the atomic `rmdir` posture, decided in the same
    /// locked walk that removes the entry, never by a caller-side stat.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `components` is empty.
    /// * [`VfsError::NotFound`] if the child does not exist.
    /// * [`VfsError::NotEmpty`] if it is a non-empty directory.
    /// * [`VfsError::NotADirectory`] if `dir_only` and the child is not a
    ///   directory.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::Io`].
    pub fn remove(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        dir_only: bool,
    ) -> Result<(), VfsError> {
        let parent = self.parent_for_write(cred, components)?;
        let name = components[components.len() - 1].as_bytes();
        // Ensure it exists (distinguishing NotFound), and report NotEmpty
        // for a non-empty directory rather than the driver's generic Busy.
        let node = self.fs.lookup(parent, name).map_err(map_driver_error)?;
        if self.fs.node_info(node).map_err(map_driver_error)?.kind == NodeKind::Directory {
            let mut name_buf = [0u8; MAX_COMPONENT_LEN];
            if self
                .fs
                .read_dir(node, 0, &mut name_buf)
                .map_err(map_driver_error)?
                .is_some()
            {
                return Err(VfsError::NotEmpty);
            }
        } else if dir_only {
            return Err(VfsError::NotADirectory);
        }
        self.fs.remove(parent, name).map_err(map_driver_error)
    }

    /// Move the leaf at `src_components` to `dst_components` within the same
    /// delegated mount, preserving the node's identity and contents.
    ///
    /// Authorises search + write on both the source and destination parent
    /// directories; when a directory is moved to a *different* parent its
    /// `..` link is rewritten, so write permission on the moved directory
    /// itself is additionally required (POSIX). The structural move — the
    /// existence, kind-compatibility, empty-target, and
    /// directory-into-its-own-subtree checks — is performed by the driver.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if either path is empty (names the mount
    ///   point itself).
    /// * [`VfsError::NotFound`] if the source does not exist.
    /// * [`VfsError::NotEmpty`] if the destination is a non-empty directory,
    ///   or the move would place a directory inside its own subtree.
    /// * [`VfsError::NotADirectory`] on a kind-incompatible replacement.
    /// * [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn rename(
        &mut self,
        cred: &Credentials<'_>,
        src_components: &[String],
        dst_components: &[String],
    ) -> Result<(), VfsError> {
        let src_parent = self.parent_for_write(cred, src_components)?;
        let dst_parent = self.parent_for_write(cred, dst_components)?;
        let src_name = src_components[src_components.len() - 1].as_bytes();
        let dst_name = dst_components[dst_components.len() - 1].as_bytes();

        // A directory moved to a different parent has its `..` rewritten, so
        // write permission on the directory itself is required as well.
        let src_node = self
            .fs
            .lookup(src_parent, src_name)
            .map_err(map_driver_error)?;
        if src_parent != dst_parent
            && self.fs.node_info(src_node).map_err(map_driver_error)?.kind == NodeKind::Directory
        {
            let meta = P::metadata(self.fs, src_node, &self.template)?;
            meta.authorize(cred, Access::Write)?;
        }

        self.fs
            .rename(src_parent, src_name, dst_parent, dst_name)
            .map_err(map_rename_error)
    }
}

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
