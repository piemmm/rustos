//! End-to-end host tests for the write-back expiry timer over a **real**
//! ARXFS volume (`plans/ARXFS-WRITEBACK.md` §10).
//!
//! The write-back policy's own tests live beside it in
//! `tairix_kernel_core::fs::writeback` and drive a spy driver. What they
//! cannot prove is the property the whole item exists for: that a volume
//! which batched a commit and then went quiet is *published* by the timer,
//! with no further operation on the driver. That needs the driver, the
//! registry, and the timer wired together, which is exactly this crate.
//!
//! Publication is read off the **medium**, not off the driver: the device's
//! image is shared with the test, so each assertion re-opens the bytes the
//! device currently holds exactly as a remount after a power cut would, and
//! finds the name published or absent. A driver's own answer would prove
//! nothing here — it would serve its own staged blocks either way.
//!
//! The clock is the test's own, because a host build installs no scheduler
//! clock: the fixture registry is its own write-back host, reading a settable
//! reading and forwarding the deadlines the driver reports. That is the shape
//! production uses — `LATE_FILESYSTEM` installs itself — with the one
//! substitution the host build forces.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tairix_abi::driver::filesystem::{
    FilesystemRead, FilesystemWrite, NodeKind, NodeSecurity, WritebackHost,
};
use tairix_abi::driver::{DriverError, DriverHandle};
use tairix_drv_fs_arxfs::{EntropySource, ARXFS, SYSTEM_VOLUME_KEY};
use tairix_kernel_core::fs::writeback;
use tairix_kernel_core::fs::LateFilesystem;
use tairix_kernel_core::test_sink::TestSink;
use tairix_sync::SpinLock;

use crate::kernel_fs::KernelFs;
use crate::test_support::{RamBlock, SharedRamBlock, BLOCK_SIZE};

/// 8 MiB of backing: past the format's metadata reserve, small enough to
/// format quickly.
const SECTORS_8MIB: u64 = (8 << 20) / BLOCK_SIZE as u64;

/// The mount handle the fixture registers its first volume under.
const VOLUME: u64 = 0x5742_0001;

/// The window a removable volume is served — the class [`RamBlock`] declares.
const REMOVABLE_WINDOW_NS: u64 = 30_000_000_000;

/// One second, the spacing between the crowded-machine fixture's volumes.
const ONE_SECOND_NS: u64 = 1_000_000_000;

/// Deterministic entropy: the image never leaves the test, so a counter is
/// the right source.
struct FixtureEntropy(u8);

impl EntropySource for FixtureEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
        Ok(())
    }
}

/// A mount registry that is also its own write-back host, over a settable
/// monotonic reading.
///
/// `armed` stands in for the registry's own flusher-live gate: production
/// reads it inside `LateFilesystem::now_ns` (proven in
/// `tairix_kernel_core::fs::writeback`), which a host build cannot exercise
/// because it installs no scheduler clock for that gate to guard. Keeping the
/// same shape here is what makes the *driver's* answer to a withdrawn timer
/// observable end to end.
struct Fixture {
    mounts: LateFilesystem<Box<dyn KernelFs>>,
    now_ns: AtomicU64,
    armed: AtomicBool,
    reports: AtomicU64,
}

impl WritebackHost for Fixture {
    fn now_ns(&self) -> Option<u64> {
        if !self.armed.load(Ordering::Acquire) {
            return None;
        }
        Some(self.now_ns.load(Ordering::Acquire))
    }

    fn writeback_due(&self, volume: DriverHandle, deadline_ns: Option<u64>) {
        self.reports.fetch_add(1, Ordering::AcqRel);
        self.mounts.note_writeback_due(volume, deadline_ns);
    }
}

/// One registered volume: its mount handle and the device image the test
/// re-reads to see what has actually been published.
struct Volume {
    handle: DriverHandle,
    medium: Arc<SpinLock<RamBlock>>,
}

