//! Behavioural tests for driver delegation: the [`Vfs`] resolving a path
//! under a driver-backed mount through a [`FilesystemRead`] driver, with the
//! §5.3 permission template applied at the mount point.

use crate::fs::{Mode, Path, Vfs, VfsError};

use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, MountFlags, NodeId, NodeInfo, NodeKind,
};
use rustos_abi::driver::{DriverError, DriverHandle};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, UserId};

use crate::fs::perm::Credentials;

const ADMIN_UID: u32 = 1;
const ADMIN_GID: u32 = 1;

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
            }),
            KERNEL => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: KERNEL_BODY.len() as u64,
            }),
            README => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: README_BODY.len() as u64,
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
            }),
            DOCS => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: 3,
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
