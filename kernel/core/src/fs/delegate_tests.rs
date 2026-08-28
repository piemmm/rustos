//! Behavioural tests for driver delegation: the [`Vfs`] resolving a path
//! under a driver-backed mount through a [`FilesystemRead`] driver, with the
//! permission template applied at the mount point.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fs::delegate::SYMLINK_HOP_MAX;
use crate::fs::{
    FinalLink, Mode, MountBacking, Path, Vfs, VfsError, MAX_COMPONENT_LEN, MAX_PATH_COMPONENTS,
};

use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemWrite, MountFlags, NodeId, NodeInfo, NodeKind, NodeTimes,
};
use tairix_abi::driver::{DriverError, DriverHandle};
use tairix_abi::fs::{OpenFlags, RealpathMode};
use tairix_abi::time::Time64;
use tairix_abi::Errno;
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::{GroupId, UserId};

use crate::fs::perm::Credentials;

// The in-memory read/write driver fixture is shared with the mounted-service
// tests; the one definition lives in `crate::fs::memfs` (no per-test copy).
use crate::fs::memfs::{RwMockFs, ADMIN_GID, ADMIN_UID};

/// The fixed last-modification stamp `MockFs` reports for every entry, so
/// the tests can assert the stamp travels through the delegation unchanged.
const MOCK_MODIFIED: Time64 = Time64::from_secs(1_234_567);
/// The [`NodeTimes`] `MockFs` reports for every node (only `modified` is
/// non-trivial, so the listing/stat stamp path is exercised end to end).
const MOCK_TIMES: NodeTimes = NodeTimes {
    created: Time64::UNIX_EPOCH,
    modified: MOCK_MODIFIED,
    accessed: Time64::UNIX_EPOCH,
    changed: Time64::UNIX_EPOCH,
};

fn p(text: &str) -> Path {
    Path::parse(text).expect("valid path")
}

/// A mount backing by driver `raw`. Delegation is indifferent to the
/// device medium, so these fixtures record it as unknown rather than
/// naming one no mock device reported.
fn backing_of(raw: u64) -> MountBacking {
    MountBacking::new(DriverHandle::from_raw(raw).expect("non-zero handle"), None)
}

fn cred(uid: u32, gid: u32, caps: &CapabilitySet) -> Credentials<'_> {
    Credentials {
        uid: UserId(uid),
        gid: GroupId(gid),
        supplementary_gids: &[],
        caps,
    }
}

/// A fixed, allocation-free `FilesystemRead` over the tree
///
/// ```text
/// /                 (root, dir)
/// ├── docs/         (dir)
/// │   └── readme.txt  "hello world"
/// └── kernel.img    "ELF\0"
/// ```
struct MockFs;

const ROOT: u64 = 1;
const DOCS: u64 = 2;
const KERNEL: u64 = 3;
const README: u64 = 4;

const KERNEL_BODY: &[u8] = b"ELF\0";
const README_BODY: &[u8] = b"hello world";

impl FilesystemRead for MockFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        match node.raw() {
            ROOT | DOCS => Ok(NodeInfo {
                kind: NodeKind::Directory,
                nlink: 2,
                size: 0,
                allocated: 0,
                times: MOCK_TIMES,
            }),
            KERNEL => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: KERNEL_BODY.len() as u64,
                allocated: KERNEL_BODY.len() as u64,
                times: MOCK_TIMES,
            }),
            README => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: README_BODY.len() as u64,
                allocated: README_BODY.len() as u64,
                times: MOCK_TIMES,
            }),
            _ => Err(DriverError::NotFound),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        match dir.raw() {
            ROOT => match name {
                b"docs" => Ok(NodeId::from_raw(DOCS)),
                b"kernel.img" => Ok(NodeId::from_raw(KERNEL)),
                _ => Err(DriverError::NotFound),
            },
            DOCS => match name {
                b"readme.txt" => Ok(NodeId::from_raw(README)),
                _ => Err(DriverError::NotFound),
            },
            KERNEL | README => Err(DriverError::Unsupported),
            _ => Err(DriverError::NotFound),
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        let body = match file.raw() {
            KERNEL => KERNEL_BODY,
            README => README_BODY,
            ROOT | DOCS => return Err(DriverError::Unsupported),
            _ => return Err(DriverError::NotFound),
        };
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= body.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), body.len() - start);
        buf[..n].copy_from_slice(&body[start..start + n]);
        Ok(n)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let entries: &[(&[u8], u64)] = match dir.raw() {
            ROOT => &[(b"docs", DOCS), (b"kernel.img", KERNEL)],
            DOCS => &[(b"readme.txt", README)],
            KERNEL | README => return Err(DriverError::Unsupported),
            _ => return Err(DriverError::NotFound),
        };
        let Ok(i) = usize::try_from(cursor) else {
            return Ok(None);
        };
        let Some(&(name, node)) = entries.get(i) else {
            return Ok(None);
        };
        if name_out.len() < name.len() {
            return Err(DriverError::BufferTooSmall);
        }
        name_out[..name.len()].copy_from_slice(name);
        let info = self.node_info(NodeId::from_raw(node))?;
        Ok(Some(DirEntry {
            node: NodeId::from_raw(node),
            info,
            name_len: name.len(),
            next_cursor: cursor + 1,
        }))
    }
}

/// A driver whose root holds one file `x`, but which faults every byte read
/// and reports a non-UTF-8 directory entry — to exercise the [`VfsError::Io`]
/// paths.
struct BadFs;

impl FilesystemRead for BadFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        match node.raw() {
            ROOT => Ok(NodeInfo {
                kind: NodeKind::Directory,
                nlink: 2,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            }),
            DOCS => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: 3,
                allocated: 3,
                times: NodeTimes::default(),
            }),
            _ => Err(DriverError::NotFound),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        if dir.raw() == ROOT && name == b"x" {
            Ok(NodeId::from_raw(DOCS))
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn read_at(
        &mut self,
        _file: NodeId,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        if dir.raw() != ROOT || cursor != 0 {
            return Ok(None);
        }
        // A name that is not valid UTF-8.
        name_out[0] = 0xff;
        name_out[1] = 0xff;
        Ok(Some(DirEntry {
            node: NodeId::from_raw(DOCS),
            info: NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            },
            name_len: 2,
            next_cursor: 1,
        }))
    }
}

/// A default-layout VFS with `/Storage/usb0` created and mounted as a
/// driver-backed mount (owner `admin`, mode `mount_mode`).
fn backed_vfs(mount_mode: u16) -> Vfs {
    let mut vfs = Vfs::with_default_layout(UserId(ADMIN_UID), GroupId(ADMIN_GID));
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    vfs.mkdir(&admin, &p("/Storage/usb0"), Mode::from_bits(mount_mode))
        .expect("create mount point");
    let backing = backing_of(7);
    vfs.mounts_write()
        .mount(p("/Storage/usb0"), MountFlags::READ_ONLY, Some(backing))
        .expect("mount backed");
    vfs
}

#[test]
fn delegated_read_resolves_through_subdir() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let mut buf = [0u8; 32];
    let n = vfs
        .read_via(
            &admin,
            &p("/Storage/usb0/docs/readme.txt"),
            &mut fs,
            0,
            &mut buf,
        )
        .expect("delegated read");
    assert_eq!(&buf[..n], README_BODY);
}

#[test]
fn delegated_read_honours_offset_and_eof() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let mut buf = [0u8; 32];
    let path = p("/Storage/usb0/docs/readme.txt");
    let n = vfs
        .read_via(&admin, &path, &mut fs, 6, &mut buf)
        .expect("read");
    assert_eq!(&buf[..n], b"world");
    // Reading at EOF yields zero bytes.
    assert_eq!(
        vfs.read_via(&admin, &path, &mut fs, README_BODY.len() as u64, &mut buf),
        Ok(0)
    );
}

/// The uniform-policy half of the canonicalisation pair answers the same
/// `/`-view spelling as the per-inode half: the mount point followed by the
/// canonical remainder, and the mount point itself for the mount point.
#[test]
fn delegated_realpath_spells_a_view_path_under_the_mount_point() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    assert_eq!(
        vfs.realpath_via(
            &admin,
            &p("/Storage/usb0/docs/readme.txt"),
            &mut fs,
            RealpathMode::Existing
        ),
        Ok(String::from("/Storage/usb0/docs/readme.txt"))
    );
    // The mount point is its own canonical name: the walk lands on the
    // projected root, which contributes no component of its own.
    assert_eq!(
        vfs.realpath_via(&admin, &p("/Storage/usb0"), &mut fs, RealpathMode::Existing),
        Ok(String::from("/Storage/usb0"))
    );
    // A vacant final name is refused under the strict reading and carried
    // into the answer under the tolerant one.
    let vacant = p("/Storage/usb0/docs/absent");
    assert_eq!(
        vfs.realpath_via(&admin, &vacant, &mut fs, RealpathMode::Existing),
        Err(VfsError::NotFound)
    );
    assert_eq!(
        vfs.realpath_via(&admin, &vacant, &mut fs, RealpathMode::Final),
        Ok(String::from("/Storage/usb0/docs/absent"))
    );
    // Only `Missing` names a path whose *ancestor* is absent too.
    let deep = p("/Storage/usb0/gone/away/leaf");
    assert_eq!(
        vfs.realpath_via(&admin, &deep, &mut fs, RealpathMode::Final),
        Err(VfsError::NotFound)
    );
    assert_eq!(
        vfs.realpath_via(&admin, &deep, &mut fs, RealpathMode::Missing),
        Ok(String::from("/Storage/usb0/gone/away/leaf"))
    );
    // A directory the caller may not search hides what is under it from
    // canonicalisation exactly as it does from a read: under the uniform
    // policy the mount point's own mode is what every node is judged
    // against, so a mount with no search bit refuses at the first step.
    let closed = backed_vfs(0o600);
    assert_eq!(
        closed.realpath_via(&admin, &deep, &mut fs, RealpathMode::Missing),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn delegated_list_of_mount_point_lists_driver_root() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let names = vfs
        .list_via(&admin, &p("/Storage/usb0"), &mut fs, FinalLink::Follow)
        .expect("list mount root");
    let kinds: Vec<(NodeKind, String)> = names
        .into_iter()
        .map(|entry| (entry.info.kind, entry.name))
        .collect();
    assert_eq!(
        kinds,
        [
            (NodeKind::Directory, String::from("docs")),
            (NodeKind::RegularFile, String::from("kernel.img")),
        ]
    );
}

