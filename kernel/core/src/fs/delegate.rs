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

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::driver::filesystem::{
    FilesystemAttrs, FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeInfo,
    NodeKind, NodeTimes,
};
use tairix_abi::driver::DriverError;
use tairix_abi::fs::{
    OpenFlags, RealpathMode, FS_GROUP_EXEC_BIT, FS_OWNER_UNCHANGED, FS_SETGID_BIT, FS_SETUID_BIT,
    FS_SYMLINK_MAX,
};
use tairix_abi::CapabilityId;
use tairix_fsmeta::{AttrKey, NamespaceAccess, KEY_MAX};
use tairix_kernel_sec::{GroupId, UserId};

use super::path::{parse_link_target, TargetStep, MAX_COMPONENT_LEN, MAX_PATH_COMPONENTS};
use super::perm::{Access, Credentials, Metadata};
use super::VfsError;

/// Maximum symbolic links a single resolution may follow.
///
/// The conventional Unix `MAXSYMLINKS`. It is a fail-closed *security*
/// bound on untrusted on-disk structure, not a capacity: a cycle is refused
/// with [`VfsError::LinkLoop`] after this many hops rather than walked until
/// the kernel runs out of stack.
pub const SYMLINK_HOP_MAX: usize = 40;

/// Maximum path steps a single resolution may consume.
///
/// Derived from the two bounds that produce steps — a caller's own path is
/// at most [`MAX_PATH_COMPONENTS`], and each of the [`SYMLINK_HOP_MAX`]
/// permitted hops may splice at most that many more — so the total work of
/// one resolution is bounded even when every component is a link.
pub const MAX_RESOLVE_STEPS: usize = MAX_PATH_COMPONENTS * (SYMLINK_HOP_MAX + 1);

/// Whether a symbolic link in a path's **final** position is followed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FinalLink {
    /// Resolve through it to whatever it names (the default posture).
    Follow,
    /// Stop at the link itself — the `lstat` / `readlink` / `unlink`
    /// posture.
    Keep,
}

impl FinalLink {
    /// The posture an open descriptor's flags fix.
    ///
    /// [`OpenFlags::NO_FOLLOW`] makes the descriptor name the link itself,
    /// so every resolution later performed on that descriptor's behalf has
    /// to keep the link rather than report what it points at. Deriving it
    /// here, once, is what stops an open and a subsequent stat through the
    /// same handle from disagreeing.
    #[must_use]
    pub const fn for_open(flags: OpenFlags) -> Self {
        if flags.is_no_follow() {
            Self::Keep
        } else {
            Self::Follow
        }
    }
}

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
    /// How many directory entries name this node, as the driver read it
    /// from the format. Carried through unchanged: the VFS could not count
    /// names without walking every directory on the volume, so the format's
    /// own record is the only honest source.
    pub nlink: u32,
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

/// One entry of a delegated directory listing: the child's name, the
/// structural metadata the listing driver reported for it, and the driver's
/// node number for it.
///
/// The node number is what lets a listing consumer tell a second *name* for
/// one node from a second node — the hard-link deduplication `du` needs —
/// without re-`stat`ing every child by path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedEntry {
    /// The driver's stable node number for the child (its [`NodeId`] raw
    /// value), the same number [`DelegatedInfo::node`] reports for it.
    pub node: u64,
    /// The child's structural metadata (kind, name count, sizes, times), as
    /// the listing driver read it.
    pub info: NodeInfo,
    /// The child's name (a single component, never `.`/`..`).
    pub name: String,
}

/// One node a path walk stands on, with everything the walk learned about
/// it: the driver's node id, the name that led there, and its structural and
/// permission metadata.
///
/// Carrying the metadata makes `..` free — ascending re-reads nothing — and
/// carrying the name is what lets a completed walk report the *place* its
/// final component occupies, which every `(dir, name)`-keyed driver mutation
/// needs.
struct Walked {
    node: NodeId,
    /// The name this node was looked up under, or `None` for the mount root,
    /// which no directory holds.
    name: Option<String>,
    info: NodeInfo,
    meta: Metadata,
}

/// The place a path's final name occupies.
///
/// The driver mutation surface is keyed `(dir, name)` rather than by node,
/// so a walk has to report where a name lives and not only what currently
/// lives there. Under [`FinalLink::Follow`] the place is the place of
/// whatever a final symbolic link *names*, which is what makes a write, a
/// truncate, or an `O_CREAT` act on the target instead of on the link.
struct Place {
    /// The directory holding the name.
    parent: NodeId,
    /// That directory's own permission metadata, for a caller that must
    /// authorise a change to its entries.
    parent_meta: Metadata,
    /// The final name within `parent`.
    name: String,
    /// The node occupying the place, looked up by the walk itself, or `None`
    /// when the name is vacant.
    found: Option<(NodeId, NodeInfo, Metadata)>,
}

