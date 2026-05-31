//! The [`Vfs`]: an in-RAM directory tree that enforces the `AGENTS.md`
//! §16 layout and the §5.3 permission model on every operation.
//!
//! Each [`Node`] owns its children directly (a directory is a
//! `BTreeMap<String, Node>`), so resolution is a borrow walk and removal
//! simply drops a subtree — no arena, no index bookkeeping, no panicking
//! "this can't happen" fallbacks. This is the boot-time root filesystem
//! before a block-backed `drivers/filesystem/*` driver mounts; the
//! structure and the policy it enforces are identical regardless of what
//! eventually backs a subtree.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, MountFlags};
use rustos_abi::CapabilityId;
use rustos_kernel_sec::{GroupId, UserId};

use super::delegate::{DelegatedFs, DelegatedInfo};
use super::mount::MountTable;
use super::path::{is_reserved_top_level, Path, ROOT_TEMPLATE};
use super::perm::{Access, Credentials, Metadata, Mode};
use super::VfsError;

/// A node's payload: a directory (name → child) or a file (bytes).
#[derive(Clone, Debug, Eq, PartialEq)]
enum NodeKind {
    Directory(BTreeMap<String, Node>),
    File(Vec<u8>),
}

/// A single inode: its access-control [`Metadata`] and its payload.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Node {
    meta: Metadata,
    kind: NodeKind,
}

impl Node {
    fn directory(meta: Metadata) -> Self {
        Self {
            meta,
            kind: NodeKind::Directory(BTreeMap::new()),
        }
    }

    fn file(meta: Metadata, contents: Vec<u8>) -> Self {
        Self {
            meta,
            kind: NodeKind::File(contents),
        }
    }
}

/// An in-RAM virtual filesystem tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vfs {
    root: Node,
    mounts: MountTable,
}

impl Vfs {
    /// Construct a VFS whose root directory carries `root_meta` and a
    /// single writable root mount. The root starts empty.
    #[must_use]
    pub fn new(root_meta: Metadata) -> Self {
        Self {
            root: Node::directory(root_meta),
            mounts: MountTable::new(MountFlags::default()),
        }
    }

    /// Construct a VFS pre-populated with the `AGENTS.md` §16 default
    /// layout: exactly the four permitted top-level directories
    /// (`/System`, `/Users`, `/Apps`, `/Storage`), the two writable
    /// exceptions under `/System` (`/System/Logs`, `/System/Settings`),
    /// and the matching mount policy (§16.2, §16.3).
    ///
    /// `owner`/`group` own the system directories. The layout is fixed and
    /// correct by construction.
    #[must_use]
    pub fn with_default_layout(owner: UserId, group: GroupId) -> Self {
        let nosuid_nodev = MountFlags::NOSUID.union(MountFlags::NODEV);
        let nosuid_nodev_noexec = nosuid_nodev.union(MountFlags::NOEXEC);
        let dir_meta = || Metadata::new(owner, group, Mode::from_bits(0o755));

        let mut system = Node::directory(dir_meta());
        if let NodeKind::Directory(entries) = &mut system.kind {
            entries.insert("Logs".to_string(), Node::directory(dir_meta()));
            entries.insert("Settings".to_string(), Node::directory(dir_meta()));
        }

        let mut root = Node::directory(dir_meta());
        if let NodeKind::Directory(entries) = &mut root.kind {
            for name in ROOT_TEMPLATE {
                let node = if name == "System" {
                    system.clone()
                } else {
                    Node::directory(dir_meta())
                };
                entries.insert(name.to_string(), node);
            }
        }

        let mut mounts = MountTable::new(MountFlags::default());
        // The longest-prefix resolution in `MountTable` lets the writable
        // `/System/Logs` and `/System/Settings` child mounts shadow the
        // read-only `/System` (`AGENTS.md` §16.2 / §16.3).
        let policy = [
            ("/System", MountFlags::READ_ONLY),
            ("/System/Logs", nosuid_nodev_noexec),
            ("/System/Settings", nosuid_nodev_noexec),
            ("/Users", nosuid_nodev),
            ("/Apps", nosuid_nodev),
            ("/Storage", nosuid_nodev_noexec),
        ];
        for (path, flags) in policy {
            if let Ok(p) = Path::parse(path) {
                let _ = mounts.mount(p, flags, None);
            }
        }

        Self { root, mounts }
    }

