//! The [`Vfs`]: an in-RAM directory tree that enforces the layout and the permission model on every operation.
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

use tairix_abi::driver::filesystem::{
    FilesystemAttrs, FilesystemRead, FilesystemSecurity, FilesystemWrite, MountFlags,
    NodeKind as DriverNodeKind,
};
use tairix_abi::fs::{RealpathMode, FS_PATH_MAX};
use tairix_abi::CapabilityId;
use tairix_kernel_sec::{GroupId, UserId};
use tairix_sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::delegate::{DelegatedEntry, DelegatedFs, DelegatedInfo, FinalLink, MountProjection};
use super::mount::MountTable;
use super::path::{spell, Path, MAX_PATH_COMPONENTS, ROOT_TEMPLATE};
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
///
/// The mount table lives behind its own [`RwLock`] so a runtime volume
/// attach/detach can add or retract a mount through a shared `&Vfs` (the
/// set-once boot cell) while concurrent operations resolve against it.
/// Every reader takes the guard only for the tiny lookup and copies what
/// it needs out — never across a driver call, which may park.
pub struct Vfs {
    root: Node,
    mounts: RwLock<MountTable>,
}

impl Vfs {
    /// Construct a VFS whose root directory carries `root_meta` and a
    /// single writable root mount. The root starts empty.
    #[must_use]
    pub fn new(root_meta: Metadata) -> Self {
        Self {
            root: Node::directory(root_meta),
            mounts: RwLock::new(MountTable::new(MountFlags::default())),
        }
    }

    /// Construct a VFS pre-populated with the default
    /// layout: exactly the four permitted top-level directories
    /// (`/System`, `/Users`, `/Apps`, `/Storage`), the two writable
    /// exceptions under `/System` (`/System/Logs`, `/System/Settings`),
    /// and the matching mount policy.
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
        // read-only `/System`.
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