impl Fixture {
    /// A leaked fixture with its timer installed and armed, holding no
    /// volumes yet.
    fn empty() -> &'static Self {
        let fixture: &'static Self = Box::leak(Box::new(Self {
            mounts: LateFilesystem::new(),
            now_ns: AtomicU64::new(0),
            armed: AtomicBool::new(true),
            reports: AtomicU64::new(0),
        }));
        fixture
            .mounts
            .install_writeback_host(fixture)
            .expect("the timer installs");
        fixture.mounts.set_writeback_armed(true);
        fixture
    }

    /// A fixture holding one freshly formatted, registered ARXFS volume.
    fn with_volume() -> (&'static Self, Volume) {
        let fixture = Self::empty();
        let volume = fixture.attach(VOLUME, 1);
        (fixture, volume)
    }

    /// Format, register, and return a volume under mount handle `raw`.
    fn attach(&self, raw: u64, entropy: u8) -> Volume {
        let (device, medium) = SharedRamBlock::new(SECTORS_8MIB);
        let mut fs = ARXFS::format(device, 64, &SYSTEM_VOLUME_KEY, &mut FixtureEntropy(entropy))
            .expect("format the fixture volume");
        let root = fs.root();
        // ARXFS carries its own per-inode records rather than taking a mount
        // template, so the root must admit the principal the test writes as.
        fs.set_security(root, NodeSecurity::new(0o755, 0, 0))
            .expect("root security");
        fs.flush().expect("commit the fixture");

        let handle = DriverHandle::from_raw(raw).expect("non-zero handle");
        let driver: Box<dyn KernelFs> = Box::new(fs);
        self.mounts
            .register(handle, driver, "fixture", "arxfs", [0u8; 16])
            .expect("register");
        Volume { handle, medium }
    }

    fn set_now(&self, now_ns: u64) {
        self.now_ns.store(now_ns, Ordering::Release);
    }

    /// Withdraw (or restore) the timer, as the flusher does when it stops.
    fn set_armed(&self, armed: bool) {
        self.armed.store(armed, Ordering::Release);
        self.mounts.set_writeback_armed(armed);
    }

    /// Create `name` on `volume` through the registered driver, as an
    /// ordinary mutating operation does.
    fn create(&self, volume: &Volume, name: &[u8]) {
        let driver = self.mounts.driver(volume.handle).expect("registered");
        let mut driver = driver.lock();
        let root = driver.root();
        driver
            .create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
}

impl Volume {
    /// Whether `name` is on the medium: a fresh reader over the bytes the
    /// device currently holds, exactly as a remount would see them.
    fn published(&self, name: &[u8]) -> bool {
        let image = self.medium.lock().data.clone();
        let device = RamBlock { data: image };
        let mut disk = ARXFS::open(device, &SYSTEM_VOLUME_KEY).expect("the published image mounts");
        let root = disk.root();
        disk.lookup(root, name).is_ok()
    }
}

fn sink() -> &'static TestSink {
    Box::leak(Box::new(TestSink::new()))
}

#[test]
fn a_volume_that_goes_quiet_is_published_by_the_timer_alone() {
    let (fixture, volume) = Fixture::with_volume();

    // One ordinary operation. It joins a transaction that stays open, so
    // nothing reaches the medium and the volume reports when it must.
    fixture.create(&volume, b"quiet");
    assert!(
        !volume.published(b"quiet"),
        "the operation joined an open transaction rather than publishing one"
    );
    assert_eq!(
        fixture.mounts.earliest_writeback_due(),
        Some(REMOVABLE_WINDOW_NS),
        "the volume told the timer exactly when its window elapses"
    );

    // The volume now goes quiet: no operation of any kind reaches the
    // driver. Before the window nothing is due.
    fixture.set_now(REMOVABLE_WINDOW_NS - 1);
    assert_eq!(
        writeback::publish_due(&fixture.mounts, sink(), REMOVABLE_WINDOW_NS - 1),
        Some(REMOVABLE_WINDOW_NS)
    );
    assert!(!volume.published(b"quiet"));

    // The window elapses and the flusher publishes it — the whole point of
    // the item: recency is bounded in *time*, not merely in content.
    fixture.set_now(REMOVABLE_WINDOW_NS);
    assert_eq!(
        writeback::publish_due(&fixture.mounts, sink(), REMOVABLE_WINDOW_NS),
        None,
        "with the transaction published nothing is left for the timer to fire"
    );
    assert!(
        volume.published(b"quiet"),
        "the aged-out transaction reached the medium without the driver being \
         called by any operation"
    );
}

