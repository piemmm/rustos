//! `rustfs` host unit tests against an in-memory [`MockBlock`] device.
//!
//! The crate is `no_std`; the test harness links `std`, so the backing
//! store is a heap `Vec` (a multi-kilobyte fixed array would trip the
//! `large_stack_arrays` lint and the journal needs a non-trivial volume).
//! Every test formats a fresh volume and drives the real
//! `format`/`open`/`FilesystemRead`/`FilesystemWrite`/`security` paths.

extern crate std;

use super::*;
use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::{DriverError, DriverKind};
use std::vec::Vec;

const BS: usize = 512;
const BS_U32: u32 = 512;
const COUNT: u64 = 128;
const STORE: usize = BS * 128;
const INODES: u32 = 32;

/// In-memory block device with optional write-fault injection.
struct MockBlock {
    store: Vec<u8>,
    writes_left: Option<usize>,
}

impl MockBlock {
    fn new() -> Self {
        Self {
            store: std::vec![0u8; STORE],
            writes_left: None,
        }
    }
}

impl Block for MockBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BS_U32,
            block_count: COUNT,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        if buf.is_empty() || buf.len() % BS != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (buf.len() / BS) as u64;
        if lba.saturating_add(blocks) > COUNT {
            return Err(DriverError::LengthOutOfRange);
        }
        let start = usize::try_from(lba).unwrap_or(usize::MAX) * BS;
        buf.copy_from_slice(&self.store[start..start + buf.len()]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        if buf.is_empty() || buf.len() % BS != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (buf.len() / BS) as u64;
        if lba.saturating_add(blocks) > COUNT {
            return Err(DriverError::LengthOutOfRange);
        }
        if let Some(n) = self.writes_left {
            if n == 0 {
                return Err(DriverError::DeviceFault);
            }
            self.writes_left = Some(n - 1);
        }
        let start = usize::try_from(lba).unwrap_or(usize::MAX) * BS;
        self.store[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

fn fresh() -> RustFs<MockBlock> {
    RustFs::format(MockBlock::new(), INODES).expect("format")
}

/// Minimal [`DriverHost`] granting (or withholding) a single capability.
struct Host {
    grant_load: bool,
}

impl DriverHost for Host {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.grant_load && cap == CapabilityId::DRV_LOAD
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

#[test]
fn register_requires_drv_load() {
    assert!(register(&Host { grant_load: true }).is_ok());
    assert_eq!(
        register(&Host { grant_load: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn format_then_open_round_trips_the_root() {
    let fs = fresh();
    let dev = fs.into_block();
    let mut fs = RustFs::open(dev).expect("open");
    let root = fs.root();
    let info = fs.node_info(root).expect("root info");
    assert_eq!(info.kind, NodeKind::Directory);
    // An empty root lists no children.
    let mut name = [0u8; NAME_MAX];
    assert_eq!(fs.read_dir(root, 0, &mut name), Ok(None));
}

#[test]
fn open_rejects_unformatted_device() {
    assert_eq!(
        RustFs::open(MockBlock::new()).map(|_| ()),
        Err(DriverError::BadMagic)
    );
}

#[test]
fn create_write_read_file() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"hello.txt", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(
        fs.write_at(root, b"hello.txt", 0, b"Hello, rustfs!"),
        Ok(14)
    );
    let info = fs.node_info(file).expect("info");
    assert_eq!(info.kind, NodeKind::RegularFile);
    assert_eq!(info.size, 14);
    let mut buf = [0u8; 32];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"Hello, rustfs!");
    // Reading at EOF yields zero bytes.
    assert_eq!(fs.read_at(file, 14, &mut buf), Ok(0));
    // Offset read.
    let n = fs.read_at(file, 7, &mut buf).expect("read offset");
    assert_eq!(&buf[..n], b"rustfs!");
}

#[test]
fn create_rejects_duplicate() {
    let mut fs = fresh();
    let root = fs.root();
    fs.create(root, b"dup", NodeKind::RegularFile)
        .expect("first");
    assert_eq!(
        fs.create(root, b"dup", NodeKind::RegularFile),
        Err(DriverError::Busy)
    );
}

#[test]
fn lookup_and_kind_errors() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.lookup(root, b"f"), Ok(file));
    assert_eq!(fs.lookup(root, b"missing"), Err(DriverError::NotFound));
    // lookup inside a regular file is unsupported.
    assert_eq!(fs.lookup(file, b"x"), Err(DriverError::Unsupported));
    // read_at on a directory is unsupported.
    let mut buf = [0u8; 4];
    assert_eq!(fs.read_at(root, 0, &mut buf), Err(DriverError::Unsupported));
}

#[test]
fn nested_directories_and_listing() {
    let mut fs = fresh();
    let root = fs.root();
    let dir = fs.create(root, b"sub", NodeKind::Directory).expect("mkdir");
    let info = fs.node_info(dir).expect("dir info");
    assert_eq!(info.kind, NodeKind::Directory);
    fs.create(dir, b"a.bin", NodeKind::RegularFile)
        .expect("nested file");
    assert_eq!(fs.write_at(dir, b"a.bin", 0, b"data"), Ok(4));
    let child = fs.lookup(dir, b"a.bin").expect("lookup nested");
    let mut buf = [0u8; 8];
    let n = fs.read_at(child, 0, &mut buf).expect("read nested");
    assert_eq!(&buf[..n], b"data");
    // The root lists exactly "sub" (skips "." / "..").
    let mut name = [0u8; NAME_MAX];
    let e = fs
        .read_dir(root, 0, &mut name)
        .expect("entry")
        .expect("some");
    assert_eq!(&name[..e.name_len], b"sub");
    assert_eq!(e.kind, NodeKind::Directory);
    assert_eq!(fs.read_dir(root, 1, &mut name), Ok(None));
}

#[test]
fn read_dir_lists_multiple_and_buffer_guard() {
    let mut fs = fresh();
    let root = fs.root();
    fs.create(root, b"one", NodeKind::RegularFile).expect("one");
    fs.create(root, b"two", NodeKind::RegularFile).expect("two");
    fs.create(root, b"three", NodeKind::RegularFile)
        .expect("three");
    let mut count = 0;
    let mut name = [0u8; NAME_MAX];
    while fs
        .read_dir(root, count, &mut name)
        .expect("iterate")
        .is_some()
    {
        count += 1;
    }
    assert_eq!(count, 3);
    // A too-small name buffer is reported, not truncated.
    let mut tiny = [0u8; 2];
    assert_eq!(
        fs.read_dir(root, 0, &mut tiny),
        Err(DriverError::BufferTooSmall)
    );
}

#[test]
fn write_extends_across_block_boundary_and_sparse() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"big", NodeKind::RegularFile)
        .expect("create");
    // Straddle the first block boundary (BS = 512).
    let mut payload = [0u8; 600];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = u8::try_from(i % 251).unwrap_or(0);
    }
    assert_eq!(fs.write_at(root, b"big", 0, &payload), Ok(600));
    let mut back = [0u8; 600];
    assert_eq!(fs.read_at(file, 0, &mut back), Ok(600));
    assert_eq!(back, payload);
    // A sparse write past EOF zero-fills the gap.
    assert_eq!(fs.write_at(root, b"big", 1000, b"Z"), Ok(1));
    assert_eq!(fs.node_info(file).expect("info").size, 1001);
    let mut hole = [0xAAu8; 8];
    let n = fs.read_at(file, 700, &mut hole).expect("hole");
    assert_eq!(n, 8);
    assert!(hole.iter().all(|&b| b == 0));
}

