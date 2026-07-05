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
//! [`FilesystemSecurity`] (for a driver like `rustfs` that stores full
//! per-inode ownership, mode, ACL, and capability gate).
//!
//! The adapter is constructed per call with a `&mut dyn FilesystemRead`
//! rather than stored in the [`Vfs`](super::Vfs): the VFS tree is
//! `Clone + Eq`, and the live driver — mapped from the mount's
//! [`DriverHandle`](rustos_abi::driver::DriverHandle) by the kernel host —
//! is neither.

use core::marker::PhantomData;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeInfo, NodeKind,
};
use rustos_abi::driver::DriverError;

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
/// per-inode record ([`PerInode`], for a driver like `rustfs`). Both feed
/// the *same* [`Metadata::authorize`] decision (the VFS is the single policy point).
///
/// The two implementors [`Uniform`] and [`PerInode`] are the only ones the
/// crate provides; callers select between them with [`DelegatedFs::new`]
/// and [`DelegatedFs::new_secured`] rather than naming this trait.
pub trait MetaPolicy<R: FilesystemRead + ?Sized> {
    /// The permission metadata governing `node`.
    fn metadata(fs: &mut R, node: NodeId, template: &Metadata) -> Result<Metadata, VfsError>;
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
}

impl<R: FilesystemRead + FilesystemSecurity + ?Sized> MetaPolicy<R> for PerInode {
    fn metadata(fs: &mut R, node: NodeId, _template: &Metadata) -> Result<Metadata, VfsError> {
        let sec = fs.security(node).map_err(map_driver_error)?;
        Ok(Metadata::from_node_security(&sec))
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
        let (_, info, meta) = self.resolve(cred, components)?;
        Ok(DelegatedInfo {
            kind: info.kind,
            size: info.size,
            allocated: info.allocated,
            meta,
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

    /// List the entry names of the directory at `components`, in the
    /// driver's stable on-disk order.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotADirectory`] if `components` names a file.
    /// * [`VfsError::PermissionDenied`] if the node's metadata denies read.
    /// * [`VfsError::NotFound`] or [`VfsError::Io`] (the latter also for a
    ///   directory entry whose on-disk name is not valid UTF-8).
    pub fn list(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<Vec<String>, VfsError> {
        let (node, info, meta) = self.resolve(cred, components)?;
        if info.kind != NodeKind::Directory {
            return Err(VfsError::NotADirectory);
        }
        meta.authorize(cred, Access::Read)?;

        let mut names = Vec::new();
        let mut name_buf = [0u8; MAX_COMPONENT_LEN];
        let mut index: u64 = 0;
        while let Some(entry) = self
            .fs
            .read_dir(node, index, &mut name_buf)
            .map_err(map_driver_error)?
        {
            let name =
                core::str::from_utf8(&name_buf[..entry.name_len]).map_err(|_| VfsError::Io)?;
            names.push(name.to_string());
            index += 1;
        }
        Ok(names)
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
        self.fs
            .create(parent, name, kind)
            .map_err(map_driver_error)?;
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
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `components` is empty.
    /// * [`VfsError::NotFound`] if the child does not exist.
    /// * [`VfsError::NotEmpty`] if it is a non-empty directory.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::Io`].
    pub fn remove(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
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