    /// The mount table.
    #[must_use]
    pub fn mounts(&self) -> &MountTable {
        &self.mounts
    }

    /// The mount table, mutably, for wiring a block-backed filesystem
    /// driver into a subtree.
    pub fn mounts_mut(&mut self) -> &mut MountTable {
        &mut self.mounts
    }

    /// Read from a file under a driver-backed mount, delegating the I/O to
    /// `fs` (`AGENTS.md` §2.4 / §5.4).
    ///
    /// The covering mount must be driver-backed (its
    /// [`backing`](super::MountPoint::backing) is `Some`); the caller maps
    /// that handle to the live `fs`. Resolution walks the in-RAM tree to the
    /// mount point — authorising search permission on every ancestor — and
    /// then delegates the remaining components to `fs`, applying the mount
    /// point's [`Metadata`] as the permission template (see [`DelegatedFs`]).
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * [`VfsError::IsADirectory`] if `path` names a directory.
    /// * [`VfsError::PermissionDenied`] if a traversal or the read is
    ///   denied.
    /// * [`VfsError::NotADirectory`], [`VfsError::InvalidPath`], or
    ///   [`VfsError::Io`].
    pub fn read_via(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut dyn FilesystemRead,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, VfsError> {
        let (template, remainder) = self.delegate_context(cred, path)?;
        DelegatedFs::new(fs, template).read(cred, &remainder, offset, buf)
    }

    /// List a directory under a driver-backed mount, delegating to `fs`.
    ///
    /// See [`Vfs::read_via`] for the resolution and permission model. An
    /// empty remainder (i.e. `path` is the mount point itself) lists the
    /// driver's root directory.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * [`VfsError::NotADirectory`] if `path` names a file.
    /// * [`VfsError::PermissionDenied`], [`VfsError::InvalidPath`], or
    ///   [`VfsError::Io`].
    pub fn list_via(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut dyn FilesystemRead,
    ) -> Result<Vec<String>, VfsError> {
        let (template, remainder) = self.delegate_context(cred, path)?;
        DelegatedFs::new(fs, template).list(cred, &remainder)
    }

    /// Report the structural metadata of a node under a driver-backed
    /// mount, delegating to `fs`.
    ///
    /// See [`Vfs::read_via`] for the resolution and permission model.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path` or
    ///   the node does not exist.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`],
    ///   [`VfsError::InvalidPath`], or [`VfsError::Io`].
    pub fn stat_via(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut dyn FilesystemRead,
    ) -> Result<DelegatedInfo, VfsError> {
        let (template, remainder) = self.delegate_context(cred, path)?;
        DelegatedFs::new(fs, template).stat(cred, &remainder)
    }

    /// Resolve the driver-backed mount covering `path`, returning the
    /// permission template to apply to delegated nodes and the path
    /// components below the mount point.
    ///
    /// Walking to the mount point through [`Vfs::resolve`] authorises
    /// search permission on every ancestor directory; the mount point's own
    /// metadata becomes the template, and search permission on the mount
    /// point itself is enforced by the delegated walk (it is the template's
    /// `Execute` bit).
    fn delegate_context(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
    ) -> Result<(Metadata, Vec<String>), VfsError> {
        let mount = self.mounts.resolve(path);
        if mount.backing().is_none() {
            return Err(VfsError::NotFound);
        }
        let mount_depth = mount.path().depth();
        let mount_path = mount.path().clone();
        let node = self.resolve(cred, &mount_path)?;
        let NodeKind::Directory(_) = &node.kind else {
            return Err(VfsError::NotADirectory);
        };
        let template = node.meta.clone();
        let remainder = path.components()[mount_depth..].to_vec();
        Ok((template, remainder))
    }

    /// Look up the [`Metadata`] of the inode at `path`.
    ///
    /// Requires search (execute) permission on every intermediate
    /// directory but no permission on the target itself, mirroring POSIX
    /// `stat`.
    ///
    /// # Errors
    ///
    /// [`VfsError::InvalidPath`], [`VfsError::NotFound`],
    /// [`VfsError::NotADirectory`], or [`VfsError::PermissionDenied`].
    pub fn metadata(&self, cred: &Credentials<'_>, path: &Path) -> Result<&Metadata, VfsError> {
        Ok(&self.resolve(cred, path)?.meta)
    }

    /// Create a directory at `path` with mode `mode`.
    ///
    /// # Errors
    ///
    /// * [`VfsError::ReservedPath`] if `path` is a reserved legacy POSIX
    ///   top-level name (`AGENTS.md` §16.1).
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::PermissionDenied`] if the caller lacks write
    ///   permission on the parent directory.
    /// * [`VfsError::AlreadyExists`], [`VfsError::NotFound`],
    ///   [`VfsError::NotADirectory`], or [`VfsError::InvalidPath`].
    pub fn mkdir(
        &mut self,
        cred: &Credentials<'_>,
        path: &Path,
        mode: Mode,
    ) -> Result<(), VfsError> {
        let meta = Metadata::new(cred.uid, cred.gid, mode);
        self.create(cred, path, Node::directory(meta))
    }

    /// Create a regular file at `path` with mode `mode` and `contents`.
    ///
    /// # Errors
    ///
    /// As [`Vfs::mkdir`].
    pub fn create_file(
        &mut self,
        cred: &Credentials<'_>,
        path: &Path,
        mode: Mode,
        contents: Vec<u8>,
    ) -> Result<(), VfsError> {
        let meta = Metadata::new(cred.uid, cred.gid, mode);
        self.create(cred, path, Node::file(meta, contents))
    }

    /// Read the contents of the file at `path`.
    ///
    /// # Errors
    ///
    /// * [`VfsError::IsADirectory`] if `path` names a directory.
    /// * [`VfsError::PermissionDenied`] if the caller lacks read
    ///   permission on the file.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::InvalidPath`].
    pub fn read(&self, cred: &Credentials<'_>, path: &Path) -> Result<&[u8], VfsError> {
        let node = self.resolve(cred, path)?;
        let NodeKind::File(bytes) = &node.kind else {
            return Err(VfsError::IsADirectory);
        };
        node.meta.authorize(cred, Access::Read)?;
        Ok(bytes)
    }

