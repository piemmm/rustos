//! The mount table and its per-mount permission policy.
//!
//! A mount associates a subtree (identified by its mount-point [`Path`])
//! with a set of [`MountFlags`] (`ro`, `nosuid`, `nodev`, `noexec`) and,
//! optionally, the [`MountBacking`] — the filesystem driver serving it and
//! the storage medium of the block device that driver reads.
//! The VFS consults the table on every write to decide whether the most
//! specific mount covering a path forbids it (e.g. `/System` is mounted
//! read-only; its `/System/Logs` and `/System/Settings` children are
//! writable child mounts).
//!
//! "Most specific" is the longest mount-point [`Path`] that is a prefix of
//! the queried path. The root mount (`/`) covers everything, so resolution
//! always succeeds.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::filesystem::MountFlags;
use tairix_abi::driver::DriverHandle;

use super::path::Path;
use super::perm::Metadata;
use super::VfsError;

/// What stands behind a mount: the filesystem driver serving it, and the
/// storage medium of the block device that driver reads.
///
/// The two travel together because the medium is a property of the device
/// the driver was attached to, learned at attach time and never re-derived
/// later. Pairing them means a mount with no backing cannot carry a medium
/// at all, so "nothing is mounted here" and "this volume sits on a spinning
/// disk" are distinct states rather than one field two callers must keep
/// consistent.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MountBacking {
    driver: DriverHandle,
    medium: Option<BlkDeviceClass>,
}

impl MountBacking {
    /// A backing by `driver`, reading a device of `medium`.
    ///
    /// `medium` is `None` when the attach path has no classified block
    /// device in hand — a driver serving synthetic content, or a
    /// bootstrap-floor volume brought up before any classified block client
    /// exists. Unknown is recorded as unknown; a medium is never assumed.
    #[must_use]
    pub const fn new(driver: DriverHandle, medium: Option<BlkDeviceClass>) -> Self {
        Self { driver, medium }
    }

    /// The filesystem driver serving the mount.
    #[must_use]
    pub const fn driver(self) -> DriverHandle {
        self.driver
    }

    /// The storage medium of the block device behind the driver, or `None`
    /// when the attach path could not name one.
    #[must_use]
    pub const fn medium(self) -> Option<BlkDeviceClass> {
        self.medium
    }
}

/// One entry in the [`MountTable`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountPoint {
    path: Path,
    flags: MountFlags,
    backing: Option<MountBacking>,
    /// The path *within the backing volume* at which this mount is rooted.
    ///
    /// Empty for a mount rooted at its driver's own root directory (the
    /// common case: a whole volume mounted at its mount point). Non-empty
    /// for a **sub-mount** — a subtree of a larger volume bound at a mount
    /// point whose path differs from the subtree's path on the volume. The
    /// delegated walk prepends these components to the path remainder below
    /// the mount point, so the driver still resolves from its own
    /// [`root`](tairix_abi::driver::filesystem::FilesystemRead::root): e.g.
    /// `/System/Logs` backed by the encrypted root volume's own
    /// `/System/Logs` directory carries `["System", "Logs"]` here.
    backing_subtree: Vec<String>,
    /// The permission template the delegated walk applies at and below the
    /// mount point, for a **runtime** mount whose mount point has no node
    /// in the in-RAM layout tree (a hotplug volume under `/Storage/<name>`).
    /// `None` for the boot layout's mounts, whose mount-point node in the
    /// tree is the template.
    template: Option<Metadata>,
}

impl MountPoint {
    /// The mount-point path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mount's permission flags.
    #[must_use]
    pub fn flags(&self) -> MountFlags {
        self.flags
    }

    /// The backing filesystem driver handle, if any. The root mount and
    /// the in-RAM default layout have no backing driver.
    #[must_use]
    pub fn backing(&self) -> Option<DriverHandle> {
        self.backing.map(MountBacking::driver)
    }

    /// The storage medium of the block device backing this mount, or `None`
    /// when none is known.
    ///
    /// Unknown covers both a mount with no block device behind it at all (a
    /// synthetic or view mount, the in-RAM layout) and a backed mount whose
    /// attach path could not name the device's medium. Both are reported as
    /// unknown rather than as a plausible-looking medium the device never
    /// declared.
    #[must_use]
    pub fn medium(&self) -> Option<BlkDeviceClass> {
        self.backing.and_then(MountBacking::medium)
    }

    /// The path components within the backing volume at which this mount is
    /// rooted. Empty for a mount rooted at its driver's own root directory;
    /// non-empty for a sub-mount of a larger volume (e.g. `["System",
    /// "Logs"]` for `/System/Logs` backed by the root volume's own
    /// `/System/Logs` directory).
    #[must_use]
    pub fn backing_subtree(&self) -> &[String] {
        &self.backing_subtree
    }