#[test]
fn delegated_list_of_subdir() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let names = vfs
        .list_via(&admin, &p("/Storage/usb0/docs"), &mut fs, FinalLink::Follow)
        .expect("list subdir");
    let entries: Vec<(NodeKind, u64, Time64, String)> = names
        .into_iter()
        .map(|entry| {
            (
                entry.info.kind,
                entry.info.size,
                entry.info.times.modified,
                entry.name,
            )
        })
        .collect();
    // The listing carries the child's own size and modification stamp,
    // read once by the driver.
    assert_eq!(
        entries,
        [(
            NodeKind::RegularFile,
            README_BODY.len() as u64,
            MOCK_MODIFIED,
            String::from("readme.txt")
        )]
    );
}

#[test]
fn delegated_stat_reports_kind_size_and_template() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let file = vfs
        .stat_via(
            &admin,
            &p("/Storage/usb0/kernel.img"),
            &mut fs,
            FinalLink::Follow,
        )
        .expect("stat file");
    assert_eq!(file.kind, NodeKind::RegularFile);
    assert_eq!(file.size, KERNEL_BODY.len() as u64);
    // The permission template is the mount point's metadata.
    assert_eq!(file.meta.owner, UserId(ADMIN_UID));
    assert_eq!(file.meta.mode, Mode::from_bits(0o755));

    let dir = vfs
        .stat_via(&admin, &p("/Storage/usb0/docs"), &mut fs, FinalLink::Follow)
        .expect("stat dir");
    assert_eq!(dir.kind, NodeKind::Directory);
    assert_eq!(dir.size, 0);
}

#[test]
fn delegated_read_of_directory_is_is_a_directory() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(&admin, &p("/Storage/usb0/docs"), &mut fs, 0, &mut buf),
        Err(VfsError::IsADirectory)
    );
}

#[test]
fn delegated_list_of_file_is_not_a_directory() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    assert_eq!(
        vfs.list_via(
            &admin,
            &p("/Storage/usb0/kernel.img"),
            &mut fs,
            FinalLink::Follow
        ),
        Err(VfsError::NotADirectory)
    );
}

#[test]
fn delegated_missing_child_is_not_found() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(&admin, &p("/Storage/usb0/docs/nope"), &mut fs, 0, &mut buf),
        Err(VfsError::NotFound)
    );
}

#[test]
fn delegation_on_non_backed_mount_is_not_found() {
    // `/Users` is not driver-backed, so the delegated path has no driver.
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(&admin, &p("/Users/admin/x"), &mut fs, 0, &mut buf),
        Err(VfsError::NotFound)
    );
}

#[test]
fn delegated_traversal_enforces_search_permission_on_template() {
    // Mount point mode 0700 owned by admin: a different user has no execute
    // (search) bit, so descending into the delegated subtree is denied.
    let vfs = backed_vfs(0o700);
    let caps = CapabilitySet::empty();
    let other = cred(9, 9, &caps);
    let mut fs = MockFs;
    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(
            &other,
            &p("/Storage/usb0/docs/readme.txt"),
            &mut fs,
            0,
            &mut buf
        ),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn device_fault_surfaces_as_io() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = BadFs;
    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(&admin, &p("/Storage/usb0/x"), &mut fs, 0, &mut buf),
        Err(VfsError::Io)
    );
}

#[test]
fn non_utf8_directory_name_surfaces_as_io() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = BadFs;
    assert_eq!(
        vfs.list_via(&admin, &p("/Storage/usb0"), &mut fs, FinalLink::Follow),
        Err(VfsError::Io)
    );
}

// ---------------------------------------------------------------------
// Write-path delegation tests.
//
// The in-memory `RwMockFs` (read + write + security) standing in for a
// block-backed driver lives in `crate::fs::memfs`, shared with the
// mounted-service tests (kernel/core may not depend on `drivers/*`).
// ---------------------------------------------------------------------

use tairix_abi::driver::filesystem::{FilesystemSecurity, NodeSecurity};
use tairix_abi::CapabilityId;

/// A default-layout VFS with `/Storage/usb0` mounted writable (no
/// `READ_ONLY` flag), owner `admin`, mode `mount_mode`.
fn backed_vfs_rw(mount_mode: u16) -> Vfs {
    let mut vfs = Vfs::with_default_layout(UserId(ADMIN_UID), GroupId(ADMIN_GID));
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    vfs.mkdir(&admin, &p("/Storage/usb0"), Mode::from_bits(mount_mode))
        .expect("create mount point");
    let backing = backing_of(8);
    let flags = MountFlags::from_bits(0).expect("empty flags");
    vfs.mounts_write()
        .mount(p("/Storage/usb0"), flags, Some(backing))
        .expect("mount backed");
    vfs
}

#[test]
fn delegated_create_then_write_and_read_back() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/notes.txt");

    vfs.create_via(&admin, &path, &mut fs).expect("create");
    assert_eq!(vfs.write_via(&admin, &path, &mut fs, 0, b"hi there"), Ok(8));

    let mut buf = [0u8; 16];
    let n = vfs
        .read_via(&admin, &path, &mut fs, 0, &mut buf)
        .expect("read");
    assert_eq!(&buf[..n], b"hi there");
}

#[test]
fn delegated_mkdir_then_create_inside() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();

    vfs.mkdir_via(&admin, &p("/Storage/usb0/sub"), &mut fs)
        .expect("mkdir");
    let inner = p("/Storage/usb0/sub/inner.bin");
    vfs.create_via(&admin, &inner, &mut fs)
        .expect("create inside");
    vfs.write_via(&admin, &inner, &mut fs, 0, b"nested")
        .expect("write inside");

    let names = vfs
        .list_via(&admin, &p("/Storage/usb0/sub"), &mut fs, FinalLink::Follow)
        .expect("list");
    let kinds: Vec<(NodeKind, String)> = names
        .into_iter()
        .map(|entry| (entry.info.kind, entry.name))
        .collect();
    assert_eq!(kinds, [(NodeKind::RegularFile, String::from("inner.bin"))]);
}

#[test]
fn delegated_truncate_changes_size() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/t.bin");
    vfs.create_via(&admin, &path, &mut fs).expect("create");
    vfs.write_via(&admin, &path, &mut fs, 0, b"0123456789")
        .expect("write");
    vfs.truncate_via(&admin, &path, &mut fs, 4)
        .expect("truncate");
    assert_eq!(
        vfs.stat_via(&admin, &path, &mut fs, FinalLink::Follow)
            .expect("stat")
            .size,
        4
    );
}

#[test]
fn delegated_remove_unlinks() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/gone.txt");
    vfs.create_via(&admin, &path, &mut fs).expect("create");
    vfs.remove_via(&admin, &path, &mut fs, false)
        .expect("remove");
    let mut buf = [0u8; 4];
    assert_eq!(
        vfs.read_via(&admin, &path, &mut fs, 0, &mut buf),
        Err(VfsError::NotFound)
    );
}

#[test]
fn delegated_create_existing_is_already_exists() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/dup.txt");
    vfs.create_via(&admin, &path, &mut fs).expect("create");
    assert_eq!(
        vfs.create_via(&admin, &path, &mut fs),
        Err(VfsError::AlreadyExists)
    );
}

#[test]
fn delegated_write_to_directory_is_is_a_directory() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.mkdir_via(&admin, &p("/Storage/usb0/d"), &mut fs)
        .expect("mkdir");
    assert_eq!(
        vfs.write_via(&admin, &p("/Storage/usb0/d"), &mut fs, 0, b"x"),
        Err(VfsError::IsADirectory)
    );
}

#[test]
fn delegated_remove_non_empty_directory_is_not_empty() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.mkdir_via(&admin, &p("/Storage/usb0/d"), &mut fs)
        .expect("mkdir");
    vfs.create_via(&admin, &p("/Storage/usb0/d/f"), &mut fs)
        .expect("create child");
    assert_eq!(
        vfs.remove_via(&admin, &p("/Storage/usb0/d"), &mut fs, false),
        Err(VfsError::NotEmpty)
    );
}

#[test]
fn delegated_dir_only_remove_of_a_file_is_not_a_directory() {
    // The atomic `rmdir` posture: a directory-only removal reaching a file
    // is refused in the same locked walk that would remove it, and the file
    // survives.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/plain.txt");
    vfs.create_via(&admin, &path, &mut fs).expect("create");
    assert_eq!(
        vfs.remove_via(&admin, &path, &mut fs, true),
        Err(VfsError::NotADirectory)
    );
    vfs.stat_via(&admin, &path, &mut fs, FinalLink::Follow)
        .expect("file survives");
}

