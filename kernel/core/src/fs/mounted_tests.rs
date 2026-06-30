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
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    FilesystemRead, FilesystemWrite, MountFlags, NodeKind, NodeSecurity,
};
use rustos_abi::driver::DriverHandle;
use rustos_abi::{Errno, FileKind, OpenFlags};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{
    GroupId, GroupRecord, IdentityTable, IdentityTableBuilder, UserId, UserRecord,
};
use rustos_log::{Event, Sink};

use super::{LateFilesystem, LateIdentity, MountedFilesystemService};
use crate::fs::memfs::RwMockFs;
use crate::fs::perm::Credentials;
use crate::fs::service::FilesystemService;
use crate::fs::{Mode, Path, Vfs};

/// The test principal that owns the mounted volume's files.
const TEST_UID: u32 = 1000;
const TEST_GID: u32 = 1000;
/// The mount point the in-memory driver is mounted at.
const MOUNT: &str = "/Storage/vol";

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
    vfs.mounts_mut()
        .mount(mount, flags, Some(handle))
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
    vfs.mounts_mut()
        .mount_rebased(
            Path::parse("/Storage/logs").expect("path"),
            nosuid,
            Some(h_shared),
            alloc::vec![alloc::string::String::from("LogsArea")],
        )
        .expect("logs mount");
    vfs.mounts_mut()
        .mount_rebased(
            Path::parse("/Storage/settings").expect("path"),
            nosuid,
            Some(h_shared),
            alloc::vec![alloc::string::String::from("SettingsArea")],
        )
        .expect("settings mount");
    vfs.mounts_mut()
        .mount(
            Path::parse("/Storage/other").expect("path"),
            nosuid,
            Some(h_other),
        )
        .expect("other mount");

    let cell: &'static LateFilesystem<RwMockFs> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_vfs(vfs).expect("install vfs");
    cell.register(h_shared, shared).expect("register shared");
    cell.register(h_other, other).expect("register other");
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
        cell.register(handle, driver()).expect("register driver");
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
        svc.stat(TEST_UID, &caps, &path("x")),
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
        svc.stat(TEST_UID, &caps, &path("x")),
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
        svc.stat(4242, &caps, &path("x")),
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

    let st = svc.stat(TEST_UID, &caps, &p).expect("stat");
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

    let mut entries = svc.readdir(TEST_UID, &caps, MOUNT).expect("readdir");
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        entries,
        alloc::vec![
            (FileKind::Regular, alloc::string::String::from("file")),
            (FileKind::Directory, alloc::string::String::from("sub")),
        ]
    );
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
    svc.unlink(TEST_UID, &caps, &p).expect("unlink");
    let mut buf = [0u8; 4];
    assert_eq!(
        svc.read(TEST_UID, &caps, &p, 0, &mut buf),
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
    assert_eq!(svc.stat(TEST_UID, &caps, &p).expect("stat").size, 4);
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
    // `AlreadyExists` collapses onto `OutOfRange` at the ABI boundary.
    assert_eq!(
        svc.open(TEST_UID, &caps, &p, exclusive),
        Err(Errno::OutOfRange)
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
    // `NotADirectory` collapses onto `OutOfRange` at the ABI boundary.
    assert_eq!(
        svc.open(TEST_UID, &caps, &p, OpenFlags::DIRECTORY),
        Err(Errno::OutOfRange)
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
fn rename_before_a_mount_is_installed_fails_closed() {
    let svc = service(false, true, false);
    let caps = caps();
    assert_eq!(
        svc.rename(TEST_UID, &caps, &path("a"), &path("b")),
        Err(Errno::NotImplemented)
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