    /// `true` if this mount is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.flags.contains(MountFlags::READ_ONLY)
    }

    /// The owned permission template of a runtime mount, or `None` for a
    /// boot-layout mount (whose mount-point node in the in-RAM tree is the
    /// template).
    #[must_use]
    pub fn template(&self) -> Option<&Metadata> {
        self.template.as_ref()
    }
}

/// The system mount table.
///
/// Always contains a root mount at `/`; [`MountTable::resolve`] therefore
/// never fails. Mounts are stored in insertion order and scanned linearly:
/// the table is small (one entry per mounted volume) so an index would be
/// bloat.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountTable {
    mounts: Vec<MountPoint>,
}

impl MountTable {
    /// Construct a table whose only entry is a writable root mount.
    #[must_use]
    pub fn new(root_flags: MountFlags) -> Self {
        Self {
            mounts: alloc::vec![MountPoint {
                path: Path::root(),
                flags: root_flags,
                backing: None,
                backing_subtree: Vec::new(),
                template: None,
            }],
        }
    }

    /// Add a mount at `path` with `flags`, backed by `backing`.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::AlreadyExists`] if a mount already covers
    /// exactly `path`.
    pub fn mount(
        &mut self,
        path: Path,
        flags: MountFlags,
        backing: Option<MountBacking>,
    ) -> Result<(), VfsError> {
        self.mount_rebased(path, flags, backing, Vec::new())
    }