#[test]
fn delegated_dir_only_remove_of_an_empty_directory_succeeds() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.mkdir_via(&admin, &p("/Storage/usb0/d"), &mut fs)
        .expect("mkdir");
    vfs.remove_via(&admin, &p("/Storage/usb0/d"), &mut fs, true)
        .expect("dir-only remove of an empty directory");
    assert_eq!(
        vfs.stat_via(&admin, &p("/Storage/usb0/d"), &mut fs, FinalLink::Follow),
        Err(VfsError::NotFound)
    );
}

#[test]
fn delegated_write_on_read_only_mount_is_read_only() {
    // `backed_vfs` mounts with `READ_ONLY`.
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    assert_eq!(
        vfs.create_via(&admin, &p("/Storage/usb0/x"), &mut fs),
        Err(VfsError::ReadOnly)
    );
}

#[test]
fn delegated_create_without_write_permission_is_denied() {
    // Mount mode 0755 owned by admin: a different user has search but no
    // write bit, so creating in the directory is denied.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let other = cred(9, 9, &caps);
    let mut fs = RwMockFs::new();
    assert_eq!(
        vfs.create_via(&other, &p("/Storage/usb0/x"), &mut fs),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn delegated_mutation_of_mount_root_is_invalid() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    assert_eq!(
        vfs.create_via(&admin, &p("/Storage/usb0"), &mut fs),
        Err(VfsError::InvalidPath)
    );
}

/// A VFS whose **root** mount is driver-backed (the production shape once
/// the writable root volume backs `/`), so a delegated create at `/<name>`
/// lands a top-level directory on the volume.
fn root_backed_rw_vfs() -> Vfs {
    let vfs = Vfs::with_default_layout(UserId(ADMIN_UID), GroupId(ADMIN_GID));
    vfs.mounts_write()
        .back_root(backing_of(9))
        .expect("back the root mount");
    vfs
}

#[test]
fn delegated_create_of_legacy_top_level_name_is_allowed() {
    // With the writable root volume backing `/`, a delegated mkdir/create
    // of a legacy POSIX top-level name is *not* refused by the VFS: the OS
    // never authors these names, but it does not police a user's own
    // request — a top-level create is governed by write permission on the
    // root directory like any other. Rename is likewise not policed.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();

    for name in ["/etc", "/home", "/usr", "/bin", "/tmp", "/dev", "/proc"] {
        vfs.mkdir_via(&admin, &p(name), &mut fs)
            .unwrap_or_else(|e| panic!("delegated mkdir of {name} is allowed, got {e:?}"));
    }

    vfs.mkdir_via(&admin, &p("/Scratch"), &mut fs)
        .expect("create a renameable source");
    vfs.rename_via(&admin, &p("/Scratch"), &p("/var"), &mut fs)
        .expect("delegated rename into a legacy top-level name is allowed");
}

#[test]
fn delegated_rename_across_two_backed_mounts_is_cross_volume() {
    // A rename cannot preserve a node's identity across two independent
    // backings, so it is refused with the dedicated `CrossVolume` error
    // (the EXDEV-equivalent `mv` falls back to copy-then-remove on) —
    // never a generic path error a caller could not act on.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let flags = MountFlags::from_bits(0).expect("empty flags");
    vfs.mounts_write()
        .mount(p("/Storage/usb0"), flags, Some(backing_of(11)))
        .expect("mount a second backed volume");
    let mut fs = RwMockFs::new();
    vfs.mkdir_via(&admin, &p("/Scratch"), &mut fs)
        .expect("create a renameable source on the root volume");
    assert_eq!(
        vfs.rename_via(&admin, &p("/Scratch"), &p("/Storage/usb0/Scratch"), &mut fs),
        Err(VfsError::CrossVolume)
    );
}

#[test]
fn delegated_link_gives_one_node_a_second_name() {
    // The defining property at the VFS seam: both names resolve to one node,
    // the count rises, and nothing was copied.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.create_via(&admin, &p("/file"), &mut fs)
        .expect("create the node");
    vfs.write_via(&admin, &p("/file"), &mut fs, 0, b"body")
        .expect("write it");

    vfs.link_via(&admin, &p("/file"), &p("/alias"), &mut fs, FinalLink::Keep)
        .expect("add a second name");
    let first = vfs
        .stat_via(&admin, &p("/file"), &mut fs, FinalLink::Keep)
        .expect("stat the first name");
    let second = vfs
        .stat_via(&admin, &p("/alias"), &mut fs, FinalLink::Keep)
        .expect("stat the second name");
    assert_eq!(first.node, second.node);
    assert_eq!(second.nlink, 2);

    // One node, so a write through either name is one file's bytes.
    let mut buf = [0u8; 4];
    let read = vfs
        .read_via(&admin, &p("/alias"), &mut fs, 0, &mut buf)
        .expect("read through the second name");
    assert_eq!(&buf[..read], b"body");
}

#[test]
fn delegated_link_refuses_a_directory_before_touching_the_new_parent() {
    // The refusal is the VFS's, not each driver's: the tree staying a tree is
    // what makes the resolver's physical `..` well-defined.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.mkdir_via(&admin, &p("/dir"), &mut fs)
        .expect("create a directory");
    assert_eq!(
        vfs.link_via(&admin, &p("/dir"), &p("/alias"), &mut fs, FinalLink::Keep),
        Err(VfsError::IsADirectory)
    );
    assert_eq!(
        vfs.stat_via(&admin, &p("/alias"), &mut fs, FinalLink::Keep)
            .map(|i| i.size),
        Err(VfsError::NotFound)
    );
}

#[test]
fn delegated_link_takes_its_posture_from_the_caller_for_the_existing_name() {
    // `Keep` is POSIX `link()` (the link itself gains the name) and `Follow`
    // is `linkat(AT_SYMLINK_FOLLOW)` (its target does). The *new* name is
    // never followed under either.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.create_via(&admin, &p("/target"), &mut fs)
        .expect("create the target");
    vfs.symlink_via(&admin, &p("/sym"), &mut fs, "/target")
        .expect("create the symbolic link");

    vfs.link_via(&admin, &p("/sym"), &p("/kept"), &mut fs, FinalLink::Keep)
        .expect("name the link itself");
    assert_eq!(
        vfs.stat_via(&admin, &p("/kept"), &mut fs, FinalLink::Keep)
            .expect("stat")
            .kind,
        NodeKind::Symlink
    );

    vfs.link_via(
        &admin,
        &p("/sym"),
        &p("/followed"),
        &mut fs,
        FinalLink::Follow,
    )
    .expect("name what the link names");
    assert_eq!(
        vfs.stat_via(&admin, &p("/followed"), &mut fs, FinalLink::Keep)
            .expect("stat")
            .kind,
        NodeKind::RegularFile
    );
}

#[test]
fn delegated_link_refuses_a_taken_name_and_a_missing_source() {
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.create_via(&admin, &p("/file"), &mut fs)
        .expect("create the node");
    vfs.create_via(&admin, &p("/taken"), &mut fs)
        .expect("create the occupant");
    assert_eq!(
        vfs.link_via(&admin, &p("/file"), &p("/taken"), &mut fs, FinalLink::Keep),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(
        vfs.link_via(
            &admin,
            &p("/absent"),
            &p("/alias"),
            &mut fs,
            FinalLink::Keep
        ),
        Err(VfsError::NotFound)
    );
    // Neither refusal raised the count or left a name behind.
    assert_eq!(
        vfs.stat_via(&admin, &p("/file"), &mut fs, FinalLink::Keep)
            .expect("stat")
            .nlink,
        1
    );
}

#[test]
fn delegated_link_reports_a_format_that_holds_one_name_per_node() {
    // The permanent format limit a caller can tell apart from a structural
    // refusal, exactly as `symlink` reports one.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = NoLinksFs::new();
    // The fixture's one creatable child, so the existing name resolves and
    // the refusal is the driver's own rather than a missing source.
    vfs.create_via(&admin, &p("/file"), &mut fs)
        .expect("the format creates ordinary files perfectly well");
    assert_eq!(
        vfs.link_via(&admin, &p("/file"), &p("/alias"), &mut fs, FinalLink::Keep),
        Err(VfsError::NotSupported)
    );
}

#[test]
fn delegated_link_at_the_formats_name_ceiling_fails_closed() {
    // The per-node count is a fixed on-disk bound, not a capacity to grow:
    // one more name fails closed with its own errno rather than wrapping a
    // count whose zero would free storage a live name still reaches.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    vfs.create_via(&admin, &p("/file"), &mut fs)
        .expect("create the node");
    let node = fs.lookup(NodeId::from_raw(1), b"file").expect("the node");
    fs.set_link_count(node, u32::MAX);

    assert_eq!(
        vfs.link_via(&admin, &p("/file"), &p("/alias"), &mut fs, FinalLink::Keep),
        Err(VfsError::TooManyLinks)
    );
    assert_eq!(
        vfs.stat_via(&admin, &p("/alias"), &mut fs, FinalLink::Keep)
            .map(|i| i.size),
        Err(VfsError::NotFound),
        "nothing was created"
    );
}

#[test]
fn delegated_link_across_two_backed_mounts_is_cross_volume() {
    // A hard link is a second directory entry for one inode, and a directory
    // entry addresses an inode in its own backing — so the pair cannot span
    // two volumes, and the refusal is the same dedicated `CrossVolume` a
    // rename gives rather than a generic path error.
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let flags = MountFlags::from_bits(0).expect("empty flags");
    vfs.mounts_write()
        .mount(p("/Storage/usb0"), flags, Some(backing_of(11)))
        .expect("mount a second backed volume");
    let mut fs = RwMockFs::new();
    vfs.create_via(&admin, &p("/file"), &mut fs)
        .expect("create a linkable node on the root volume");
    assert_eq!(
        vfs.link_via(
            &admin,
            &p("/file"),
            &p("/Storage/usb0/alias"),
            &mut fs,
            FinalLink::Keep,
        ),
        Err(VfsError::CrossVolume)
    );
}

// ---------------------------------------------------------------------
// Per-inode (`FilesystemSecurity`) delegation tests.
//
// `SecMockFs` is a read-only driver that, unlike `MockFs`, stores a full
// record per node. The mount point itself is admin-owned `0o755`, so
// the *uniform* `*_via` methods would grant admin every access; the
// secured `*_via_secured` methods instead honour each node's own stored
// record.
// ---------------------------------------------------------------------

const SECRET_FILE: u64 = 2;
const SECRET_BODY: &[u8] = b"top secret";
/// The owner uid baked into `SecMockFs`'s `secret.txt` record.
const SECRET_OWNER: u32 = 7;

struct SecMockFs;

impl FilesystemRead for SecMockFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        match node.raw() {
            ROOT => Ok(NodeInfo {
                kind: NodeKind::Directory,
                nlink: 2,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            }),
            SECRET_FILE => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: SECRET_BODY.len() as u64,
                allocated: SECRET_BODY.len() as u64,
                times: NodeTimes::default(),
            }),
            _ => Err(DriverError::NotFound),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        match dir.raw() {
            ROOT if name == b"secret.txt" => Ok(NodeId::from_raw(SECRET_FILE)),
            ROOT => Err(DriverError::NotFound),
            _ => Err(DriverError::Unsupported),
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        if file.raw() != SECRET_FILE {
            return Err(DriverError::Unsupported);
        }
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= SECRET_BODY.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), SECRET_BODY.len() - start);
        buf[..n].copy_from_slice(&SECRET_BODY[start..start + n]);
        Ok(n)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        if dir.raw() != ROOT || cursor != 0 {
            return Ok(None);
        }
        let name = b"secret.txt";
        if name_out.len() < name.len() {
            return Err(DriverError::BufferTooSmall);
        }
        name_out[..name.len()].copy_from_slice(name);
        Ok(Some(DirEntry {
            node: NodeId::from_raw(SECRET_FILE),
            info: NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: SECRET_BODY.len() as u64,
                allocated: SECRET_BODY.len() as u64,
                times: NodeTimes::default(),
            },
            name_len: name.len(),
            next_cursor: 1,
        }))
    }
}

