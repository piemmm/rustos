//! Behavioural tests for [`MountedFilesystemService`]: the production
//! `fs_*` producer routing each operation through the secured VFS under a
//! late-installed mount and identity table.
//!
//! The shared in-memory driver fixture (`crate::fs::memfs::RwMockFs`) stands
//! in for a block-backed `drivers/filesystem/*` driver; these tests exercise
//! the *service* layer — fail-closed gating, kernel-attested identity and
//! group resolution, and the `open`/append/`readdir` semantics — not the VFS
//! internals (covered by `delegate_tests`). True lock contention across a
//! task park is exercised by the `SleepLock` tests and the FS QEMU vertical;
//! a host test has no installed scheduler to park on.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU8;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::filesystem::{
    FilesystemAttrs as _, FilesystemRead, FilesystemWrite, MountFlags, NodeKind, NodeSecurity,
};
use tairix_abi::driver::DriverHandle;
use tairix_abi::sysinfo::MountAvailability;
use tairix_abi::{
    CapabilityId, Errno, FileKind, OpenFlags, RealpathMode, UnlinkFlags, FS_OWNER_UNCHANGED,
};
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::{
    GroupId, GroupRecord, IdentityTable, IdentityTableBuilder, UserId, UserRecord,
};
use tairix_log::{Event, Sink};

use super::{LateFilesystem, LateIdentity, MountedFilesystemService};
use crate::fs::blkclient::{BlkHealthCountersAtomic, VolumeHealthSource};
use crate::fs::memfs::RwMockFs;
use crate::fs::perm::Credentials;
use crate::fs::service::FilesystemService;
use crate::fs::{FinalLink, Mode, MountBacking, Path, Vfs};

/// The test principal that owns the mounted volume's files.
const TEST_UID: u32 = 1000;
const TEST_GID: u32 = 1000;
/// The mount point the in-memory driver is mounted at.
const MOUNT: &str = "/Storage/vol";
/// The storage medium the fixture's block device declares, so the snapshot
/// can be checked against a value only the attach path could have supplied.
const MOUNT_MEDIUM: BlkDeviceClass = BlkDeviceClass::SolidState;

/// A sink that discards every event — the identity-table verifier audits its
/// outcome but these tests assert behaviour, not the audit trail.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

fn caps() -> CapabilitySet {
    CapabilitySet::empty()
}

/// An identity table holding exactly the test principal (uid/gid 1000).
fn identity_table() -> IdentityTable {
    let mut builder = IdentityTableBuilder::new();
    builder.push_group(GroupRecord {
        gid: GroupId(TEST_GID),
    });
    builder.push_user(UserRecord {
        uid: UserId(TEST_UID),
        primary_gid: GroupId(TEST_GID),
        supplementary_gids: Vec::new(),
        capability_grants: CapabilitySet::empty(),
    });
    builder
        .verify(&NullSink)
        .expect("well-formed identity table")
}

/// A default-layout VFS with the in-memory driver mounted at [`MOUNT`],
/// read-only when `read_only`.
fn vfs(read_only: bool) -> Vfs {
    let mut vfs = Vfs::with_default_layout(UserId(TEST_UID), GroupId(TEST_GID));
    let caps = caps();
    let cred = Credentials {
        uid: UserId(TEST_UID),
        gid: GroupId(TEST_GID),
        supplementary_gids: &[],
        caps: &caps,
    };
    let mount = Path::parse(MOUNT).expect("valid mount path");
    vfs.mkdir(&cred, &mount, Mode::from_bits(0o755))
        .expect("create mount point");
    let handle = DriverHandle::from_raw(9).expect("non-zero handle");
    let flags = if read_only {
        MountFlags::READ_ONLY
    } else {
        MountFlags::from_bits(0).expect("empty flags")
    };
    // The fixture stands in for a volume attached from a classified block
    // device, so the medium it reports is the one the device declared.
    vfs.mounts_write()
        .mount(
            mount,
            flags,
            Some(MountBacking::new(handle, Some(MOUNT_MEDIUM))),
        )
        .expect("mount backed");
    vfs
}

/// The in-memory driver, with its root and created files owned by the test
/// principal so it can traverse, create, and write.
fn driver() -> RwMockFs {
    let mut fs = RwMockFs::new().with_create_owner(TEST_UID, TEST_GID, 0o644);
    fs.set_root_security(NodeSecurity::new(0o755, TEST_UID, TEST_GID));
    fs
}

/// A driver owned by the test principal whose created nodes are
/// world-traversable directories (mode `0o755`), so a rebased mount can walk
/// the backing-subtree directories it pre-creates.
fn dir_driver() -> RwMockFs {
    let mut fs = RwMockFs::new().with_create_owner(TEST_UID, TEST_GID, 0o755);
    fs.set_root_security(NodeSecurity::new(0o755, TEST_UID, TEST_GID));
    fs
}

/// Two rebased sub-mounts that share **one** backing driver (the
/// `/System/Logs` + `/System/Settings`-on-one-volume shape) route each to
/// their own backing subtree and stay isolated; a second driver under a
/// different handle is fully independent.
#[test]
fn rebased_submounts_route_to_their_backing_subtree_and_handle() {
    let nosuid = MountFlags::NOSUID;
    let h_shared = DriverHandle::from_raw(9).expect("handle");
    let h_other = DriverHandle::from_raw(10).expect("handle");

    // One driver carrying two backing subtrees, plus an independent second
    // driver. Both are owned by the test principal.
    let mut shared = dir_driver();
    let sroot = shared.root();
    shared
        .create(sroot, b"LogsArea", NodeKind::Directory)
        .expect("logs subtree");
    shared
        .create(sroot, b"SettingsArea", NodeKind::Directory)
        .expect("settings subtree");
    let other = dir_driver();

    // The VFS: two rebased mounts on the shared driver and one plain mount on
    // the other, with their in-RAM mount-point directories created so the
    // delegated walk has a permission template.
    let mut vfs = Vfs::with_default_layout(UserId(TEST_UID), GroupId(TEST_GID));
    let caps = caps();
    let cred = Credentials {
        uid: UserId(TEST_UID),
        gid: GroupId(TEST_GID),
        supplementary_gids: &[],
        caps: &caps,
    };
    for mp in ["/Storage/logs", "/Storage/settings", "/Storage/other"] {
        vfs.mkdir(
            &cred,
            &Path::parse(mp).expect("path"),
            Mode::from_bits(0o755),
        )
        .expect("mount point");
    }
    for (mount_point, subtree) in [
        ("/Storage/logs", "LogsArea"),
        ("/Storage/settings", "SettingsArea"),
    ] {
        vfs.mounts_write()
            .mount_rebased(
                Path::parse(mount_point).expect("path"),
                nosuid,
                Some(MountBacking::new(h_shared, None)),
                alloc::vec![alloc::string::String::from(subtree)],
            )
            .expect("rebased mount");
    }
    vfs.mounts_write()
        .mount(
            Path::parse("/Storage/other").expect("path"),
            nosuid,
            Some(MountBacking::new(h_other, None)),
        )
        .expect("other mount");

    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs).expect("install vfs");
    cell.register(h_shared, shared, "shared", "memfs", [0u8; 16])
        .expect("register shared");
    cell.register(h_other, other, "other", "memfs", [0u8; 16])
        .expect("register other");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(identity_table()).expect("identity");
    let svc = MountedFilesystemService::new(cell, identity);

    let create = OpenFlags::CREATE.union(OpenFlags::WRITE);
    svc.open(TEST_UID, &caps, "/Storage/logs/a", create)
        .expect("create under logs");
    svc.write(TEST_UID, &caps, "/Storage/logs/a", 0, false, b"L")
        .expect("write logs");
    svc.open(TEST_UID, &caps, "/Storage/settings/b", create)
        .expect("create under settings");
    svc.write(TEST_UID, &caps, "/Storage/settings/b", 0, false, b"S")
        .expect("write settings");
    svc.open(TEST_UID, &caps, "/Storage/other/c", create)
        .expect("create under other");
    svc.write(TEST_UID, &caps, "/Storage/other/c", 0, false, b"O")
        .expect("write other");

    // Each file reads back through its own mount.
    let mut buf = [0u8; 1];
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/logs/a", 0, &mut buf),
        Ok(1)
    );
    assert_eq!(&buf, b"L");
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/settings/b", 0, &mut buf),
        Ok(1)
    );
    assert_eq!(&buf, b"S");
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/other/c", 0, &mut buf),
        Ok(1)
    );
    assert_eq!(&buf, b"O");

    // The two rebased mounts are isolated: `a` exists only under the logs
    // subtree, `b` only under settings.
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/settings/a", 0, &mut buf),
        Err(Errno::NotFound)
    );
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/logs/b", 0, &mut buf),
        Err(Errno::NotFound)
    );
    // The second driver is independent: its file is not visible on the
    // shared driver's subtrees.
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/logs/c", 0, &mut buf),
        Err(Errno::NotFound)
    );
}