#[test]
fn indirect_blocks_hold_large_files() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"l", NodeKind::RegularFile)
        .expect("create");
    // Larger than the 16 direct blocks (16 * 512 = 8192 bytes).
    let mut payload = [0u8; 12000];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = u8::try_from(i % 256).unwrap_or(0);
    }
    assert_eq!(fs.write_at(root, b"l", 0, &payload), Ok(12000));
    let dev = fs.into_block();
    let mut fs = RustFs::open(dev).expect("reopen");
    let mut back = [0u8; 12000];
    assert_eq!(fs.read_at(file, 0, &mut back), Ok(12000));
    assert_eq!(back, payload);
}

#[test]
fn truncate_shrink_and_grow() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"t", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"t", 0, b"abcdefghij").expect("write");
    fs.truncate(root, b"t", 4).expect("shrink");
    assert_eq!(fs.node_info(file).expect("info").size, 4);
    let mut buf = [0u8; 16];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"abcd");
    // Grow back: the gap reads as zeros.
    fs.truncate(root, b"t", 8).expect("grow");
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"abcd\0\0\0\0");
}

#[test]
fn remove_file_and_name_reuse() {
    let mut fs = fresh();
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"bye").expect("write");
    fs.remove(root, b"f").expect("remove");
    assert_eq!(fs.lookup(root, b"f"), Err(DriverError::NotFound));
    // The name (and freed storage) can be reused.
    let f2 = fs
        .create(root, b"f", NodeKind::RegularFile)
        .expect("recreate");
    let mut buf = [0u8; 4];
    assert_eq!(fs.read_at(f2, 0, &mut buf), Ok(0));
}

