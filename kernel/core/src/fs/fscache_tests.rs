//! Host tests for the clean, rebuildable filesystem cache
//! (`plans/SMARTRAM.md` section 6.1): hit/miss behaviour, precise
//! invalidation on every mutation, budget-bounded eviction with
//! hysteresis, large-read bypass, authorisation-sensitive reuse, and
//! ledger consistency.

use super::*;

use crate::fs::memfs::RwMockFs;
use crate::fs::perm::{Credentials, Metadata, Mode};
use crate::fs::{Path, Vfs, VfsError};
use crate::test_pressure::{free_for, pressured, unpressured, TestSource};
use crate::test_sink::TestSink;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::DriverHandle;
use tairix_caps::CapabilitySet;
use tairix_kernel_mem::PressureBand;
use tairix_kernel_sec::{GroupId, UserId};

/// A driver wrapper counting every structural call, so a test can prove
/// a cache hit never reached the device.
struct Counting<F> {
    inner: F,
    calls: u64,
}

impl<F> Counting<F> {
    fn new(inner: F) -> Self {
        Self { inner, calls: 0 }
    }
}

impl<F: FilesystemRead> FilesystemRead for Counting<F> {
    fn root(&self) -> NodeId {
        self.inner.root()
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        self.calls += 1;
        self.inner.node_info(node)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        self.calls += 1;
        self.inner.lookup(dir, name)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        self.calls += 1;
        self.inner.read_at(file, offset, buf)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        self.calls += 1;
        self.inner.read_dir(dir, cursor, name_out)
    }
}

impl<F: FilesystemWrite> FilesystemWrite for Counting<F> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.calls += 1;
        self.inner.create(dir, name, kind)
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.calls += 1;
        self.inner.write_at(dir, name, offset, data)
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.calls += 1;
        self.inner.truncate(dir, name, size)
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.calls += 1;
        self.inner.remove(dir, name)
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        self.calls += 1;
        self.inner.rename(src_dir, src_name, dst_dir, dst_name)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.calls += 1;
        self.inner.flush()
    }
}

impl<F: FilesystemSecurity> FilesystemSecurity for Counting<F> {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        self.calls += 1;
        self.inner.security(node)
    }

    fn set_security(&mut self, node: NodeId, security: NodeSecurity) -> Result<(), DriverError> {
        self.calls += 1;
        self.inner.set_security(node, security)
    }
}

impl<F: FilesystemStats> FilesystemStats for Counting<F> {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        self.calls += 1;
        self.inner.stats()
    }
}

/// A generous test budget (1 MiB hard limit).
fn budget() -> CacheBudget {
    CacheBudget::from_backing(16 * 1024 * 1024)
}

/// The test volume's reclaim owner.
fn owner() -> ReclaimOwner {
    ReclaimOwner::FilesystemVolume { volume: 1 }
}

/// A leaked capturing sink for the cache's audit records.
fn sink() -> &'static TestSink {
    Box::leak(Box::new(TestSink::new()))
}

/// The wrapped counting driver's call total.
fn calls(cache: &CachedFs<Counting<RwMockFs>>) -> u64 {
    cache.inner_driver().calls
}

/// A cached mock volume with `/dir/file.txt` holding `contents`.
fn fixture(contents: &[u8]) -> CachedFs<Counting<RwMockFs>> {
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"dir", NodeKind::Directory).expect("mkdir");
    let dir = fs.lookup(root, b"dir").expect("dir resolves");
    fs.create(dir, b"file.txt", NodeKind::RegularFile)
        .expect("create");
    let written = fs
        .write_at(dir, b"file.txt", 0, contents)
        .expect("seed contents");
    assert_eq!(written, contents.len());
    CachedFs::new(Counting::new(fs), budget(), owner(), unpressured(), sink())
}

/// A leaked cache-admission control with the filesystem class disabled
/// (`cache.filesystem off`), its own instance so it never touches the
/// process-global control other tests rely on.
fn fs_disabled_control() -> &'static CacheControl {
    let control: &'static CacheControl = Box::leak(Box::new(CacheControl::new()));
    control.set(CacheClass::Filesystem, crate::CacheMode::Off);
    control
}