/// Listing a driver-backed directory merges the mount points of its
/// direct-child mounts — the `Storage:` catalog enumeration: a runtime
/// mount with no node of its name on the parent volume appears as a
/// structural directory entry, a name the parent volume already holds is
/// not repeated, and the merged name resolves through to the mounted
/// volume's own content.
#[test]
fn readdir_merges_direct_child_mounts_into_the_parent_listing() {
    use crate::fs::perm::Metadata;
    use tairix_abi::time::Time64;

    let h_parent = DriverHandle::from_raw(9).expect("handle");
    let h_usb = DriverHandle::from_raw(10).expect("handle");
    let h_dup = DriverHandle::from_raw(11).expect("handle");

    // The parent volume holds one plain directory and one whose name
    // collides with a child mount point.
    let mut parent = dir_driver();
    let proot = parent.root();
    parent
        .create(proot, b"existing", NodeKind::Directory)
        .expect("existing dir");
    parent
        .create(proot, b"dup", NodeKind::Directory)
        .expect("dup dir");

    // The hotplug volume carries one file, so resolution through the
    // merged name is provable end to end.
    let mut usb = driver();
    let uroot = usb.root();
    usb.create(uroot, b"note", NodeKind::RegularFile)
        .expect("note file");

    let vfs = Vfs::with_default_layout(UserId(TEST_UID), GroupId(TEST_GID));
    vfs.mounts_write()
        .set_backing(
            &Path::parse("/Storage").expect("path"),
            MountBacking::new(h_parent, None),
            Vec::new(),
        )
        .expect("back /Storage");
    let template = Metadata::new(UserId(TEST_UID), GroupId(TEST_GID), Mode::from_bits(0o775));
    vfs.mounts_write()
        .mount_with_template(
            Path::parse("/Storage/usb1").expect("path"),
            MountFlags::NOSUID,
            MountBacking::new(h_usb, Some(BlkDeviceClass::Removable)),
            template.clone(),
        )
        .expect("runtime mount");
    vfs.mounts_write()
        .mount_with_template(
            Path::parse("/Storage/dup").expect("path"),
            MountFlags::NOSUID,
            MountBacking::new(h_dup, Some(BlkDeviceClass::Removable)),
            template,
        )
        .expect("colliding runtime mount");

    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs).expect("install vfs");
    cell.register(h_parent, parent, "root", "memfs", [0u8; 16])
        .expect("register parent");
    cell.register(h_usb, usb, "usb1", "memfs", [0u8; 16])
        .expect("register usb");
    cell.register(h_dup, driver(), "dup", "memfs", [0u8; 16])
        .expect("register dup");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(identity_table()).expect("identity");
    let svc = MountedFilesystemService::new(cell, identity);
    let caps = caps();

    let entries = svc
        .readdir(TEST_UID, &caps, "/Storage", FinalLink::Follow)
        .expect("listing succeeds");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"existing"),
        "parent content listed: {names:?}"
    );
    assert!(names.contains(&"usb1"), "child mount merged: {names:?}");
    // The colliding name appears exactly once (the parent volume's own
    // node; the mount is not repeated behind it).
    assert_eq!(
        names.iter().filter(|n| **n == "dup").count(),
        1,
        "colliding mount name deduplicated: {names:?}"
    );
    let usb1 = entries
        .iter()
        .find(|e| e.name == "usb1")
        .expect("merged entry");
    assert_eq!(usb1.kind, FileKind::Directory);
    assert_eq!((usb1.size, usb1.allocated), (0, 0));
    assert_eq!(usb1.modified, Time64::UNIX_EPOCH);
    // The parent volume holds no node of that name, so it offers no identity
    // or name count for it — a placeholder, never a key a walk may compare.
    assert_eq!(usb1.id, tairix_abi::FileId::NONE);
    assert_eq!(usb1.nlink, NodeInfo::SINGLE_NAME);

    // The merged name is not a phantom: it resolves through to the mounted
    // volume's own content.
    let inside = svc
        .readdir(TEST_UID, &caps, "/Storage/usb1", FinalLink::Follow)
        .expect("child volume lists");
    assert!(inside.iter().any(|e| e.name == "note"), "{inside:?}");
}

/// Build a service over freshly leaked mount + identity cells. `mount` and
/// `identity` select whether each cell is installed, so a test can exercise
/// the fail-closed pre-install paths.
fn service(
    mount_installed: bool,
    identity_installed: bool,
    read_only: bool,
) -> MountedFilesystemService<RwMockFs> {
    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    if mount_installed {
        cell.install_vfs(vfs(read_only)).expect("install vfs");
        // The in-memory driver is mounted at [`MOUNT`] under handle 9 (see
        // `vfs`); register it under that same handle so the service routes
        // operations on `/Storage/vol/...` to it.
        let handle = DriverHandle::from_raw(9).expect("non-zero handle");
        cell.register(handle, driver(), "vol", "memfs", [0u8; 16])
            .expect("register driver");
    }
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    if identity_installed {
        identity
            .install(identity_table())
            .expect("install identity");
    }
    MountedFilesystemService::new(cell, identity)
}

/// A fully-installed writable service (the common case).
fn ready() -> MountedFilesystemService<RwMockFs> {
    service(true, true, false)
}

/// A fully-installed writable service whose created directories are
/// traversable (mode `0o755`), so a test can build a tree and then walk it.
fn ready_traversable() -> MountedFilesystemService<RwMockFs> {
    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs(false)).expect("install vfs");
    let handle = DriverHandle::from_raw(9).expect("non-zero handle");
    cell.register(handle, dir_driver(), "vol", "memfs", [0u8; 16])
        .expect("register driver");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity
        .install(identity_table())
        .expect("install identity");
    MountedFilesystemService::new(cell, identity)
}

fn path(name: &str) -> alloc::string::String {
    alloc::format!("{MOUNT}/{name}")
}

#[test]
fn an_uninstalled_mount_fails_closed_not_implemented() {
    // Identity is present, but no volume is mounted: every op is refused
    // exactly like the hollow `NULL_FILESYSTEM`.
    let svc = service(false, true, false);
    let caps = caps();
    let mut buf = [0u8; 4];
    assert_eq!(
        svc.read(TEST_UID, &caps, &path("x"), 0, &mut buf),
        Err(Errno::NotImplemented)
    );
    assert_eq!(
        svc.stat(TEST_UID, &caps, &path("x"), FinalLink::Follow),
        Err(Errno::NotImplemented)
    );
}

#[test]
fn an_uninstalled_identity_fails_closed_not_implemented() {
    // The volume is mounted, but the authoritative identity table has not
    // been installed yet (the disk is not unlocked): groups cannot be
    // resolved, so the op is refused rather than run with a guessed identity.
    let svc = service(true, false, false);
    let caps = caps();
    assert_eq!(
        svc.stat(TEST_UID, &caps, &path("x"), FinalLink::Follow),
        Err(Errno::NotImplemented)
    );
}

