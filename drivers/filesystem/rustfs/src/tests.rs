//! Unit tests for the copy-on-write rustfs driver (`AGENTS.md` §7, §16).
//!
//! They exercise the Stage-1 foundation — format → open round-trip, the
//! superblock-ring selection, and crash replay at every write count during a
//! commit — plus the full read/write surface the VFS consumes, all over an
//! in-memory [`MemBlock`] double.

use super::*;
use rustos_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemTimestamps, FilesystemWrite, NodeKind,
};

/// In-memory block device. Optionally drops writes once a budget is reached,
/// modelling a power loss mid-commit: a dropped write simply never reaches the
/// platter, and the driver's in-memory state is discarded by re-opening from
/// the stored bytes.
struct MemBlock {
    store: alloc::vec::Vec<u8>,
    block_size: u32,
    block_count: u64,
    writes: u32,
    write_budget: Option<u32>,
}

impl MemBlock {
    fn new(block_size: u32, block_count: u64) -> Self {
        let len = block_size as usize * as_usize(block_count);
        Self {
            store: alloc::vec![0u8; len],
            block_size,
            block_count,
            writes: 0,
            write_budget: None,
        }
    }

    fn from_bytes(bytes: alloc::vec::Vec<u8>, block_size: u32, block_count: u64) -> Self {
        Self {
            store: bytes,
            block_size,
            block_count,
            writes: 0,
            write_budget: None,
        }
    }

    fn bytes(&self) -> alloc::vec::Vec<u8> {
        self.store.clone()
    }
}

impl Block for MemBlock {
    fn geometry(&self) -> Result<rustos_abi::driver::block::BlockGeometry, DriverError> {
        Ok(rustos_abi::driver::block::BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.block_size as usize;
        if buf.is_empty() || buf.len() % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = as_usize(lba) * bs;
        let end = start + buf.len();
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let bs = self.block_size as usize;
        if buf.is_empty() || buf.len() % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = as_usize(lba) * bs;
        let end = start + buf.len();
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        // Model power loss: once the budget is spent, the write never lands.
        let drop_write = matches!(self.write_budget, Some(b) if self.writes >= b);
        self.writes += 1;
        if !drop_write {
            self.store[start..end].copy_from_slice(buf);
        }
        Ok(())
    }
}

fn fixed_clock() -> Time64 {
    Time64::from_secs(1_700_000_000)
}

fn fmt(block_size: u32, block_count: u64, inodes: u32) -> RustFs<MemBlock> {
    RustFs::format(MemBlock::new(block_size, block_count), inodes)
        .expect("format a blank device")
        .with_clock(fixed_clock)
}

#[test]
fn format_then_open_round_trips() {
    let fs = fmt(512, 256, 32);
    let bytes = fs.into_block().bytes();
    let reopened = RustFs::open(MemBlock::from_bytes(bytes, 512, 256)).expect("reopen");
    assert_eq!(reopened.root(), NodeId::from_raw(1));
}

#[test]
fn open_rejects_an_unformatted_device() {
    let dev = MemBlock::new(512, 256);
    assert!(matches!(RustFs::open(dev), Err(DriverError::BadMagic)));
}

#[test]
fn create_write_read_back_and_survive_remount() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0xABu8; 9000];
    assert_eq!(fs.write_at(root, b"file", 0, &body), Ok(9000));
    let node = fs.lookup(root, b"file").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, 9000);

    let mut back = alloc::vec![0u8; 9000];
    let mut done = 0;
    while done < 9000 {
        let n = fs
            .read_at(node, done as u64, &mut back[done..])
            .expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(back, body);

    // Survive a remount.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256)).expect("reopen");
    let node = fs.lookup(fs.root(), b"file").expect("lookup after remount");
    let mut back2 = alloc::vec![0u8; 9000];
    let mut done = 0;
    while done < 9000 {
        let n = fs
            .read_at(node, done as u64, &mut back2[done..])
            .expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(back2, body);
}

#[test]
fn indirect_blocks_back_large_files() {
    // A file larger than DIRECT_PTRS blocks forces the single-indirect path.
    let mut fs = fmt(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0xCDu8; 512 * (DIRECT_PTRS + 20)];
    assert_eq!(fs.write_at(root, b"big", 0, &body), Ok(body.len()));
    let node = fs.lookup(root, b"big").expect("lookup");
    let mut back = alloc::vec![0u8; body.len()];
    let mut done = 0;
    while done < body.len() {
        let n = fs
            .read_at(node, done as u64, &mut back[done..])
            .expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(back, body);
}

#[test]
fn nested_directories_and_listing() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"sub", NodeKind::Directory).expect("mkdir");
    let sub = fs.lookup(root, b"sub").expect("lookup sub");
    fs.create(sub, b"inner", NodeKind::RegularFile)
        .expect("create inner");
    let inner = fs.lookup(sub, b"inner").unwrap();
    assert_eq!(fs.node_info(inner).unwrap().size, 0);

    let mut names = alloc::vec::Vec::new();
    let mut buf = [0u8; 64];
    let mut i = 0u64;
    while let Some(e) = fs.read_dir(root, i, &mut buf).expect("read_dir") {
        names.push(buf[..e.name_len].to_vec());
        i += 1;
    }
    assert!(names.iter().any(|n| n == b"sub"));
}