/// Like [`fixture`], but binding `control` instead of the global.
fn fixture_with_control(
    contents: &[u8],
    control: &'static CacheControl,
) -> CachedFs<Counting<RwMockFs>> {
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"dir", NodeKind::Directory).expect("mkdir");
    let dir = fs.lookup(root, b"dir").expect("dir resolves");
    fs.create(dir, b"file.txt", NodeKind::RegularFile)
        .expect("create");
    let written = fs
        .write_at(dir, b"file.txt", 0, contents)
        .expect("seed contents");
    assert_eq!(written, contents.len());
    CachedFs::new(Counting::new(fs), budget(), owner(), unpressured(), sink())
        .with_cache_control(control)
}

fn dir_of(cache: &mut CachedFs<Counting<RwMockFs>>) -> NodeId {
    let root = cache.root();
    cache.lookup(root, b"dir").expect("dir resolves")
}

fn file_of(cache: &mut CachedFs<Counting<RwMockFs>>) -> NodeId {
    let dir = dir_of(cache);
    cache.lookup(dir, b"file.txt").expect("file resolves")
}

#[test]
fn repeated_reads_are_served_without_driver_calls() {
    let mut cache = fixture(b"hello, cache");
    let file = file_of(&mut cache);

    let mut first = [0u8; 32];
    let n = cache.read_at(file, 0, &mut first).expect("reads");
    assert_eq!(&first[..n], b"hello, cache");

    let calls_before = calls(&cache);
    let mut second = [0u8; 32];
    let m = cache.read_at(file, 0, &mut second).expect("reads");
    assert_eq!(&second[..m], b"hello, cache");
    assert_eq!(
        calls(&cache),
        calls_before,
        "a warm read never reaches the driver"
    );
    assert!(cache.accounting().hits() > 0);
}

#[test]
fn repeated_metadata_reads_are_served_without_driver_calls() {
    let mut cache = fixture(b"data");
    let dir = dir_of(&mut cache);
    let file = file_of(&mut cache);

    let info = cache.node_info(file).expect("stat");
    let sec = cache.security(file).expect("security");
    let calls_before = calls(&cache);

    assert_eq!(cache.node_info(file).expect("stat again"), info);
    assert_eq!(cache.security(file).expect("security again"), sec);
    assert_eq!(cache.lookup(dir, b"file.txt").expect("lookup again"), file);
    assert_eq!(
        calls(&cache),
        calls_before,
        "warm metadata never reaches the driver"
    );
}

#[test]
fn cache_hit_and_miss_return_identical_results() {
    let mut cache = fixture(b"same answer");
    let file = file_of(&mut cache);

    let mut cold = [0u8; 16];
    let cold_n = cache.read_at(file, 3, &mut cold).expect("cold read");
    let cold_info = cache.node_info(file).expect("cold stat");

    let mut warm = [0u8; 16];
    let warm_n = cache.read_at(file, 3, &mut warm).expect("warm read");
    let warm_info = cache.node_info(file).expect("warm stat");

    assert_eq!(cold_n, warm_n);
    assert_eq!(cold, warm);
    assert_eq!(cold_info, warm_info);
}

#[test]
fn write_invalidates_cached_data_and_stat() {
    let mut cache = fixture(b"before");
    let dir = dir_of(&mut cache);
    let file = file_of(&mut cache);

    let mut buf = [0u8; 16];
    let n = cache.read_at(file, 0, &mut buf).expect("warm the cache");
    assert_eq!(&buf[..n], b"before");
    let stale_size = cache.node_info(file).expect("stat").size;
    assert_eq!(stale_size, 6);

    let written = cache
        .write_at(dir, b"file.txt", 0, b"after!!")
        .expect("write");
    assert_eq!(written, 7);

    let mut fresh = [0u8; 16];
    let m = cache.read_at(file, 0, &mut fresh).expect("re-read");
    assert_eq!(&fresh[..m], b"after!!");
    assert_eq!(cache.node_info(file).expect("fresh stat").size, 7);
    assert!(cache.accounting().invalidations() > 0);
}

#[test]
fn truncate_invalidates_cached_data_and_stat() {
    let mut cache = fixture(b"truncate me");
    let dir = dir_of(&mut cache);
    let file = file_of(&mut cache);

    let mut buf = [0u8; 16];
    cache.read_at(file, 0, &mut buf).expect("warm");
    cache.node_info(file).expect("warm stat");

    cache.truncate(dir, b"file.txt", 8).expect("truncates");

    assert_eq!(cache.node_info(file).expect("stat").size, 8);
    let mut fresh = [0u8; 16];
    let n = cache.read_at(file, 0, &mut fresh).expect("re-read");
    assert_eq!(&fresh[..n], b"truncate");
}

