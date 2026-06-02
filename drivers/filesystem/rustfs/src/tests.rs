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
fn creating_files_until_the_inode_table_is_full_reports_no_space() {
    let mut fs = fresh();
    let root = fs.root();
    // The fixture has INODES inodes; root and inode 0 are reserved, so a
    // bounded run of creates must eventually exhaust the table. The driver
    // must report NoSpace (the volume is full), never DeviceFault.
    let mut last = Ok(NodeId::from_raw(0));
    for i in 0..INODES + 8 {
        let mut name = *b"f0000000";
        let mut v = i;
        for slot in name[1..].iter_mut().rev() {
            *slot = b'0' + u8::try_from(v % 10).unwrap_or(0);
            v /= 10;
        }
        last = fs.create(root, &name, NodeKind::RegularFile);
        if last.is_err() {
            break;
        }
    }
    assert_eq!(last, Err(DriverError::NoSpace));
}

#[test]
fn filling_the_data_region_reports_no_space() {
    // A single file is capped by its addressing limit, so spread the writes
    // across several files to exhaust the data region itself. Append
    // block-sized chunks round-robin until allocation fails. The data
    // region must run dry with a NoSpace error (the volume is full), never
    // a DeviceFault.
    const FILES: usize = 4;
    let mut fs = fresh();
    let root = fs.root();
    let names: [&[u8]; FILES] = [b"a", b"b", b"c", b"d"];
    for name in names {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
    let chunk = [0xABu8; BS];
    let mut offset = [0u64; FILES];
    let result = 'fill: loop {
        let mut progressed = false;
        for (i, name) in names.iter().enumerate() {
            match fs.write_at(root, name, offset[i], &chunk) {
                Ok(n) => {
                    offset[i] += n as u64;
                    progressed = true;
                }
                // This file has reached its addressing limit; the data
                // region may still have room, so try the next file.
                Err(DriverError::LengthOutOfRange) => {}
                other => break 'fill other.map(|_| ()),
            }
        }
        assert!(progressed, "data region never reported NoSpace");
    };
    assert_eq!(result, Err(DriverError::NoSpace));
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

/// A deterministic, seeded multi-operation **journal soak**: a long
/// randomised script of `create`/`write`/`truncate`/`remove` operations
/// is driven against the volume, and *every* operation is independently
/// crash-tested at *every* device-write count. After each simulated
/// crash the recovered volume must equal the whole-tree snapshot either
/// before the operation (transaction rolled back) or after it
/// (transaction replayed) — never a torn intermediate — and must remain
/// mountable and listable. The soak proves the per-operation atomicity
/// of §16 / §5.3 metadata across the full mutation surface, not just a
/// single overwrite.
const SOAK_BUDGET: usize = 32;
const SOAK_NAMES: [&[u8]; 4] = [b"a", b"b", b"c", b"d"];

/// One scripted mutation, addressed by a `(root, name)` pair.
#[derive(Debug, Clone)]
enum SoakOp {
    CreateFile(&'static [u8]),
    CreateDir(&'static [u8]),
    Write(&'static [u8], u64, Vec<u8>),
    Truncate(&'static [u8], u64),
    Remove(&'static [u8]),
}

/// Small linear-congruential PRNG (no external dependency, §2.12) seeded
/// for a fully reproducible script.
struct Lcg(u64);

impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next_u32() % n
    }
}

/// Build a store-backed device from a captured image.
fn device_from(store: &[u8], writes_left: Option<usize>) -> MockBlock {
    MockBlock {
        store: store.to_vec(),
        writes_left,
    }
}

/// Read a regular file's full contents (bounded by its reported size).
fn read_file(fs: &mut RustFs<MockBlock>, id: NodeId) -> Vec<u8> {
    let size = usize::try_from(fs.node_info(id).map_or(0, |i| i.size)).unwrap_or(0);
    let mut out: Vec<u8> = Vec::new();
    let mut off = 0u64;
    let mut buf = [0u8; BS];
    while out.len() < size {
        match fs.read_at(id, off, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let take = n.min(size - out.len());
                out.extend_from_slice(&buf[..take]);
                off = off.saturating_add(n as u64);
            }
        }
    }
    out
}

/// Recursively snapshot a directory into sorted `(path, kind, content)`
/// triples. `kind` is `0` for files, `1` for directories.
fn collect(
    fs: &mut RustFs<MockBlock>,
    dir: NodeId,
    prefix: &[u8],
    out: &mut Vec<(Vec<u8>, u8, Vec<u8>)>,
) {
    let mut entries: Vec<(Vec<u8>, NodeKind)> = Vec::new();
    let mut idx = 0u64;
    let mut name = [0u8; NAME_MAX];
    while let Ok(Some(e)) = fs.read_dir(dir, idx, &mut name) {
        entries.push((name[..e.name_len].to_vec(), e.kind));
        idx += 1;
    }
    for (nm, kind) in entries {
        let mut path = prefix.to_vec();
        path.push(b'/');
        path.extend_from_slice(&nm);
        if kind == NodeKind::Directory {
            out.push((path.clone(), 1, Vec::new()));
            if let Ok(id) = fs.lookup(dir, &nm) {
                collect(fs, id, &path, out);
            }
        } else {
            let content = fs
                .lookup(dir, &nm)
                .map(|id| read_file(fs, id))
                .unwrap_or_default();
            out.push((path, 0, content));
        }
    }
}

/// Open a clone of `store` (replaying the journal) and snapshot the whole
/// tree. A failed mount is a recovery defect and fails the test here.
fn snapshot(store: &[u8]) -> Vec<(Vec<u8>, u8, Vec<u8>)> {
    let mut fs = RustFs::open(device_from(store, None)).expect("snapshot mount");
    let root = fs.root();
    let mut out = Vec::new();
    collect(&mut fs, root, b"", &mut out);
    out.sort();
    out
}

/// Apply one scripted operation; errors (duplicate, missing, wrong kind)
/// are legitimate no-ops for consistency and are ignored.
fn apply_op(fs: &mut RustFs<MockBlock>, root: NodeId, op: &SoakOp) {
    let _ = match op {
        SoakOp::CreateFile(n) => fs.create(root, n, NodeKind::RegularFile).map(|_| ()),
        SoakOp::CreateDir(n) => fs.create(root, n, NodeKind::Directory).map(|_| ()),
        SoakOp::Write(n, off, data) => fs.write_at(root, n, *off, data).map(|_| ()),
        SoakOp::Truncate(n, len) => fs.truncate(root, n, *len),
        SoakOp::Remove(n) => fs.remove(root, n),
    };
}

/// Produce the committed image that results from applying `op` to `store`.
fn commit(store: &[u8], op: &SoakOp) -> Vec<u8> {
    let mut fs = RustFs::open(device_from(store, None)).expect("commit mount");
    let root = fs.root();
    apply_op(&mut fs, root, op);
    fs.into_block().store
}

#[test]
fn journal_soak_is_crash_consistent_across_a_random_op_stream() {
    let mut rng = Lcg(0x0BAD_F00D_DEAD_BEEF);
    let mut script: Vec<SoakOp> = Vec::new();
    for _ in 0..24 {
        let name = SOAK_NAMES[(rng.next_u32() as usize) % SOAK_NAMES.len()];
        script.push(match rng.below(5) {
            0 => SoakOp::CreateFile(name),
            1 => SoakOp::CreateDir(name),
            2 => {
                let off = u64::from(rng.below(2048));
                let len = (rng.below(600) + 1) as usize;
                let mut data = Vec::with_capacity(len);
                for _ in 0..len {
                    data.push((rng.next_u32() & 0xFF) as u8);
                }
                SoakOp::Write(name, off, data)
            }
            3 => SoakOp::Truncate(name, u64::from(rng.below(2048))),
            _ => SoakOp::Remove(name),
        });
    }

    let mut good = fresh().into_block().store;
    let mut saw_change = false;
    let mut saw_old = false;
    let mut saw_new = false;

    for op in &script {
        let old_snap = snapshot(&good);
        let new_store = commit(&good, op);
        let new_snap = snapshot(&new_store);
        if old_snap != new_snap {
            saw_change = true;
        }

        for k in 0..SOAK_BUDGET {
            let mut fs = RustFs::open(device_from(&good, Some(k))).expect("crash mount");
            let root = fs.root();
            apply_op(&mut fs, root, op);
            let crashed = fs.into_block().store;
            let recovered = snapshot(&crashed);
            if recovered == old_snap {
                saw_old = true;
            } else if recovered == new_snap {
                saw_new = true;
            } else {
                panic!("torn volume after crash at write {k} for op {op:?}");
            }
        }

        good = new_store;
    }

    assert!(saw_change, "the script never mutated the volume");
    assert!(saw_old, "no crash point ever rolled an operation back");
    assert!(saw_new, "no crash point ever replayed an operation");
}

// The clock seam is a stateless `fn() -> Time64`, so each instant a test
// needs is its own constant-returning clock, re-installed with
// `RustFs::with_clock` between steps. This keeps the tests deterministic
// and free of shared mutable state (`AGENTS.md` §2.1 — no global mutable
// static; §7 — no flaky tests).
fn clock_1000() -> Time64 {
    Time64::from_secs(1_000)
}
fn clock_2000() -> Time64 {
    Time64::from_secs(2_000)
}
fn clock_3000() -> Time64 {
    Time64::from_secs(3_000)
}
fn clock_4000() -> Time64 {
    Time64::from_secs(4_000)
}
/// ~1906: a pre-1970 instant whose seconds value is negative.
fn clock_pre_1970() -> Time64 {
    Time64::from_secs(-2_000_000_000)
}
/// ~2096: a post-2038 instant beyond the signed 32-bit seconds wall.
fn clock_post_2038() -> Time64 {
    Time64::from_secs(4_000_000_000)
}
fn clock_100() -> Time64 {
    Time64::from_secs(100)
}
fn clock_200() -> Time64 {
    Time64::from_secs(200)
}

#[test]
fn timestamps_default_to_the_epoch_without_a_clock() {
    let mut fs = fresh();
    let root = fs.root();
    let file = fs
        .create(root, b"e", NodeKind::RegularFile)
        .expect("create");
    let t = fs.times(file).expect("times");
    assert_eq!(t.created, Time64::UNIX_EPOCH);
    assert_eq!(t.modified, Time64::UNIX_EPOCH);
    assert_eq!(t.accessed, Time64::UNIX_EPOCH);
    assert_eq!(t.changed, Time64::UNIX_EPOCH);
}

#[test]
fn timestamps_are_stamped_persisted_and_span_the_64bit_range() {
    let mut fs = RustFs::format(MockBlock::new(), INODES)
        .expect("format")
        .with_clock(clock_1000);
    let root = fs.root();

    // Create stamps all four timestamps to the creation instant.
    let file = fs
        .create(root, b"t", NodeKind::RegularFile)
        .expect("create");
    let t = fs.times(file).expect("times");
    assert_eq!(t.created, Time64::from_secs(1_000));
    assert_eq!(t.modified, Time64::from_secs(1_000));
    assert_eq!(t.accessed, Time64::from_secs(1_000));
    assert_eq!(t.changed, Time64::from_secs(1_000));

    // A write advances mtime/atime/ctime but never the creation time.
    fs = fs.with_clock(clock_2000);
    fs.write_at(root, b"t", 0, b"hello").expect("write");
    let t = fs.times(file).expect("times");
    assert_eq!(t.created, Time64::from_secs(1_000));
    assert_eq!(t.modified, Time64::from_secs(2_000));
    assert_eq!(t.accessed, Time64::from_secs(2_000));
    assert_eq!(t.changed, Time64::from_secs(2_000));

    // A metadata change touches only ctime.
    fs = fs.with_clock(clock_3000);
    fs.set_security(file, Security::new(0o600, 1, 2))
        .expect("set security");
    let t = fs.times(file).expect("times");
    assert_eq!(t.modified, Time64::from_secs(2_000));
    assert_eq!(t.changed, Time64::from_secs(3_000));

    // A truncate is a content change: mtime and ctime advance.
    fs = fs.with_clock(clock_4000);
    fs.truncate(root, b"t", 2).expect("truncate");
    let t = fs.times(file).expect("times");
    assert_eq!(t.modified, Time64::from_secs(4_000));
    assert_eq!(t.changed, Time64::from_secs(4_000));

    // A pre-1970 creation time and a post-2038 modification time (beyond
    // the i32-seconds wall) round-trip without truncation (§21).
    fs = fs.with_clock(clock_pre_1970);
    let old = fs
        .create(root, b"old", NodeKind::RegularFile)
        .expect("create old");
    fs = fs.with_clock(clock_post_2038);
    fs.write_at(root, b"old", 0, b"x").expect("write old");

    // Every timestamp is on-disk inode state and survives a remount.
    let dev = fs.into_block();
    let mut fs = RustFs::open(dev).expect("reopen");
    let t = fs.times(file).expect("reload times");
    assert_eq!(t.created, Time64::from_secs(1_000));
    assert_eq!(t.modified, Time64::from_secs(4_000));
    let told = fs.times(old).expect("reload old times");
    assert_eq!(told.created, clock_pre_1970());
    assert_eq!(told.modified, clock_post_2038());
    assert!(told.created.secs() < 0, "pre-1970 time lost its sign");
    assert!(
        told.modified.secs() > i64::from(i32::MAX),
        "post-2038 time was truncated to 32 bits"
    );
}

#[test]
fn directory_timestamps_track_create_and_remove() {
    let mut fs = RustFs::format(MockBlock::new(), INODES)
        .expect("format")
        .with_clock(clock_100);
    let root = fs.root();

    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create");
    let rt = fs.times(root).expect("root times");
    assert_eq!(rt.modified, Time64::from_secs(100));
    assert_eq!(rt.changed, Time64::from_secs(100));

    fs = fs.with_clock(clock_200);
    fs.remove(root, b"a").expect("remove");
    let rt = fs.times(root).expect("root times");
    assert_eq!(rt.modified, Time64::from_secs(200));
    assert_eq!(rt.changed, Time64::from_secs(200));
}