#[test]
fn truncate_keeps_the_surviving_prefix() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![7u8; 4096];
    fs.write_at(root, b"f", 0, &body).expect("write");
    fs.truncate(root, b"f", 2048).expect("truncate");
    let node = fs.lookup(root, b"f").unwrap();
    assert_eq!(fs.node_info(node).unwrap().size, 2048);
    let mut back = alloc::vec![0u8; 2048];
    fs.read_at(node, 0, &mut back).expect("read");
    assert_eq!(back, alloc::vec![7u8; 2048]);
}

#[test]
fn fail_closed_extremes() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"dup", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(
        fs.create(root, b"dup", NodeKind::RegularFile),
        Err(DriverError::Busy)
    );
    assert_eq!(
        fs.create(root, b"", NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
    let oversize = alloc::vec![b'x'; NAME_MAX + 1];
    assert_eq!(
        fs.create(root, &oversize, NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
    fs.create(root, b"d", NodeKind::Directory).expect("mkdir");
    let d = fs.lookup(root, b"d").unwrap();
    fs.create(d, b"child", NodeKind::RegularFile)
        .expect("child");
    assert_eq!(fs.remove(root, b"d"), Err(DriverError::Busy));
    assert_eq!(fs.remove(root, b"nope"), Err(DriverError::NotFound));
}

#[test]
fn remove_reclaims_space_so_allocation_resumes() {
    // Tiny device: fill until NoSpace, free one file, confirm a write succeeds.
    let mut fs = fmt(512, 64, 16);
    let root = fs.root();
    let body = alloc::vec![0x5Au8; 4096];
    let mut last = alloc::string::String::new();
    let mut idx = 0;
    loop {
        let name = alloc::format!("f{idx}");
        if fs
            .create(root, name.as_bytes(), NodeKind::RegularFile)
            .is_err()
        {
            break;
        }
        match fs.write_at(root, name.as_bytes(), 0, &body) {
            Ok(_) => {
                last = name;
                idx += 1;
            }
            Err(DriverError::NoSpace) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
        assert!(idx <= 10_000, "never ran out of space");
    }
    assert!(!last.is_empty(), "at least one file should have landed");
    fs.remove(root, last.as_bytes())
        .expect("remove frees space");
    fs.create(root, b"after", NodeKind::RegularFile)
        .expect("create after free");
    assert_eq!(
        fs.write_at(root, b"after", 0, &alloc::vec![1u8; 512]),
        Ok(512)
    );
}

#[test]
fn timestamps_round_trip_extreme_values() {
    fn old() -> Time64 {
        Time64::from_secs(-2_000_000_000)
    }
    fn future() -> Time64 {
        Time64::from_secs(4_200_000_000)
    }
    let mut fs = RustFs::format(MemBlock::new(512, 256), 32)
        .expect("format")
        .with_clock(old);
    let root = fs.root();
    fs.create(root, b"t", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"t").unwrap();
    assert_eq!(fs.times(node).unwrap().created, old());

    // A far-future value survives a remount.
    let mut sec = Security::new(0o600, 1, 2);
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    fs = fs.with_clock(future);
    fs.set_security(node, sec).expect("set_security");
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256)).expect("reopen");
    let node = fs.lookup(fs.root(), b"t").unwrap();
    assert_eq!(fs.security(node).unwrap(), sec);
    assert_eq!(fs.times(node).unwrap().changed, future());
}

#[test]
fn superblock_ring_selects_the_highest_committed_generation() {
    // Several commits advance the generation; the latest must be selected.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    for i in 0..8 {
        let name = alloc::format!("g{i}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
    }
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256)).expect("reopen");
    for i in 0..8 {
        let name = alloc::format!("g{i}");
        assert!(fs.lookup(fs.root(), name.as_bytes()).is_ok());
    }
}

#[test]
fn crash_at_every_write_count_during_commit_never_tears() {
    // Baseline: a formatted volume with a committed file and an empty target
    // file. The crashed trial performs exactly one transaction (a single
    // `write_at`), so the only valid post-crash outcomes are "the write
    // committed in full" or "it did not commit at all" — never a torn middle.
    let mut base = fmt(512, 256, 32);
    let root = base.root();
    base.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    base.write_at(root, b"keep", 0, b"baseline")
        .expect("write keep");
    base.create(root, b"new", NodeKind::RegularFile)
        .expect("create new");
    let baseline = base.into_block().bytes();

    for budget in 0..64u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 512, 256);
        dev.write_budget = Some(budget);
        let mut fs = RustFs::open(dev).expect("baseline opens");
        let root = fs.root();
        // The single transaction may be cut short at `budget` writes.
        let _ = fs.write_at(root, b"new", 0, b"freshdata");
        let bytes = fs.into_block().bytes();

        // Re-open from the (possibly torn) image: it must mount, and the
        // pre-existing files must always be intact.
        let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256))
            .expect("post-crash mount always succeeds");
        let root = fs.root();
        let keep = fs.lookup(root, b"keep").expect("keep always survives");
        let mut buf = [0u8; 8];
        let n = fs.read_at(keep, 0, &mut buf).expect("read keep");
        assert_eq!(&buf[..n], b"baseline");

        // "new" is always present (created in the baseline). Its contents are
        // either the committed write (9 bytes) or the pre-write empty file —
        // never a torn partial.
        let node = fs.lookup(root, b"new").expect("new always survives");
        let size = fs.node_info(node).expect("info").size;
        assert!(size == 0 || size == 9, "torn size {size}");
        if size == 9 {
            let mut nb = [0u8; 9];
            let n = fs.read_at(node, 0, &mut nb).expect("read new");
            assert_eq!(&nb[..n], b"freshdata");
        }
    }
}