#[test]
fn remove_invalidates_lookup_stat_and_data() {
    let mut cache = fixture(b"doomed");
    let dir = dir_of(&mut cache);
    let file = file_of(&mut cache);

    let mut buf = [0u8; 8];
    cache.read_at(file, 0, &mut buf).expect("warm data");
    cache.node_info(file).expect("warm stat");
    cache.security(file).expect("warm security");

    let invalidations_before = cache.accounting().invalidations();
    cache.remove(dir, b"file.txt").expect("removes");

    assert_eq!(
        cache.lookup(dir, b"file.txt").unwrap_err(),
        DriverError::NotFound,
        "the cached lookup does not resurrect a removed file"
    );
    // The node's stat, security, and data entries were all dropped —
    // the next queries go to the driver, not a cached ghost.
    assert!(cache.accounting().invalidations() >= invalidations_before + 4);
    let calls_before = calls(&cache);
    let _ = cache.node_info(file);
    assert!(calls(&cache) > calls_before, "stat is no longer cached");
}

#[test]
fn rename_invalidates_both_names_and_keeps_contents() {
    let mut cache = fixture(b"movable");
    let dir = dir_of(&mut cache);
    let file = file_of(&mut cache);

    let mut buf = [0u8; 8];
    cache.read_at(file, 0, &mut buf).expect("warm data");

    cache
        .rename(dir, b"file.txt", dir, b"renamed.txt")
        .expect("renames");

    assert_eq!(
        cache.lookup(dir, b"file.txt").unwrap_err(),
        DriverError::NotFound,
        "the old binding is gone"
    );
    let moved = cache.lookup(dir, b"renamed.txt").expect("new binding");
    let mut fresh = [0u8; 8];
    let n = cache.read_at(moved, 0, &mut fresh).expect("reads");
    assert_eq!(&fresh[..n], b"movable");
}

#[test]
fn create_invalidates_directory_listings() {
    let mut cache = fixture(b"x");
    let dir = dir_of(&mut cache);

    // Warm the listing.
    let mut names = Vec::new();
    let mut cursor = 0;
    let mut name_buf = [0u8; 64];
    while let Some(entry) = cache.read_dir(dir, cursor, &mut name_buf).expect("lists") {
        names.push(name_buf[..entry.name_len].to_vec());
        cursor = entry.next_cursor;
    }
    assert_eq!(names, vec![b"file.txt".to_vec()]);

    cache
        .create(dir, b"second.txt", NodeKind::RegularFile)
        .expect("creates");

    let mut fresh = Vec::new();
    let mut cursor = 0;
    while let Some(entry) = cache.read_dir(dir, cursor, &mut name_buf).expect("lists") {
        fresh.push(name_buf[..entry.name_len].to_vec());
        cursor = entry.next_cursor;
    }
    assert_eq!(
        fresh.len(),
        2,
        "the cached listing does not hide a created entry"
    );
}

#[test]
fn set_security_invalidates_the_cached_record() {
    let mut cache = fixture(b"secured");
    let file = file_of(&mut cache);

    let before = cache.security(file).expect("warm security");
    assert_eq!(before.mode, 0o755);

    let tightened = NodeSecurity::new(0o600, 7, 7);
    cache.set_security(file, tightened).expect("sets");

    let after = cache.security(file).expect("re-read");
    assert_eq!(after.mode, 0o600);
    assert_eq!(after.uid, 7);
}