/// What a mount projects into the VFS path space: the permission template
/// its delegated nodes are judged against, and the path *on the backing
/// volume* at which it is rooted.
///
/// A whole-volume mount is rooted at the driver's own root directory (an
/// empty [`subtree`](Self::subtree)); a **sub-mount** projects only a
/// subtree of a larger volume. That subtree's root, never the driver root,
/// is the floor a walk clamps `..` and an absolute link target to — so a
/// link stored inside the mount cannot resolve to a node the mount does not
/// project, and every node a walk reaches has a path under the mount point.
///
/// The two facts travel together because a walk needs both and neither is
/// derivable from the path: passing them as one value is what stops a call
/// site from resolving against the driver root by omission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountProjection {
    /// The metadata delegated nodes are judged against under [`Uniform`],
    /// and the mount point's own record under [`PerInode`].
    pub template: Metadata,
    /// The mount's root path on the backing volume, empty for a mount
    /// rooted at the driver's own root.
    pub subtree: Vec<String>,
}

/// Whether a descent may run out of tree before it runs out of steps.
///
/// A vacant *final* name is always reported rather than refused — it is the
/// place a create acts on — so this governs only an **ancestor** that does
/// not exist. Every ordinary operation refuses one: a write cannot land in a
/// directory that is not there. Canonicalisation is the one caller that may
/// ask for the rest of the path anyway, because naming a path that does not
/// exist yet is what `realpath -m` is for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MissingSteps {
    /// A missing ancestor is [`VfsError::NotFound`].
    Refused,
    /// A missing ancestor ends resolution; the steps below it are carried
    /// through unresolved.
    Carried,
}

/// Where a descent ended.
enum Descent {
    /// Every step resolved to a real node. The stack the walk stands on,
    /// floor first and never empty.
    Landed(Vec<Walked>),
    /// The walk ran out of tree: the stack it reached, and the names below
    /// it that nothing occupies. Exactly one name unless the descent was
    /// permitted to carry a missing ancestor.
    Absent {
        stack: Vec<Walked>,
        names: Vec<String>,
    },
}

/// Where a path walk ended.
enum Walk {
    /// The path named the mount point itself, which no directory holds — so
    /// it is not a place anything can be created at or removed from.
    Root {
        node: NodeId,
        info: NodeInfo,
        meta: Metadata,
    },
    /// The path's final name lives in a directory.
    Leaf(Place),
}

/// Maps a [`DriverError`] onto the VFS error type.
///
/// Every structural refusal a driver can report carries its own code, so
/// this mapping is total and the same wherever a driver call is made: a
/// taken name is [`VfsError::AlreadyExists`] whichever operation met it, a
/// populated directory is [`VfsError::NotEmpty`], and a move that would make
/// a directory its own descendant is [`VfsError::DirectoryCycle`]. Only an
/// unrecoverable backing fault reaches [`VfsError::Io`].
const fn map_driver_error(error: DriverError) -> VfsError {
    match error {
        DriverError::NotFound => VfsError::NotFound,
        DriverError::Unsupported => VfsError::NotADirectory,
        DriverError::BufferTooSmall | DriverError::LengthOutOfRange => VfsError::InvalidPath,
        DriverError::AlreadyExists => VfsError::AlreadyExists,
        DriverError::DirectoryNotEmpty => VfsError::NotEmpty,
        DriverError::DirectoryCycle => VfsError::DirectoryCycle,
        // A fixed on-disk count, exhausted: reported as itself so a caller
        // is not told to free space that would not help.
        DriverError::TooManyLinks => VfsError::TooManyLinks,
        _ => VfsError::Io,
    }
}