impl FilesystemSecurity for SecMockFs {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        match node.raw() {
            ROOT => Ok(NodeSecurity::new(0o755, 0, 0)),
            SECRET_FILE => {
                let mut sec = NodeSecurity::new(0o600, SECRET_OWNER, 0);
                sec.required_cap = Some(CapabilityId::AUDIT_READ);
                Ok(sec)
            }
            _ => Err(DriverError::NotFound),
        }
    }

    fn set_security(&mut self, _node: NodeId, _security: NodeSecurity) -> Result<(), DriverError> {
        // The mock's records are fixed; a security write is refused.
        Err(DriverError::Unsupported)
    }
}

#[test]
fn secured_read_honours_per_inode_owner_and_capability_gate() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = SecMockFs;
    let path = p("/Storage/usb0/secret.txt");
    let mut buf = [0u8; 32];

    // Uniform delegation judges against the admin-owned 0o755 template, so
    // admin can read the file.
    let n = vfs
        .read_via(&admin, &path, &mut fs, 0, &mut buf)
        .expect("uniform read allowed");
    assert_eq!(&buf[..n], SECRET_BODY);

    // The secured path uses the file's own record (owner 7, mode 0o600,
    // gated on CAP_AUDIT_READ): admin holds neither the ownership nor the
    // capability, so it is denied.
    assert_eq!(
        vfs.read_via_secured(&admin, &path, &mut fs, 0, &mut buf),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn secured_read_allows_owner_holding_the_capability() {
    let vfs = backed_vfs(0o755);
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::AUDIT_READ);
    let owner = cred(SECRET_OWNER, 0, &caps);
    let mut fs = SecMockFs;
    let mut buf = [0u8; 32];
    let n = vfs
        .read_via_secured(&owner, &p("/Storage/usb0/secret.txt"), &mut fs, 0, &mut buf)
        .expect("secured read allowed");
    assert_eq!(&buf[..n], SECRET_BODY);
}

#[test]
fn secured_stat_reports_per_inode_metadata() {
    let vfs = backed_vfs(0o755);
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::AUDIT_READ);
    let owner = cred(SECRET_OWNER, 0, &caps);
    let mut fs = SecMockFs;
    let info = vfs
        .stat_via_secured(
            &owner,
            &p("/Storage/usb0/secret.txt"),
            &mut fs,
            FinalLink::Follow,
        )
        .expect("secured stat");
    assert_eq!(info.kind, NodeKind::RegularFile);
    assert_eq!(info.size, SECRET_BODY.len() as u64);
    assert_eq!(info.meta.owner, UserId(SECRET_OWNER));
    assert_eq!(info.meta.mode, Mode::from_bits(0o600));
    assert_eq!(info.meta.required_cap, Some(CapabilityId::AUDIT_READ));
}

#[test]
fn secured_list_of_mount_root_lists_driver_root() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = SecMockFs;
    // The driver root is 0o755, world-readable, so listing it is allowed.
    let names = vfs
        .list_via_secured(&admin, &p("/Storage/usb0"), &mut fs, FinalLink::Follow)
        .expect("secured list");
    let kinds: Vec<(NodeKind, String)> = names
        .into_iter()
        .map(|entry| (entry.info.kind, entry.name))
        .collect();
    assert_eq!(kinds, [(NodeKind::RegularFile, String::from("secret.txt"))]);
}

#[test]
fn secured_write_honours_per_inode_parent_permission() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    // Re-own the driver root to uid 7 with owner-only access; admin now has
    // no write bit on the parent under the per-inode policy.
    fs.set_root_security(NodeSecurity::new(0o700, SECRET_OWNER, 0));

    // Uniform delegation still uses the admin-owned mount template → allowed.
    let path = p("/Storage/usb0/a.txt");
    vfs.create_via(&admin, &path, &mut fs)
        .expect("uniform create");

    // Secured delegation consults the (uid-7-owned) parent → admin denied.
    assert_eq!(
        vfs.create_via_secured(&admin, &p("/Storage/usb0/b.txt"), &mut fs),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn secured_set_mode_by_owner_rewrites_only_the_mode() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/a.txt");
    vfs.create_via_secured(&admin, &path, &mut fs)
        .expect("create");

    vfs.set_mode_via_secured(&admin, &path, &mut fs, 0o640)
        .expect("owner chmod");

    // The mode changed; ownership and the (absent) capability gate did not.
    let info = vfs
        .stat_via_secured(&admin, &path, &mut fs, FinalLink::Follow)
        .expect("secured stat");
    assert_eq!(info.meta.mode, Mode::from_bits(0o640));
    assert_eq!(info.meta.owner, UserId(ADMIN_UID));
    assert_eq!(info.meta.group, GroupId(ADMIN_GID));
    assert_eq!(info.meta.required_cap, None);
}

#[test]
fn secured_set_mode_by_non_owner_is_denied_even_with_write_access() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/a.txt");
    vfs.create_via_secured(&admin, &path, &mut fs)
        .expect("create");
    // World-writable: write access must still not grant chmod.
    vfs.set_mode_via_secured(&admin, &path, &mut fs, 0o777)
        .expect("owner opens the file up");

    let other = cred(9, 9, &caps);
    assert_eq!(
        vfs.set_mode_via_secured(&other, &path, &mut fs, 0o600),
        Err(VfsError::PermissionDenied)
    );
    // The refused change did not land.
    let info = vfs
        .stat_via_secured(&admin, &path, &mut fs, FinalLink::Follow)
        .expect("secured stat");
    assert_eq!(info.meta.mode, Mode::from_bits(0o777));
}

#[test]
fn secured_set_mode_honours_the_nodes_capability_gate() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/a.txt");
    vfs.create_via_secured(&admin, &path, &mut fs)
        .expect("create");
    // Gate the node on CAP_AUDIT_READ through the driver's own surface.
    let node = fs
        .lookup(fs.root(), b"a.txt")
        .expect("created node resolves");
    let mut sec = fs.security(node).expect("record exists");
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    fs.set_security(node, sec).expect("gate installed");

    // The owner without the capability is refused; with it, allowed.
    assert_eq!(
        vfs.set_mode_via_secured(&admin, &path, &mut fs, 0o600),
        Err(VfsError::PermissionDenied)
    );
    let mut holding = CapabilitySet::empty();
    holding.insert(CapabilityId::AUDIT_READ);
    let admin_holding = cred(ADMIN_UID, ADMIN_GID, &holding);
    vfs.set_mode_via_secured(&admin_holding, &path, &mut fs, 0o600)
        .expect("gated owner chmod");
    // The gate itself survived the mode change.
    let after = fs.security(node).expect("record exists");
    assert_eq!(after.required_cap, Some(CapabilityId::AUDIT_READ));
    assert_eq!(after.mode, 0o600);
}