#[test]
fn security_change_is_seen_by_the_secured_vfs_permission_check() {
    // The VFS is the policy point; this proves a *cached* security
    // record cannot let a caller keep an authorisation the stored
    // record no longer grants.
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"Users", NodeKind::Directory)
        .expect("mkdir");
    let users = fs.lookup(root, b"Users").expect("resolves");
    fs.create(users, b"open.txt", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(users, b"open.txt", 0, b"visible")
        .expect("seed");
    let file = fs.lookup(users, b"open.txt").expect("resolves");
    // World-readable to start with.
    fs.set_security(file, NodeSecurity::new(0o644, 1, 1))
        .expect("open mode");

    let mut cache = CachedFs::new(Counting::new(fs), budget(), owner(), unpressured(), sink());
    let vfs = Vfs::new(Metadata::new(UserId(0), GroupId(0), Mode::from_bits(0o755)));
    vfs.mounts_write()
        .back_root(DriverHandle::from_raw(1).expect("handle"))
        .expect("backs root");

    let caps = CapabilitySet::empty();
    let other = Credentials {
        uid: UserId(42),
        gid: GroupId(42),
        supplementary_gids: &[],
        caps: &caps,
    };
    let path = Path::parse("/Users/open.txt").expect("parses");

    // Warm read under world-readable mode succeeds (and caches the
    // security record).
    let mut buf = [0u8; 16];
    let n = vfs
        .read_via_secured(&other, &path, &mut cache, 0, &mut buf)
        .expect("world-readable read succeeds");
    assert_eq!(&buf[..n], b"visible");

    // Tighten to owner-only through the same (cache-wrapped) driver.
    cache
        .set_security(file, NodeSecurity::new(0o600, 1, 1))
        .expect("tightens");

    // The same caller is now refused: the cached record was
    // invalidated, so the permission check sees the tightened mode.
    assert_eq!(
        vfs.read_via_secured(&other, &path, &mut cache, 0, &mut buf),
        Err(VfsError::PermissionDenied),
        "a cached security record never outlives a security change"
    );
}

#[test]
fn large_reads_bypass_the_cache() {
    let contents = vec![0xA5u8; 64 * 1024];
    let mut cache = fixture(&contents);
    let file = file_of(&mut cache);

    let mut big = vec![0u8; 32 * 1024];
    let n = cache.read_at(file, 0, &mut big).expect("bulk read");
    assert_eq!(n, 32 * 1024);
    assert_eq!(
        cache.accounting().class_bytes(ReclaimClass::CleanFileData),
        0,
        "a bulk read is not admitted into the cache"
    );
}

#[test]
fn eviction_honours_budget_and_hysteresis_and_takes_data_first() {
    // A budget small enough that a few chunks overflow it: hard = 16 KiB.
    let contents = vec![0x5Au8; 200 * 1024];
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"big", 0, &contents).expect("seed");
    let mut cache = CachedFs::new(
        Counting::new(fs),
        CacheBudget::from_backing(256 * 1024),
        owner(),
        unpressured(),
        sink(),
    );
    let hard = cache.budget().hard();
    let low = cache.budget().low();
    assert!(hard < contents.len());

    let file = cache.lookup(root, b"big").expect("resolves");
    let warm_meta = cache.node_info(file).expect("stat");
    assert_eq!(warm_meta.size, contents.len() as u64);

    // Stream the file through the cache in chunk-sized reads.
    let mut buf = [0u8; 4096];
    let mut offset = 0u64;
    loop {
        let n = cache.read_at(file, offset, &mut buf).expect("reads");
        if n == 0 {
            break;
        }
        offset += n as u64;
        assert!(
            cache.accounting().total_bytes() <= hard,
            "the ledger never exceeds the hard budget"
        );
    }
    assert!(cache.accounting().evictions() > 0, "eviction ran");
    // Hysteresis: the last eviction pass shrank to the low watermark
    // before the most recent admissions grew again, so usage sits well
    // under the hard limit rather than pinned at it.
    assert!(cache.accounting().total_bytes() <= hard);
    assert!(low < hard);

    // Metadata outlives data under eviction pressure: the stat record
    // is still served warm.
    let calls_before = calls(&cache);
    assert_eq!(cache.node_info(file).expect("stat"), warm_meta);
    assert_eq!(calls(&cache), calls_before);
}

#[test]
fn readdir_populates_child_stat_records() {
    let mut cache = fixture(b"payload");
    let dir = dir_of(&mut cache);

    let mut name_buf = [0u8; 64];
    let entry = cache
        .read_dir(dir, 0, &mut name_buf)
        .expect("lists")
        .expect("one entry");

    // The child's stat now serves warm, straight from the listing.
    let calls_before = calls(&cache);
    let info = cache.node_info(entry.node).expect("stat");
    assert_eq!(info, entry.info);
    assert_eq!(calls(&cache), calls_before);
}