        Self {
            root,
            mounts: RwLock::new(mounts),
        }
    }

    /// The mount table, read-locked for the duration of the returned guard.
    ///
    /// Hold the guard only for the lookup and copy the needed facts out;
    /// never hold it across a driver operation, which may park while the
    /// guard's contenders would spin.
    pub fn mounts(&self) -> RwLockReadGuard<'_, MountTable> {
        self.mounts.read()
    }

    /// The mount table, write-locked, for wiring a block-backed filesystem
    /// driver into a subtree or adding/retracting a runtime mount.
    pub fn mounts_write(&self) -> RwLockWriteGuard<'_, MountTable> {
        self.mounts.write()
    }

    /// Read from a file under a driver-backed mount, delegating the I/O to
    /// `fs`.
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
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new(fs, mount).read(cred, &remainder, offset, buf)
    }

    /// List a directory under a driver-backed mount, delegating to `fs`.
    /// Each [`DelegatedEntry`] carries the child's driver node number and
    /// the structural [`NodeInfo`](tairix_abi::driver::filesystem::NodeInfo)
    /// the listing
    /// driver reports, so a caller never re-resolves a child by path (a
    /// child path shadowed by another mount would be judged against the
    /// wrong volume, and each re-resolution would be a fresh full walk).
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
        final_link: FinalLink,
    ) -> Result<Vec<DelegatedEntry>, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new(fs, mount).list(cred, &remainder, final_link)
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
        final_link: FinalLink,
    ) -> Result<DelegatedInfo, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new(fs, mount).stat(cred, &remainder, final_link)
    }

    /// Read the stored target of the symbolic link at `path` under a
    /// driver-backed mount, delegating to `fs`.
    ///
    /// The final component is never followed; the target comes back exactly
    /// as it was stored. See [`Vfs::read_via`] for the resolution and
    /// permission model.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path` or
    ///   the link does not exist.
    /// * [`VfsError::InvalidPath`] if `path` names something other than a
    ///   symbolic link.
    /// * [`VfsError::NotSupported`] if the mounted format stores no links.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`],
    ///   [`VfsError::LinkLoop`], or [`VfsError::Io`].
    pub fn readlink_via(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut dyn FilesystemRead,
    ) -> Result<String, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new(fs, mount).read_link(cred, &remainder)
    }

    /// Canonicalise `path` under a driver-backed mount, delegating to `fs`:
    /// the one path that names what `path` resolves to, with every symbolic
    /// link followed and every `..` applied to the nodes the walk really
    /// traversed.
    ///
    /// The answer is the covering mount point's own path followed by the
    /// canonical remainder the delegated walk reported, so it is a `/`-view
    /// path in the caller's own namespace rather than a path on the backing
    /// volume. That composition is total because a walk cannot leave what
    /// its mount projects: `..` and an absolute link target are both floored
    /// at the mount's own root.
    ///
    /// See [`Vfs::read_via`] for the resolution and permission model —
    /// search permission is required on every directory the resolution
    /// passes through, whether the caller spelled it or a link supplied it.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`, or
    ///   if `mode` requires a component that does not exist.
    /// * [`VfsError::InvalidPath`] if the canonical path would fall outside
    ///   the grammar [`Path::parse`] accepts — an answer the kernel reports
    ///   is always one it would take back.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`],
    ///   [`VfsError::LinkLoop`], or [`VfsError::Io`].
    pub fn realpath_via(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut dyn FilesystemRead,
        mode: RealpathMode,
    ) -> Result<String, VfsError> {
        let mount_path = self.covering_mount_path(path);
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        let below = DelegatedFs::new(fs, mount).canonicalize(cred, &remainder, mode)?;
        Self::spell_canonical(&mount_path, below)
    }

    /// Per-inode counterpart of [`Vfs::realpath_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::realpath_via`].
    pub fn realpath_via_secured<F: FilesystemRead + FilesystemSecurity + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        mode: RealpathMode,
    ) -> Result<String, VfsError> {
        let mount_path = self.covering_mount_path(path);
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        let below = DelegatedFs::new_secured(fs, mount).canonicalize(cred, &remainder, mode)?;
        Self::spell_canonical(&mount_path, below)
    }

    /// The path of the mount covering `path`, read under a short lock.
    fn covering_mount_path(&self, path: &Path) -> Path {
        self.mounts.read().resolve(path).path().clone()
    }

    /// Spell the canonical `/`-view path of `below` beneath the mount point
    /// `mount_path`, refusing an answer this VFS would not accept back.
    ///
    /// Holding that invariant here is what makes the call safe to feed
    /// straight into another syscall: a canonical path is never longer than
    /// [`FS_PATH_MAX`] and never carries more than
    /// [`MAX_PATH_COMPONENTS`] components, so a caller cannot be handed a
    /// name it is then refused for spelling.
    fn spell_canonical(mount_path: &Path, below: Vec<String>) -> Result<String, VfsError> {
        let mut components = mount_path.components().to_vec();
        components.extend(below);
        if components.len() > MAX_PATH_COMPONENTS {
            return Err(VfsError::InvalidPath);
        }
        let spelled = spell(&components);
        if spelled.len() > FS_PATH_MAX {
            return Err(VfsError::InvalidPath);
        }
        Ok(spelled)
    }

    /// Create an empty regular file under a driver-backed mount,
    /// delegating to `fs`.
    ///
    /// The covering mount must be driver-backed and writable; resolution
    /// and the checks match [`Vfs::read_via`], plus write permission
    /// on the parent directory's template.
    ///
    /// This is the `open`-with-`O_CREAT` create, so a final symbolic link is
    /// followed: creating through a *dangling* link creates the file the link
    /// names, as POSIX specifies, rather than reporting the link's own name
    /// as already taken.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::AlreadyExists`], [`VfsError::PermissionDenied`],
    ///   [`VfsError::NotADirectory`], [`VfsError::InvalidPath`], or
    ///   [`VfsError::Io`].
    pub fn create_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new(fs, mount).create(
            cred,
            &remainder,
            DriverNodeKind::RegularFile,
            FinalLink::Follow,
        )
    }

    /// Create a symbolic link at `path` under a driver-backed mount whose
    /// stored target is `target`, delegating to `fs`.
    ///
    /// `target` is stored verbatim and is never resolved here, so the call
    /// authorises only the right to create a name in the link's own parent
    /// and the link may legitimately dangle. See [`Vfs::create_via`] for the
    /// resolution and permission model.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `target` fails the link-target
    ///   grammar.
    /// * [`VfsError::NotSupported`] if the mounted format has no link object
    ///   type.
    /// * Otherwise as for [`Vfs::create_via`].
    pub fn symlink_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        target: &str,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new(fs, mount).create_link(cred, &remainder, target)
    }

    /// Add `link` as a second name for the node `existing` already names
    /// under a driver-backed mount — a hard link — delegating to `fs`.
    ///
    /// Both paths must resolve under the **same** writable driver-backed
    /// mount: a directory entry addresses an inode in its own backing, so a
    /// pair that crosses mounts is [`VfsError::CrossVolume`]. `existing_link`
    /// selects whether the existing name's final symbolic link is resolved
    /// ([`FinalLink::Follow`], the `ln -L` posture) or the link itself gains
    /// the second name ([`FinalLink::Keep`], POSIX `link()`); the new name is
    /// never followed, and a directory is refused.
    ///
    /// # Errors
    ///
    /// * [`VfsError::CrossVolume`] if the two paths are on different mounts.
    /// * [`VfsError::IsADirectory`] if `existing` names a directory.
    /// * [`VfsError::TooManyLinks`] if the format's name count would
    ///   overflow.
    /// * [`VfsError::NotSupported`] if the format holds one name per node.
    /// * Otherwise as for [`Vfs::create_via`].
    pub fn link_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        existing: &Path,
        link: &Path,
        fs: &mut F,
        existing_link: FinalLink,
    ) -> Result<(), VfsError> {
        let (mount, existing_rem, link_rem) = self.delegate_pair_context(cred, existing, link)?;
        DelegatedFs::new(fs, mount).link(cred, &existing_rem, &link_rem, existing_link)
    }

    /// Per-inode counterpart of [`Vfs::link_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::link_via`].
    pub fn link_via_secured<F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        existing: &Path,
        link: &Path,
        fs: &mut F,
        existing_link: FinalLink,
    ) -> Result<(), VfsError> {
        let (mount, existing_rem, link_rem) = self.delegate_pair_context(cred, existing, link)?;
        DelegatedFs::new_secured(fs, mount).link(cred, &existing_rem, &link_rem, existing_link)
    }

    /// Create a directory under a driver-backed mount, delegating to `fs`.
    ///
    /// See [`Vfs::create_via`] for the resolution and permission model,
    /// except that `mkdir` does **not** follow a final symbolic link: making
    /// a directory over an existing link is [`VfsError::AlreadyExists`].
    ///
    /// # Errors
    ///
    /// As for [`Vfs::create_via`].
    pub fn mkdir_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new(fs, mount).create(
            cred,
            &remainder,
            DriverNodeKind::Directory,
            FinalLink::Keep,
        )
    }

    /// Write `data` into a file under a driver-backed mount starting at
    /// `offset`, delegating to `fs` and returning the bytes written.
    ///
    /// See [`Vfs::create_via`] for the resolution and permission model. A
    /// final symbolic link is followed, so the bytes reach the target, and
    /// the write permission this VFS asks for on a write's parent applies to
    /// the directory the target actually lives in.
    ///
    /// # Errors
    ///
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::IsADirectory`] if `path` names a directory.
    /// * [`VfsError::NotFound`], [`VfsError::PermissionDenied`],
    ///   [`VfsError::NotADirectory`], [`VfsError::InvalidPath`], or
    ///   [`VfsError::Io`].
    pub fn write_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new(fs, mount).write(cred, &remainder, offset, data)
    }

    /// Set the length of a file under a driver-backed mount, delegating to
    /// `fs`.
    ///
    /// See [`Vfs::write_via`] for the resolution and permission model.
    ///
    /// # Errors
    ///
    /// As for [`Vfs::write_via`].
    pub fn truncate_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        size: u64,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new(fs, mount).truncate(cred, &remainder, size)
    }

    /// Unlink a child under a driver-backed mount, delegating to `fs`.
    ///
    /// See [`Vfs::create_via`] for the resolution and permission model.
    /// With `dir_only` the removal succeeds only when the name is an
    /// (empty) directory — the atomic `rmdir` posture, decided here under
    /// the mount's lock, never by a caller-side stat.
    ///
    /// # Errors
    ///
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::NotEmpty`] if `path` is a non-empty directory.
    /// * [`VfsError::NotADirectory`] if `dir_only` and `path` is not a
    ///   directory.
    /// * [`VfsError::NotFound`], [`VfsError::PermissionDenied`],
    ///   [`VfsError::InvalidPath`], or [`VfsError::Io`].
    pub fn remove_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        dir_only: bool,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new(fs, mount).remove(cred, &remainder, dir_only)
    }

    // -----------------------------------------------------------------
    // Per-inode (`FilesystemSecurity`) variants.
    //
    // These mirror the `*_via` methods above but judge every delegated
    // node against the driver's *own* stored record rather than the
    // uniform mount-point template. The kernel
    // host calls these for a driver such as `arxfs` that stores full
    // per-inode ownership, mode, ACL, and capability gate; the
    // template-based `*_via` methods remain for drivers (e.g. FAT) that
    // store no per-file owner. The driver still makes no permission
    // decision — the VFS applies the model to the record it reports.
    // -----------------------------------------------------------------

    /// Per-inode counterpart of [`Vfs::read_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::read_via`].
    pub fn read_via_secured<F: FilesystemRead + FilesystemSecurity + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new_secured(fs, mount).read(cred, &remainder, offset, buf)
    }

    /// Per-inode counterpart of [`Vfs::list_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::list_via`].
    pub fn list_via_secured<F: FilesystemRead + FilesystemSecurity + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        final_link: FinalLink,
    ) -> Result<Vec<DelegatedEntry>, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new_secured(fs, mount).list(cred, &remainder, final_link)
    }

    /// Per-inode counterpart of [`Vfs::stat_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::stat_via`].
    pub fn stat_via_secured<F: FilesystemRead + FilesystemSecurity + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        final_link: FinalLink,
    ) -> Result<DelegatedInfo, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new_secured(fs, mount).stat(cred, &remainder, final_link)
    }

    /// Per-inode counterpart of [`Vfs::readlink_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::readlink_via`].
    pub fn readlink_via_secured<F: FilesystemRead + FilesystemSecurity + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
    ) -> Result<String, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new_secured(fs, mount).read_link(cred, &remainder)
    }

    /// Per-inode counterpart of [`Vfs::create_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::create_via`].
    pub fn create_via_secured<F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).create(
            cred,
            &remainder,
            DriverNodeKind::RegularFile,
            FinalLink::Follow,
        )
    }

    /// Per-inode counterpart of [`Vfs::symlink_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::symlink_via`].
    pub fn symlink_via_secured<
        F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized,
    >(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        target: &str,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).create_link(cred, &remainder, target)
    }

    /// Per-inode counterpart of [`Vfs::mkdir_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::create_via`].
    pub fn mkdir_via_secured<F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).create(
            cred,
            &remainder,
            DriverNodeKind::Directory,
            FinalLink::Keep,
        )
    }

    /// Per-inode counterpart of [`Vfs::write_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::write_via`].
    pub fn write_via_secured<F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).write(cred, &remainder, offset, data)
    }

    /// Per-inode counterpart of [`Vfs::truncate_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::write_via`].
    pub fn truncate_via_secured<
        F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized,
    >(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        size: u64,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).truncate(cred, &remainder, size)
    }

    /// Per-inode counterpart of [`Vfs::remove_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::remove_via`].
    pub fn remove_via_secured<F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        dir_only: bool,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).remove(cred, &remainder, dir_only)
    }

    /// Set the permission bits of the node at `path` to `mode` under a
    /// driver-backed mount, delegating to `fs` (the `chmod(2)` shape).
    ///
    /// Only the node's owner may change its mode, and a `required_cap`
    /// gate on the node is honoured (see
    /// [`DelegatedFs::set_mode`](super::delegate::DelegatedFs::set_mode)).
    /// Secured-only: a uniform-template mount stores no per-node record
    /// for the change to land in, so no unsecured twin exists.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::PermissionDenied`] if the caller is not the node's
    ///   owner or fails a search/capability check on the way to it.
    /// * [`VfsError::NotADirectory`] or [`VfsError::Io`].
    pub fn set_mode_via_secured<F: FilesystemRead + FilesystemSecurity + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        mode: u32,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).set_mode(cred, &remainder, mode)
    }

    /// Set the owning user and/or group of the node at `path` under a
    /// driver-backed mount, delegating to `fs` (the `chown(2)` /
    /// `chgrp(2)` shape).
    ///
    /// The per-inode authority rule is
    /// [`DelegatedFs::set_owner`](super::delegate::DelegatedFs::set_owner)'s:
    /// reassigning the uid, or setting a gid the caller is not a member of,
    /// requires [`tairix_abi::CapabilityId::FS_CHOWN`]; otherwise only the
    /// owner may change the group, and only to a group they belong to. Any
    /// change clears the set-*id* bits. Secured-only, like `set_mode`: a
    /// uniform-template mount stores no per-node ownership record for the
    /// change to land in.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::PermissionDenied`] if the caller lacks the authority
    ///   for the requested change or fails a search/capability check on the
    ///   way to the node.
    /// * [`VfsError::NotADirectory`] or [`VfsError::Io`].
    pub fn set_owner_via_secured<F: FilesystemRead + FilesystemSecurity + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        uid: u32,
        gid: u32,
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).set_owner(cred, &remainder, uid, gid)
    }

    /// Read one extended attribute of the node at `path` under a
    /// driver-backed mount, delegating to `fs` (the `getxattr(2)` shape).
    ///
    /// The per-inode rule is
    /// [`DelegatedFs::get_attr`](super::delegate::DelegatedFs::get_attr)'s:
    /// read permission on the node, privileged namespaces refused,
    /// `required_cap` honoured. Secured-only, like `set_mode`: attribute
    /// storage is a per-inode record no uniform-template mount carries.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * As [`DelegatedFs::get_attr`](super::delegate::DelegatedFs::get_attr).
    pub fn get_attr_via_secured<
        F: FilesystemRead + FilesystemSecurity + FilesystemAttrs + ?Sized,
    >(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<usize, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new_secured(fs, mount).get_attr(cred, &remainder, key, value_out)
    }

    /// Set one extended attribute of the node at `path` under a writable
    /// driver-backed mount, delegating to `fs` (the `setxattr(2)` shape).
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * As [`DelegatedFs::set_attr`](super::delegate::DelegatedFs::set_attr).
    pub fn set_attr_via_secured<
        F: FilesystemRead + FilesystemSecurity + FilesystemAttrs + ?Sized,
    >(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).set_attr(cred, &remainder, key, value)
    }

    /// Yield the `index`-th visible extended-attribute key of the node at
    /// `path` under a driver-backed mount, delegating to `fs`.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * As [`DelegatedFs::list_attr`](super::delegate::DelegatedFs::list_attr).
    pub fn list_attr_via_secured<
        F: FilesystemRead + FilesystemSecurity + FilesystemAttrs + ?Sized,
    >(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, false)?;
        DelegatedFs::new_secured(fs, mount).list_attr(cred, &remainder, index, key_out)
    }

    /// Remove one extended attribute of the node at `path` under a writable
    /// driver-backed mount, delegating to `fs` (the `removexattr(2)` shape).
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers `path`.
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * As [`DelegatedFs::remove_attr`](super::delegate::DelegatedFs::remove_attr).
    pub fn remove_attr_via_secured<
        F: FilesystemRead + FilesystemSecurity + FilesystemAttrs + ?Sized,
    >(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        fs: &mut F,
        key: &[u8],
    ) -> Result<(), VfsError> {
        let (mount, remainder) = self.delegate_context(cred, path, true)?;
        DelegatedFs::new_secured(fs, mount).remove_attr(cred, &remainder, key)
    }

    /// Move `src` to `dst` under a driver-backed mount, delegating to `fs`.
    ///
    /// Both paths must lie under the *same* writable driver-backed mount
    /// (rename never crosses mounts); see [`Vfs::create_via`] for the
    /// resolution and permission model.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no driver-backed mount covers a path.
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::InvalidPath`] if `src` and `dst` are on different
    ///   mounts.
    /// * [`VfsError::NotEmpty`], [`VfsError::PermissionDenied`],
    ///   [`VfsError::NotADirectory`], or [`VfsError::Io`].
    pub fn rename_via<F: FilesystemRead + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        src: &Path,
        dst: &Path,
        fs: &mut F,
    ) -> Result<(), VfsError> {
        let (mount, src_rem, dst_rem) = self.delegate_pair_context(cred, src, dst)?;
        DelegatedFs::new(fs, mount).rename(cred, &src_rem, &dst_rem)
    }

    /// Per-inode counterpart of [`Vfs::rename_via`].
    ///
    /// # Errors
    ///
    /// As [`Vfs::rename_via`].
    pub fn rename_via_secured<F: FilesystemRead + FilesystemSecurity + FilesystemWrite + ?Sized>(
        &self,
        cred: &Credentials<'_>,
        src: &Path,
        dst: &Path,
        fs: &mut F,
    ) -> Result<(), VfsError> {
        let (mount, src_rem, dst_rem) = self.delegate_pair_context(cred, src, dst)?;
        DelegatedFs::new_secured(fs, mount).rename(cred, &src_rem, &dst_rem)
    }

    /// Resolve the driver-backed mount covering *both* `first` and `second`
    /// for a two-path mutation, returning what that mount projects and the
    /// components of each path below the shared mount point.
    ///
    /// Both paths must be covered by the same writable driver-backed mount.
    /// The two callers need that for the same reason — a rename preserves
    /// the node's identity and a hard link is a second directory entry for
    /// one inode, and neither can span two independent backings — so a pair
    /// that crosses mounts is refused with [`VfsError::CrossVolume`], the
    /// dedicated refusal a mover falls back to copy-then-remove on.
    fn delegate_pair_context(
        &self,
        cred: &Credentials<'_>,
        first: &Path,
        second: &Path,
    ) -> Result<(MountProjection, Vec<String>, Vec<String>), VfsError> {
        // Copy the mount facts out under a short read lock; nothing below
        // holds the guard.
        let (mount_depth, mount_path, subtree, mount_template) = {
            let mounts = self.mounts.read();
            let first_mount = mounts.resolve(first);
            let second_mount = mounts.resolve(second);
            if first_mount.backing().is_none() || second_mount.backing().is_none() {
                return Err(VfsError::NotFound);
            }
            if first_mount.is_read_only() || second_mount.is_read_only() {
                return Err(VfsError::ReadOnly);
            }
            if first_mount.path() != second_mount.path() {
                return Err(VfsError::CrossVolume);
            }
            (
                first_mount.path().depth(),
                first_mount.path().clone(),
                first_mount.backing_subtree().to_vec(),
                first_mount.template().cloned(),
            )
        };
        let template = self.mount_point_template(cred, &mount_path, mount_template)?;
        let below = |path: &Path| path.components()[mount_depth..].to_vec();
        Ok((
            MountProjection { template, subtree },
            below(first),
            below(second),
        ))
    }

    /// Resolve the driver-backed mount covering `path`, returning what that
    /// mount projects — the permission template for delegated nodes and its
    /// root on the backing volume — and the path components below the mount
    /// point.
    ///
    /// Walking to the mount point through [`Vfs::resolve`] authorises
    /// search permission on every ancestor directory; the mount point's own
    /// metadata becomes the template, and search permission on the mount
    /// point itself is enforced by the delegated walk (it is the template's
    /// `Execute` bit).
    ///
    /// The mount's `backing_subtree` stays a property of the *mount* rather
    /// than being folded into the returned components: it is the floor the
    /// delegated walk clamps to, so a link stored inside a sub-mount cannot
    /// resolve to a node outside what the mount projects.
    fn delegate_context(
        &self,
        cred: &Credentials<'_>,
        path: &Path,
        require_writable: bool,
    ) -> Result<(MountProjection, Vec<String>), VfsError> {
        // Copy the mount facts out under a short read lock; nothing below
        // holds the guard.
        let (mount_depth, mount_path, subtree, mount_template) = {
            let mounts = self.mounts.read();
            let mount = mounts.resolve(path);
            if mount.backing().is_none() {
                return Err(VfsError::NotFound);
            }
            if require_writable && mount.is_read_only() {
                return Err(VfsError::ReadOnly);
            }
            (
                mount.path().depth(),
                mount.path().clone(),
                mount.backing_subtree().to_vec(),
                mount.template().cloned(),
            )
        };
        let template = self.mount_point_template(cred, &mount_path, mount_template)?;
        let remainder = path.components()[mount_depth..].to_vec();
        Ok((MountProjection { template, subtree }, remainder))
    }

    /// The permission template the delegated walk applies at and below a
    /// mount point.
    ///
    /// A boot-layout mount's template is its mount-point node in the
    /// in-RAM tree, resolved with search authorisation on every ancestor.
    /// A **runtime** mount (a hotplug volume under `/Storage/<name>`) has
    /// no node at its mount point, so its template travels with the mount
    /// itself; the ancestors that do exist are still walked with search
    /// authorisation — including the mount point's parent — so a caller
    /// with no reach into `/Storage` cannot reach a volume mounted there.
    fn mount_point_template(
        &self,
        cred: &Credentials<'_>,
        mount_path: &Path,
        mount_template: Option<Metadata>,
    ) -> Result<Metadata, VfsError> {
        if let Some(template) = mount_template {
            let parent = mount_path.parent().ok_or(VfsError::NotFound)?;
            let node = self.resolve(cred, &parent)?;
            let NodeKind::Directory(_) = &node.kind else {
                return Err(VfsError::NotADirectory);
            };
            node.meta.authorize(cred, Access::Execute)?;
            Ok(template)
        } else {
            let node = self.resolve(cred, mount_path)?;
            let NodeKind::Directory(_) = &node.kind else {
                return Err(VfsError::NotADirectory);
            };
            Ok(node.meta.clone())
        }
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
        if self.mounts.read().is_read_only(path) {
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
    /// With `dir_only` the removal succeeds only when the name is an
    /// (empty) directory — the atomic `rmdir` posture, decided here in the
    /// same lookup that removes the entry, never by a caller-side stat a
    /// concurrent rename could invalidate.
    ///
    /// # Errors
    ///
    /// * [`VfsError::ReadOnly`] if the covering mount is read-only.
    /// * [`VfsError::NotEmpty`] if `path` is a non-empty directory.
    /// * [`VfsError::NotADirectory`] if `dir_only` and `path` is not a
    ///   directory.
    /// * [`VfsError::PermissionDenied`] if the caller lacks write
    ///   permission on the parent directory.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`], or
    ///   [`VfsError::InvalidPath`].
    pub fn remove(
        &mut self,
        cred: &Credentials<'_>,
        path: &Path,
        dir_only: bool,
    ) -> Result<(), VfsError> {
        let name = path.file_name().ok_or(VfsError::InvalidPath)?.to_string();
        let parent_path = path.parent().ok_or(VfsError::InvalidPath)?;
        // Removal mutates the parent directory, so the parent's covering
        // mount governs writability (e.g. removing the `/System/Logs`
        // mount point is forbidden by the read-only `/System`).
        if self.mounts.read().is_read_only(&parent_path) {
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
            Some(Node {
                kind: NodeKind::Directory(_),
                ..
            }) => {}
            Some(_) if dir_only => return Err(VfsError::NotADirectory),
            Some(_) => {}
        }
        entries.remove(&name);
        Ok(())
    }

    /// Set (or clear) the capability gate on the inode at `path`. Only the inode's owner may change it.
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
        if self.mounts.read().is_read_only(path) {
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
    /// read-only refusal, parent resolution, parent write permission, and the
    /// no-clobber existence check.
    fn create(&mut self, cred: &Credentials<'_>, path: &Path, child: Node) -> Result<(), VfsError> {
        let name = path.file_name().ok_or(VfsError::InvalidPath)?.to_string();
        let parent_path = path.parent().ok_or(VfsError::InvalidPath)?;
        // Creation mutates the parent directory, so the parent's covering
        // mount governs writability.
        if self.mounts.read().is_read_only(&parent_path) {
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