#[test]
fn secured_set_mode_on_read_only_mount_is_read_only() {
    // `backed_vfs` mounts with `READ_ONLY`.
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    assert_eq!(
        vfs.set_mode_via_secured(&admin, &p("/Storage/usb0/a.txt"), &mut fs, 0o640),
        Err(VfsError::ReadOnly)
    );
}

#[test]
fn secured_set_mode_of_missing_path_is_not_found() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    assert_eq!(
        vfs.set_mode_via_secured(&admin, &p("/Storage/usb0/absent"), &mut fs, 0o640),
        Err(VfsError::NotFound)
    );
}

/// A created node is stamped with its **creator's** identity, not the
/// driver's raw default: the driver mints the record with its own
/// baked-in owner (`ARXFS` stamps the system user), and before the create
/// returns the secured path rewrites the ownership to the creating
/// caller, so a user can immediately use a private-mode file it just
/// made. Before the stamp landed this read failed `PermissionDenied` —
/// the creator was locked out of its own file.
#[test]
fn secured_create_stamps_the_creator_as_owner() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let user = cred(7, 3, &caps);
    // The driver's raw create default: admin-owned, private mode — the
    // shape that locked a non-admin creator out.
    let mut fs = RwMockFs::new().with_create_owner(ADMIN_UID, ADMIN_GID, 0o600);
    fs.set_root_security(NodeSecurity::new(0o777, ADMIN_UID, ADMIN_GID));
    let path = p("/Storage/usb0/mine.txt");

    vfs.create_via_secured(&user, &path, &mut fs)
        .expect("create");

    // The stored record names the creator, mode untouched.
    let node = fs.lookup(fs.root(), b"mine.txt").expect("created child");
    let sec = fs.security(node).expect("security record");
    assert_eq!((sec.uid, sec.gid), (7, 3));
    assert_eq!(sec.mode, 0o600);
    // The behavioural proof: the creator reads its own private file.
    let mut buf = [0u8; 4];
    assert_eq!(
        vfs.read_via_secured(&user, &path, &mut fs, 0, &mut buf),
        Ok(0)
    );
}

/// A created directory is stamped like a file, so a user can immediately
/// populate a private-mode directory it just made — the scratch-tree
/// shape a per-user cache directory (`Library/<app>/`) relies on.
#[test]
fn secured_mkdir_stamps_the_creator_as_owner() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let user = cred(7, 3, &caps);
    let mut fs = RwMockFs::new().with_create_owner(ADMIN_UID, ADMIN_GID, 0o700);
    fs.set_root_security(NodeSecurity::new(0o777, ADMIN_UID, ADMIN_GID));

    vfs.mkdir_via_secured(&user, &p("/Storage/usb0/scratch"), &mut fs)
        .expect("mkdir");
    // Creating inside needs search + write on the new directory: only
    // its stamped owner passes a 0o700 mode.
    vfs.create_via_secured(&user, &p("/Storage/usb0/scratch/unit.bin"), &mut fs)
        .expect("create inside own private directory");
}

/// A driver whose directory cursor never advances, exercising the delegated
/// listing's no-progress guard: a corrupt directory must fail the listing
/// closed, never spin the kernel forever.
struct StuckCursorFs;

impl FilesystemRead for StuckCursorFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        if node.raw() == ROOT {
            Ok(NodeInfo {
                kind: NodeKind::Directory,
                nlink: 2,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            })
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn lookup(&mut self, _dir: NodeId, _name: &[u8]) -> Result<NodeId, DriverError> {
        Err(DriverError::NotFound)
    }

    fn read_at(
        &mut self,
        _file: NodeId,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        Err(DriverError::Unsupported)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        if dir.raw() != ROOT {
            return Err(DriverError::NotFound);
        }
        name_out[0] = b'x';
        Ok(Some(DirEntry {
            node: NodeId::from_raw(DOCS),
            info: NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            },
            name_len: 1,
            next_cursor: cursor,
        }))
    }
}

#[test]
fn a_listing_whose_cursor_never_advances_fails_closed() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = StuckCursorFs;
    assert_eq!(
        vfs.list_via(&admin, &p("/Storage/usb0"), &mut fs, FinalLink::Follow),
        Err(VfsError::Io)
    );
}

// --- symbolic-link resolution ------------------------------------------
//
// The matrix runs against `RwMockFs` (a real `read_link`/`create_link`
// backing) through the ordinary `*_via` delegation path, so what is proven
// is the VFS's resolution rather than a test double's shortcut.

/// Build a subtree in the mock, returning nothing: helpers keep each test
/// about the resolution it is testing rather than about tree construction.
fn mk_dir(fs: &mut RwMockFs, parent: NodeId, name: &str) -> NodeId {
    fs.create(parent, name.as_bytes(), NodeKind::Directory)
        .expect("mkdir in mock")
}

fn mk_file(fs: &mut RwMockFs, parent: NodeId, name: &str, body: &[u8]) {
    fs.create(parent, name.as_bytes(), NodeKind::RegularFile)
        .expect("create in mock");
    fs.write_at(parent, name.as_bytes(), 0, body)
        .expect("write in mock");
}

fn mk_link(fs: &mut RwMockFs, parent: NodeId, name: &str, target: &str) {
    fs.create_link(parent, name.as_bytes(), target.as_bytes())
        .expect("create_link in mock");
}

fn read_to_string(vfs: &Vfs, admin: &Credentials<'_>, fs: &mut RwMockFs, path: &str) -> String {
    let mut buf = [0u8; 64];
    let n = vfs
        .read_via(admin, &p(path), fs, 0, &mut buf)
        .expect("read through the link");
    String::from_utf8(buf[..n].to_vec()).expect("utf-8 body")
}

#[test]
fn resolution_follows_a_link_to_its_target() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_file(&mut fs, root, "real.txt", b"payload");
    mk_link(&mut fs, root, "alias", "/real.txt");

    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/alias"),
        "payload"
    );
}

#[test]
fn resolution_follows_a_link_used_as_an_interior_directory() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let real = mk_dir(&mut fs, root, "real");
    mk_file(&mut fs, real, "leaf", b"inside");
    mk_link(&mut fs, root, "via", "/real");

    // `via` is not the final component here: it is being used as a
    // directory, so what matters is what it names.
    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/via/leaf"),
        "inside"
    );
}

#[test]
fn a_link_cycle_is_refused_rather_than_walked() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_link(&mut fs, root, "a", "/b");
    mk_link(&mut fs, root, "b", "/a");

    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(&admin, &p("/Storage/usb0/a"), &mut fs, 0, &mut buf),
        Err(VfsError::LinkLoop)
    );
}

#[test]
fn a_self_referential_link_is_refused() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_link(&mut fs, root, "loop", "/loop");

    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(&admin, &p("/Storage/usb0/loop"), &mut fs, 0, &mut buf),
        Err(VfsError::LinkLoop)
    );
}

#[test]
fn a_chain_longer_than_the_hop_budget_is_refused_and_a_shorter_one_resolves() {
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);

    // Exactly at the budget: hop_n -> hop_{n-1} -> ... -> hop_0 -> real.
    let build = |links: usize| {
        let mut fs = RwMockFs::new();
        let root = fs.root();
        mk_file(&mut fs, root, "real.txt", b"end");
        mk_link(&mut fs, root, "hop0", "/real.txt");
        for i in 1..links {
            let target = alloc::format!("/hop{}", i - 1);
            mk_link(&mut fs, root, &alloc::format!("hop{i}"), &target);
        }
        fs
    };

    let vfs = backed_vfs_rw(0o755);
    let mut ok = build(SYMLINK_HOP_MAX);
    assert_eq!(
        read_to_string(
            &vfs,
            &admin,
            &mut ok,
            &alloc::format!("/Storage/usb0/hop{}", SYMLINK_HOP_MAX - 1)
        ),
        "end"
    );

    let mut over = build(SYMLINK_HOP_MAX + 2);
    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(
            &admin,
            &p(&alloc::format!("/Storage/usb0/hop{}", SYMLINK_HOP_MAX + 1)),
            &mut over,
            0,
            &mut buf
        ),
        Err(VfsError::LinkLoop)
    );
}

#[test]
fn a_dangling_link_reports_not_found_not_a_loop() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_link(&mut fs, root, "nowhere", "/absent");

    let mut buf = [0u8; 8];
    assert_eq!(
        vfs.read_via(&admin, &p("/Storage/usb0/nowhere"), &mut fs, 0, &mut buf),
        Err(VfsError::NotFound)
    );
}

#[test]
fn dot_dot_after_a_link_is_physical_rather_than_lexical() {
    // The escape this guards: collapsing `/a/via/../other` textually gives
    // `/a/other`, which is NOT where the walk actually stands once `via`
    // has been followed to `/b/c`. Physical resolution reads `/b/other`.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let a = mk_dir(&mut fs, root, "a");
    mk_file(&mut fs, a, "other", b"lexical-would-read-this");
    let b = mk_dir(&mut fs, root, "b");
    let c = mk_dir(&mut fs, b, "c");
    mk_file(&mut fs, c, "leaf", b"target");
    mk_file(&mut fs, b, "other", b"physical-reads-this");
    mk_link(&mut fs, a, "via", "/b/c");

    // A caller may not spell `..`, so the `..` under test comes from a
    // link target — exactly how it reaches resolution in production.
    mk_link(&mut fs, a, "up", "/b/c/../other");
    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/a/up"),
        "physical-reads-this"
    );
}