#[test]
fn cached_dirent_reports_buffer_too_small_like_the_driver() {
    let mut cache = fixture(b"x");
    let dir = dir_of(&mut cache);

    let mut name_buf = [0u8; 64];
    cache
        .read_dir(dir, 0, &mut name_buf)
        .expect("warm")
        .expect("entry");

    let mut tiny = [0u8; 2];
    assert_eq!(
        cache.read_dir(dir, 0, &mut tiny).unwrap_err(),
        DriverError::BufferTooSmall
    );
}

#[test]
fn eof_short_chunk_is_authoritative_until_invalidated() {
    let mut cache = fixture(b"short");
    let dir = dir_of(&mut cache);
    let file = file_of(&mut cache);

    let mut buf = [0u8; 16];
    assert_eq!(cache.read_at(file, 0, &mut buf).expect("reads"), 5);
    // Past-EOF reads serve zero from the cached short chunk.
    assert_eq!(cache.read_at(file, 5, &mut buf).expect("reads"), 0);
    assert_eq!(cache.read_at(file, 100, &mut buf).expect("reads"), 0);

    // Growing the file invalidates the EOF marker.
    cache
        .write_at(dir, b"file.txt", 5, b" grown")
        .expect("grows");
    let n = cache.read_at(file, 0, &mut buf).expect("re-reads");
    assert_eq!(&buf[..n], b"short grown");
}

#[test]
fn counters_track_hits_misses_and_insertions() {
    let mut cache = fixture(b"counted");
    let file = file_of(&mut cache);

    let misses_before = cache.accounting().misses();
    cache.node_info(file).expect("cold stat");
    assert_eq!(cache.accounting().misses(), misses_before + 1);

    let hits_before = cache.accounting().hits();
    cache.node_info(file).expect("warm stat");
    assert_eq!(cache.accounting().hits(), hits_before + 1);
    assert!(cache.accounting().insertions() > 0);
}

#[test]
fn construction_classifies_and_charges_the_volume_owner() {
    let cache = fixture(b"owned");
    assert_eq!(cache.owner(), Some(owner()));
}

#[test]
fn empty_and_zero_budget_cache_still_serves_correctly() {
    // A zero budget admits nothing; every operation still round-trips.
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"uncached").expect("seed");
    let mut cache = CachedFs::new(
        Counting::new(fs),
        CacheBudget::from_backing(0),
        owner(),
        unpressured(),
        sink(),
    );

    let file = cache.lookup(root, b"f").expect("resolves");
    let mut buf = [0u8; 16];
    let n = cache.read_at(file, 0, &mut buf).expect("reads");
    assert_eq!(&buf[..n], b"uncached");
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert!(cache.accounting().refusals() > 0);
}

/// A cached mock volume over an adjustable pressure source, warmed so
/// both file data and metadata are resident.
fn pressured_fixture(contents: &[u8]) -> (&'static TestSource, CachedFs<Counting<RwMockFs>>) {
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"dir", NodeKind::Directory).expect("mkdir");
    let dir = fs.lookup(root, b"dir").expect("dir resolves");
    fs.create(dir, b"file.txt", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(dir, b"file.txt", 0, contents).expect("seed");
    let mut cache = CachedFs::new(Counting::new(fs), budget(), owner(), pressure, sink());
    let dir = {
        let root = cache.root();
        cache.lookup(root, b"dir").expect("dir resolves")
    };
    let file = cache.lookup(dir, b"file.txt").expect("file resolves");
    let mut buf = [0u8; 64];
    cache.read_at(file, 0, &mut buf).expect("warm data");
    cache.node_info(file).expect("warm stat");
    assert!(cache.accounting().class_bytes(ReclaimClass::CleanFileData) > 0);
    assert!(cache.accounting().class_bytes(ReclaimClass::FsMetadata) > 0);
    (source, cache)
}

#[test]
fn admission_stops_outside_normal_pressure() {
    let (source, pressure) = pressured(free_for(PressureBand::Mild));
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"still served").expect("seed");
    let mut cache = CachedFs::new(Counting::new(fs), budget(), owner(), pressure, sink());

    // Under mild pressure nothing is admitted, but every operation is
    // still served correctly straight from the driver.
    let file = cache.lookup(root, b"f").expect("resolves");
    let mut buf = [0u8; 16];
    let n = cache.read_at(file, 0, &mut buf).expect("reads");
    assert_eq!(&buf[..n], b"still served");
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert!(cache.accounting().refusals() > 0);

    // Back at normal pressure the same cache grows again.
    source.set_free(free_for(PressureBand::Normal));
    let m = cache.read_at(file, 0, &mut buf).expect("reads again");
    assert_eq!(&buf[..m], b"still served");
    assert!(cache.accounting().total_bytes() > 0);
}