#[test]
fn an_idle_volume_arms_nothing_at_all() {
    let (fixture, volume) = Fixture::with_volume();
    assert_eq!(
        fixture.mounts.earliest_writeback_due(),
        None,
        "a volume holding no transaction gives the timer nothing to arm, so \
         an idle machine takes no wakeup"
    );
    assert_eq!(
        writeback::publish_due(&fixture.mounts, sink(), writeback::EVERYTHING_DUE),
        None,
        "and a pass over it spends no device barrier on a clean volume"
    );
    assert!(!volume.published(b"anything"));
}

#[test]
fn a_sync_publishes_the_transaction_and_withdraws_its_deadline() {
    let (fixture, volume) = Fixture::with_volume();
    fixture.create(&volume, b"synced");
    assert_eq!(
        fixture.mounts.earliest_writeback_due(),
        Some(REMOVABLE_WINDOW_NS)
    );
    fixture
        .mounts
        .driver(volume.handle)
        .expect("registered")
        .lock()
        .flush()
        .expect("sync");
    assert!(volume.published(b"synced"));
    assert_eq!(
        fixture.mounts.earliest_writeback_due(),
        None,
        "an explicit sync leaves the timer nothing to come back for"
    );
}

#[test]
fn a_burst_of_operations_moves_the_timer_once() {
    let (fixture, volume) = Fixture::with_volume();
    let reports_before = fixture.reports.load(Ordering::Acquire);
    for name in [b"a".as_slice(), b"b", b"c", b"d", b"e", b"f", b"g", b"h"] {
        fixture.create(&volume, name);
    }
    assert!(
        !volume.published(b"a"),
        "every operation in the window joined the one transaction"
    );
    assert_eq!(
        fixture.reports.load(Ordering::Acquire) - reports_before,
        1,
        "the burst moved the timer once, when it opened — not per operation"
    );

    fixture.set_now(REMOVABLE_WINDOW_NS);
    assert_eq!(
        writeback::publish_due(&fixture.mounts, sink(), REMOVABLE_WINDOW_NS),
        None
    );
    for name in [b"a".as_slice(), b"b", b"c", b"d", b"e", b"f", b"g", b"h"] {
        assert!(
            volume.published(name),
            "eight operations cost one commit, one barrier, and one slot"
        );
    }
}

#[test]
fn several_volumes_dirty_at_once_are_each_published_at_their_own_window() {
    // The combined floor: more than one volume batching at the same time on
    // a machine whose RAM is not proportional to them. Each is published at
    // its own deadline, in order, and the flusher's state is one `u64` per
    // mount.
    let fixture = Fixture::empty();
    let mut volumes = Vec::new();
    for n in 1..=3u64 {
        // Each volume's transaction opens at a different reading, so each
        // takes its own deadline a second apart.
        fixture.set_now(n * ONE_SECOND_NS);
        let volume = fixture.attach(VOLUME + n, u8::try_from(n).expect("small"));
        fixture.create(&volume, b"dirty");
        volumes.push(volume);
    }

    assert_eq!(
        fixture.mounts.earliest_writeback_due(),
        Some(ONE_SECOND_NS + REMOVABLE_WINDOW_NS),
        "the volume that has waited longest is the one the flusher parks for"
    );

    // The first volume's window elapses; the other two are untouched.
    let first_due = ONE_SECOND_NS + REMOVABLE_WINDOW_NS;
    fixture.set_now(first_due);
    assert_eq!(
        writeback::publish_due(&fixture.mounts, sink(), first_due),
        Some(2 * ONE_SECOND_NS + REMOVABLE_WINDOW_NS)
    );
    assert!(volumes[0].published(b"dirty"));
    assert!(!volumes[1].published(b"dirty"));
    assert!(!volumes[2].published(b"dirty"));

    // Then the rest, in one pass.
    let last_due = 3 * ONE_SECOND_NS + REMOVABLE_WINDOW_NS;
    fixture.set_now(last_due);
    assert_eq!(
        writeback::publish_due(&fixture.mounts, sink(), last_due),
        None
    );
    for volume in &volumes {
        assert!(volume.published(b"dirty"));
    }
}

#[test]
fn a_disarmed_timer_makes_every_operation_publish_again() {
    let (fixture, volume) = Fixture::with_volume();
    fixture.set_armed(false);
    fixture.create(&volume, b"eager");
    assert!(
        volume.published(b"eager"),
        "with nothing left to fire a window, durability is never deferred"
    );
    assert_eq!(fixture.mounts.earliest_writeback_due(), None);
}