#[test]
fn dot_dot_cannot_climb_out_of_the_volume() {
    // The stack starts at the mounted volume's own root and `..` never pops
    // past it, so a foreign volume's link cannot name anything above the
    // mount point. `/..` is `/`, as POSIX specifies.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_file(&mut fs, root, "confined.txt", b"still-inside");
    mk_link(&mut fs, root, "escape", "/../../../../confined.txt");

    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/escape"),
        "still-inside"
    );
}

#[test]
fn search_permission_is_required_on_a_directory_a_link_leads_through() {
    // Following a link does not bypass the per-component check: the spliced
    // components are authorised exactly as typed ones are.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let other = cred(ADMIN_UID + 7, ADMIN_GID + 7, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let closed = mk_dir(&mut fs, root, "closed");
    mk_file(&mut fs, closed, "secret", b"classified");
    mk_link(&mut fs, root, "peek", "/closed/secret");
    // Owner-only search on the directory the link leads through.
    fs.set_security(closed, NodeSecurity::new(0o700, ADMIN_UID, ADMIN_GID))
        .expect("tighten the directory");

    // The owner still reads it through the link...
    let mut buf = [0u8; 16];
    let n = vfs
        .read_via_secured(&admin, &p("/Storage/usb0/peek"), &mut fs, 0, &mut buf)
        .expect("owner reads through the link");
    assert_eq!(&buf[..n], b"classified");
    // ...and a stranger is refused at the traversed directory, not handed
    // the target because a link named it.
    let mut other_buf = [0u8; 16];
    assert_eq!(
        vfs.read_via_secured(&other, &p("/Storage/usb0/peek"), &mut fs, 0, &mut other_buf),
        Err(VfsError::PermissionDenied)
    );
}

// --- the `NO_FOLLOW` posture and the two link operations ----------------
//
// `FinalLink::Keep` had no entry point before the syscalls reached the VFS,
// so this half of the matrix closes with them. What each case pins down is
// the *difference* the posture makes, never a restatement of the follow half
// above.

/// A driver whose format has no link object type: `create_link` and
/// `read_link` are left at their trait defaults. It stands for FAT32 and
/// ADFS, so what is proven is the VFS reporting a format limit rather than
/// approximating a link with something else.
struct NoLinksFs {
    child: Option<String>,
}

impl NoLinksFs {
    fn new() -> Self {
        Self { child: None }
    }
}

impl FilesystemRead for NoLinksFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(ROOT)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        match node.raw() {
            ROOT => Ok(NodeInfo {
                kind: NodeKind::Directory,
                nlink: 2,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            }),
            DOCS => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                nlink: 1,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            }),
            _ => Err(DriverError::NotFound),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        if dir.raw() != ROOT {
            return Err(DriverError::Unsupported);
        }
        match (&self.child, core::str::from_utf8(name)) {
            (Some(held), Ok(want)) if held == want => Ok(NodeId::from_raw(DOCS)),
            _ => Err(DriverError::NotFound),
        }
    }

    fn read_at(
        &mut self,
        _file: NodeId,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        Ok(0)
    }

    fn read_dir(
        &mut self,
        _dir: NodeId,
        _cursor: u64,
        _name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        Ok(None)
    }
}

impl FilesystemWrite for NoLinksFs {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        if dir.raw() != ROOT || kind != NodeKind::RegularFile {
            return Err(DriverError::Unsupported);
        }
        self.child = Some(
            core::str::from_utf8(name)
                .map_err(|_| DriverError::LengthOutOfRange)?
                .to_string(),
        );
        Ok(NodeId::from_raw(DOCS))
    }

    fn write_at(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        Ok(data.len())
    }

    fn truncate(&mut self, _dir: NodeId, _name: &[u8], _size: u64) -> Result<(), DriverError> {
        Ok(())
    }

    fn remove(&mut self, _dir: NodeId, _name: &[u8]) -> Result<(), DriverError> {
        self.child = None;
        Ok(())
    }

    fn rename(
        &mut self,
        _src_dir: NodeId,
        _src_name: &[u8],
        _dst_dir: NodeId,
        _dst_name: &[u8],
    ) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

#[test]
fn keeping_the_final_link_stats_the_link_and_following_stats_its_target() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_file(&mut fs, root, "real.txt", b"payload");
    mk_link(&mut fs, root, "alias", "/real.txt");
    let path = p("/Storage/usb0/alias");

    let kept = vfs
        .stat_via(&admin, &path, &mut fs, FinalLink::Keep)
        .expect("stat the link itself");
    assert_eq!(kept.kind, NodeKind::Symlink);
    // A link's size is its target's length, which is how `ls -l` renders it.
    assert_eq!(kept.size, "/real.txt".len() as u64);

    let followed = vfs
        .stat_via(&admin, &path, &mut fs, FinalLink::Follow)
        .expect("stat through the link");
    assert_eq!(followed.kind, NodeKind::RegularFile);
    assert_eq!(followed.size, "payload".len() as u64);
    // The two postures name different nodes, not merely different sizes.
    assert_ne!(kept.node, followed.node);
}

#[test]
fn a_dangling_link_is_statable_when_the_final_link_is_kept() {
    // The load-bearing case for `ls -l`: a link whose target is absent is
    // still a real directory entry, and only `Keep` can describe it.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_link(&mut fs, root, "nowhere", "/absent");
    let path = p("/Storage/usb0/nowhere");

    let kept = vfs
        .stat_via(&admin, &path, &mut fs, FinalLink::Keep)
        .expect("the link itself exists");
    assert_eq!(kept.kind, NodeKind::Symlink);
    assert_eq!(
        vfs.stat_via(&admin, &path, &mut fs, FinalLink::Follow),
        Err(VfsError::NotFound)
    );
}

#[test]
fn keeping_the_final_link_refuses_to_list_it_as_a_directory() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let real = mk_dir(&mut fs, root, "real");
    mk_file(&mut fs, real, "leaf", b"inside");
    mk_link(&mut fs, root, "via", "/real");
    let path = p("/Storage/usb0/via");

    // A `NO_FOLLOW` descriptor names the link, and a link is not a
    // directory — so its target's entries are never listed in its place.
    assert_eq!(
        vfs.list_via(&admin, &path, &mut fs, FinalLink::Keep),
        Err(VfsError::NotADirectory)
    );
    let entries = vfs
        .list_via(&admin, &path, &mut fs, FinalLink::Follow)
        .expect("following lists the target");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "leaf");
}

#[test]
fn readlink_returns_the_stored_target_verbatim() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let sub = mk_dir(&mut fs, root, "sub");
    mk_file(&mut fs, root, "real.txt", b"payload");
    // A relative target carrying `..` comes back exactly as authored: the
    // target is data, so `readlink` neither normalises nor resolves it.
    mk_link(&mut fs, sub, "up", "../real.txt");
    mk_link(&mut fs, root, "abs", "/real.txt");

    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/sub/up"), &mut fs),
        Ok(String::from("../real.txt"))
    );
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/abs"), &mut fs),
        Ok(String::from("/real.txt"))
    );
}

#[test]
fn readlink_reads_a_dangling_link_and_refuses_a_non_link() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_file(&mut fs, root, "plain.txt", b"bytes");
    mk_dir(&mut fs, root, "dir");
    mk_link(&mut fs, root, "nowhere", "/absent");

    // The target is readable even though nothing is there to reach.
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/nowhere"), &mut fs),
        Ok(String::from("/absent"))
    );
    // Neither a file nor a directory has a target; both fail closed rather
    // than handing back their own bytes or name.
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/plain.txt"), &mut fs),
        Err(VfsError::InvalidPath)
    );
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/dir"), &mut fs),
        Err(VfsError::InvalidPath)
    );
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/absent"), &mut fs),
        Err(VfsError::NotFound)
    );
}

#[test]
fn readlink_needs_search_permission_on_the_way_to_the_link() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let other = cred(ADMIN_UID + 7, ADMIN_GID + 7, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let closed = mk_dir(&mut fs, root, "closed");
    mk_link(&mut fs, closed, "alias", "/elsewhere");
    fs.set_security(closed, NodeSecurity::new(0o700, ADMIN_UID, ADMIN_GID))
        .expect("tighten the directory");
    let path = p("/Storage/usb0/closed/alias");

    assert_eq!(
        vfs.readlink_via_secured(&admin, &path, &mut fs),
        Ok(String::from("/elsewhere"))
    );
    assert_eq!(
        vfs.readlink_via_secured(&other, &path, &mut fs),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn a_created_link_resolves_and_reads_back_its_target() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new().with_create_owner(ADMIN_UID, ADMIN_GID, 0o755);
    let root = fs.root();
    mk_file(&mut fs, root, "real.txt", b"payload");
    let link = p("/Storage/usb0/alias");

    vfs.symlink_via(&admin, &link, &mut fs, "/real.txt")
        .expect("create the link");
    assert_eq!(
        vfs.readlink_via(&admin, &link, &mut fs),
        Ok(String::from("/real.txt"))
    );
    // The whole round trip through the VFS: created here, then followed.
    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/alias"),
        "payload"
    );
}