    /// Add a **sub-mount** at `path` with `flags`, backed by `backing`
    /// rooted at `backing_subtree` within that backing volume.
    ///
    /// `backing_subtree` is the path *on the backing volume* at which the
    /// mount's content lives, used when the mount-point path differs from the
    /// content's path on the volume (a subtree of a larger volume bound at a
    /// `/System/...` mount point). An empty `backing_subtree` is exactly
    /// [`MountTable::mount`] (the content is at the driver's own root).
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::AlreadyExists`] if a mount already covers exactly
    /// `path`.
    pub fn mount_rebased(
        &mut self,
        path: Path,
        flags: MountFlags,
        backing: Option<MountBacking>,
        backing_subtree: Vec<String>,
    ) -> Result<(), VfsError> {
        if self.mounts.iter().any(|m| m.path == path) {
            return Err(VfsError::AlreadyExists);
        }
        self.mounts.push(MountPoint {
            path,
            flags,
            backing,
            backing_subtree,
            template: None,
        });
        Ok(())
    }

    /// Add a **runtime** mount at `path` with `flags`, backed by `backing`
    /// and carrying its own permission `template`.
    ///
    /// A runtime mount point (a hotplug volume under `/Storage/<name>`)
    /// has no node in the in-RAM layout tree, so the template the
    /// delegated walk applies at and below the mount point travels with
    /// the mount itself.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::AlreadyExists`] if a mount already covers
    /// exactly `path`.
    pub fn mount_with_template(
        &mut self,
        path: Path,
        flags: MountFlags,
        backing: MountBacking,
        template: Metadata,
    ) -> Result<(), VfsError> {
        if self.mounts.iter().any(|m| m.path == path) {
            return Err(VfsError::AlreadyExists);
        }
        self.mounts.push(MountPoint {
            path,
            flags,
            backing: Some(backing),
            backing_subtree: Vec::new(),
            template: Some(template),
        });
        Ok(())
    }

    /// Give the permanent root mount a backing filesystem driver — the
    /// shape of a block-backed root volume (the whole
    /// tree lives on one driver-mounted volume, as the installer lays it
    /// out). The root mount's flags are unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::AlreadyExists`] if the root mount already has
    /// a backing driver: a second root volume is a wiring defect, never
    /// a silent re-mount (fail closed).
    pub fn back_root(&mut self, backing: MountBacking) -> Result<(), VfsError> {
        let root = &mut self.mounts[0];
        if root.backing.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        root.backing = Some(backing);
        Ok(())
    }

    /// Attach a backing filesystem driver to the **existing** mount at
    /// exactly `path`, rooting its content at `backing_subtree` within that
    /// volume, without changing the mount's permission flags.
    ///
    /// This turns a policy-only mount of the default layout (the
    /// `ro`/`nosuid`/… mount points [`Vfs::with_default_layout`](super::Vfs::with_default_layout)
    /// lays down) into a driver-backed one once the boot path knows which
    /// volume backs it: the flags come from the layout, the backing volume
    /// and its sub-path from the wiring. A whole-volume mount (the backing
    /// volume's own root is the mount's content) passes an empty
    /// `backing_subtree`; a sub-mount of a larger volume passes the path its
    /// content lives at on that volume (e.g. `["System", "Logs"]` for
    /// `/System/Logs` carved out of the root volume).
    ///
    /// # Errors
    ///
    /// * [`VfsError::NotFound`] if no mount covers exactly `path`.
    /// * [`VfsError::AlreadyExists`] if that mount already has a backing
    ///   driver — a mount is backed once, never silently re-backed (fail
    ///   closed).
    pub fn set_backing(
        &mut self,
        path: &Path,
        backing: MountBacking,
        backing_subtree: Vec<String>,
    ) -> Result<(), VfsError> {
        let mount = self
            .mounts
            .iter_mut()
            .find(|m| &m.path == path)
            .ok_or(VfsError::NotFound)?;
        if mount.backing.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        mount.backing = Some(backing);
        mount.backing_subtree = backing_subtree;
        Ok(())
    }

    /// Remove the mount at exactly `path`.
    ///
    /// # Errors
    ///
    /// * [`VfsError::InvalidPath`] if `path` is the root mount, which
    ///   cannot be unmounted.
    /// * [`VfsError::NotFound`] if no mount covers exactly `path`.
    pub fn unmount(&mut self, path: &Path) -> Result<(), VfsError> {
        if path.is_root() {
            return Err(VfsError::InvalidPath);
        }
        let before = self.mounts.len();
        self.mounts.retain(|m| &m.path != path);
        if self.mounts.len() == before {
            return Err(VfsError::NotFound);
        }
        Ok(())
    }

    /// The most specific mount covering `path` (the longest mount-point
    /// that is a prefix of `path`). Never fails: the root mount covers
    /// every path.
    #[must_use]
    pub fn resolve(&self, path: &Path) -> &MountPoint {
        self.mounts
            .iter()
            .filter(|m| m.path.is_prefix_of(path))
            .max_by_key(|m| m.path.depth())
            .unwrap_or(&self.mounts[0])
    }

    /// `true` if writes to `path` are forbidden by the covering mount's
    /// read-only flag.
    #[must_use]
    pub fn is_read_only(&self, path: &Path) -> bool {
        self.resolve(path).is_read_only()
    }

    /// The driver-backed mounts whose mount point is a **direct child** of
    /// `path`, in insertion order.
    ///
    /// These are the entries a directory listing merges into `path`'s own
    /// listing so a covered mount point appears in its parent directory
    /// even when the parent volume holds no node of that name — the
    /// runtime `/Storage/<name>` mounts are exactly that shape, and this
    /// is what enumerates the `Storage:` catalog view. A backing-less
    /// mount (a policy-only entry of the boot layout) is not listed: it
    /// projects no content of its own.
    pub fn direct_children<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = &'a MountPoint> {
        self.mounts.iter().filter(move |m| {
            m.backing.is_some() && m.path.depth() == path.depth() + 1 && path.is_prefix_of(&m.path)
        })
    }

    /// Every mount in the table, in insertion order (the permanent root
    /// mount first).
    ///
    /// A read-only view for the System Information introspection feed: the
    /// order is stable across calls while the table is unchanged, so a broker
    /// paging the mount list never skips or repeats an entry.
    pub fn iter(&self) -> impl Iterator<Item = &MountPoint> {
        self.mounts.iter()
    }

    /// Number of mounts (including the root mount).
    #[must_use]
    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    /// Always `false`: the root mount is permanent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Path {
        Path::parse(text).expect("valid path")
    }

    /// A backing whose device medium the attach path could not name.
    fn unclassified(raw: u64) -> MountBacking {
        MountBacking::new(DriverHandle::from_raw(raw).expect("non-zero handle"), None)
    }

    #[test]
    fn root_mount_covers_everything() {
        let table = MountTable::new(MountFlags::default());
        assert_eq!(table.resolve(&p("/anything/deep")).path(), &Path::root());
        assert!(!table.is_read_only(&p("/anything")));
    }

    #[test]
    fn longest_prefix_wins() {
        let mut table = MountTable::new(MountFlags::default());
        table
            .mount(p("/System"), MountFlags::READ_ONLY, None)
            .expect("mount /System");
        table
            .mount(p("/System/Logs"), MountFlags::NOSUID, None)
            .expect("mount /System/Logs");

        assert!(table.is_read_only(&p("/System/Drivers/x")));
        // The writable child mount shadows the read-only parent.
        assert!(!table.is_read_only(&p("/System/Logs/boot")));
        assert_eq!(
            table.resolve(&p("/System/Logs/boot")).path(),
            &p("/System/Logs")
        );
    }

    #[test]
    fn duplicate_mount_is_rejected() {
        let mut table = MountTable::new(MountFlags::default());
        table
            .mount(p("/Storage"), MountFlags::default(), None)
            .expect("first");
        assert_eq!(
            table.mount(p("/Storage"), MountFlags::READ_ONLY, None),
            Err(VfsError::AlreadyExists)
        );
    }

    #[test]
    fn back_root_attaches_a_driver_exactly_once() {
        let mut table = MountTable::new(MountFlags::default());
        assert_eq!(table.resolve(&p("/System")).backing(), None);

        let backing = unclassified(0x5EC0);
        table.back_root(backing).expect("first backing attaches");
        assert_eq!(
            table.resolve(&p("/System")).backing(),
            Some(backing.driver())
        );

        // A second root volume is refused, and the first stays attached.
        assert_eq!(
            table.back_root(unclassified(0x5EC1)),
            Err(VfsError::AlreadyExists)
        );
        assert_eq!(
            table.resolve(&p("/System")).backing(),
            Some(backing.driver())
        );
    }

    #[test]
    fn an_attached_backing_records_the_medium_the_device_reported() {
        let mut table = MountTable::new(MountFlags::default());
        let driver = DriverHandle::from_raw(0x5ED0).expect("non-zero handle");

        // Every attach seam carries the medium the attach path learned from
        // the block device, so which one a volume arrived through cannot
        // change what the mount reports.
        table
            .back_root(MountBacking::new(driver, Some(BlkDeviceClass::Rotational)))
            .expect("root backing");
        assert_eq!(
            table.resolve(&Path::root()).medium(),
            Some(BlkDeviceClass::Rotational)
        );

        table
            .mount(
                p("/Storage/ssd"),
                MountFlags::NOSUID,
                Some(MountBacking::new(driver, Some(BlkDeviceClass::SolidState))),
            )
            .expect("mount");
        assert_eq!(
            table.resolve(&p("/Storage/ssd")).medium(),
            Some(BlkDeviceClass::SolidState)
        );

        table
            .mount(p("/Storage/stick"), MountFlags::NOSUID, None)
            .expect("policy mount");
        table
            .set_backing(
                &p("/Storage/stick"),
                MountBacking::new(driver, Some(BlkDeviceClass::Removable)),
                Vec::new(),
            )
            .expect("backing attaches");
        assert_eq!(
            table.resolve(&p("/Storage/stick")).medium(),
            Some(BlkDeviceClass::Removable)
        );
    }

    #[test]
    fn a_mount_with_no_block_backing_reports_an_unknown_medium() {
        let mut table = MountTable::new(MountFlags::default());
        // The permanent root mount, before anything backs it.
        assert_eq!(table.resolve(&Path::root()).medium(), None);

        // A policy-only mount of the boot layout: no driver, no device.
        table
            .mount(p("/System"), MountFlags::READ_ONLY, None)
            .expect("policy mount");
        assert_eq!(table.resolve(&p("/System")).medium(), None);

        // A driver-backed mount whose attach path had no classified device
        // reports unknown too, rather than a plausible-looking medium.
        table
            .mount(
                p("/Storage/x"),
                MountFlags::NOSUID,
                Some(unclassified(0x5ED1)),
            )
            .expect("mount");
        let mount = table.resolve(&p("/Storage/x"));
        assert!(mount.backing().is_some());
        assert_eq!(mount.medium(), None);
    }

    #[test]
    fn set_backing_attaches_a_driver_and_subtree_preserving_flags() {
        let mut table = MountTable::new(MountFlags::default());
        table
            .mount(
                p("/Users"),
                MountFlags::NOSUID.union(MountFlags::NODEV),
                None,
            )
            .expect("mount /Users");

        let backing = unclassified(0x5701);
        table
            .set_backing(&p("/Users"), backing, alloc::vec!["Users".into()])
            .expect("first backing attaches");

        let mount = table.resolve(&p("/Users/alice"));
        assert_eq!(mount.backing(), Some(backing.driver()));
        // The layout's flags are untouched by attaching a backing volume.
        assert_eq!(mount.flags(), MountFlags::NOSUID.union(MountFlags::NODEV));
        // The sub-mount is rooted at the volume's own `/Users` directory.
        assert_eq!(mount.backing_subtree(), &[String::from("Users")]);
    }

    #[test]
    fn set_backing_is_refused_for_an_unknown_mount_or_a_second_backing() {
        let mut table = MountTable::new(MountFlags::default());
        let backing = unclassified(0x5702);
        // No mount covers exactly `/Storage` yet.
        assert_eq!(
            table.set_backing(&p("/Storage"), backing, Vec::new()),
            Err(VfsError::NotFound)
        );

        table
            .mount(p("/Storage"), MountFlags::default(), None)
            .expect("mount /Storage");
        table
            .set_backing(&p("/Storage"), backing, Vec::new())
            .expect("first backing attaches");
        // A second backing is refused, and the first stays attached.
        let other = MountBacking::new(
            DriverHandle::from_raw(0x5703).expect("non-zero handle"),
            Some(BlkDeviceClass::SolidState),
        );
        assert_eq!(
            table.set_backing(&p("/Storage"), other, Vec::new()),
            Err(VfsError::AlreadyExists)
        );
        let mount = table.resolve(&p("/Storage"));
        assert_eq!(mount.backing(), Some(backing.driver()));
        // The refused attach left the first backing's medium untouched.
        assert_eq!(mount.medium(), None);
    }

    #[test]
    fn a_runtime_mount_carries_its_own_template() {
        use super::super::perm::{Metadata, Mode};
        use tairix_kernel_sec::{GroupId, UserId};

        let mut table = MountTable::new(MountFlags::default());
        let handle = DriverHandle::from_raw(0x564F).expect("non-zero handle");
        let backing = MountBacking::new(handle, Some(BlkDeviceClass::Removable));
        let template = Metadata::new(UserId(0), GroupId(0), Mode::from_bits(0o755));
        table
            .mount_with_template(
                p("/Storage/usb1"),
                MountFlags::NOSUID,
                backing,
                template.clone(),
            )
            .expect("runtime mount");

        let mount = table.resolve(&p("/Storage/usb1/file"));
        assert_eq!(mount.backing(), Some(handle));
        assert_eq!(mount.medium(), Some(BlkDeviceClass::Removable));
        assert_eq!(mount.template(), Some(&template));
        // Boot-layout mounts carry no template (their tree node is it).
        assert_eq!(table.resolve(&p("/other")).template(), None);
        // The runtime mount unmounts like any other, and a duplicate is
        // refused while mounted.
        assert_eq!(
            table.mount_with_template(
                p("/Storage/usb1"),
                MountFlags::NOSUID,
                backing,
                template.clone()
            ),
            Err(VfsError::AlreadyExists)
        );
        table.unmount(&p("/Storage/usb1")).expect("unmount");
    }

    #[test]
    fn direct_children_lists_backed_child_mounts_only() {
        let mut table = MountTable::new(MountFlags::default());
        let backed = unclassified(0x5704);
        table
            .mount(p("/Storage/usb1"), MountFlags::NOSUID, Some(backed))
            .expect("backed child");
        // A backing-less policy mount projects no content and is not listed.
        table
            .mount(p("/Storage/policy"), MountFlags::NOSUID, None)
            .expect("policy child");
        // A deeper mount is not a *direct* child of /Storage.
        table
            .mount(p("/Storage/usb1/nested"), MountFlags::NOSUID, Some(backed))
            .expect("grandchild");
        // An unrelated sibling subtree is not listed either.
        table
            .mount(p("/Users/vol"), MountFlags::NOSUID, Some(backed))
            .expect("unrelated");

        let storage = p("/Storage");
        let children: Vec<_> = table
            .direct_children(&storage)
            .map(MountPoint::path)
            .collect();
        assert_eq!(children, [&p("/Storage/usb1")]);
        // Every mount here sits two levels deep, so the root itself has no
        // direct-child mounts.
        let root = Path::root();
        assert_eq!(table.direct_children(&root).count(), 0);
    }

    #[test]
    fn unmount_root_is_refused() {
        let mut table = MountTable::new(MountFlags::default());
        assert_eq!(table.unmount(&Path::root()), Err(VfsError::InvalidPath));
    }

    #[test]
    fn unmount_unknown_is_not_found() {
        let mut table = MountTable::new(MountFlags::default());
        assert_eq!(table.unmount(&p("/Storage/usb0")), Err(VfsError::NotFound));
    }

    #[test]
    fn mount_then_unmount_round_trips() {
        let mut table = MountTable::new(MountFlags::default());
        table
            .mount(p("/Storage/usb0"), MountFlags::NODEV, None)
            .expect("mount");
        assert_eq!(table.len(), 2);
        table.unmount(&p("/Storage/usb0")).expect("unmount");
        assert_eq!(table.len(), 1);
        assert!(!table.is_empty());
    }
}