#[test]
fn an_unknown_principal_is_denied() {
    // A uid with no account is denied — never granted a guessed identity, and
    // the refusal does not distinguish "unknown uid".
    let svc = ready();
    let caps = caps();
    assert_eq!(
        svc.stat(4242, &caps, &path("x"), FinalLink::Follow),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn the_system_principal_resolves_before_the_identity_table_installs() {
    // The volume is mounted but the identity table is not installed (the
    // encrypted root is still locked): the kernel-defined system principal
    // (uid 0) resolves to the capability-less bootstrap identity, so its
    // operation reaches the volume and reports the path's real state
    // (`NotFound` here) instead of the pre-fix `NotImplemented` that killed
    // every pre-unlock store-bundle spawn.
    let svc = service(true, false, false);
    let caps = caps();
    assert_eq!(
        svc.stat(0, &caps, &path("x"), FinalLink::Follow),
        Err(Errno::NotFound)
    );
}

#[test]
fn an_installed_table_without_uid0_still_resolves_the_system_principal() {
    // An installed table that defines no uid 0 account (the installer-built
    // shape) does not strand the system principal: uid 0 falls back to the
    // bootstrap identity while every other unknown uid stays denied.
    let svc = ready();
    let caps = caps();
    assert_eq!(
        svc.stat(0, &caps, &path("x"), FinalLink::Follow),
        Err(Errno::NotFound)
    );
    assert_eq!(
        svc.stat(4242, &caps, &path("x"), FinalLink::Follow),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn create_write_read_back_under_attested_identity() {
    let svc = ready();
    let caps = caps();
    let p = path("notes.txt");
    let flags = OpenFlags::CREATE
        .union(OpenFlags::WRITE)
        .union(OpenFlags::READ);
    svc.open(TEST_UID, &caps, &p, flags).expect("create-open");
    assert_eq!(svc.write(TEST_UID, &caps, &p, 0, false, b"hello"), Ok(5));

    let mut buf = [0u8; 16];
    let n = svc.read(TEST_UID, &caps, &p, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"hello");
}

#[test]
fn an_append_write_ignores_the_offset_and_extends() {
    let svc = ready();
    let caps = caps();
    let p = path("log");
    svc.open(
        TEST_UID,
        &caps,
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    assert_eq!(svc.write(TEST_UID, &caps, &p, 0, false, b"abc"), Ok(3));
    // The supplied offset (0) is ignored; the append lands at end of file.
    assert_eq!(svc.write(TEST_UID, &caps, &p, 0, true, b"de"), Ok(2));

    let mut buf = [0u8; 8];
    let n = svc.read(TEST_UID, &caps, &p, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"abcde");
}

#[test]
fn stat_reports_kind_size_and_attested_owner() {
    let svc = ready();
    let caps = caps();
    let p = path("f");
    svc.open(
        TEST_UID,
        &caps,
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    svc.write(TEST_UID, &caps, &p, 0, false, b"1234")
        .expect("write");

    let st = svc
        .stat(TEST_UID, &caps, &p, FinalLink::Follow)
        .expect("stat");
    assert_eq!(st.kind, FileKind::Regular);
    assert_eq!(st.size, 4);
    assert_eq!(st.uid, TEST_UID);
    assert_eq!(st.gid, TEST_GID);
    assert_eq!(st.mode, 0o644);
}

#[test]
fn mkdir_then_readdir_reports_each_entrys_kind() {
    let svc = ready();
    let caps = caps();
    svc.mkdir(TEST_UID, &caps, &path("sub")).expect("mkdir");
    svc.open(
        TEST_UID,
        &caps,
        &path("file"),
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create file");

    let mut entries = svc
        .readdir(TEST_UID, &caps, MOUNT, FinalLink::Follow)
        .expect("readdir");
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let kinds: Vec<(FileKind, &str)> = entries.iter().map(|e| (e.kind, e.name.as_str())).collect();
    assert_eq!(
        kinds,
        alloc::vec![(FileKind::Regular, "file"), (FileKind::Directory, "sub"),]
    );
}

/// A listing reports each entry's system-wide identity and name count, so a
/// walk can tell a second *name* for one node from a second node — the fact
/// `du`'s hard-link deduplication rests on. Without the identity on the
/// readdir record the two names below are indistinguishable from two files.
#[test]
fn readdir_reports_one_identity_for_two_names_of_one_node() {
    let svc = ready();
    let caps = caps();
    svc.open(
        TEST_UID,
        &caps,
        &path("first"),
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    svc.link(
        TEST_UID,
        &caps,
        &path("first"),
        &path("second"),
        FinalLink::Keep,
    )
    .expect("second name");

    let entries = svc
        .readdir(TEST_UID, &caps, MOUNT, FinalLink::Follow)
        .expect("readdir");
    let first = entries
        .iter()
        .find(|e| e.name == "first")
        .expect("first name listed");
    let second = entries
        .iter()
        .find(|e| e.name == "second")
        .expect("second name listed");
    assert_eq!(first.id, second.id);
    assert!(!first.id.is_none(), "a real volume names a real node");
    assert_eq!((first.nlink, second.nlink), (2, 2));

    // The listing's identity is the same one `stat` reports for the path, so
    // the two views of a node cannot disagree about what it is.
    let stat = svc
        .stat(TEST_UID, &caps, &path("first"), FinalLink::Follow)
        .expect("stat");
    assert_eq!(stat.id, first.id);
    assert_eq!(stat.nlink, first.nlink);
}

/// Two distinct single-named files are two identities, so a deduplicating
/// walk never collapses them.
#[test]
fn readdir_reports_distinct_identities_for_distinct_nodes() {
    let svc = ready();
    let caps = caps();
    for name in ["one", "two"] {
        svc.open(
            TEST_UID,
            &caps,
            &path(name),
            OpenFlags::CREATE.union(OpenFlags::WRITE),
        )
        .expect("create");
    }
    let entries = svc
        .readdir(TEST_UID, &caps, MOUNT, FinalLink::Follow)
        .expect("readdir");
    let ids: Vec<tairix_abi::FileId> = entries.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert!(entries.iter().all(|e| e.nlink == NodeInfo::SINGLE_NAME));
}

#[test]
fn unlink_removes_the_file() {
    let svc = ready();
    let caps = caps();
    let p = path("gone");
    svc.open(
        TEST_UID,
        &caps,
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    svc.unlink(TEST_UID, &caps, &p, UnlinkFlags::empty())
        .expect("unlink");
    let mut buf = [0u8; 4];
    assert_eq!(
        svc.read(TEST_UID, &caps, &p, 0, &mut buf),
        Err(Errno::NotFound)
    );
}

#[test]
fn directory_only_unlink_refuses_a_file_and_removes_an_empty_directory() {
    // The `rmdir` posture through the whole service: `DIRECTORY` reaching a
    // file is the dedicated `NotADirectory` errno and the file survives; the
    // same flag removes an (empty) directory.
    let svc = ready();
    let caps = caps();
    let file = path("plain");
    svc.open(
        TEST_UID,
        &caps,
        &file,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    assert_eq!(
        svc.unlink(TEST_UID, &caps, &file, UnlinkFlags::DIRECTORY),
        Err(Errno::NotADirectory)
    );
    svc.stat(TEST_UID, &caps, &file, FinalLink::Follow)
        .expect("file survives the refused dir-only removal");
    let dir = path("empty");
    svc.mkdir(TEST_UID, &caps, &dir).expect("mkdir");
    svc.unlink(TEST_UID, &caps, &dir, UnlinkFlags::DIRECTORY)
        .expect("dir-only remove of an empty directory");
    assert_eq!(
        svc.stat(TEST_UID, &caps, &dir, FinalLink::Follow),
        Err(Errno::NotFound)
    );
}

#[test]
fn truncate_changes_the_size() {
    let svc = ready();
    let caps = caps();
    let p = path("t");
    svc.open(
        TEST_UID,
        &caps,
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    svc.write(TEST_UID, &caps, &p, 0, false, b"0123456789")
        .expect("write");
    svc.truncate(TEST_UID, &caps, &p, 4).expect("truncate");
    assert_eq!(
        svc.stat(TEST_UID, &caps, &p, FinalLink::Follow)
            .expect("stat")
            .size,
        4
    );
}

#[test]
fn an_exclusive_create_of_an_existing_path_fails_closed() {
    let svc = ready();
    let caps = caps();
    let p = path("dup");
    svc.open(
        TEST_UID,
        &caps,
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("first create");
    let exclusive = OpenFlags::CREATE
        .union(OpenFlags::EXCLUSIVE)
        .union(OpenFlags::WRITE);
    // The dedicated `EEXIST` equivalent crosses the ABI boundary intact.
    assert_eq!(
        svc.open(TEST_UID, &caps, &p, exclusive),
        Err(Errno::AlreadyExists)
    );
}

#[test]
fn a_directory_open_of_a_regular_file_fails_closed() {
    let svc = ready();
    let caps = caps();
    let p = path("plain");
    svc.open(
        TEST_UID,
        &caps,
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    // The dedicated `ENOTDIR` equivalent crosses the ABI boundary intact.
    assert_eq!(
        svc.open(TEST_UID, &caps, &p, OpenFlags::DIRECTORY),
        Err(Errno::NotADirectory)
    );
}

#[test]
fn a_write_to_a_read_only_mount_fails_closed() {
    // The mount carries `READ_ONLY`, so the secured write is refused before
    // the driver is touched; `ReadOnly` collapses onto `PermissionDenied`.
    let svc = service(true, true, true);
    let caps = caps();
    assert_eq!(
        svc.write(TEST_UID, &caps, &path("x"), 0, false, b"nope"),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn sync_before_a_mount_is_installed_fails_closed() {
    let svc = service(false, true, false);
    let caps = caps();
    assert_eq!(svc.sync(TEST_UID, &caps), Err(Errno::NotImplemented));
}

#[test]
fn sync_flushes_the_mounted_volume() {
    // The in-memory driver flushes as a no-op; the call still resolves the
    // mount and returns success rather than the hollow `NotImplemented`.
    let svc = ready();
    let caps = caps();
    assert_eq!(svc.sync(TEST_UID, &caps), Ok(()));
}

#[test]
fn rename_moves_a_file_under_the_attested_identity() {
    let svc = ready();
    let caps = caps();
    let src = path("a");
    let dst = path("b");
    svc.open(
        TEST_UID,
        &caps,
        &src,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    svc.write(TEST_UID, &caps, &src, 0, false, b"hi")
        .expect("write");
    svc.rename(TEST_UID, &caps, &src, &dst).expect("rename");
    let mut buf = [0u8; 2];
    assert_eq!(
        svc.read(TEST_UID, &caps, &src, 0, &mut buf),
        Err(Errno::NotFound)
    );
    assert_eq!(svc.read(TEST_UID, &caps, &dst, 0, &mut buf), Ok(2));
    assert_eq!(&buf, b"hi");
}

#[test]
fn rename_of_a_missing_source_fails_closed() {
    let svc = ready();
    let caps = caps();
    assert_eq!(
        svc.rename(TEST_UID, &caps, &path("nope"), &path("x")),
        Err(Errno::NotFound)
    );
}

#[test]
fn rename_on_a_read_only_mount_fails_closed() {
    // `ReadOnly` collapses onto `PermissionDenied` at the ABI boundary.
    let svc = service(true, true, true);
    let caps = caps();
    assert_eq!(
        svc.rename(TEST_UID, &caps, &path("a"), &path("b")),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn set_mode_rewrites_the_permission_bits_for_the_owner() {
    let svc = ready();
    let caps = caps();
    let p = path("f");
    svc.open(
        TEST_UID,
        &caps,
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");

    svc.set_mode(TEST_UID, &caps, &p, 0o600).expect("chmod");

    let st = svc
        .stat(TEST_UID, &caps, &p, FinalLink::Follow)
        .expect("stat");
    assert_eq!(st.mode, 0o600);
    assert_eq!(st.uid, TEST_UID);
    assert_eq!(st.gid, TEST_GID);
}

#[test]
fn set_mode_above_the_permission_mask_fails_closed() {
    // Defence in depth at the service seam: a file-type bit is refused
    // before any resolution, so the record can never be corrupted.
    let svc = ready();
    let caps = caps();
    assert_eq!(
        svc.set_mode(TEST_UID, &caps, &path("f"), 0o10_0644),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn set_mode_on_a_read_only_mount_fails_closed() {
    // `ReadOnly` collapses onto `PermissionDenied` at the ABI boundary.
    let svc = service(true, true, true);
    let caps = caps();
    assert_eq!(
        svc.set_mode(TEST_UID, &caps, &path("a"), 0o600),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn set_mode_before_a_mount_is_installed_fails_closed() {
    let svc = service(false, true, false);
    let caps = caps();
    assert_eq!(
        svc.set_mode(TEST_UID, &caps, &path("a"), 0o600),
        Err(Errno::NotImplemented)
    );
}

#[test]
fn rename_before_a_mount_is_installed_fails_closed() {
    let svc = service(false, true, false);
    let caps = caps();
    assert_eq!(
        svc.rename(TEST_UID, &caps, &path("a"), &path("b")),
        Err(Errno::NotImplemented)
    );
}

/// A capability set holding `CAP_FS_CHOWN` — the privileged authority to
/// reassign a node's owner (and to set any group).
fn chown_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::FS_CHOWN);
    caps
}

/// A fully-installed writable service whose identity table also makes the
/// test principal a member of the supplementary group `gid`, so the
/// unprivileged owner-may-set-a-group-they-belong-to branch is exercised.
fn ready_with_supplementary(gid: u32) -> MountedFilesystemService<RwMockFs> {
    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs(false)).expect("install vfs");
    let handle = DriverHandle::from_raw(9).expect("non-zero handle");
    cell.register(handle, driver(), "vol", "memfs", [0u8; 16])
        .expect("register driver");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    let mut builder = IdentityTableBuilder::new();
    builder.push_group(GroupRecord {
        gid: GroupId(TEST_GID),
    });
    builder.push_group(GroupRecord { gid: GroupId(gid) });
    builder.push_user(UserRecord {
        uid: UserId(TEST_UID),
        primary_gid: GroupId(TEST_GID),
        supplementary_gids: Vec::from([GroupId(gid)]),
        capability_grants: CapabilitySet::empty(),
    });
    identity
        .install(
            builder
                .verify(&NullSink)
                .expect("well-formed identity table"),
        )
        .expect("install identity");
    MountedFilesystemService::new(cell, identity)
}

/// Create a regular file owned by the test principal and return its path.
fn create_file(svc: &MountedFilesystemService<RwMockFs>, name: &str) -> alloc::string::String {
    let p = path(name);
    svc.open(
        TEST_UID,
        &caps(),
        &p,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    p
}

#[test]
fn set_owner_reassigns_the_uid_only_for_a_privileged_caller() {
    let svc = ready();
    let p = create_file(&svc, "f");

    // Unprivileged: reassigning the owner is refused and nothing changes.
    assert_eq!(
        svc.set_owner(TEST_UID, &caps(), &p, 2000, FS_OWNER_UNCHANGED),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.stat(TEST_UID, &caps(), &p, FinalLink::Follow)
            .expect("stat")
            .uid,
        TEST_UID
    );

    // Privileged (`CAP_FS_CHOWN`): the owner is reassigned.
    svc.set_owner(TEST_UID, &chown_caps(), &p, 2000, FS_OWNER_UNCHANGED)
        .expect("chown");
    assert_eq!(
        svc.stat(TEST_UID, &caps(), &p, FinalLink::Follow)
            .expect("stat")
            .uid,
        2000
    );
}

#[test]
fn set_owner_strips_the_setuid_bit_on_a_change() {
    let svc = ready();
    let p = create_file(&svc, "bin");
    // A setuid, group-executable file.
    svc.set_mode(TEST_UID, &caps(), &p, 0o4755).expect("chmod");

    svc.set_owner(TEST_UID, &chown_caps(), &p, 2000, FS_OWNER_UNCHANGED)
        .expect("chown");

    let st = svc
        .stat(TEST_UID, &caps(), &p, FinalLink::Follow)
        .expect("stat");
    assert_eq!(st.uid, 2000);
    // The setuid bit is gone; a reassigned file cannot stay setuid.
    assert_eq!(st.mode & 0o7777, 0o0755);
}

#[test]
fn set_owner_group_change_to_a_non_member_group_is_denied_unprivileged() {
    let svc = ready();
    let p = create_file(&svc, "g");
    // The owner belongs only to gid 1000; setting an unrelated group without
    // `CAP_FS_CHOWN` is refused, and the group is unchanged.
    assert_eq!(
        svc.set_owner(TEST_UID, &caps(), &p, FS_OWNER_UNCHANGED, 55),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.stat(TEST_UID, &caps(), &p, FinalLink::Follow)
            .expect("stat")
            .gid,
        TEST_GID
    );
}

#[test]
fn set_owner_group_change_to_a_member_group_is_allowed_unprivileged() {
    let svc = ready_with_supplementary(7);
    let p = create_file(&svc, "g");
    // The owner is a member of gid 7, so an unprivileged `chgrp` to it
    // succeeds — no capability needed.
    svc.set_owner(TEST_UID, &caps(), &p, FS_OWNER_UNCHANGED, 7)
        .expect("chgrp to a member group");
    assert_eq!(
        svc.stat(TEST_UID, &caps(), &p, FinalLink::Follow)
            .expect("stat")
            .gid,
        7
    );
}

#[test]
fn set_owner_privileged_group_change_needs_no_membership() {
    let svc = ready();
    let p = create_file(&svc, "g");
    // A `CAP_FS_CHOWN` holder may set any group, member or not.
    svc.set_owner(TEST_UID, &chown_caps(), &p, FS_OWNER_UNCHANGED, 55)
        .expect("privileged chgrp");
    assert_eq!(
        svc.stat(TEST_UID, &caps(), &p, FinalLink::Follow)
            .expect("stat")
            .gid,
        55
    );
}

#[test]
fn set_owner_leaving_both_unchanged_is_a_noop_even_unprivileged() {
    let svc = ready();
    let p = create_file(&svc, "n");
    // Both fields sentinel: a no-op that succeeds without `CAP_FS_CHOWN`.
    svc.set_owner(
        TEST_UID,
        &caps(),
        &p,
        FS_OWNER_UNCHANGED,
        FS_OWNER_UNCHANGED,
    )
    .expect("no-op chown");
    let st = svc
        .stat(TEST_UID, &caps(), &p, FinalLink::Follow)
        .expect("stat");
    assert_eq!(st.uid, TEST_UID);
    assert_eq!(st.gid, TEST_GID);
}

#[test]
fn set_owner_on_a_read_only_mount_fails_closed() {
    // `ReadOnly` collapses onto `PermissionDenied` at the ABI boundary.
    let svc = service(true, true, true);
    assert_eq!(
        svc.set_owner(
            TEST_UID,
            &chown_caps(),
            &path("a"),
            2000,
            FS_OWNER_UNCHANGED
        ),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn set_owner_before_a_mount_is_installed_fails_closed() {
    let svc = service(false, true, false);
    assert_eq!(
        svc.set_owner(
            TEST_UID,
            &chown_caps(),
            &path("a"),
            2000,
            FS_OWNER_UNCHANGED
        ),
        Err(Errno::NotImplemented)
    );
}

/// The production `/System` shape: the in-RAM layout and every node of the
/// read-only volume are owned by the system user (uid 0) at mode `0o755`,
/// and an ordinary account (uid 1000, no supplementary groups, no
/// capabilities) lists the mount. The read-only volume also carries `Logs`
/// and `Settings` entries whose *paths* are covered by the writable root
/// volume's rebased sub-mounts (the shipped-image shape), so the listing
/// must not judge those children against the read-only volume's driver.
/// A refusal here is the "cannot list inside /System as a user" regression.
#[test]
fn an_ordinary_user_lists_the_system_owned_read_only_mount() {
    // The read-only `/System` volume: content at the volume root, including
    // the same-named `Logs`/`Settings` the writable mounts shadow.
    let mut system = RwMockFs::new().with_create_owner(0, 0, 0o755);
    system.set_root_security(NodeSecurity::new(0o755, 0, 0));
    let root = system.root();
    for name in [&b"Kernel"[..], b"Drivers", b"Logs", b"Settings"] {
        system
            .create(root, name, NodeKind::Directory)
            .expect("mkdir system subdir");
    }

    // The writable root volume: carries its own `/System/Logs` and
    // `/System/Settings` directories the sub-mounts rebase onto.
    let mut rootvol = RwMockFs::new().with_create_owner(0, 0, 0o755);
    rootvol.set_root_security(NodeSecurity::new(0o755, 0, 0));
    let rv_root = rootvol.root();
    let rv_system = rootvol
        .create(rv_root, b"System", NodeKind::Directory)
        .expect("mkdir System");
    for name in [&b"Logs"[..], b"Settings"] {
        rootvol
            .create(rv_system, name, NodeKind::Directory)
            .expect("mkdir writable exception");
    }

    // The production mount layout: writable root as `/`, read-only volume
    // over `/System`, writable exceptions rebased back out of it.
    let vfs = Vfs::with_default_layout(UserId(0), GroupId(0));
    let system_handle = DriverHandle::from_raw(9).expect("handle");
    let root_handle = DriverHandle::from_raw(10).expect("handle");
    let root_backing = MountBacking::new(root_handle, None);
    vfs.mounts_write().back_root(root_backing).expect("back /");
    vfs.mounts_write()
        .set_backing(
            &Path::parse("/System").expect("path"),
            MountBacking::new(system_handle, None),
            Vec::new(),
        )
        .expect("back /System");
    for sub in ["/System/Logs", "/System/Settings"] {
        let path = Path::parse(sub).expect("path");
        let subtree = path.components().to_vec();
        vfs.mounts_write()
            .set_backing(&path, root_backing, subtree)
            .expect("back writable exception");
    }

    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs).expect("install vfs");
    cell.register(system_handle, system, "system", "memfs", [0u8; 16])
        .expect("register system");
    cell.register(root_handle, rootvol, "root", "memfs", [0u8; 16])
        .expect("register root");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(identity_table()).expect("identity");
    let svc = MountedFilesystemService::new(cell, identity);

    let caps = caps();
    let entries = svc
        .readdir(TEST_UID, &caps, "/System", FinalLink::Follow)
        .expect("an ordinary user lists /System");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for expected in ["Kernel", "Drivers", "Logs", "Settings"] {
        assert!(names.contains(&expected), "{expected} listed: {names:?}");
    }
    assert!(entries.iter().all(|e| e.kind == FileKind::Directory));
}

/// The mount snapshot carries the registration names and the live space
/// accounting of a backed mount, and the honest empties (no names, the
/// all-zero usage) for a mount whose backing is the in-RAM layout — the
/// rows `df` renders. This is the regression guard for the "snapshot
/// reported every source/fstype empty and no capacity" gap.
#[test]
fn mount_snapshot_reports_names_and_usage_from_the_registered_driver() {
    let svc = ready();
    let records = svc.mount_snapshot();
    // The unbacked root mount reports the truthful "nothing known".
    let root = records
        .iter()
        .find(|record| record.target_bytes() == b"/")
        .expect("the root mount is always listed");
    assert_eq!(root.source_bytes(), b"");
    assert_eq!(root.fstype_bytes(), b"");
    assert_eq!(root.usage().total_blocks, 0);
    // With no block device behind it, the root mount's medium is unknown
    // rather than a plausible-looking guess.
    assert_eq!(root.medium(), None);
    // The backed volume reports its registration names and the driver's
    // live accounting.
    let vol = records
        .iter()
        .find(|record| record.source_bytes() == b"vol")
        .expect("the registered volume is listed by its source name");
    assert_eq!(vol.fstype_bytes(), b"memfs");
    // The medium the attach path recorded reaches the record userland reads.
    assert_eq!(vol.medium(), Some(MOUNT_MEDIUM));
    let usage = vol.usage();
    assert_eq!(usage.block_size, 512);
    assert_eq!(usage.total_blocks, 4096);
    assert!(usage.avail_blocks <= usage.free_blocks);
    assert!(usage.free_blocks <= usage.total_blocks);
}

/// The mount snapshot overlays a live device's reported I/O health onto an
/// otherwise-`Available` volume, but a genuine surprise-removal state always
/// wins over the overlay so a vanished volume never masquerades as merely
/// unwell (`plans/FIX-IO.md` IO2/IO3; `plans/DEVICES.md` D4).
#[test]
fn mount_snapshot_overlays_reported_block_health() {
    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs(false)).expect("install vfs");
    let handle = DriverHandle::from_raw(9).expect("handle");
    cell.register(handle, driver(), "vol", "memfs", [0u8; 16])
        .expect("register");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(identity_table()).expect("identity");
    let svc = MountedFilesystemService::new(cell, identity);

    let vol_availability = |svc: &MountedFilesystemService<RwMockFs>| {
        svc.mount_snapshot()
            .iter()
            .find(|record| record.source_bytes() == b"vol")
            .expect("the registered volume is listed")
            .availability()
    };

    // With no health source the volume reads plainly available.
    assert_eq!(vol_availability(&svc), MountAvailability::Available);

    // With no health source the volume-health query lists no device.
    assert!(svc.volume_io_health_snapshot().is_empty());

    // A live block-health overlay reporting a blip surfaces as recovering.
    let availability = Arc::new(AtomicU8::new(MountAvailability::Recovering.as_u8()));
    let counters = Arc::new(BlkHealthCountersAtomic::default());
    cell.set_health_source(
        handle,
        VolumeHealthSource {
            dev: 0x42,
            availability: Arc::clone(&availability),
            counters: Arc::clone(&counters),
        },
    )
    .expect("attach health source");
    assert_eq!(vol_availability(&svc), MountAvailability::Recovering);

    // The volume-health query now lists exactly this device, naming its
    // serving endpoint and overlaying the same live availability the mount
    // snapshot shows.
    let health_records = svc.volume_io_health_snapshot();
    assert_eq!(health_records.len(), 1);
    assert_eq!(health_records[0].dev(), 0x42);
    assert_eq!(
        health_records[0].availability(),
        MountAvailability::Recovering
    );
    assert_eq!(health_records[0].counters().completions, 0);

    // A surprise-removal state is authoritative: even with the overlay now
    // claiming the device is fine, the vanished state stands.
    cell.set_availability(handle, MountAvailability::UnavailableDirty)
        .expect("mark unavailable");
    availability.store(
        MountAvailability::Available.as_u8(),
        core::sync::atomic::Ordering::Relaxed,
    );
    assert_eq!(vol_availability(&svc), MountAvailability::UnavailableDirty);

    // Back to a live volume, the overlay's degraded reading shows through.
    cell.set_availability(handle, MountAvailability::Available)
        .expect("mark available");
    availability.store(
        MountAvailability::Degraded.as_u8(),
        core::sync::atomic::Ordering::Relaxed,
    );
    assert_eq!(vol_availability(&svc), MountAvailability::Degraded);
    // The volume-health query reflects the same degraded overlay.
    assert_eq!(
        svc.volume_io_health_snapshot()[0].availability(),
        MountAvailability::Degraded
    );
}

#[test]
fn rename_to_a_path_outside_the_mounted_volume_fails_closed() {
    // The destination resolves to a different, backing-less mount, so the
    // move never escapes the volume; it fails closed rather than fabricating
    // a cross-device move.
    let svc = ready();
    let caps = caps();
    let src = path("a");
    svc.open(
        TEST_UID,
        &caps,
        &src,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    assert_eq!(
        svc.rename(TEST_UID, &caps, &src, "/Apps/b"),
        Err(Errno::NotFound)
    );
}

/// Build the service over `fs_driver` mounted at [`MOUNT`], with the test
/// principal installed — the harness the extended-attribute tests share.
fn attr_service<F>(fs_driver: F, read_only: bool) -> MountedFilesystemService<F>
where
    F: tairix_abi::driver::filesystem::FilesystemRead
        + tairix_abi::driver::filesystem::FilesystemWrite
        + tairix_abi::driver::filesystem::FilesystemSecurity
        + tairix_abi::driver::filesystem::FilesystemStats
        + tairix_abi::driver::filesystem::FilesystemAttrsProvider
        + Send
        + 'static,
{
    let cell: &'static LateFilesystem<F> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs(read_only)).expect("install vfs");
    cell.register(
        DriverHandle::from_raw(9).expect("handle"),
        fs_driver,
        "vol",
        "memfs",
        [0u8; 16],
    )
    .expect("register");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(identity_table()).expect("identity");
    MountedFilesystemService::new(cell, identity)
}

/// Set/get/list/remove round-trip for an ordinary-namespace attribute, with
/// the absent-attribute answer distinct from an empty value.
#[test]
fn attrs_round_trip_through_the_service() {
    let caps = caps();
    let svc = attr_service(driver(), false);
    let create = OpenFlags::CREATE.union(OpenFlags::WRITE);
    let path = "/Storage/vol/f";
    svc.open(TEST_UID, &caps, path, create).expect("create");

    svc.attr_set(TEST_UID, &caps, path, b"user.comment", b"hi")
        .expect("set");
    let mut value = [0u8; 8];
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, path, b"user.comment", &mut value),
        Ok(2)
    );
    assert_eq!(&value[..2], b"hi");

    // An empty value is stored and read back as zero bytes, not "absent".
    svc.attr_set(TEST_UID, &caps, path, b"user.empty", b"")
        .expect("set empty");
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, path, b"user.empty", &mut value),
        Ok(0)
    );

    // The listing yields the visible keys in stored order, then ends.
    let mut key = [0u8; 64];
    assert_eq!(
        svc.attr_list(TEST_UID, &caps, path, 0, &mut key),
        Ok(Some(b"user.comment".len()))
    );
    assert_eq!(&key[..b"user.comment".len()], b"user.comment");
    assert_eq!(
        svc.attr_list(TEST_UID, &caps, path, 1, &mut key),
        Ok(Some(b"user.empty".len()))
    );
    assert_eq!(svc.attr_list(TEST_UID, &caps, path, 2, &mut key), Ok(None));

    svc.attr_remove(TEST_UID, &caps, path, b"user.comment")
        .expect("remove");
    // Absence is the dedicated code, for get and for a second remove alike.
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, path, b"user.comment", &mut value),
        Err(Errno::NoData)
    );
    assert_eq!(
        svc.attr_remove(TEST_UID, &caps, path, b"user.comment"),
        Err(Errno::NoData)
    );
}

/// Attribute reads need read permission and writes need write permission on
/// the node itself; a read-only mount refuses mutation outright.
#[test]
fn attr_access_follows_the_nodes_own_permissions() {
    // A second principal, in the identity table but with no rights over the
    // test principal's 0o600 file.
    const OTHER_UID: u32 = 1001;
    let mut builder = IdentityTableBuilder::new();
    builder.push_group(GroupRecord {
        gid: GroupId(TEST_GID),
    });
    builder.push_group(GroupRecord { gid: GroupId(1001) });
    builder.push_user(UserRecord {
        uid: UserId(TEST_UID),
        primary_gid: GroupId(TEST_GID),
        supplementary_gids: Vec::new(),
        capability_grants: CapabilitySet::empty(),
    });
    builder.push_user(UserRecord {
        uid: UserId(OTHER_UID),
        primary_gid: GroupId(1001),
        supplementary_gids: Vec::new(),
        capability_grants: CapabilitySet::empty(),
    });
    let table = builder.verify(&NullSink).expect("well-formed table");

    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs(false)).expect("install vfs");
    cell.register(
        DriverHandle::from_raw(9).expect("handle"),
        driver(),
        "vol",
        "memfs",
        [0u8; 16],
    )
    .expect("register");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(table).expect("identity");
    let svc = MountedFilesystemService::new(cell, identity);

    let caps = caps();
    let create = OpenFlags::CREATE.union(OpenFlags::WRITE);
    let path = "/Storage/vol/private";
    svc.open(TEST_UID, &caps, path, create).expect("create");
    svc.attr_set(TEST_UID, &caps, path, b"user.comment", b"hi")
        .expect("owner sets");
    svc.set_mode(TEST_UID, &caps, path, 0o600).expect("chmod");

    // The other principal can neither read nor write the node's attributes.
    let mut buf = [0u8; 8];
    assert_eq!(
        svc.attr_get(OTHER_UID, &caps, path, b"user.comment", &mut buf),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.attr_list(OTHER_UID, &caps, path, 0, &mut buf),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.attr_set(OTHER_UID, &caps, path, b"user.comment", b"x"),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.attr_remove(OTHER_UID, &caps, path, b"user.comment"),
        Err(Errno::PermissionDenied)
    );
}

/// A read-only mount refuses attribute mutation but still serves reads.
#[test]
fn attr_mutation_on_a_read_only_mount_fails_closed() {
    let caps = caps();
    // Seed the file and its attribute directly on the driver: the mount is
    // read-only, so nothing can be created through the service.
    let mut fs = driver();
    let root = fs.root();
    let node = fs
        .create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.set_attr(node, b"user.comment", b"hi").expect("seed");
    let svc = attr_service(fs, true);
    let path = "/Storage/vol/f";

    let mut buf = [0u8; 8];
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, path, b"user.comment", &mut buf),
        Ok(2)
    );
    assert_eq!(
        svc.attr_set(TEST_UID, &caps, path, b"user.comment", b"x"),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.attr_remove(TEST_UID, &caps, path, b"user.comment"),
        Err(Errno::PermissionDenied)
    );
}

/// The privileged namespaces are refused for every caller, and a stored
/// privileged key is omitted from the listing — its existence is never
/// revealed, not even as an index gap.
#[test]
fn privileged_namespaces_are_refused_and_hidden() {
    let caps = caps();
    let mut fs = driver();
    let root = fs.root();
    let node = fs
        .create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    // Stored out-of-band (as a privileged service one day would); the
    // syscall surface itself refuses to write these namespaces.
    fs.set_attr(node, b"system.hidden", b"s").expect("seed");
    fs.set_attr(node, b"user.visible", b"v").expect("seed");
    let svc = attr_service(fs, false);
    let path = "/Storage/vol/f";

    let mut buf = [0u8; 64];
    assert_eq!(
        svc.attr_set(TEST_UID, &caps, path, b"system.hidden", b"x"),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, path, b"system.hidden", &mut buf),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        svc.attr_remove(TEST_UID, &caps, path, b"trusted.x"),
        Err(Errno::PermissionDenied)
    );
    // The listing shows only the visible key: index 0 is the user key and
    // index 1 is already the end, with no gap betraying the hidden one.
    assert_eq!(
        svc.attr_list(TEST_UID, &caps, path, 0, &mut buf),
        Ok(Some(b"user.visible".len()))
    );
    assert_eq!(&buf[..b"user.visible".len()], b"user.visible");
    assert_eq!(svc.attr_list(TEST_UID, &caps, path, 1, &mut buf), Ok(None));
}

/// Malformed keys and undersized buffers fail closed with their dedicated
/// codes; nothing is truncated or guessed.
#[test]
fn attr_key_grammar_and_buffers_fail_closed() {
    let caps = caps();
    let svc = attr_service(driver(), false);
    let create = OpenFlags::CREATE.union(OpenFlags::WRITE);
    let path = "/Storage/vol/f";
    svc.open(TEST_UID, &caps, path, create).expect("create");
    svc.attr_set(TEST_UID, &caps, path, b"user.comment", b"hello")
        .expect("set");

    // No `namespace.rest` split, and an unknown namespace: both refused.
    assert_eq!(
        svc.attr_set(TEST_UID, &caps, path, b"nodot", b"x"),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        svc.attr_set(TEST_UID, &caps, path, b"bogus.key", b"x"),
        Err(Errno::OutOfRange)
    );
    // A value that does not fit is refused whole, never truncated.
    let mut tiny = [0u8; 2];
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, path, b"user.comment", &mut tiny),
        Err(Errno::BufferTooSmall)
    );
    let mut tiny_key = [0u8; 4];
    assert_eq!(
        svc.attr_list(TEST_UID, &caps, path, 0, &mut tiny_key),
        Err(Errno::BufferTooSmall)
    );
    // A missing path is the path's own error, not an attribute answer.
    let mut buf = [0u8; 8];
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, "/Storage/vol/absent", b"user.x", &mut buf),
        Err(Errno::NotFound)
    );
}

use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrsProvider, FilesystemSecurity, FilesystemStats, NodeId, NodeInfo,
    VolumeStats,
};
use tairix_abi::DriverError;

/// [`RwMockFs`] minus its attribute store: the facet keeps its default
/// `None` answer, standing in for a FAT32/ext4-class mount.
struct NoAttrsFs(RwMockFs);
impl FilesystemRead for NoAttrsFs {
    fn root(&self) -> NodeId {
        self.0.root()
    }
    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        self.0.node_info(node)
    }
    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        self.0.lookup(dir, name)
    }
    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        self.0.read_at(file, offset, buf)
    }
    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        self.0.read_dir(dir, index, name_out)
    }
}
impl FilesystemWrite for NoAttrsFs {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.0.create(dir, name, kind)
    }
    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.0.write_at(dir, name, offset, data)
    }
    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.0.truncate(dir, name, size)
    }
    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.0.remove(dir, name)
    }
    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        self.0.rename(src_dir, src_name, dst_dir, dst_name)
    }
    fn flush(&mut self) -> Result<(), DriverError> {
        self.0.flush()
    }
}
impl FilesystemSecurity for NoAttrsFs {
    fn security(
        &mut self,
        node: NodeId,
    ) -> Result<tairix_abi::driver::filesystem::NodeSecurity, DriverError> {
        self.0.security(node)
    }
    fn set_security(
        &mut self,
        node: NodeId,
        security: tairix_abi::driver::filesystem::NodeSecurity,
    ) -> Result<(), DriverError> {
        self.0.set_security(node, security)
    }
}
impl FilesystemStats for NoAttrsFs {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        self.0.stats()
    }
}
impl FilesystemAttrsProvider for NoAttrsFs {}