#[test]
fn moderate_pressure_drains_file_data_and_keeps_metadata() {
    let (source, mut cache) = pressured_fixture(b"drained at moderate");
    let meta_before = cache.accounting().class_bytes(ReclaimClass::FsMetadata);

    source.set_free(free_for(PressureBand::Moderate));
    let file = {
        let dir = dir_of(&mut cache);
        cache.lookup(dir, b"file.txt").expect("file resolves")
    };
    let mut buf = [0u8; 64];
    let n = cache.read_at(file, 0, &mut buf).expect("still served");
    assert_eq!(&buf[..n], b"drained at moderate");
    assert_eq!(
        cache.accounting().class_bytes(ReclaimClass::CleanFileData),
        0,
        "clean file data finishes reclaim at moderate pressure"
    );
    let meta_after = cache.accounting().class_bytes(ReclaimClass::FsMetadata);
    assert!(meta_after > 0, "hot metadata is preserved at moderate");
    assert!(meta_after <= meta_before.max(cache.budget().low()));
}

#[test]
fn severe_pressure_forces_every_class_to_shrink_to_zero() {
    let (source, mut cache) = pressured_fixture(b"gone at severe");

    source.set_free(free_for(PressureBand::Severe));
    let dir = dir_of(&mut cache);
    let file = cache.lookup(dir, b"file.txt").expect("still served");
    let mut buf = [0u8; 64];
    let n = cache.read_at(file, 0, &mut buf).expect("still served");
    assert_eq!(&buf[..n], b"gone at severe");
    assert_eq!(
        cache.accounting().total_bytes(),
        0,
        "severe pressure empties every cache class"
    );
    assert!(cache.accounting().evictions() > 0);
}

#[test]
fn forced_reclaim_racing_lookup_serves_correct_data() {
    // Reclaim and lookup are serialised behind the registered driver's
    // lock; this drives the interleaving that lock admits — a forced
    // shrink between a warm read and its repeat — and proves the
    // repeat is correct (rebuilt from the driver, never stale).
    let (source, mut cache) = pressured_fixture(b"correct under reclaim");
    let file = {
        let dir = dir_of(&mut cache);
        cache.lookup(dir, b"file.txt").expect("file resolves")
    };

    source.set_free(free_for(PressureBand::Critical));
    let mut buf = [0u8; 64];
    let n = cache
        .read_at(file, 0, &mut buf)
        .expect("served post-reclaim");
    assert_eq!(&buf[..n], b"correct under reclaim");
    assert_eq!(cache.accounting().total_bytes(), 0);

    // Relaxing back to normal lets the cache rebuild and serve warm.
    source.set_free(free_for(PressureBand::Normal));
    // Hysteresis: critical relaxes one band per sample past each exit
    // watermark, so a few samples walk it back to normal.
    for _ in 0..PressureBand::ALL.len() {
        cache.node_info(file).expect("stat");
    }
    let calls_before = calls(&cache);
    cache.node_info(file).expect("warm stat");
    assert_eq!(calls(&cache), calls_before, "the rebuilt cache serves warm");
}

#[test]
fn owner_teardown_after_forced_reclaim_balances_the_ledger() {
    let (source, mut cache) = pressured_fixture(b"torn down cleanly");
    source.set_free(free_for(PressureBand::Severe));
    let dir = dir_of(&mut cache);
    cache.node_info(dir).expect("served under pressure");
    assert_eq!(cache.accounting().total_bytes(), 0);
    // Teardown after a forced reclaim: the drop path purges and zeroes
    // whatever remains without unbalancing the (already empty) ledger.
    drop(cache);
}