    /// Replace the contents of the file at `path`.
    ///
    /// # Errors
    ///
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::IsADirectory`] if `path` names a directory.
    /// * [`VfsError::PermissionDenied`] if the caller lacks write
    ///   permission on the file.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::InvalidPath`].
    pub fn write(
        &mut self,
        cred: &Credentials<'_>,
        path: &Path,
        contents: Vec<u8>,
    ) -> Result<(), VfsError> {
        if self.mounts.is_read_only(path) {
            return Err(VfsError::ReadOnly);
        }
        let node = self.resolve_mut(cred, path)?;
        let NodeKind::File(bytes) = &mut node.kind else {
            return Err(VfsError::IsADirectory);
        };
        node.meta.authorize(cred, Access::Write)?;
        *bytes = contents;
        Ok(())
    }

    /// List the entry names of the directory at `path`, sorted.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotADirectory`] if `path` names a file.
    /// * [`VfsError::PermissionDenied`] if the caller lacks read
    ///   permission on the directory.
    /// * [`VfsError::NotFound`] or [`VfsError::InvalidPath`].
    pub fn list(&self, cred: &Credentials<'_>, path: &Path) -> Result<Vec<String>, VfsError> {
        let node = self.resolve(cred, path)?;
        let NodeKind::Directory(entries) = &node.kind else {
            return Err(VfsError::NotADirectory);
        };
        node.meta.authorize(cred, Access::Read)?;
        Ok(entries.keys().cloned().collect())
    }

    /// Remove the (empty) directory or file at `path`.
    ///
    /// # Errors
    ///
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::NotEmpty`] if `path` is a non-empty directory.
    /// * [`VfsError::PermissionDenied`] if the caller lacks write
    ///   permission on the parent directory.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::InvalidPath`].
    pub fn remove(&mut self, cred: &Credentials<'_>, path: &Path) -> Result<(), VfsError> {
        let name = path.file_name().ok_or(VfsError::InvalidPath)?.to_string();
        let parent_path = path.parent().ok_or(VfsError::InvalidPath)?;
        // Removal mutates the parent directory, so the parent's covering
        // mount governs writability (e.g. removing the `/System/Logs`
        // mount point is forbidden by the read-only `/System`).
        if self.mounts.is_read_only(&parent_path) {
            return Err(VfsError::ReadOnly);
        }
        let parent = self.resolve_mut(cred, &parent_path)?;
        parent.meta.authorize(cred, Access::Write)?;
        let NodeKind::Directory(entries) = &mut parent.kind else {
            return Err(VfsError::NotADirectory);
        };

        match entries.get(&name) {
            None => return Err(VfsError::NotFound),
            Some(Node {
                kind: NodeKind::Directory(children),
                ..
            }) if !children.is_empty() => return Err(VfsError::NotEmpty),
            Some(_) => {}
        }
        entries.remove(&name);
        Ok(())
    }

    /// Set (or clear) the capability gate on the inode at `path`
    /// (`AGENTS.md` §5.3). Only the inode's owner may change it.
    ///
    /// # Errors
    ///
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::PermissionDenied`] if the caller is not the owner.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::InvalidPath`].
    pub fn set_required_cap(
        &mut self,
        cred: &Credentials<'_>,
        path: &Path,
        cap: Option<CapabilityId>,
    ) -> Result<(), VfsError> {
        if self.mounts.is_read_only(path) {
            return Err(VfsError::ReadOnly);
        }
        let node = self.resolve_mut(cred, path)?;
        if node.meta.owner != cred.uid {
            return Err(VfsError::PermissionDenied);
        }
        node.meta.required_cap = cap;
        Ok(())
    }

    /// Insert `child` at `path`, enforcing the shared create preconditions:
    /// reserved-name refusal, read-only refusal, parent resolution, parent
    /// write permission, and the no-clobber existence check.
    fn create(&mut self, cred: &Credentials<'_>, path: &Path, child: Node) -> Result<(), VfsError> {
        let name = path.file_name().ok_or(VfsError::InvalidPath)?.to_string();
        if path.depth() == 1 && is_reserved_top_level(&name) {
            return Err(VfsError::ReservedPath);
        }
        let parent_path = path.parent().ok_or(VfsError::InvalidPath)?;
        // Creation mutates the parent directory, so the parent's covering
        // mount governs writability (`AGENTS.md` §16.2).
        if self.mounts.is_read_only(&parent_path) {
            return Err(VfsError::ReadOnly);
        }
        let parent = self.resolve_mut(cred, &parent_path)?;
        parent.meta.authorize(cred, Access::Write)?;
        let NodeKind::Directory(entries) = &mut parent.kind else {
            return Err(VfsError::NotADirectory);
        };
        if entries.contains_key(&name) {
            return Err(VfsError::AlreadyExists);
        }
        entries.insert(name, child);
        Ok(())
    }

    /// Resolve `path` to a node, enforcing search (execute) permission on
    /// every directory descended into.
    fn resolve(&self, cred: &Credentials<'_>, path: &Path) -> Result<&Node, VfsError> {
        let mut node = &self.root;
        for component in path.components() {
            let NodeKind::Directory(entries) = &node.kind else {
                return Err(VfsError::NotADirectory);
            };
            node.meta.authorize(cred, Access::Execute)?;
            node = entries.get(component).ok_or(VfsError::NotFound)?;
        }
        Ok(node)
    }

    /// Mutable counterpart to [`Vfs::resolve`].
    fn resolve_mut(&mut self, cred: &Credentials<'_>, path: &Path) -> Result<&mut Node, VfsError> {
        let mut node = &mut self.root;
        for component in path.components() {
            let NodeKind::Directory(entries) = &mut node.kind else {
                return Err(VfsError::NotADirectory);
            };
            node.meta.authorize(cred, Access::Execute)?;
            node = entries.get_mut(component).ok_or(VfsError::NotFound)?;
        }
        Ok(node)
    }
}

#[cfg(test)]
#[path = "vfs_tests.rs"]
mod tests;