/// A driver whose format stores no attributes answers every attribute call
/// with the typed unsupported-backing refusal, decided per mount through
/// the attribute facet.
#[test]
fn attrs_on_an_unsupporting_backing_are_a_typed_refusal() {
    let caps = caps();
    let svc = attr_service(NoAttrsFs(driver()), false);
    let create = OpenFlags::CREATE.union(OpenFlags::WRITE);
    let path = "/Storage/vol/f";
    svc.open(TEST_UID, &caps, path, create).expect("create");

    let mut buf = [0u8; 8];
    assert_eq!(
        svc.attr_get(TEST_UID, &caps, path, b"user.x", &mut buf),
        Err(Errno::NotSupported)
    );
    assert_eq!(
        svc.attr_set(TEST_UID, &caps, path, b"user.x", b"v"),
        Err(Errno::NotSupported)
    );
    assert_eq!(
        svc.attr_list(TEST_UID, &caps, path, 0, &mut buf),
        Err(Errno::NotSupported)
    );
    assert_eq!(
        svc.attr_remove(TEST_UID, &caps, path, b"user.x"),
        Err(Errno::NotSupported)
    );
}

// --- symbolic links at the service seam --------------------------------
//
// The VFS's own resolution matrix lives in `delegate_tests`; what these
// cases pin down is the *service*: the two new operations, and the follow
// posture an open fixes travelling with its descriptor so a later stat or
// listing cannot contradict it.

