//! permission-model conformance, exercised end-to-end: the VFS
//! applies mode bits, ACL grants, and the optional per-inode capability
//! gate to the record the real `arxfs` driver stores. This is the suite's
//! analogue of `pjdfstest`'s `chmod`/`granular` permission cases, plus the
//! capability gate the charter and `PLAN.md` Stage 5
//! call out explicitly.
//!
//! The decision never branches on `uid == 0`: an owning user is
//! granted by its owner triad, not by being uid 0.

use rustos_test_posix_fs_suite::*;

const OWNER_UID: u32 = 1000;
const OWNER_GID: u32 = 1000;
const OTHER_UID: u32 = 2000;
const SHARED_GID: u32 = 42;

/// Create `name` at the volume root as the installer (uid 0) and write
/// `body`, then re-stamp its stored record with `sec`. Returns once
/// the new record is durable.
fn planted_file(vfs: &Vfs, fs: &mut LiveFs, name: &str, body: &[u8], sec: NodeSecurity) {
    let caps = CapabilitySet::empty();
    let admin = cred(ROOT_UID, ROOT_GID, &caps);
    vfs.create_via_secured(&admin, &vol_path(name), fs)
        .expect("create as installer");
    vfs.write_via_secured(&admin, &vol_path(name), fs, 0, body)
        .expect("write as installer");
    let node = root_node_id(fs, name.as_bytes());
    fs.set_security(node, sec)
        .expect("re-stamp security record");
}

#[test]
fn owner_can_read_a_private_file_but_a_stranger_cannot() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    planted_file(
        &vfs,
        &mut fs,
        "secret",
        b"top secret",
        NodeSecurity::new(0o600, OWNER_UID, OWNER_GID),
    );

    let caps = CapabilitySet::empty();
    let mut buf = [0u8; 16];

    let owner = cred(OWNER_UID, OWNER_GID, &caps);
    let read = vfs
        .read_via_secured(&owner, &vol_path("secret"), &mut fs, 0, &mut buf)
        .expect("owner reads its own 0600 file");
    assert_eq!(&buf[..read], b"top secret");

    let stranger = cred(OTHER_UID, OTHER_UID, &caps);
    assert_eq!(
        vfs.read_via_secured(&stranger, &vol_path("secret"), &mut fs, 0, &mut buf),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn capability_gate_blocks_read_even_at_mode_0644() {
    // PLAN.md Stage 5 /: a file marked with a required
    // capability is unreadable without it, even though mode 0644 would
    // otherwise grant the read.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let mut sec = NodeSecurity::new(0o644, OWNER_UID, OWNER_GID);
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    planted_file(&vfs, &mut fs, "audit.log", b"audit trail", sec);

    let mut buf = [0u8; 16];

    let without = CapabilitySet::empty();
    let denied = cred(OTHER_UID, OTHER_UID, &without);
    assert_eq!(
        vfs.read_via_secured(&denied, &vol_path("audit.log"), &mut fs, 0, &mut buf),
        Err(VfsError::PermissionDenied)
    );

    let mut with = CapabilitySet::empty();
    with.insert(CapabilityId::AUDIT_READ);
    let allowed = cred(OTHER_UID, OTHER_UID, &with);
    let read = vfs
        .read_via_secured(&allowed, &vol_path("audit.log"), &mut fs, 0, &mut buf)
        .expect("holder of the capability reads the file");
    assert_eq!(&buf[..read], b"audit trail");
}

#[test]
fn acl_grant_allows_a_read_the_mode_bits_would_deny() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let mut sec = NodeSecurity::new(0o600, OWNER_UID, OWNER_GID);
    sec.push_acl(SecurityAcl {
        subject: SecuritySubject::Group(SHARED_GID),
        perms: 0b100,
    })
    .expect("room for one ACL entry");
    planted_file(&vfs, &mut fs, "shared", b"shared bytes", sec);

    let caps = CapabilitySet::empty();
    let mut buf = [0u8; 16];

    // A member of the granted group reads despite the 0600 mode bits.
    let groups = [GroupId(SHARED_GID)];
    let member = cred_with_groups(OTHER_UID, OTHER_UID, &groups, &caps);
    let read = vfs
        .read_via_secured(&member, &vol_path("shared"), &mut fs, 0, &mut buf)
        .expect("ACL grant admits the group member");
    assert_eq!(&buf[..read], b"shared bytes");

    // A user outside the group still falls through to the denying mode.
    let outsider = cred(OTHER_UID, OTHER_UID, &caps);
    assert_eq!(
        vfs.read_via_secured(&outsider, &vol_path("shared"), &mut fs, 0, &mut buf),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn directory_without_search_permission_blocks_traversal() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let admin = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&admin, &vol_path("priv"), &mut fs)
        .expect("mkdir priv");
    vfs.create_via_secured(&admin, &vol_path("priv/inside"), &mut fs)
        .expect("create inside");
    vfs.write_via_secured(&admin, &vol_path("priv/inside"), &mut fs, 0, b"hidden")
        .expect("write inside");

    // Restrict the directory to its owner with no access for others.
    let dir = root_node_id(&mut fs, b"priv");
    fs.set_security(dir, NodeSecurity::new(0o700, OWNER_UID, OWNER_GID))
        .expect("restrict directory");

    let mut buf = [0u8; 16];

    let stranger = cred(OTHER_UID, OTHER_UID, &caps);
    assert_eq!(
        vfs.read_via_secured(&stranger, &vol_path("priv/inside"), &mut fs, 0, &mut buf),
        Err(VfsError::PermissionDenied)
    );

    // The directory's owner has search permission and reaches the file.
    let owner = cred(OWNER_UID, OWNER_GID, &caps);
    let read = vfs
        .read_via_secured(&owner, &vol_path("priv/inside"), &mut fs, 0, &mut buf)
        .expect("owner traverses its own directory");
    assert_eq!(&buf[..read], b"hidden");
}

#[test]
fn write_into_a_read_only_directory_is_denied() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let admin = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&admin, &vol_path("ro"), &mut fs)
        .expect("mkdir ro");
    let dir = root_node_id(&mut fs, b"ro");
    // r-x for the owner: search is allowed, but creating an entry is not.
    fs.set_security(dir, NodeSecurity::new(0o500, OWNER_UID, OWNER_GID))
        .expect("make directory non-writable");

    let owner = cred(OWNER_UID, OWNER_GID, &caps);
    assert_eq!(
        vfs.create_via_secured(&owner, &vol_path("ro/new"), &mut fs),
        Err(VfsError::PermissionDenied)
    );
}
