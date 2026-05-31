//! Routing VFS resolution to a `drivers/filesystem/*` driver
//! (`AGENTS.md` §2.4 / §5.4).
//!
//! [`DelegatedFs`] adapts a borrowed
//! [`FilesystemRead`] driver to the VFS's path-resolution and permission
//! model for the subtree below a driver-backed mount point. The driver
//! supplies **structural** I/O only — it mints opaque [`NodeId`]s and
//! reports each node's kind, size, children, and bytes; it makes no
//! permission decision (`AGENTS.md` §5.4). Ownership, mode bits, ACLs, and
//! the §5.3 capability gate stay in the VFS: every delegated node inherits
//! the mount point's [`Metadata`] as a uniform template (the natural model
//! for a filesystem like FAT that stores no per-file owner), and
//! [`DelegatedFs`] authorises against it before reading and on every
//! directory it descends into.
//!
//! The adapter is constructed per call with a `&mut dyn FilesystemRead`
//! rather than stored in the [`Vfs`](super::Vfs): the VFS tree is
//! `Clone + Eq`, and the live driver — mapped from the mount's
//! [`DriverHandle`](rustos_abi::driver::DriverHandle) by the kernel host —
//! is neither.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, NodeId, NodeInfo, NodeKind};
use rustos_abi::driver::DriverError;

use super::path::MAX_COMPONENT_LEN;
use super::perm::{Access, Credentials, Metadata};
use super::VfsError;

/// Structural metadata of a delegated node, paired with the VFS
/// [`Metadata`] template the mount applies to it.
///
/// `kind` and `size` come from the driver's on-disk layout; `meta` is the
/// mount point's metadata, the policy the §5.3 checks use for every node in
/// the delegated subtree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedInfo {
    /// Whether the node is a directory or a regular file.
    pub kind: NodeKind,
    /// File length in bytes; `0` for a directory.
    pub size: u64,
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

/// A `FilesystemRead` driver bound to the [`Metadata`] template of its
/// mount point, exposing VFS-shaped resolution over the delegated subtree.
///
/// `components` passed to the methods are the path *relative to the mount
/// point* (the VFS strips the mount prefix); an empty slice names the mount
/// point itself, i.e. the driver's root directory.
pub struct DelegatedFs<'fs> {
    fs: &'fs mut dyn FilesystemRead,
    template: Metadata,
}

impl<'fs> DelegatedFs<'fs> {
    /// Bind `fs` to the `template` metadata its mount point carries.
    #[must_use]
    pub fn new(fs: &'fs mut dyn FilesystemRead, template: Metadata) -> Self {
        Self { fs, template }
    }

    /// Resolve `components` from the driver root, enforcing search
    /// (execute) permission on every directory descended into — the same
    /// rule the in-RAM [`Vfs`](super::Vfs) applies, decided against the
    /// uniform template.
    fn resolve(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<(NodeId, NodeInfo), VfsError> {
        let mut node = self.fs.root();
        let mut info = self.fs.node_info(node).map_err(map_driver_error)?;
        for component in components {
            if info.kind != NodeKind::Directory {
                return Err(VfsError::NotADirectory);
            }
            self.template.authorize(cred, Access::Execute)?;
            node = self
                .fs
                .lookup(node, component.as_bytes())
                .map_err(map_driver_error)?;
            info = self.fs.node_info(node).map_err(map_driver_error)?;
        }
        Ok((node, info))
    }

    /// Report the structural metadata of the node at `components`, paired
    /// with the mount's permission template. Like POSIX `stat`, this needs
    /// search permission on every intermediate directory but none on the
    /// target itself.
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
        let (_, info) = self.resolve(cred, components)?;
        Ok(DelegatedInfo {
            kind: info.kind,
            size: info.size,
            meta: self.template.clone(),
        })
    }

    /// Read up to `buf.len()` bytes from the file at `components` starting
    /// at `offset`, returning the number of bytes read (short of
    /// `buf.len()` at end-of-file).
    ///
    /// # Errors
    ///
    /// * [`VfsError::IsADirectory`] if `components` names a directory.
    /// * [`VfsError::PermissionDenied`] if the template denies read.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::Io`].
    pub fn read(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, VfsError> {
        let (node, info) = self.resolve(cred, components)?;
        if info.kind == NodeKind::Directory {
            return Err(VfsError::IsADirectory);
        }
        self.template.authorize(cred, Access::Read)?;
        self.fs.read_at(node, offset, buf).map_err(map_driver_error)
    }

    /// List the entry names of the directory at `components`, in the
    /// driver's stable on-disk order.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotADirectory`] if `components` names a file.
    /// * [`VfsError::PermissionDenied`] if the template denies read.
    /// * [`VfsError::NotFound`] or [`VfsError::Io`] (the latter also for a
    ///   directory entry whose on-disk name is not valid UTF-8).
    pub fn list(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<Vec<String>, VfsError> {
        let (node, info) = self.resolve(cred, components)?;
        if info.kind != NodeKind::Directory {
            return Err(VfsError::NotADirectory);
        }
        self.template.authorize(cred, Access::Read)?;

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

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