#[test]
fn symlink_then_readlink_round_trips_the_stored_target() {
    let svc = ready();
    let caps = caps();
    let link = path("alias");

    svc.symlink(TEST_UID, &caps, "/target/name", &link)
        .expect("create the link");
    assert_eq!(
        svc.readlink(TEST_UID, &caps, &link),
        Ok(alloc::string::String::from("/target/name"))
    );
    // A second link of the same name is refused, not silently replaced.
    assert_eq!(
        svc.symlink(TEST_UID, &caps, "/other", &link),
        Err(Errno::AlreadyExists)
    );
}

#[test]
fn readlink_of_a_non_link_fails_closed() {
    let svc = ready();
    let caps = caps();
    let file = path("plain");
    svc.open(
        TEST_UID,
        &caps,
        &file,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");

    assert_eq!(svc.readlink(TEST_UID, &caps, &file), Err(Errno::OutOfRange));
    assert_eq!(
        svc.readlink(TEST_UID, &caps, &path("absent")),
        Err(Errno::NotFound)
    );
}

#[test]
fn symlink_on_a_read_only_mount_is_refused() {
    let svc = service(true, true, true);
    let caps = caps();
    assert_eq!(
        svc.symlink(TEST_UID, &caps, "/target", &path("alias")),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn stat_describes_the_link_under_keep_and_its_target_under_follow() {
    let svc = ready();
    let caps = caps();
    let file = path("real");
    svc.open(
        TEST_UID,
        &caps,
        &file,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    svc.write(TEST_UID, &caps, &file, 0, false, b"1234")
        .expect("write");
    let link = path("alias");
    let target = alloc::format!("/{}", "real");
    svc.symlink(TEST_UID, &caps, &target, &link)
        .expect("create the link");

    let kept = svc
        .stat(TEST_UID, &caps, &link, FinalLink::Keep)
        .expect("lstat");
    assert_eq!(kept.kind, FileKind::Symlink);
    assert_eq!(kept.size, target.len() as u64);

    let followed = svc
        .stat(TEST_UID, &caps, &link, FinalLink::Follow)
        .expect("stat");
    assert_eq!(followed.kind, FileKind::Regular);
    assert_eq!(followed.size, 4);
}

#[test]
fn an_open_asking_for_bytes_of_a_link_it_may_not_follow_is_refused() {
    let svc = ready();
    let caps = caps();
    let link = path("alias");
    svc.symlink(TEST_UID, &caps, "/nowhere", &link)
        .expect("create the link");

    // A link stores a path, not bytes: `NO_FOLLOW` plus byte access over one
    // that really is a link is the documented `LinkLoop` refusal.
    for flags in [
        OpenFlags::NO_FOLLOW.union(OpenFlags::READ),
        OpenFlags::NO_FOLLOW.union(OpenFlags::WRITE),
    ] {
        assert_eq!(
            svc.open(TEST_UID, &caps, &link, flags),
            Err(Errno::LinkLoop),
            "{flags:?} over a link must not yield a byte handle"
        );
    }
    // The resolve-only handle — the `lstat` posture — is what succeeds, even
    // though the target does not exist.
    svc.open(TEST_UID, &caps, &link, OpenFlags::NO_FOLLOW)
        .expect("a resolve-only handle on the link itself");
}

#[test]
fn no_follow_does_not_disturb_an_open_whose_final_component_is_not_a_link() {
    // The flag combination is legal because the final component usually is
    // not a link; only a real one refuses.
    let svc = ready();
    let caps = caps();
    let file = path("plain");
    svc.open(
        TEST_UID,
        &caps,
        &file,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");

    svc.open(
        TEST_UID,
        &caps,
        &file,
        OpenFlags::NO_FOLLOW.union(OpenFlags::READ),
    )
    .expect("a regular file reads fine under NO_FOLLOW");
}

#[test]
fn an_open_that_follows_a_link_to_a_directory_still_refuses_byte_access() {
    let svc = ready();
    let caps = caps();
    svc.mkdir(TEST_UID, &caps, &path("dir")).expect("mkdir");
    let link = path("todir");
    svc.symlink(TEST_UID, &caps, "/dir", &link)
        .expect("create the link");

    // Following reaches the directory, so the ordinary "no byte access to a
    // directory" rule applies to what the link names.
    assert_eq!(
        svc.open(TEST_UID, &caps, &link, OpenFlags::READ),
        Err(Errno::IsADirectory)
    );
    // And a directory open through the link succeeds, because it is one.
    svc.open(TEST_UID, &caps, &link, OpenFlags::DIRECTORY)
        .expect("a directory open follows the link");
    // Kept, the same name is not a directory at all.
    assert_eq!(
        svc.open(
            TEST_UID,
            &caps,
            &link,
            OpenFlags::DIRECTORY.union(OpenFlags::NO_FOLLOW)
        ),
        Err(Errno::NotADirectory)
    );
}

#[test]
fn readdir_under_keep_refuses_a_link_rather_than_listing_its_target() {
    let svc = ready();
    let caps = caps();
    svc.mkdir(TEST_UID, &caps, &path("dir")).expect("mkdir");
    // The fixture's driver mints a node `0o644`; a directory needs its
    // search bit before anything can be created inside it.
    svc.set_mode(TEST_UID, &caps, &path("dir"), 0o755)
        .expect("make the directory traversable");
    svc.open(
        TEST_UID,
        &caps,
        &path("dir/inside"),
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create inside");
    let link = path("todir");
    svc.symlink(TEST_UID, &caps, "/dir", &link)
        .expect("create the link");

    let followed = svc
        .readdir(TEST_UID, &caps, &link, FinalLink::Follow)
        .expect("following lists the target");
    assert!(followed.iter().any(|e| e.name == "inside"), "{followed:?}");
    assert_eq!(
        svc.readdir(TEST_UID, &caps, &link, FinalLink::Keep),
        Err(Errno::NotADirectory)
    );
}

#[test]
fn readdir_reports_a_link_entry_as_a_link_never_as_its_target() {
    let svc = ready();
    let caps = caps();
    svc.mkdir(TEST_UID, &caps, &path("dir")).expect("mkdir");
    svc.symlink(TEST_UID, &caps, "/dir", &path("todir"))
        .expect("create the link");

    let mut entries = svc
        .readdir(TEST_UID, &caps, MOUNT, FinalLink::Follow)
        .expect("readdir");
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let kinds: Vec<(FileKind, &str)> = entries.iter().map(|e| (e.kind, e.name.as_str())).collect();
    assert_eq!(
        kinds,
        alloc::vec![(FileKind::Directory, "dir"), (FileKind::Symlink, "todir")]
    );
}

#[test]
fn an_uninstalled_mount_refuses_both_link_operations() {
    let svc = service(false, true, false);
    let caps = caps();
    assert_eq!(
        svc.symlink(TEST_UID, &caps, "/t", &path("alias")),
        Err(Errno::NotImplemented)
    );
    assert_eq!(
        svc.readlink(TEST_UID, &caps, &path("alias")),
        Err(Errno::NotImplemented)
    );
}

#[test]
fn an_open_creating_through_a_dangling_link_creates_the_target() {
    // The `O_CREAT` half of "a write follows a final link": the create lands
    // where the link points, so the link stops dangling rather than the open
    // reporting the link's own name as already taken.
    let svc = ready();
    let caps = caps();
    let link = path("alias");
    svc.symlink(TEST_UID, &caps, "/made-later", &link)
        .expect("create the link");

    svc.open(
        TEST_UID,
        &caps,
        &link,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create through the dangling link");

    let target = svc
        .stat(TEST_UID, &caps, &path("made-later"), FinalLink::Keep)
        .expect("the target was created");
    assert_eq!(target.kind, FileKind::Regular);
    // The link itself is untouched and now resolves.
    assert_eq!(
        svc.stat(TEST_UID, &caps, &link, FinalLink::Keep)
            .expect("the link survived")
            .kind,
        FileKind::Symlink
    );
    assert_eq!(
        svc.stat(TEST_UID, &caps, &link, FinalLink::Follow)
            .expect("the link resolves")
            .kind,
        FileKind::Regular
    );
}

#[test]
fn writes_through_a_link_reach_the_target_and_leave_the_link_intact() {
    let svc = ready();
    let caps = caps();
    let file = path("real");
    svc.open(
        TEST_UID,
        &caps,
        &file,
        OpenFlags::CREATE.union(OpenFlags::WRITE),
    )
    .expect("create");
    let link = path("alias");
    svc.symlink(TEST_UID, &caps, "/real", &link)
        .expect("create the link");

    assert_eq!(
        svc.write(TEST_UID, &caps, &link, 0, false, b"payload"),
        Ok(7)
    );
    let mut buf = [0u8; 16];
    assert_eq!(svc.read(TEST_UID, &caps, &file, 0, &mut buf), Ok(7));
    assert_eq!(&buf[..7], b"payload");
    // An append resolves the size through the link too.
    assert_eq!(svc.write(TEST_UID, &caps, &link, 0, true, b"!"), Ok(1));
    assert_eq!(
        svc.stat(TEST_UID, &caps, &file, FinalLink::Follow)
            .expect("stat the target")
            .size,
        8
    );
    // Truncate-on-open reaches the target, not the link.
    svc.open(
        TEST_UID,
        &caps,
        &link,
        OpenFlags::WRITE.union(OpenFlags::TRUNCATE),
    )
    .expect("open the link for truncation");
    assert_eq!(
        svc.stat(TEST_UID, &caps, &file, FinalLink::Follow)
            .expect("stat the target")
            .size,
        0
    );
    assert_eq!(
        svc.readlink(TEST_UID, &caps, &link),
        Ok(alloc::string::String::from("/real"))
    );
}

#[test]
fn mkdir_over_a_link_reports_it_as_already_existing() {
    let svc = ready();
    let caps = caps();
    let link = path("alias");
    svc.symlink(TEST_UID, &caps, "/absent", &link)
        .expect("create the link");

    assert_eq!(svc.mkdir(TEST_UID, &caps, &link), Err(Errno::AlreadyExists));
    assert_eq!(
        svc.stat(TEST_UID, &caps, &path("absent"), FinalLink::Keep),
        Err(Errno::NotFound)
    );
}

/// A symbolic link stored inside a **sub-mount** cannot resolve to a node
/// the mount does not project: an absolute target names the mount's own
/// root, and `..` cannot climb above it.
///
/// The volume carries a decoy `inside` at its own root and the real one
/// inside the projected subtree, so each link's resolution names exactly one
/// of them and the assertion distinguishes a clamped walk from a walk floored
/// at the driver root.
#[test]
fn a_link_in_a_submount_cannot_resolve_outside_the_projected_subtree() {
    let h = DriverHandle::from_raw(9).expect("handle");

    let mut volume = dir_driver();
    let vroot = volume.root();
    // The decoy: same name, at the volume root, outside what the mount
    // projects.
    volume
        .create(vroot, b"inside", NodeKind::RegularFile)
        .expect("decoy");
    let area = volume
        .create(vroot, b"Area", NodeKind::Directory)
        .expect("subtree");
    volume
        .create(area, b"inside", NodeKind::RegularFile)
        .expect("projected file");
    volume
        .create_link(area, b"abs", b"/inside")
        .expect("absolute link");
    volume
        .create_link(area, b"up", b"../inside")
        .expect("ascending link");
    volume
        .write_at(vroot, b"inside", 0, b"V")
        .expect("decoy contents");
    volume
        .write_at(area, b"inside", 0, b"I")
        .expect("projected contents");

    let mut vfs = Vfs::with_default_layout(UserId(TEST_UID), GroupId(TEST_GID));
    let caps = caps();
    let cred = Credentials {
        uid: UserId(TEST_UID),
        gid: GroupId(TEST_GID),
        supplementary_gids: &[],
        caps: &caps,
    };
    vfs.mkdir(
        &cred,
        &Path::parse("/Storage/area").expect("path"),
        Mode::from_bits(0o755),
    )
    .expect("mount point");
    vfs.mounts_write()
        .mount_rebased(
            Path::parse("/Storage/area").expect("path"),
            MountFlags::NOSUID,
            Some(MountBacking::new(h, None)),
            alloc::vec![alloc::string::String::from("Area")],
        )
        .expect("rebased mount");

    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs).expect("install vfs");
    cell.register(h, volume, "vol", "memfs", [0u8; 16])
        .expect("register");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(identity_table()).expect("identity");
    let svc = MountedFilesystemService::new(cell, identity);

    let mut buf = [0u8; 1];
    // An absolute target names the mount's own root, so it reaches the
    // projected file rather than the volume-root decoy.
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/area/abs", 0, &mut buf),
        Ok(1)
    );
    assert_eq!(&buf, b"I");
    // `..` is floored at the projected root, so ascending from the subtree
    // stays inside it.
    assert_eq!(
        svc.read(TEST_UID, &caps, "/Storage/area/up", 0, &mut buf),
        Ok(1)
    );
    assert_eq!(&buf, b"I");
}

// --- canonicalisation at the service seam ------------------------------
//
// The resolution matrix itself is the VFS's (`delegate_tests`); what these
// cases pin down is the answer's *spelling* — a `/`-view path in the
// caller's own namespace, mount point included — and the three readings of
// how much of a path must exist.

#[test]
fn realpath_resolves_every_link_and_reports_a_view_path() {
    let svc = ready_traversable();
    let caps = caps();
    let create = OpenFlags::CREATE.union(OpenFlags::WRITE);

    svc.mkdir(TEST_UID, &caps, &path("dir")).expect("dir");
    svc.open(TEST_UID, &caps, &path("dir/file"), create)
        .expect("file");
    // An interior link and a final link, so one answer proves both are
    // followed.
    svc.symlink(TEST_UID, &caps, "/dir", &path("d"))
        .expect("interior link");
    svc.symlink(TEST_UID, &caps, "file", &path("dir/f"))
        .expect("final link");

    let canonical = alloc::format!("{MOUNT}/dir/file");
    for spelling in ["dir/file", "d/file", "dir/f", "d/f"] {
        assert_eq!(
            svc.realpath(TEST_UID, &caps, &path(spelling), RealpathMode::Existing),
            Ok(canonical.clone()),
            "canonicalising {spelling}"
        );
    }
    // The mount point itself is its own canonical name.
    assert_eq!(
        svc.realpath(TEST_UID, &caps, MOUNT, RealpathMode::Existing),
        Ok(alloc::string::String::from(MOUNT))
    );
}

#[test]
fn realpath_resolves_dot_dot_through_a_link_physically() {
    let svc = ready_traversable();
    let caps = caps();
    let create = OpenFlags::CREATE.union(OpenFlags::WRITE);

    // A caller may not spell `..` — the path grammar refuses it — so the
    // only `..` a resolution ever meets comes from a link's stored target.
    // `here/link` names `/there`, and `/there/up` stores `../sibling`: the
    // `..` must pop the directory the walk really came through (`/there`),
    // reaching the real `/sibling`, never the `/here/sibling` decoy a
    // lexical collapse of `here/link/..` would name. Both exist, so a
    // lexical answer would look plausible.
    svc.mkdir(TEST_UID, &caps, &path("here")).expect("here");
    svc.mkdir(TEST_UID, &caps, &path("there")).expect("there");
    svc.open(TEST_UID, &caps, &path("sibling"), create)
        .expect("real sibling");
    svc.open(TEST_UID, &caps, &path("here/sibling"), create)
        .expect("decoy sibling");
    svc.symlink(TEST_UID, &caps, "/there", &path("here/link"))
        .expect("link");
    svc.symlink(TEST_UID, &caps, "../sibling", &path("there/up"))
        .expect("ascending link");

    assert_eq!(
        svc.realpath(
            TEST_UID,
            &caps,
            &path("here/link/up"),
            RealpathMode::Existing
        ),
        Ok(alloc::format!("{MOUNT}/sibling"))
    );
}

#[test]
fn realpath_modes_decide_how_much_of_the_path_must_exist() {
    let svc = ready_traversable();
    let caps = caps();

    svc.mkdir(TEST_UID, &caps, &path("dir")).expect("dir");

    // A vacant final name: refused by `-e`, named by `-f` and `-m`.
    let vacant = path("dir/absent");
    let vacant_canonical = alloc::format!("{MOUNT}/dir/absent");
    assert_eq!(
        svc.realpath(TEST_UID, &caps, &vacant, RealpathMode::Existing),
        Err(Errno::NotFound)
    );
    for mode in [RealpathMode::Final, RealpathMode::Missing] {
        assert_eq!(
            svc.realpath(TEST_UID, &caps, &vacant, mode),
            Ok(vacant_canonical.clone())
        );
    }

    // A missing *ancestor*: only `-m` names it.
    let deep = path("gone/away/leaf");
    for mode in [RealpathMode::Existing, RealpathMode::Final] {
        assert_eq!(
            svc.realpath(TEST_UID, &caps, &deep, mode),
            Err(Errno::NotFound)
        );
    }
    assert_eq!(
        svc.realpath(TEST_UID, &caps, &deep, RealpathMode::Missing),
        Ok(alloc::format!("{MOUNT}/gone/away/leaf"))
    );
}

#[test]
fn realpath_of_a_dangling_link_follows_its_target() {
    let svc = ready_traversable();
    let caps = caps();

    svc.mkdir(TEST_UID, &caps, &path("dir")).expect("dir");
    // A dangling link whose parent exists: `-f` names the target, `-e`
    // refuses it. The link's own name is never the answer.
    svc.symlink(TEST_UID, &caps, "/dir/nothing", &path("dangling"))
        .expect("dangling link");
    let link = path("dangling");
    assert_eq!(
        svc.realpath(TEST_UID, &caps, &link, RealpathMode::Existing),
        Err(Errno::NotFound)
    );
    assert_eq!(
        svc.realpath(TEST_UID, &caps, &link, RealpathMode::Final),
        Ok(alloc::format!("{MOUNT}/dir/nothing"))
    );

    // A dangling link whose target's *parent* is absent too: `-f` refuses
    // it (only the last component may be missing), `-m` names it.
    svc.symlink(TEST_UID, &caps, "/gone/leaf", &path("deeper"))
        .expect("deeper link");
    let deeper = path("deeper");
    assert_eq!(
        svc.realpath(TEST_UID, &caps, &deeper, RealpathMode::Final),
        Err(Errno::NotFound)
    );
    assert_eq!(
        svc.realpath(TEST_UID, &caps, &deeper, RealpathMode::Missing),
        Ok(alloc::format!("{MOUNT}/gone/leaf"))
    );
}

#[test]
fn realpath_refuses_a_cycle_and_needs_search_permission_on_the_way() {
    let svc = ready_traversable();
    let caps = caps();

    svc.symlink(TEST_UID, &caps, "/loop", &path("loop"))
        .expect("self-cycle");
    for mode in [
        RealpathMode::Existing,
        RealpathMode::Final,
        RealpathMode::Missing,
    ] {
        assert_eq!(
            svc.realpath(TEST_UID, &caps, &path("loop"), mode),
            Err(Errno::LinkLoop),
            "a cycle is refused under every reading"
        );
    }

    // A directory nobody may search — its owner included, since TAIRiX has
    // no ambient root — hides what is under it from canonicalisation exactly
    // as it does from a read. Asked under the most permissive reading, so it
    // is the permission check refusing and not the absence.
    svc.mkdir(TEST_UID, &caps, &path("closed")).expect("closed");
    svc.set_mode(TEST_UID, &caps, &path("closed"), 0o600)
        .expect("drop the search bit");
    assert_eq!(
        svc.realpath(
            TEST_UID,
            &caps,
            &path("closed/inside"),
            RealpathMode::Missing
        ),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn realpath_answers_a_submount_with_its_own_mount_point() {
    // A sub-mount projects a subtree, so a canonical path names the mount
    // point rather than the subtree's path on the backing volume — and a
    // link inside it that tries to escape resolves within the projection.
    let h = DriverHandle::from_raw(9).expect("handle");
    let mut volume = dir_driver();
    let vroot = volume.root();
    let area = volume
        .create(vroot, b"Area", NodeKind::Directory)
        .expect("subtree");
    volume
        .create(area, b"inside", NodeKind::RegularFile)
        .expect("file");
    volume
        .create_link(area, b"up", b"../inside")
        .expect("ascending link");

    let mut vfs = Vfs::with_default_layout(UserId(TEST_UID), GroupId(TEST_GID));
    let caps = caps();
    let cred = Credentials {
        uid: UserId(TEST_UID),
        gid: GroupId(TEST_GID),
        supplementary_gids: &[],
        caps: &caps,
    };
    vfs.mkdir(
        &cred,
        &Path::parse("/Storage/area").expect("path"),
        Mode::from_bits(0o755),
    )
    .expect("mount point");
    vfs.mounts_write()
        .mount_rebased(
            Path::parse("/Storage/area").expect("path"),
            MountFlags::NOSUID,
            Some(MountBacking::new(h, None)),
            alloc::vec![alloc::string::String::from("Area")],
        )
        .expect("rebased mount");
    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs).expect("install vfs");
    cell.register(h, volume, "vol", "memfs", [0u8; 16])
        .expect("register");
    let identity: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    identity.install(identity_table()).expect("identity");
    let svc = MountedFilesystemService::new(cell, identity);

    let canonical = alloc::string::String::from("/Storage/area/inside");
    assert_eq!(
        svc.realpath(
            TEST_UID,
            &caps,
            "/Storage/area/inside",
            RealpathMode::Existing
        ),
        Ok(canonical.clone())
    );
    assert_eq!(
        svc.realpath(TEST_UID, &caps, "/Storage/area/up", RealpathMode::Existing),
        Ok(canonical)
    );
}
