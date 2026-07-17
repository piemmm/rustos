//! File creation and I/O conformance (`open(2)` with `O_CREAT`, `read`,
//! `write`): creation, `EEXIST`, `ENOENT`, `EISDIR`, `ENOTDIR`, and the
//! round-trip of bytes through the driver.

use rustos_test_posix_fs_suite::*;

#[test]
fn create_makes_an_empty_regular_file() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("f"), &mut fs)
        .expect("create");

    let info = vfs
        .stat_via_secured(&owner, &vol_path("f"), &mut fs)
        .expect("stat");
    assert_eq!(info.kind, NodeKind::RegularFile);
    assert_eq!(info.size, 0);
}

#[test]
fn create_existing_name_is_already_exists() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("dup"), &mut fs)
        .expect("first create");
    assert_eq!(
        vfs.create_via_secured(&owner, &vol_path("dup"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
}

#[test]
fn create_in_missing_directory_is_not_found() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.create_via_secured(&owner, &vol_path("nodir/f"), &mut fs),
        Err(VfsError::NotFound)
    );
}

#[test]
fn write_then_read_round_trips_across_a_block_boundary() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("data.bin"), &mut fs)
        .expect("create");

    // Larger than one 512-byte block to exercise multi-block I/O.
    let payload: Vec<u8> = (0..1000u32)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    let written = vfs
        .write_via_secured(&owner, &vol_path("data.bin"), &mut fs, 0, &payload)
        .expect("write");
    assert_eq!(written, payload.len());

    let mut buf = vec![0u8; payload.len()];
    let read = vfs
        .read_via_secured(&owner, &vol_path("data.bin"), &mut fs, 0, &mut buf)
        .expect("read");
    assert_eq!(read, payload.len());
    assert_eq!(buf, payload);
}

#[test]
fn write_at_offset_leaves_a_sparse_zero_prefix() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("sparse"), &mut fs)
        .expect("create");
    vfs.write_via_secured(&owner, &vol_path("sparse"), &mut fs, 8, b"tail")
        .expect("write at offset 8");

    let mut buf = [0xAAu8; 12];
    let read = vfs
        .read_via_secured(&owner, &vol_path("sparse"), &mut fs, 0, &mut buf)
        .expect("read");
    assert_eq!(read, 12);
    assert_eq!(&buf[..8], &[0u8; 8]);
    assert_eq!(&buf[8..], b"tail");
}

#[test]
fn write_to_a_directory_is_is_a_directory() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("d"), &mut fs)
        .expect("mkdir");
    assert_eq!(
        vfs.write_via_secured(&owner, &vol_path("d"), &mut fs, 0, b"x"),
        Err(VfsError::IsADirectory)
    );
}

#[test]
fn read_of_a_directory_is_is_a_directory() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("d"), &mut fs)
        .expect("mkdir");
    let mut buf = [0u8; 4];
    assert_eq!(
        vfs.read_via_secured(&owner, &vol_path("d"), &mut fs, 0, &mut buf),
        Err(VfsError::IsADirectory)
    );
}

#[test]
fn read_past_end_of_file_returns_zero_bytes() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("short"), &mut fs)
        .expect("create");
    vfs.write_via_secured(&owner, &vol_path("short"), &mut fs, 0, b"hi")
        .expect("write");

    let mut buf = [0u8; 8];
    let read = vfs
        .read_via_secured(&owner, &vol_path("short"), &mut fs, 16, &mut buf)
        .expect("read past EOF");
    assert_eq!(read, 0);
}