#[test]
fn a_forced_pressure_shrink_is_counted() {
    let (source, mut cache) = pressured_fixture(b"shrink is counted");
    assert_eq!(cache.accounting().pressure_shrinks(), 0);
    source.set_free(free_for(PressureBand::Severe));
    let root = cache.root();
    let _ = cache.lookup(root, b"dir");
    assert!(cache.accounting().pressure_shrinks() >= 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn payload_and_metadata_bytes_are_accounted_separately() {
    let mut cache = fixture(b"split ledger");
    let file = file_of(&mut cache);
    let mut buf = [0u8; 16];
    cache.read_at(file, 0, &mut buf).expect("reads");
    cache.node_info(file).expect("stat");
    let acct = cache.accounting();
    for class in [ReclaimClass::CleanFileData, ReclaimClass::FsMetadata] {
        assert!(acct.class_payload_bytes(class) > 0);
        assert!(acct.class_metadata_bytes(class) > 0);
        assert_eq!(
            acct.class_bytes(class),
            acct.class_payload_bytes(class) + acct.class_metadata_bytes(class)
        );
    }
}

#[test]
fn a_detected_defect_is_counted_and_reported_once() {
    let captured = sink();
    let mut cache = CachedFs::new(
        Counting::new(RwMockFs::new()),
        budget(),
        owner(),
        unpressured(),
        captured,
    );
    cache.poison("ledger_imbalance");
    // The poison disables the whole cache, so the one defect is
    // attributed to both classes this cache serves.
    assert_eq!(
        cache
            .accounting()
            .class_failures(ReclaimClass::CleanFileData),
        1
    );
    assert_eq!(
        cache.accounting().class_failures(ReclaimClass::FsMetadata),
        1
    );
    assert_eq!(cache.accounting().failures(), 2);
    assert!(cache.accounting().teardowns() >= 1);
    let events = captured.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 2001);
    // The record's field shape is closed — fixed labels and numeric
    // handles only, never a file name or cached bytes.
    let keys: Vec<&str> = events[0].fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["cache", "owner", "owner_id", "cause"]);
    assert_eq!(events[0].fields[0].1, "clean_fs");
    assert_eq!(events[0].fields[1].1, "volume");
    assert_eq!(events[0].fields[2].1, "1");
    assert_eq!(events[0].fields[3].1, "ledger_imbalance");
    // An already-poisoned cache never reports again.
    cache.poison("orphan_index_slot");
    assert_eq!(captured.snapshot().len(), 1);
    assert_eq!(cache.accounting().failures(), 2);
}

#[test]
fn normal_operation_emits_no_audit_records() {
    let captured = sink();
    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"quiet").expect("seed");
    let mut cache = CachedFs::new(
        Counting::new(fs),
        budget(),
        owner(),
        unpressured(),
        captured,
    );
    let file = cache.lookup(root, b"f").expect("resolves");
    let mut buf = [0u8; 8];
    cache.read_at(file, 0, &mut buf).expect("reads");
    cache.node_info(file).expect("stat");
    assert!(captured.snapshot().is_empty());
}

#[test]
fn a_disabled_filesystem_cache_serves_correctly_but_caches_nothing() {
    let mut cache = fixture_with_control(b"hello, cache", fs_disabled_control());
    let file = file_of(&mut cache);

    let mut first = [0u8; 32];
    let n = cache.read_at(file, 0, &mut first).expect("reads");
    assert_eq!(&first[..n], b"hello, cache");
    assert_eq!(cache.accounting().total_bytes(), 0, "off admits nothing");

    // Every read reaches the driver: the switch is a real bypass.
    let calls_before = calls(&cache);
    let mut second = [0u8; 32];
    cache.read_at(file, 0, &mut second).expect("reads");
    assert!(
        calls(&cache) > calls_before,
        "a disabled cache never serves a warm read"
    );
    assert_eq!(cache.accounting().hits(), 0);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn flipping_the_filesystem_switch_off_purges_the_cache() {
    let control: &'static CacheControl = Box::leak(Box::new(CacheControl::new()));
    let mut cache = fixture_with_control(b"warm me", control);
    let file = file_of(&mut cache);

    let mut buf = [0u8; 16];
    cache.read_at(file, 0, &mut buf).expect("warm");
    let _ = cache.node_info(file).expect("stat");
    assert!(cache.accounting().total_bytes() > 0, "the cache filled");

    // The operator disables the class: the next operation drops (zeroing)
    // everything held, and thereafter every read reaches the driver.
    control.set(CacheClass::Filesystem, crate::CacheMode::Off);
    let mut again = [0u8; 16];
    cache.read_at(file, 0, &mut again).expect("still serves");
    assert_eq!(cache.accounting().total_bytes(), 0, "the purge dropped it");
    assert!(cache.accounting().teardowns() >= 1);
}