#[test]
fn remove_non_empty_directory_is_busy() {
    let mut fs = fresh();
    let root = fs.root();
    let dir = fs.create(root, b"d", NodeKind::Directory).expect("mkdir");
    fs.create(dir, b"child", NodeKind::RegularFile)
        .expect("child");
    assert_eq!(fs.remove(root, b"d"), Err(DriverError::Busy));
    // Emptying it allows removal.
    fs.remove(dir, b"child").expect("rm child");
    fs.remove(root, b"d").expect("rmdir");
    assert_eq!(fs.lookup(root, b"d"), Err(DriverError::NotFound));
}

#[test]
fn security_record_round_trips_and_persists() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"s", NodeKind::RegularFile)
        .expect("create");
    let mut sec = Security::new(0o600, 7, 9);
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    sec.push_acl(AclEntry {
        subject: AclSubject::User(3),
        perms: 0b100,
    })
    .expect("acl 1");
    sec.push_acl(AclEntry {
        subject: AclSubject::Group(11),
        perms: 0b110,
    })
    .expect("acl 2");
    fs.set_security(file, sec).expect("set security");
    assert_eq!(fs.security(file), Ok(sec));
    // It survives a remount (it is on-disk inode state).
    let dev = fs.into_block();
    let mut fs = RustFs::open(dev).expect("reopen");
    let reloaded = fs.security(file).expect("reload security");
    assert_eq!(reloaded, sec);
    assert_eq!(reloaded.required_cap, Some(CapabilityId::AUDIT_READ));
    assert_eq!(reloaded.acl().len(), 2);
}

#[test]
fn acl_overflow_is_rejected() {
    let mut sec = Security::new(0o644, 0, 0);
    for _ in 0..ACL_MAX {
        sec.push_acl(AclEntry {
            subject: AclSubject::User(1),
            perms: 0b100,
        })
        .expect("within bound");
    }
    assert_eq!(
        sec.push_acl(AclEntry {
            subject: AclSubject::User(2),
            perms: 0b100,
        }),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn cow_overwrite_persists_across_remount() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"c", 0, b"AAAAA").expect("first");
    // Copy-on-write overwrite of the same region.
    fs.write_at(root, b"c", 0, b"BBBBB").expect("overwrite");
    let dev = fs.into_block();
    let mut fs = RustFs::open(dev).expect("reopen");
    let mut buf = [0u8; 5];
    assert_eq!(fs.read_at(file, 0, &mut buf), Ok(5));
    assert_eq!(buf, *b"BBBBB");
}

/// Crash consistency: faulting the device after *every* possible write
/// count during a journalled overwrite must leave the file either fully
/// at its old contents (transaction rolled back) or fully at its new
/// contents (transaction replayed) — never torn. The sweep also proves
/// the journal genuinely exercises both outcomes.
#[test]
fn journal_is_crash_consistent_at_every_write_point() {
    const OLD: [u8; 5] = *b"AAAAA";
    const NEW: [u8; 5] = *b"BBBBB";
    let mut saw_old = false;
    let mut saw_new = false;
    for k in 0..48 {
        let mut fs = fresh();
        let root = fs.root();
        let file = fs
            .create(root, b"f", NodeKind::RegularFile)
            .expect("create");
        fs.write_at(root, b"f", 0, &OLD).expect("seed");

        // Re-open with a write budget, then attempt the overwrite. The
        // pre-overwrite open touches no blocks (the journal is clean).
        let mut dev = fs.into_block();
        dev.writes_left = Some(k);
        let mut fs = RustFs::open(dev).expect("budgeted open");
        let _ = fs.write_at(root, b"f", 0, &NEW);

        // Recover with an unbudgeted re-open and inspect the result.
        let mut dev = fs.into_block();
        dev.writes_left = None;
        let mut fs = RustFs::open(dev).expect("recovery open");
        let mut buf = [0u8; 5];
        let n = fs.read_at(file, 0, &mut buf).expect("read");
        assert_eq!(n, 5);
        if buf == OLD {
            saw_old = true;
        } else if buf == NEW {
            saw_new = true;
        } else {
            panic!("torn data after crash at write {k}: {buf:?}");
        }
        // The volume is still usable after recovery.
        assert!(fs.lookup(root, b"f").is_ok());
    }
    assert!(saw_old, "no crash point rolled the overwrite back");
    assert!(saw_new, "no crash point replayed the overwrite");
}
