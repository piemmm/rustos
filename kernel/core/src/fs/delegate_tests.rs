//! Behavioural tests for driver delegation: the [`Vfs`] resolving a path
//! under a driver-backed mount through a [`FilesystemRead`] driver, with the
//! permission template applied at the mount point.

use crate::fs::{Mode, Path, Vfs, VfsError};

use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, MountFlags, NodeId, NodeInfo, NodeKind,
};
use rustos_abi::driver::{DriverError, DriverHandle};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, UserId};

use crate::fs::perm::Credentials;

// The in-memory read/write driver fixture is shared with the mounted-service
// tests; the one definition lives in `crate::fs::memfs` (no per-test copy).
use crate::fs::memfs::{RwMockFs, ADMIN_GID, ADMIN_UID};

fn p(text: &str) -> Path {
    Path::parse(text).expect("valid path")
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
                size: 0,
                allocated: 0,
            }),
            KERNEL => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: KERNEL_BODY.len() as u64,
                allocated: KERNEL_BODY.len() as u64,
            }),
            README => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: README_BODY.len() as u64,
                allocated: README_BODY.len() as u64,
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
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let entries: &[(&[u8], u64, NodeKind)] = match dir.raw() {
            ROOT => &[
                (b"docs", DOCS, NodeKind::Directory),
                (b"kernel.img", KERNEL, NodeKind::RegularFile),
            ],
            DOCS => &[(b"readme.txt", README, NodeKind::RegularFile)],
            KERNEL | README => return Err(DriverError::Unsupported),
            _ => return Err(DriverError::NotFound),
        };
        let Ok(i) = usize::try_from(index) else {
            return Ok(None);
        };
        let Some(&(name, node, kind)) = entries.get(i) else {
            return Ok(None);
        };
        if name_out.len() < name.len() {
            return Err(DriverError::BufferTooSmall);
        }
        name_out[..name.len()].copy_from_slice(name);
        Ok(Some(DirEntry {
            node: NodeId::from_raw(node),
            kind,
            name_len: name.len(),
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
                size: 0,
                allocated: 0,
            }),
            DOCS => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: 3,
                allocated: 3,
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
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        if dir.raw() != ROOT || index != 0 {
            return Ok(None);
        }
        // A name that is not valid UTF-8.
        name_out[0] = 0xff;
        name_out[1] = 0xff;
        Ok(Some(DirEntry {
            node: NodeId::from_raw(DOCS),
            kind: NodeKind::RegularFile,
            name_len: 2,
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
    let handle = DriverHandle::from_raw(7).expect("non-zero handle");
    vfs.mounts_mut()
        .mount(p("/Storage/usb0"), MountFlags::READ_ONLY, Some(handle))
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

#[test]
fn delegated_list_of_mount_point_lists_driver_root() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let names = vfs
        .list_via(&admin, &p("/Storage/usb0"), &mut fs)
        .expect("list mount root");
    assert_eq!(names, ["docs", "kernel.img"]);
}

#[test]
fn delegated_list_of_subdir() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let names = vfs
        .list_via(&admin, &p("/Storage/usb0/docs"), &mut fs)
        .expect("list subdir");
    assert_eq!(names, ["readme.txt"]);
}

#[test]
fn delegated_stat_reports_kind_size_and_template() {
    let vfs = backed_vfs(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = MockFs;
    let file = vfs
        .stat_via(&admin, &p("/Storage/usb0/kernel.img"), &mut fs)
        .expect("stat file");
    assert_eq!(file.kind, NodeKind::RegularFile);
    assert_eq!(file.size, KERNEL_BODY.len() as u64);
    // The permission template is the mount point's metadata.
    assert_eq!(file.meta.owner, UserId(ADMIN_UID));
    assert_eq!(file.meta.mode, Mode::from_bits(0o755));

    let dir = vfs
        .stat_via(&admin, &p("/Storage/usb0/docs"), &mut fs)
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
        vfs.list_via(&admin, &p("/Storage/usb0/kernel.img"), &mut fs),
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
        vfs.list_via(&admin, &p("/Storage/usb0"), &mut fs),
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

use rustos_abi::driver::filesystem::{FilesystemSecurity, NodeSecurity};
use rustos_abi::CapabilityId;

/// A default-layout VFS with `/Storage/usb0` mounted writable (no
/// `READ_ONLY` flag), owner `admin`, mode `mount_mode`.
fn backed_vfs_rw(mount_mode: u16) -> Vfs {
    let mut vfs = Vfs::with_default_layout(UserId(ADMIN_UID), GroupId(ADMIN_GID));
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    vfs.mkdir(&admin, &p("/Storage/usb0"), Mode::from_bits(mount_mode))
        .expect("create mount point");
    let handle = DriverHandle::from_raw(8).expect("non-zero handle");
    let flags = MountFlags::from_bits(0).expect("empty flags");
    vfs.mounts_mut()
        .mount(p("/Storage/usb0"), flags, Some(handle))
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
        .list_via(&admin, &p("/Storage/usb0/sub"), &mut fs)
        .expect("list");
    assert_eq!(names, ["inner.bin"]);
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
    assert_eq!(vfs.stat_via(&admin, &path, &mut fs).expect("stat").size, 4);
}

#[test]
fn delegated_remove_unlinks() {
    let vfs = backed_vfs_rw(0o755);
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut fs = RwMockFs::new();
    let path = p("/Storage/usb0/gone.txt");
    vfs.create_via(&admin, &path, &mut fs).expect("create");
    vfs.remove_via(&admin, &path, &mut fs).expect("remove");
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
        vfs.remove_via(&admin, &p("/Storage/usb0/d"), &mut fs),
        Err(VfsError::NotEmpty)
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
    let mut vfs = Vfs::with_default_layout(UserId(ADMIN_UID), GroupId(ADMIN_GID));
    let handle = DriverHandle::from_raw(9).expect("non-zero handle");
    vfs.mounts_mut()
        .back_root(handle)
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
    let mut vfs = root_backed_rw_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let handle = DriverHandle::from_raw(11).expect("non-zero handle");
    let flags = MountFlags::from_bits(0).expect("empty flags");
    vfs.mounts_mut()
        .mount(p("/Storage/usb0"), flags, Some(handle))
        .expect("mount a second backed volume");
    let mut fs = RwMockFs::new();
    vfs.mkdir_via(&admin, &p("/Scratch"), &mut fs)
        .expect("create a renameable source on the root volume");
    assert_eq!(
        vfs.rename_via(&admin, &p("/Scratch"), &p("/Storage/usb0/Scratch"), &mut fs),
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
                size: 0,
                allocated: 0,
            }),
            SECRET_FILE => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: SECRET_BODY.len() as u64,
                allocated: SECRET_BODY.len() as u64,
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
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        if dir.raw() != ROOT || index != 0 {
            return Ok(None);
        }
        let name = b"secret.txt";
        if name_out.len() < name.len() {
            return Err(DriverError::BufferTooSmall);
        }
        name_out[..name.len()].copy_from_slice(name);
        Ok(Some(DirEntry {
            node: NodeId::from_raw(SECRET_FILE),
            kind: NodeKind::RegularFile,
            name_len: name.len(),
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
        .stat_via_secured(&owner, &p("/Storage/usb0/secret.txt"), &mut fs)
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
        .list_via_secured(&admin, &p("/Storage/usb0"), &mut fs)
        .expect("secured list");
    assert_eq!(names, ["secret.txt"]);
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