#[test]
fn a_created_link_is_owned_by_its_creator() {
    // As for `create`: the driver mints the node with its format's default
    // record, and the VFS hands it to the caller before it is observable.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let owner_id = ADMIN_UID + 11;
    let group_id = ADMIN_GID + 11;
    let creator = cred(owner_id, group_id, &caps);
    let mut fs = RwMockFs::new();
    fs.set_root_security(NodeSecurity::new(0o777, ADMIN_UID, ADMIN_GID));
    let link = p("/Storage/usb0/mine");

    vfs.symlink_via_secured(&creator, &link, &mut fs, "/target")
        .expect("create the link");
    let info = vfs
        .stat_via_secured(&creator, &link, &mut fs, FinalLink::Keep)
        .expect("stat the new link");
    assert_eq!(info.meta.owner, UserId(owner_id));
    assert_eq!(info.meta.group, GroupId(group_id));
}

#[test]
fn creating_a_link_refuses_an_existing_name_and_an_unwalkable_target() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_file(&mut fs, root, "taken", b"x");

    assert_eq!(
        vfs.symlink_via(&admin, &p("/Storage/usb0/taken"), &mut fs, "/elsewhere"),
        Err(VfsError::AlreadyExists)
    );
    // The grammar is checked before anything is written, so a target this
    // resolver could never walk is refused rather than stored as a link
    // that can only ever fail.
    let fresh = p("/Storage/usb0/fresh");
    assert_eq!(
        vfs.symlink_via(&admin, &fresh, &mut fs, ""),
        Err(VfsError::InvalidPath)
    );
    let over_long_component = alloc::format!("/{}", "n".repeat(MAX_COMPONENT_LEN + 1));
    assert_eq!(
        vfs.symlink_via(&admin, &fresh, &mut fs, &over_long_component),
        Err(VfsError::InvalidPath)
    );
    let too_many_steps = "/a".repeat(MAX_PATH_COMPONENTS + 1);
    assert_eq!(
        vfs.symlink_via(&admin, &fresh, &mut fs, &too_many_steps),
        Err(VfsError::InvalidPath)
    );
    // A refused create leaves no name behind.
    assert_eq!(
        vfs.stat_via(&admin, &fresh, &mut fs, FinalLink::Keep),
        Err(VfsError::NotFound)
    );
}

#[test]
fn creating_a_link_needs_write_permission_on_the_parent_and_a_writable_mount() {
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let other = cred(ADMIN_UID + 7, ADMIN_GID + 7, &caps);

    // Search but no write on the mount point: a stranger cannot add a name.
    let vfs = backed_vfs_rw(0o755);
    let mut fs = RwMockFs::new();
    assert_eq!(
        vfs.symlink_via(&other, &p("/Storage/usb0/alias"), &mut fs, "/t"),
        Err(VfsError::PermissionDenied)
    );

    // A read-only mount refuses before the driver is reached at all.
    let mut ro = Vfs::with_default_layout(UserId(ADMIN_UID), GroupId(ADMIN_GID));
    ro.mkdir(&admin, &p("/Storage/usb0"), Mode::from_bits(0o755))
        .expect("create mount point");
    ro.mounts_write()
        .mount(
            p("/Storage/usb0"),
            MountFlags::READ_ONLY,
            Some(backing_of(8)),
        )
        .expect("mount read-only");
    assert_eq!(
        ro.symlink_via(&admin, &p("/Storage/usb0/alias"), &mut fs, "/t"),
        Err(VfsError::ReadOnly)
    );
}

#[test]
fn a_format_without_links_refuses_rather_than_approximating_one() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = NoLinksFs::new();

    // The refusal is the permanent format limit, not "you used a file as a
    // directory": a caller must be able to tell the two apart.
    assert_eq!(
        vfs.symlink_via(&admin, &p("/Storage/usb0/alias"), &mut fs, "/target"),
        Err(VfsError::NotSupported)
    );
    // And nothing was created in its place — no regular file holding a path.
    assert_eq!(
        vfs.stat_via(&admin, &p("/Storage/usb0/alias"), &mut fs, FinalLink::Keep),
        Err(VfsError::NotFound)
    );
    // A plain create on the same driver still works, so the refusal is
    // about links and not about the mount.
    vfs.create_via(&admin, &p("/Storage/usb0/plain"), &mut fs)
        .expect("a regular file is fine");
}

#[test]
fn the_open_posture_is_derived_from_the_no_follow_flag() {
    // One definition of the mapping, so an open and every operation later
    // served for its descriptor cannot disagree about following.
    assert_eq!(FinalLink::for_open(OpenFlags::empty()), FinalLink::Follow);
    assert_eq!(FinalLink::for_open(OpenFlags::READ), FinalLink::Follow);
    assert_eq!(FinalLink::for_open(OpenFlags::NO_FOLLOW), FinalLink::Keep);
    assert_eq!(
        FinalLink::for_open(OpenFlags::NO_FOLLOW.union(OpenFlags::READ)),
        FinalLink::Keep
    );
}

#[test]
fn writing_through_a_final_link_writes_the_target_not_the_link() {
    // POSIX writes what a link names. The driver write surface is keyed
    // `(dir, name)`, so this proves the resolved node's *own* parent and
    // name reached the driver — not the link's.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let sub = mk_dir(&mut fs, root, "sub");
    mk_file(&mut fs, sub, "real.txt", b"old");
    mk_link(&mut fs, root, "alias", "/sub/real.txt");

    assert_eq!(
        vfs.write_via(&admin, &p("/Storage/usb0/alias"), &mut fs, 0, b"new"),
        Ok(3)
    );
    // The target changed...
    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/sub/real.txt"),
        "new"
    );
    // ...and the link still names it, rather than having been overwritten
    // with the bytes.
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/alias"), &mut fs),
        Ok(String::from("/sub/real.txt"))
    );
}

#[test]
fn truncating_through_a_final_link_truncates_the_target() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_file(&mut fs, root, "real.txt", b"payload");
    mk_link(&mut fs, root, "alias", "real.txt");

    vfs.truncate_via(&admin, &p("/Storage/usb0/alias"), &mut fs, 3)
        .expect("truncate through the link");
    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/real.txt"),
        "pay"
    );
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/alias"), &mut fs),
        Ok(String::from("real.txt"))
    );
}