/// Maps a [`DriverError`] from a link operation onto the VFS error type.
///
/// The one code whose meaning is genuinely surface-specific: on the link
/// surface [`DriverError::Unsupported`] is "this format stores no such
/// object", the permanent limit no retry clears, where on a path walk it is
/// "a file was used as a directory". Everything else maps as
/// [`map_driver_error`].
const fn map_link_error(error: DriverError) -> VfsError {
    match error {
        DriverError::Unsupported => VfsError::NotSupported,
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
/// point itself, i.e. the root the [`MountProjection`] describes.
pub struct DelegatedFs<'fs, R: FilesystemRead + ?Sized, P = Uniform> {
    fs: &'fs mut R,
    mount: MountProjection,
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
    /// Bind `fs` to what its mount projects; every node in the delegated
    /// subtree is judged against the projection's one template.
    #[must_use]
    pub fn new(fs: &'fs mut R, mount: MountProjection) -> Self {
        Self {
            fs,
            mount,
            _policy: PhantomData,
        }
    }
}

impl<'fs, R: FilesystemRead + FilesystemSecurity + ?Sized> DelegatedFs<'fs, R, PerInode> {
    /// Bind `fs` so each node is judged against its *own* stored
    /// record (read through [`FilesystemSecurity`]) rather than the mount
    /// template.
    ///
    /// The projection's template is retained only as the metadata of the
    /// mount point in the in-RAM tree; the delegated walk consults the
    /// driver's per-inode record for every node, including the projected
    /// root.
    #[must_use]
    pub fn new_secured(fs: &'fs mut R, mount: MountProjection) -> Self {
        Self {
            fs,
            mount,
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

    /// Set the owning user and/or group of the node at `components` (the
    /// `chown(2)` / `chgrp(2)` shape), leaving its mode's permission triads,
    /// ACL, and capability gate otherwise untouched.
    ///
    /// `uid` / `gid` of [`FS_OWNER_UNCHANGED`] leave that field as it is; a
    /// call changing neither is a no-op that touches no state. The authority
    /// rule is stricter than a mode change:
    ///
    /// * Reassigning the **uid**, or setting a **gid** the caller is not a
    ///   member of, requires [`CapabilityId::FS_CHOWN`] (the Unix `CAP_CHOWN`
    ///   privilege). Without it either is refused.
    /// * Without the capability, only the node's **owner** may change the
    ///   group, and only to a group the caller already belongs to (its
    ///   primary or a supplementary group) — the unprivileged `chgrp`.
    /// * A `required_cap` gate on the node is honoured first, exactly as for
    ///   a mode change (the gate guards *all* access to the node).
    ///
    /// On any actual change the set-user-ID bit is cleared, and the
    /// set-group-ID bit is cleared for a group-executable node (a set-group-ID
    /// directory keeps it), so a reassigned file can never carry a stale
    /// set-*id* escalation. Implemented only for the per-inode policy: a
    /// uniform-template mount has no stored per-node record to change.
    ///
    /// # Errors
    ///
    /// [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    /// [`VfsError::PermissionDenied`], or [`VfsError::Io`].
    pub fn set_owner(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        uid: u32,
        gid: u32,
    ) -> Result<(), VfsError> {
        let (node, _info, meta) = self.resolve(cred, components)?;
        if let Some(cap) = meta.required_cap {
            if !cred.caps.holds(cap) {
                return Err(VfsError::PermissionDenied);
            }
        }

        let mut sec = self.fs.security(node).map_err(map_driver_error)?;
        let owner = if uid == FS_OWNER_UNCHANGED {
            sec.uid
        } else {
            uid
        };
        let group = if gid == FS_OWNER_UNCHANGED {
            sec.gid
        } else {
            gid
        };
        let owner_changed = owner != sec.uid;
        let group_changed = group != sec.gid;
        if !owner_changed && !group_changed {
            return Ok(());
        }

        let privileged = cred.caps.holds(CapabilityId::FS_CHOWN);
        if !privileged {
            // Reassigning the owner is always privileged.
            if owner_changed {
                return Err(VfsError::PermissionDenied);
            }
            // Group change is the owner's, and only to a group they belong to.
            if group_changed && !(UserId(sec.uid) == cred.uid && cred.is_in_group(GroupId(group))) {
                return Err(VfsError::PermissionDenied);
            }
        }

        sec.uid = owner;
        sec.gid = group;
        // Strip set-*id* bits so a reassigned node cannot become an
        // escalation: always the setuid bit; the setgid bit only for a
        // group-executable node (a setgid directory keeps it).
        sec.mode &= !FS_SETUID_BIT;
        if sec.mode & FS_GROUP_EXEC_BIT != 0 {
            sec.mode &= !FS_SETGID_BIT;
        }
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
        self.resolve_final(cred, components, FinalLink::Follow)
    }

    /// Resolve `components` to the node they name, choosing whether a
    /// symbolic link in the **final** position is followed.
    ///
    /// The [`Self::walk`] view for a caller that wants the node and nothing
    /// else: a name nothing occupies is [`VfsError::NotFound`] here, which
    /// is what every read operation owes its caller.
    ///
    /// # Errors
    ///
    /// [`VfsError::NotFound`] if the final name is vacant; otherwise as
    /// [`Self::walk`].
    fn resolve_final(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        final_link: FinalLink,
    ) -> Result<(NodeId, NodeInfo, Metadata), VfsError> {
        match self.walk(cred, components, final_link)? {
            Walk::Root { node, info, meta } => Ok((node, info, meta)),
            Walk::Leaf(place) => place.found.ok_or(VfsError::NotFound),
        }
    }

    /// Walk `components` from the driver root, enforcing search (execute)
    /// permission on every directory descended into, and report the
    /// [`Place`] the final name occupies.
    ///
    /// Links in every position but the last are always followed: such a
    /// component is being used as a directory, so what matters is what it
    /// names. `final_link` decides the last one — [`FinalLink::Keep`] is the
    /// `lstat`/`readlink`/`unlink` posture, where the call is about the link
    /// itself.
    ///
    /// `..` is applied to the **walked stack** rather than by collapsing
    /// text beforehand, so it names the directory this resolution actually
    /// came through, not one a link's spelling suggests. The stack starts at
    /// the root the mount projects and `..` never pops past it, so no chain
    /// of them escapes what the mount projects (`/..` is `/`, as POSIX
    /// specifies).
    ///
    /// # Errors
    ///
    /// [`VfsError::LinkLoop`] if resolution exceeds [`SYMLINK_HOP_MAX`]
    /// hops or [`MAX_RESOLVE_STEPS`] steps; otherwise as the caller's own
    /// documented failures.
    fn walk(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        final_link: FinalLink,
    ) -> Result<Walk, VfsError> {
        let root = self.projected_root(cred)?;
        let pending = components
            .iter()
            .map(|c| TargetStep::Name(c.clone()))
            .collect();
        match self.walk_from(root, cred, pending, final_link, MissingSteps::Refused)? {
            Descent::Landed(stack) => Self::landed(stack),
            Descent::Absent { stack, mut names } => {
                // With a missing ancestor refused, the only name a descent
                // can report as absent is the final one, and that is the
                // place a create acts on.
                let (Some(name), Some(parent)) = (names.pop(), stack.last()) else {
                    return Err(VfsError::Io);
                };
                if !names.is_empty() {
                    return Err(VfsError::Io);
                }
                Ok(Walk::Leaf(Place {
                    parent: parent.node,
                    parent_meta: parent.meta.clone(),
                    name,
                    found: None,
                }))
            }
        }
    }

    /// Canonicalise `components`: the path, relative to the mount's own
    /// root, that names what they resolve to — with every symbolic link
    /// followed and every `..` applied to the nodes the walk really
    /// traversed.
    ///
    /// `mode` decides only how much of the path must exist. A component the
    /// walk could not resolve is carried through unchanged, so the answer
    /// still names the place the caller asked about; a `..` below the
    /// deepest existing node pops that carried tail, which is the only
    /// reading available where no node exists to ascend from.
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] when `mode` requires a component that does
    ///   not exist.
    /// * [`VfsError::PermissionDenied`] when the caller may not search a
    ///   directory the resolution passes through.
    /// * [`VfsError::LinkLoop`] for a cycle or an over-budget chain,
    ///   [`VfsError::NotADirectory`], [`VfsError::InvalidPath`], or
    ///   [`VfsError::Io`].
    pub fn canonicalize(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        mode: RealpathMode,
    ) -> Result<Vec<String>, VfsError> {
        let root = self.projected_root(cred)?;
        let pending = components
            .iter()
            .map(|c| TargetStep::Name(c.clone()))
            .collect();
        let missing = if mode.tolerates_missing_intermediate() {
            MissingSteps::Carried
        } else {
            MissingSteps::Refused
        };
        // Every reading resolves a final link: `readlink -f` on a link
        // reports what it names, never the link.
        let (stack, absent) =
            match self.walk_from(root, cred, pending, FinalLink::Follow, missing)? {
                Descent::Landed(stack) => (stack, Vec::new()),
                Descent::Absent { stack, names } => {
                    if !mode.tolerates_vacant_final() {
                        return Err(VfsError::NotFound);
                    }
                    (stack, names)
                }
            };
        // `stack[0]` is the mount's own root, which no directory holds and
        // which the caller's mount point already names. Every entry above it
        // was pushed with the name it was looked up under; a nameless one
        // would silently drop a component and answer with a path naming a
        // different node, so it fails closed instead.
        let mut spelled = Vec::with_capacity(stack.len() - 1 + absent.len());
        for walked in stack.into_iter().skip(1) {
            spelled.push(walked.name.ok_or(VfsError::Io)?);
        }
        spelled.extend(absent);
        Ok(spelled)
    }

    /// The [`Walk`] a landed descent reports.
    fn landed(mut stack: Vec<Walked>) -> Result<Walk, VfsError> {
        let Some(Walked {
            node,
            name,
            info,
            meta,
        }) = stack.pop()
        else {
            return Err(VfsError::Io);
        };
        match (name, stack.last()) {
            // Only `stack[0]` — the mount's own root — carries no name, and
            // no directory this mount projects holds it.
            (None, _) => Ok(Walk::Root { node, info, meta }),
            (Some(name), Some(parent)) => Ok(Walk::Leaf(Place {
                parent: parent.node,
                parent_meta: parent.meta.clone(),
                name,
                found: Some((node, info, meta)),
            })),
            // A named entry is only ever pushed on top of the directory it
            // was looked up in, so it always has a parent beneath it; fail
            // closed rather than invent one.
            (Some(_), None) => Err(VfsError::Io),
        }
    }

    /// The node the mount projects as its root: the driver's own root for a
    /// whole-volume mount, or the node the projection's subtree names for a
    /// sub-mount.
    ///
    /// The subtree is a path *on the backing volume*, so it is resolved from
    /// the driver root under the ordinary rules — search permission on every
    /// directory descended, links followed — and only then becomes the floor
    /// a caller's own walk clamps to.
    ///
    /// # Errors
    ///
    /// [`VfsError::NotFound`] if the subtree names nothing; otherwise as
    /// [`Self::walk_from`].
    fn projected_root(&mut self, cred: &Credentials<'_>) -> Result<Walked, VfsError> {
        let node = self.fs.root();
        let root = Walked {
            node,
            name: None,
            info: self.fs.node_info(node).map_err(map_driver_error)?,
            meta: P::metadata(self.fs, node, &self.mount.template)?,
        };
        if self.mount.subtree.is_empty() {
            return Ok(root);
        }
        let pending = self
            .mount
            .subtree
            .iter()
            .map(|c| TargetStep::Name(c.clone()))
            .collect();
        match self.walk_from(
            root,
            cred,
            pending,
            FinalLink::Follow,
            MissingSteps::Refused,
        )? {
            // The projected root is a *root* to everything above it: it is
            // not an entry of any directory this mount projects, so the name
            // it was found under is deliberately dropped.
            Descent::Landed(mut stack) => stack.pop().map_or(Err(VfsError::Io), |top| {
                Ok(Walked {
                    node: top.node,
                    name: None,
                    info: top.info,
                    meta: top.meta,
                })
            }),
            // A mount whose projected root does not exist projects nothing.
            Descent::Absent { .. } => Err(VfsError::NotFound),
        }
    }

    /// Walk `pending` from `root`, which becomes the walk's floor.
    ///
    /// See [`Self::walk`] for the resolution model; this is the half that
    /// takes the floor as a parameter, so the same loop resolves a mount's
    /// own subtree from the driver root and a caller's path from the mount's
    /// projected root.
    ///
    /// # Errors
    ///
    /// As [`Self::walk`].
    fn walk_from(
        &mut self,
        root: Walked,
        cred: &Credentials<'_>,
        mut pending: VecDeque<TargetStep>,
        final_link: FinalLink,
        missing: MissingSteps,
    ) -> Result<Descent, VfsError> {
        // Root-first, and never emptied: `stack[0]` is the floor, so
        // popping for `..` is clamped by construction. Each entry keeps
        // everything the walk learned about its node, so ascending costs no
        // second read and the final entry knows its own name.
        let mut stack = alloc::vec![root];
        // Names below the deepest existing node. Non-empty only once a
        // lookup has come up vacant, and then nothing above it is looked up:
        // a name in a directory that does not exist has nothing to resolve
        // against and no metadata to authorise.
        let mut absent: Vec<String> = Vec::new();

        let mut hops = 0usize;
        let mut steps = 0usize;

        while let Some(step) = pending.pop_front() {
            steps += 1;
            if steps > MAX_RESOLVE_STEPS {
                return Err(VfsError::LinkLoop);
            }
            let name = match step {
                TargetStep::Up => {
                    // Ascend. Search permission on the parent was already
                    // proven on the way down, so no second check is owed.
                    // A `..` below the deepest existing node pops that tail
                    // instead, and resolution resumes physically once the
                    // walk is back on real nodes.
                    if absent.pop().is_none() && stack.len() > 1 {
                        stack.pop();
                    }
                    continue;
                }
                TargetStep::Name(name) => name,
            };

            if !absent.is_empty() {
                absent.push(name);
                continue;
            }

            let Some(here) = stack.last() else {
                return Err(VfsError::Io);
            };
            let parent = here.node;
            if here.info.kind != NodeKind::Directory {
                return Err(VfsError::NotADirectory);
            }
            let parent_meta = here.meta.clone();
            parent_meta.authorize(cred, Access::Execute)?;

            let is_final = pending.is_empty();
            let child = match self.fs.lookup(parent, name.as_bytes()) {
                Ok(child) => child,
                // A vacant *final* name is still a place — it is what a
                // create acts on — so the walk reports it rather than
                // failing. A vacant name anywhere earlier is a genuinely
                // missing ancestor, reported unless the caller asked to be
                // told the rest of the path instead.
                Err(DriverError::NotFound) if is_final || missing == MissingSteps::Carried => {
                    absent.push(name);
                    continue;
                }
                Err(err) => return Err(map_driver_error(err)),
            };
            let child_info = self.fs.node_info(child).map_err(map_driver_error)?;
            let child_meta = P::metadata(self.fs, child, &self.mount.template)?;

            if child_info.kind == NodeKind::Symlink && !(is_final && final_link == FinalLink::Keep)
            {
                hops += 1;
                if hops > SYMLINK_HOP_MAX {
                    return Err(VfsError::LinkLoop);
                }
                let target = self.read_link_target(child, child_info.size)?;
                let parsed = parse_link_target(&target)?;
                // An absolute target restarts at the root; a relative one
                // continues from the link's own parent, which is where the
                // stack already stands (the link was never pushed).
                if parsed.is_absolute() {
                    stack.truncate(1);
                }
                for extra in parsed.steps().iter().rev() {
                    pending.push_front(extra.clone());
                }
                continue;
            }

            stack.push(Walked {
                node: child,
                name: Some(name),
                info: child_info,
                meta: child_meta,
            });
        }

        if absent.is_empty() {
            Ok(Descent::Landed(stack))
        } else {
            Ok(Descent::Absent {
                stack,
                names: absent,
            })
        }
    }

    /// Read a link's stored target, sized from the node's own reported
    /// length so nothing is truncated and no fixed buffer is guessed.
    fn read_link_target(&mut self, node: NodeId, size: u64) -> Result<String, VfsError> {
        let len = usize::try_from(size).map_err(|_| VfsError::InvalidPath)?;
        if len == 0 || len > FS_SYMLINK_MAX {
            return Err(VfsError::InvalidPath);
        }
        let mut buf = alloc::vec![0u8; len];
        let read = self.fs.read_link(node, &mut buf).map_err(map_link_error)?;
        if read != len {
            // The driver disagreed with the length its own `node_info`
            // reported; refuse rather than resolve a partial target.
            return Err(VfsError::Io);
        }
        String::from_utf8(buf).map_err(|_| VfsError::InvalidPath)
    }

    /// Report the structural metadata of the node at `components`, paired
    /// with the permission metadata that governs it. Like POSIX `stat`,
    /// this needs search permission on every intermediate directory but
    /// none on the target itself.
    ///
    /// `final_link` picks between the POSIX `stat` and `lstat` readings:
    /// [`FinalLink::Keep`] reports a final symbolic link itself — including
    /// a dangling one, which `Follow` reports as [`VfsError::NotFound`].
    ///
    /// # Errors
    ///
    /// [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    /// [`VfsError::PermissionDenied`], [`VfsError::LinkLoop`], or
    /// [`VfsError::Io`].
    pub fn stat(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        final_link: FinalLink,
    ) -> Result<DelegatedInfo, VfsError> {
        let (node, info, meta) = self.resolve_final(cred, components, final_link)?;
        Ok(DelegatedInfo {
            kind: info.kind,
            nlink: info.nlink,
            size: info.size,
            allocated: info.allocated,
            meta,
            node: node.raw(),
            times: info.times,
        })
    }

    /// Read the stored target of the symbolic link at `components`.
    ///
    /// The final component is never followed — the call is about the link
    /// itself — and the target comes back exactly as it was stored, still
    /// unresolved. Like POSIX `readlink` this needs search permission on
    /// every directory on the way to the link and none on the link.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if the final component is not a symbolic
    ///   link, or the bytes it stores are empty, over-long, or not UTF-8.
    /// * [`VfsError::NotSupported`] if the mounted format stores no links.
    /// * [`VfsError::NotFound`], [`VfsError::NotADirectory`],
    ///   [`VfsError::PermissionDenied`], [`VfsError::LinkLoop`], or
    ///   [`VfsError::Io`].
    pub fn read_link(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
    ) -> Result<String, VfsError> {
        let (node, info, _) = self.resolve_final(cred, components, FinalLink::Keep)?;
        if info.kind != NodeKind::Symlink {
            return Err(VfsError::InvalidPath);
        }
        self.read_link_target(node, info.size)
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
    /// stable on-disk order, each with the driver's node number and the
    /// structural [`NodeInfo`] (which carries the node's kind, name count,
    /// sizes, and timestamps) the driver reports for it.
    ///
    /// The kind, sizes, and identity come from the listing driver itself, so
    /// a caller never has to re-resolve each child by path — a child whose
    /// *path* is shadowed by another mount would otherwise be judged against
    /// the wrong volume, and on an uncached, authenticated volume every such
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
        final_link: FinalLink,
    ) -> Result<Vec<DelegatedEntry>, VfsError> {
        let (node, info, meta) = self.resolve_final(cred, components, final_link)?;
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
            entries.push(DelegatedEntry {
                node: entry.node.raw(),
                info: entry.info,
                name: name.to_string(),
            });
            cursor = entry.next_cursor;
        }
        Ok(entries)
    }
}

impl<F: FilesystemRead + FilesystemWrite + ?Sized, P: MetaPolicy<F>> DelegatedFs<'_, F, P> {
    /// Resolve the [`Place`] the leaf addressed by `components` occupies,
    /// authorising search on every ancestor and search + write on the
    /// directory that holds the name, judged against that directory's own
    /// metadata under the active [`MetaPolicy`].
    ///
    /// `final_link` picks whether a final symbolic link is resolved through.
    /// [`FinalLink::Follow`] makes this the place of what the link *names*,
    /// so a write or a truncate reaches the target as POSIX requires;
    /// [`FinalLink::Keep`] leaves it the link's own place, the posture
    /// `unlink`, `rename`, `mkdir`, and `symlink` need because the call is
    /// about the name as typed.
    ///
    /// Write permission is required on the holding directory. That is
    /// stricter than POSIX, which authorises a write against the file alone;
    /// it is this VFS's standing choice, and following a link only moves the
    /// requirement onto the directory the mutation really lands in. The
    /// authorisation is made *before* the occupant is inspected, so a caller
    /// that may not write the directory learns nothing about what the name
    /// holds.
    ///
    /// An empty `components` slice names the mount point itself and is
    /// rejected — the driver root cannot be the target of a mutation.
    fn place_for_write(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        final_link: FinalLink,
    ) -> Result<Place, VfsError> {
        match self.walk(cred, components, final_link)? {
            Walk::Root { .. } => Err(VfsError::InvalidPath),
            Walk::Leaf(place) => {
                place.parent_meta.authorize(cred, Access::Execute)?;
                place.parent_meta.authorize(cred, Access::Write)?;
                Ok(place)
            }
        }
    }

    /// Refuse a name-mutation whose occupant carries a capability gate the
    /// caller does not hold.
    ///
    /// Write permission on the holding directory is what authorises adding,
    /// removing, and renaming its entries — but a `required_cap` node is
    /// reachable only by a capability holder, and a principal who could
    /// unlink or rename it aside would defeat that gate without ever opening
    /// it: it could plant an ungated directory of the same name and have the
    /// gate's own service walk into it. The gate therefore guards the *name*
    /// as well as the node, exactly as it already guards a mode or ownership
    /// change.
    fn authorize_name_mutation(
        cred: &Credentials<'_>,
        occupant: Option<&Metadata>,
    ) -> Result<(), VfsError> {
        match occupant.and_then(|meta| meta.required_cap) {
            Some(cap) if !cred.caps.holds(cap) => Err(VfsError::PermissionDenied),
            _ => Ok(()),
        }
    }

    /// Create an empty child of `kind` at `components`.
    ///
    /// `final_link` is the posture the *caller's own* operation has for a
    /// final symbolic link, and the two POSIX creates differ: `mkdir` passes
    /// [`FinalLink::Keep`], so making a directory over an existing link is
    /// [`VfsError::AlreadyExists`], while `open` with `O_CREAT` passes
    /// [`FinalLink::Follow`], so creating through a *dangling* link creates
    /// the file the link names.
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
        final_link: FinalLink,
    ) -> Result<(), VfsError> {
        let place = self.place_for_write(cred, components, final_link)?;
        if place.found.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        let node = self
            .fs
            .create(place.parent, place.name.as_bytes(), kind)
            .map_err(map_driver_error)?;
        // The driver minted the node with its format's default record;
        // hand it to its creator before the create is observable, so the
        // caller is never locked out of a node it just made.
        P::stamp_creation(self.fs, node, cred)?;
        Ok(())
    }

    /// Create a symbolic link at `components` whose stored target is
    /// `target`.
    ///
    /// The target is **not** resolved here: it is data, so the only
    /// authority the call needs is the right to create a name in the link's
    /// own parent, and the link may legitimately dangle. Its *grammar* is
    /// still checked against the one link-target parser before anything is
    /// written, so a target this resolver could never walk is refused
    /// rather than stored.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `components` is empty or `target`
    ///   fails the link-target grammar.
    /// * [`VfsError::AlreadyExists`] if a child of that name already exists.
    /// * [`VfsError::NotSupported`] if the mounted format has no link object
    ///   type — it refuses rather than approximating one.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`],
    ///   [`VfsError::NotFound`] (a missing ancestor), or [`VfsError::Io`].
    pub fn create_link(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        target: &str,
    ) -> Result<(), VfsError> {
        // Caller-supplied bytes, checked before any state is touched.
        parse_link_target(target)?;
        // The new name is the name as typed: making a link over an existing
        // one replaces nothing, it is refused.
        let place = self.place_for_write(cred, components, FinalLink::Keep)?;
        if place.found.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        let node = self
            .fs
            .create_link(place.parent, place.name.as_bytes(), target.as_bytes())
            .map_err(map_link_error)?;
        // As for `create`: hand the node to its creator before the link is
        // observable, so a caller is never locked out of one it just made.
        P::stamp_creation(self.fs, node, cred)?;
        Ok(())
    }

    /// Add `link` as a second directory entry for the node `existing`
    /// already names — a hard link.
    ///
    /// `existing_link` is the posture for the **existing** name's final
    /// component: [`FinalLink::Keep`] is POSIX `link()` — the node that
    /// gains a name is the one the caller spelled, so a symbolic link
    /// planted on the way cannot name an object the caller never asked for —
    /// and [`FinalLink::Follow`] is `linkat(AT_SYMLINK_FOLLOW)`, which
    /// `ln -L` asks for. The **new** name is always kept: it is a name being
    /// created, and a create never replaces an existing one.
    ///
    /// A **directory** is refused here rather than only in each driver: the
    /// tree staying a tree is what makes the physical `..` walk well-defined,
    /// so it is a VFS invariant, not a per-format one.
    ///
    /// The new name is authorised exactly as a create in its own parent
    /// (search plus write), and the existing name is resolved under the
    /// caller's own identity like any other path. Nothing further is
    /// required of the caller against the node itself, and nothing further
    /// is granted: a second name confers no authority the first did not.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if either component list is empty.
    /// * [`VfsError::NotFound`] if `existing` resolves to nothing.
    /// * [`VfsError::IsADirectory`] if `existing` names a directory.
    /// * [`VfsError::AlreadyExists`] if `link` already names something.
    /// * [`VfsError::TooManyLinks`] if the format's per-node name count
    ///   would overflow.
    /// * [`VfsError::NotSupported`] if the format holds one name per node.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotADirectory`],
    ///   [`VfsError::LinkLoop`], or [`VfsError::Io`].
    pub fn link(
        &mut self,
        cred: &Credentials<'_>,
        existing: &[String],
        link: &[String],
        existing_link: FinalLink,
    ) -> Result<(), VfsError> {
        // Resolve the node that gains a name first: a refusal here must
        // leave the new name's directory untouched.
        let source = match self.walk(cred, existing, existing_link)? {
            Walk::Root { .. } => return Err(VfsError::InvalidPath),
            Walk::Leaf(place) => place,
        };
        let (node, info, _) = source.found.ok_or(VfsError::NotFound)?;
        match info.kind {
            NodeKind::RegularFile | NodeKind::Symlink => {}
            NodeKind::Directory => return Err(VfsError::IsADirectory),
        }
        let place = self.place_for_write(cred, link, FinalLink::Keep)?;
        if place.found.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        self.fs
            .link(place.parent, place.name.as_bytes(), node)
            .map_err(map_link_error)
    }

    /// Write `data` into the file at `components` starting at `offset`,
    /// returning the number of bytes written.
    ///
    /// A final symbolic link is **followed**: POSIX writes the target, not
    /// the link. Because the driver write surface is keyed `(dir, name)`, the
    /// pair the write is issued against is the *resolved* node's own parent
    /// and name, and the write permission this VFS asks for on a write's
    /// parent therefore applies to the directory the bytes really land in.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `components` is empty.
    /// * [`VfsError::IsADirectory`] if `components` names a directory.
    /// * [`VfsError::PermissionDenied`], [`VfsError::NotFound`],
    ///   [`VfsError::NotADirectory`], [`VfsError::LinkLoop`], or
    ///   [`VfsError::Io`].
    pub fn write(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, VfsError> {
        let place = self.place_for_write(cred, components, FinalLink::Follow)?;
        let (_, info, _) = place.found.ok_or(VfsError::NotFound)?;
        Self::deny_non_file(info.kind)?;
        self.fs
            .write_at(place.parent, place.name.as_bytes(), offset, data)
            .map_err(map_driver_error)
    }

    /// Set the length of the file at `components` to `size`.
    ///
    /// A final symbolic link is followed, exactly as for [`Self::write`].
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
        let place = self.place_for_write(cred, components, FinalLink::Follow)?;
        let (_, info, _) = place.found.ok_or(VfsError::NotFound)?;
        Self::deny_non_file(info.kind)?;
        self.fs
            .truncate(place.parent, place.name.as_bytes(), size)
            .map_err(map_driver_error)
    }

    /// Refuse a node whose bytes a write may not touch.
    ///
    /// A directory's entries are not a byte stream, and a link's content is a
    /// path — following it was the resolution's job, so a link reaching here
    /// would mean a write about to corrupt one.
    fn deny_non_file(kind: NodeKind) -> Result<(), VfsError> {
        match kind {
            NodeKind::RegularFile => Ok(()),
            NodeKind::Directory => Err(VfsError::IsADirectory),
            NodeKind::Symlink => Err(VfsError::LinkLoop),
        }
    }

    /// Unlink the child at `components`.
    ///
    /// A final symbolic link is **not** followed: POSIX unlinks the link, so
    /// removing one never touches what it names — including a link whose
    /// target is a non-empty directory.
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
    /// * [`VfsError::PermissionDenied`] — including when the child carries a
    ///   capability gate the caller does not hold: the gate guards the *name*
    ///   as well as the node, so write permission on the parent alone cannot
    ///   unlink a gated node aside.
    /// * [`VfsError::NotADirectory`] or [`VfsError::Io`].
    pub fn remove(
        &mut self,
        cred: &Credentials<'_>,
        components: &[String],
        dir_only: bool,
    ) -> Result<(), VfsError> {
        let place = self.place_for_write(cred, components, FinalLink::Keep)?;
        // Checked here so the walk's own NotFound and NotEmpty are answered
        // without a driver round trip; the driver still checks, and its
        // refusal carries the same class.
        let (node, info, meta) = place.found.ok_or(VfsError::NotFound)?;
        Self::authorize_name_mutation(cred, Some(&meta))?;
        if info.kind == NodeKind::Directory {
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
        self.fs
            .remove(place.parent, place.name.as_bytes())
            .map_err(map_driver_error)
    }

    /// Move the leaf at `src_components` to `dst_components` within the same
    /// delegated mount, preserving the node's identity and contents.
    ///
    /// Neither end follows a final symbolic link: POSIX renames the link
    /// itself and replaces a link at the destination, never what either
    /// names.
    ///
    /// Authorises search + write on both the source and destination parent
    /// directories; when a directory is moved to a *different* parent its
    /// `..` link is rewritten, so write permission on the moved directory
    /// itself is additionally required (POSIX). A capability gate on either
    /// end is honoured too, so a gated name cannot be moved aside or replaced
    /// by a caller that does not hold it — not even within one parent, where
    /// POSIX authorises nothing against the moved node. The structural move — the existence, kind-compatibility, empty-target, and
    /// directory-into-its-own-subtree checks — is performed by the driver.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if either path is empty (names the mount
    ///   point itself).
    /// * [`VfsError::NotFound`] if the source does not exist.
    /// * [`VfsError::NotEmpty`] if the destination is a non-empty directory.
    /// * [`VfsError::DirectoryCycle`] if the move would place a directory
    ///   inside its own subtree.
    /// * [`VfsError::NotADirectory`] on a kind-incompatible replacement.
    /// * [`VfsError::PermissionDenied`] — including when either end carries a
    ///   capability gate the caller does not hold.
    /// * [`VfsError::Io`].
    pub fn rename(
        &mut self,
        cred: &Credentials<'_>,
        src_components: &[String],
        dst_components: &[String],
    ) -> Result<(), VfsError> {
        let src = self.place_for_write(cred, src_components, FinalLink::Keep)?;
        let dst = self.place_for_write(cred, dst_components, FinalLink::Keep)?;
        let (_, src_info, src_meta) = src.found.ok_or(VfsError::NotFound)?;
        // Both ends move a gated name: the source loses it and a replaced
        // destination is destroyed by it.
        Self::authorize_name_mutation(cred, Some(&src_meta))?;
        Self::authorize_name_mutation(cred, dst.found.as_ref().map(|(_, _, meta)| meta))?;

        // A directory moved to a different parent has its `..` rewritten, so
        // write permission on the directory itself is required as well.
        if src.parent != dst.parent && src_info.kind == NodeKind::Directory {
            src_meta.authorize(cred, Access::Write)?;
        }

        self.fs
            .rename(
                src.parent,
                src.name.as_bytes(),
                dst.parent,
                dst.name.as_bytes(),
            )
            .map_err(map_driver_error)
    }
}

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