#[test]
fn a_write_through_a_link_needs_write_permission_on_the_targets_parent() {
    // The write permission this VFS asks for on a write's parent follows the
    // link to the directory the bytes really land in — it is not satisfied by
    // holding write on the directory the *link* sits in.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let other = cred(ADMIN_UID + 5, ADMIN_GID + 5, &caps);
    let mut fs = RwMockFs::new();
    fs.set_root_security(NodeSecurity::new(0o777, ADMIN_UID, ADMIN_GID));
    let root = fs.root();
    let locked = mk_dir(&mut fs, root, "locked");
    mk_file(&mut fs, locked, "real.txt", b"old");
    mk_link(&mut fs, root, "alias", "/locked/real.txt");
    // Searchable but not writable by anyone but its owner.
    fs.set_security(locked, NodeSecurity::new(0o755, ADMIN_UID, ADMIN_GID))
        .expect("tighten the target's parent");

    assert_eq!(
        vfs.write_via_secured(&other, &p("/Storage/usb0/alias"), &mut fs, 0, b"new"),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn a_write_through_a_dangling_link_reports_not_found() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_link(&mut fs, root, "alias", "/nowhere");

    assert_eq!(
        vfs.write_via(&admin, &p("/Storage/usb0/alias"), &mut fs, 0, b"new"),
        Err(VfsError::NotFound)
    );
    assert_eq!(
        vfs.truncate_via(&admin, &p("/Storage/usb0/alias"), &mut fs, 0),
        Err(VfsError::NotFound)
    );
    // Nothing was created in the attempt.
    assert_eq!(
        vfs.stat_via(
            &admin,
            &p("/Storage/usb0/nowhere"),
            &mut fs,
            FinalLink::Keep
        ),
        Err(VfsError::NotFound)
    );
}

#[test]
fn creating_through_a_dangling_link_creates_the_target() {
    // `open(O_CREAT)` on a dangling link creates what the link names, so the
    // link stops dangling rather than the create reporting the link's own
    // name as taken.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_link(&mut fs, root, "alias", "/made-later");

    vfs.create_via(&admin, &p("/Storage/usb0/alias"), &mut fs)
        .expect("create through the dangling link");
    // The target now exists as a regular file...
    let target = vfs
        .stat_via(
            &admin,
            &p("/Storage/usb0/made-later"),
            &mut fs,
            FinalLink::Keep,
        )
        .expect("the target was created");
    assert_eq!(target.kind, NodeKind::RegularFile);
    // ...and the link is still a link, now resolving.
    let link = vfs
        .stat_via(&admin, &p("/Storage/usb0/alias"), &mut fs, FinalLink::Keep)
        .expect("the link survived");
    assert_eq!(link.kind, NodeKind::Symlink);
    assert_eq!(
        vfs.stat_via(
            &admin,
            &p("/Storage/usb0/alias"),
            &mut fs,
            FinalLink::Follow
        )
        .expect("the link now resolves")
        .kind,
        NodeKind::RegularFile
    );
}

#[test]
fn mkdir_over_a_link_is_refused_rather_than_following_it() {
    // POSIX `mkdir` never follows a final link, so it can neither replace a
    // live link nor quietly make the directory a dangling one names.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_dir(&mut fs, root, "real");
    mk_link(&mut fs, root, "live", "/real");
    mk_link(&mut fs, root, "dead", "/absent");

    assert_eq!(
        vfs.mkdir_via(&admin, &p("/Storage/usb0/live"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(
        vfs.mkdir_via(&admin, &p("/Storage/usb0/dead"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(
        vfs.stat_via(&admin, &p("/Storage/usb0/absent"), &mut fs, FinalLink::Keep),
        Err(VfsError::NotFound)
    );
}

#[test]
fn unlink_and_rename_still_act_on_the_link_itself() {
    // The counterpart of the following writes above: the namespace
    // operations keep the name as typed.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let root = fs.root();
    mk_file(&mut fs, root, "real.txt", b"payload");
    mk_link(&mut fs, root, "alias", "/real.txt");
    mk_link(&mut fs, root, "moved", "/real.txt");

    vfs.rename_via(
        &admin,
        &p("/Storage/usb0/moved"),
        &p("/Storage/usb0/renamed"),
        &mut fs,
    )
    .expect("rename the link");
    assert_eq!(
        vfs.readlink_via(&admin, &p("/Storage/usb0/renamed"), &mut fs),
        Ok(String::from("/real.txt"))
    );

    vfs.remove_via(&admin, &p("/Storage/usb0/alias"), &mut fs, false)
        .expect("unlink the link");
    // The target is untouched by either.
    assert_eq!(
        read_to_string(&vfs, &admin, &mut fs, "/Storage/usb0/real.txt"),
        "payload"
    );
}

/// Install a `CAP_AUDIT_READ` gate on the driver node reached by `name`
/// under the mock's root, and return the credentials that hold it.
fn gate_child(fs: &mut RwMockFs, name: &[u8]) {
    let node = fs.lookup(fs.root(), name).expect("node resolves");
    let mut sec = fs.security(node).expect("record exists");
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    fs.set_security(node, sec).expect("gate installed");
}

#[test]
fn secured_remove_of_a_gated_node_needs_the_capability() {
    // Write permission on the parent is what authorises removing an entry,
    // so without this the owner of a gated node's *parent* could unlink the
    // gate away and create an ungated node of the same name — and the
    // service that reaches the tree only by holding the capability would
    // then walk into the replacement.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/gated.txt");
    vfs.create_via_secured(&admin, &path, &mut fs)
        .expect("create");
    gate_child(&mut fs, b"gated.txt");

    assert_eq!(
        vfs.remove_via_secured(&admin, &path, &mut fs, false),
        Err(VfsError::PermissionDenied)
    );

    let mut holding = CapabilitySet::empty();
    holding.insert(CapabilityId::AUDIT_READ);
    let admin_holding = cred(ADMIN_UID, ADMIN_GID, &holding);
    vfs.remove_via_secured(&admin_holding, &path, &mut fs, false)
        .expect("the capability holder may remove it");
}

#[test]
fn secured_rename_of_a_gated_node_needs_the_capability() {
    // The same-parent rename is the sharpest case: `rename` only authorises
    // the moved node itself when the parents differ, so a gated directory
    // could otherwise be moved aside within its own parent.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let src = p("/Storage/usb0/gated");
    vfs.mkdir_via_secured(&admin, &src, &mut fs).expect("mkdir");
    gate_child(&mut fs, b"gated");

    let aside = p("/Storage/usb0/stolen");
    assert_eq!(
        vfs.rename_via_secured(&admin, &src, &aside, &mut fs),
        Err(VfsError::PermissionDenied)
    );

    let mut holding = CapabilitySet::empty();
    holding.insert(CapabilityId::AUDIT_READ);
    let admin_holding = cred(ADMIN_UID, ADMIN_GID, &holding);
    vfs.rename_via_secured(&admin_holding, &src, &aside, &mut fs)
        .expect("the capability holder may move it");
}

#[test]
fn secured_rename_over_a_gated_destination_needs_the_capability() {
    // Replacing a name destroys its occupant, so a gated destination is
    // guarded exactly as a gated source is.
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let src = p("/Storage/usb0/plain.txt");
    let dst = p("/Storage/usb0/gated.txt");
    vfs.create_via_secured(&admin, &src, &mut fs)
        .expect("create source");
    vfs.create_via_secured(&admin, &dst, &mut fs)
        .expect("create destination");
    gate_child(&mut fs, b"gated.txt");

    assert_eq!(
        vfs.rename_via_secured(&admin, &src, &dst, &mut fs),
        Err(VfsError::PermissionDenied)
    );
}

/// An empty writable volume whose every write operation answers one chosen
/// [`DriverError`], so a test can observe what the VFS makes of a *driver's*
/// refusal rather than of its own pre-check.
///
/// The read surface reports a bare root holding nothing, so each pre-check
/// passes and the call reaches the driver.
struct RefusingFs {
    refusal: DriverError,
}

const REFUSING_ROOT: u64 = 1;

impl FilesystemRead for RefusingFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(REFUSING_ROOT)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        if node.raw() != REFUSING_ROOT {
            return Err(DriverError::NotFound);
        }
        Ok(NodeInfo {
            kind: NodeKind::Directory,
            nlink: 2,
            size: 0,
            allocated: 0,
            times: MOCK_TIMES,
        })
    }

    fn lookup(&mut self, _dir: NodeId, _name: &[u8]) -> Result<NodeId, DriverError> {
        Err(DriverError::NotFound)
    }

    fn read_at(
        &mut self,
        _file: NodeId,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        Err(DriverError::NotFound)
    }

    fn read_dir(
        &mut self,
        _dir: NodeId,
        _cursor: u64,
        _out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        Ok(None)
    }
}

impl FilesystemWrite for RefusingFs {
    fn create(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _kind: NodeKind,
    ) -> Result<NodeId, DriverError> {
        Err(self.refusal)
    }

    fn create_link(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _target: &[u8],
    ) -> Result<NodeId, DriverError> {
        Err(self.refusal)
    }

    fn write_at(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _offset: u64,
        _data: &[u8],
    ) -> Result<usize, DriverError> {
        Err(self.refusal)
    }

    fn truncate(&mut self, _dir: NodeId, _name: &[u8], _size: u64) -> Result<(), DriverError> {
        Err(self.refusal)
    }

    fn remove(&mut self, _dir: NodeId, _name: &[u8]) -> Result<(), DriverError> {
        Err(self.refusal)
    }

    fn rename(
        &mut self,
        _src_dir: NodeId,
        _src_name: &[u8],
        _dst_dir: NodeId,
        _dst_name: &[u8],
    ) -> Result<(), DriverError> {
        Err(self.refusal)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// A driver-reported taken name reaches the caller as a taken name, whatever
/// operation met it.
///
/// The VFS pre-checks the name and answers `AlreadyExists` itself on the
/// ordinary path, so this is the window the pre-check cannot cover: a name
/// that appears between the check and the driver call. It used to surface as
/// an I/O error on `create` (and as `EWOULDBLOCK` to anything reaching the
/// driver without the VFS's per-operation mapping), because one driver value
/// carried three meanings.
#[test]
fn a_driver_reported_taken_name_is_already_exists_on_every_surface() {
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RefusingFs {
        refusal: DriverError::AlreadyExists,
    };

    assert_eq!(
        vfs.create_via(&admin, &p("/appears"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(
        vfs.mkdir_via(&admin, &p("/appears"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(
        vfs.symlink_via(&admin, &p("/appears"), &mut fs, "/target"),
        Err(VfsError::AlreadyExists)
    );
}

/// A populated directory and a self-descending move are each reported as
/// themselves, so `rmdir` and `mv` can tell them apart.
///
/// Rename has no VFS-side pre-check for either, so both answers here come
/// from the driver. The move under itself used to arrive as `NotEmpty` —
/// advice to empty a destination that emptying could never make lawful.
#[test]
fn a_driver_reported_structural_refusal_keeps_its_own_class() {
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);

    let mut populated = RwMockFs::new();
    vfs.mkdir_via(&admin, &p("/dir"), &mut populated)
        .expect("mkdir");
    vfs.mkdir_via(&admin, &p("/dir/inner"), &mut populated)
        .expect("mkdir inner");
    vfs.mkdir_via(&admin, &p("/spare"), &mut populated)
        .expect("mkdir spare");
    assert_eq!(
        vfs.remove_via(&admin, &p("/dir"), &mut populated, true),
        Err(VfsError::NotEmpty)
    );
    assert_eq!(
        vfs.rename_via(&admin, &p("/spare"), &p("/dir"), &mut populated),
        Err(VfsError::NotEmpty)
    );

    // Moving a directory under itself can never be made lawful, so it is not
    // the emptiable `NotEmpty` and not a retryable transient.
    assert_eq!(
        vfs.rename_via(&admin, &p("/dir"), &p("/dir/inner/self"), &mut populated),
        Err(VfsError::DirectoryCycle)
    );
    assert_eq!(VfsError::DirectoryCycle.to_errno(), Errno::OutOfRange);
}

/// A genuinely transient driver refusal keeps meaning "retry": it is not
/// read as any of the structural conflicts that used to share its value.
#[test]
fn a_driver_reported_transient_is_never_read_as_a_name_conflict() {
    let vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RefusingFs {
        refusal: DriverError::Busy,
    };

    for outcome in [
        vfs.create_via(&admin, &p("/x"), &mut fs),
        vfs.mkdir_via(&admin, &p("/x"), &mut fs),
        vfs.symlink_via(&admin, &p("/x"), &mut fs, "/target"),
    ] {
        assert_eq!(outcome, Err(VfsError::Io));
    }
    assert_eq!(DriverError::Busy.as_errno(), Errno::WouldBlock);
}
