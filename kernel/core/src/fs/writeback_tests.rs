//! Host tests for the write-back expiry timer
//! (`plans/ARXFS-WRITEBACK.md` §10): the deadline slot's single-shot
//! consumption, publication in deadline order, the fail-closed clock gate,
//! and the combined floor of several simultaneously-dirty volumes.
//!
//! No monotonic clock is installed in a host build, so the *host* half of
//! the timer (`LateFilesystem::now_ns`) truthfully answers `None` here and
//! the tests drive `publish_due` with an explicit instant — exactly what the
//! flusher does with the reading it took.

use super::*;

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrsProvider, FilesystemRead, FilesystemSecurity, FilesystemStats, NodeId,
    NodeInfo, NodeKind, NodeSecurity, VolumeStats, WritebackHost,
};
use tairix_abi::driver::DriverError;

use crate::audit::AuditEvent;
use crate::fs::memfs::RwMockFs;
use crate::test_sink::TestSink;

/// A driver that records every `flush` and can be made to refuse one, so a
/// test can prove which volumes the flusher published and what a refusal
/// costs. Every other method forwards, so the wrapper is a truthful mount.
struct FlushSpy {
    inner: RwMockFs,
    flushes: u32,
    refuse: bool,
    /// The handle the host installed on this driver, if any.
    host_volume: Option<DriverHandle>,
}

impl FlushSpy {
    fn new() -> Self {
        Self {
            inner: RwMockFs::new(),
            flushes: 0,
            refuse: false,
            host_volume: None,
        }
    }

    fn refusing() -> Self {
        Self {
            refuse: true,
            ..Self::new()
        }
    }
}

impl FilesystemRead for FlushSpy {
    fn root(&self) -> NodeId {
        self.inner.root()
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        self.inner.node_info(node)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        self.inner.lookup(dir, name)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        self.inner.read_at(file, offset, buf)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        self.inner.read_dir(dir, cursor, name_out)
    }

    fn read_link(&mut self, node: NodeId, out: &mut [u8]) -> Result<usize, DriverError> {
        self.inner.read_link(node, out)
    }
}

impl FilesystemWrite for FlushSpy {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.inner.create(dir, name, kind)
    }

    fn create_link(
        &mut self,
        dir: NodeId,
        name: &[u8],
        target: &[u8],
    ) -> Result<NodeId, DriverError> {
        self.inner.create_link(dir, name, target)
    }

    fn link(&mut self, dir: NodeId, name: &[u8], node: NodeId) -> Result<(), DriverError> {
        self.inner.link(dir, name, node)
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.inner.write_at(dir, name, offset, data)
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.inner.truncate(dir, name, size)
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.inner.remove(dir, name)
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        self.inner.rename(src_dir, src_name, dst_dir, dst_name)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.flushes += 1;
        if self.refuse {
            return Err(DriverError::DeviceFault);
        }
        self.inner.flush()
    }

    fn set_writeback_host(&mut self, volume: DriverHandle, _host: &'static dyn WritebackHost) {
        self.host_volume = Some(volume);
    }
}

impl FilesystemSecurity for FlushSpy {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        self.inner.security(node)
    }

    fn set_security(&mut self, node: NodeId, security: NodeSecurity) -> Result<(), DriverError> {
        self.inner.set_security(node, security)
    }
}

impl FilesystemStats for FlushSpy {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        self.inner.stats()
    }
}

impl FilesystemAttrsProvider for FlushSpy {}

/// A leaked registry, so it can be its own `&'static` write-back host — the
/// production shape (`LATE_FILESYSTEM` installs itself).
fn registry() -> &'static LateFilesystem<FlushSpy> {
    let cell: &'static LateFilesystem<FlushSpy> = Box::leak(Box::new(LateFilesystem::new()));
    cell.install_writeback_host(cell).expect("host installs");
    cell
}

fn handle(raw: u64) -> DriverHandle {
    DriverHandle::from_raw(raw).expect("non-zero handle")
}

fn sink() -> &'static TestSink {
    Box::leak(Box::new(TestSink::new()))
}

/// Register `count` volumes numbered from 1, each holding a deadline
/// `spacing` apart starting at `first`.
fn dirty_volumes(
    mounts: &LateFilesystem<FlushSpy>,
    count: u64,
    first: u64,
    spacing: u64,
) -> Vec<DriverHandle> {
    (1..=count)
        .map(|n| {
            let volume = handle(n);
            mounts
                .register(volume, FlushSpy::new(), "vol", "spy", [0u8; 16])
                .expect("register");
            mounts.note_writeback_due(volume, Some(first + (n - 1) * spacing));
            volume
        })
        .collect()
}

fn flushes(mounts: &LateFilesystem<FlushSpy>, volume: DriverHandle) -> u32 {
    mounts.driver(volume).expect("registered").lock().flushes
}

#[test]
fn a_registered_driver_is_handed_the_timer_under_its_own_handle() {
    let mounts = registry();
    let volume = handle(7);
    mounts
        .register(volume, FlushSpy::new(), "vol", "spy", [0u8; 16])
        .expect("register");
    assert_eq!(
        mounts
            .driver(volume)
            .expect("registered")
            .lock()
            .host_volume,
        Some(volume),
        "the driver learns the timer, and the handle it must report against"
    );
}

#[test]
fn a_driver_registered_before_a_host_exists_is_handed_none() {
    let mounts: &'static LateFilesystem<FlushSpy> = Box::leak(Box::new(LateFilesystem::new()));
    let volume = handle(7);
    mounts
        .register(volume, FlushSpy::new(), "vol", "spy", [0u8; 16])
        .expect("register");
    assert_eq!(
        mounts
            .driver(volume)
            .expect("registered")
            .lock()
            .host_volume,
        None,
        "with no timer above it a driver must publish eagerly, not defer"
    );
}

#[test]
fn the_host_reads_no_clock_until_the_flusher_arms_it() {
    let mounts = registry();
    // This test's own wait clock, standing at a value no other test can move,
    // so the armed answer is checked against a known reading rather than
    // against a second read of a clock that may have advanced between the two.
    let _ = crate::test_boot::claim_scheduler();
    crate::test_boot::advance_clock(4_242);
    assert_eq!(
        WritebackHost::now_ns(mounts),
        None,
        "deferral is refused while nothing would publish it"
    );
    mounts.set_writeback_armed(true);
    assert_eq!(
        WritebackHost::now_ns(mounts),
        Some(4_242),
        "an armed host defers against the wait clock"
    );
    mounts.set_writeback_armed(false);
    assert_eq!(WritebackHost::now_ns(mounts), None);
}

#[test]
fn a_deadline_that_has_not_arrived_publishes_nothing() {
    let mounts = registry();
    let volumes = dirty_volumes(mounts, 1, 1_000, 0);
    assert_eq!(
        super::publish_due(mounts, sink(), 999),
        Some(1_000),
        "the pending deadline is reported back as the instant to park until"
    );
    assert_eq!(flushes(mounts, volumes[0]), 0);
}

#[test]
fn a_deadline_that_has_arrived_publishes_once_and_is_consumed() {
    let mounts = registry();
    let volumes = dirty_volumes(mounts, 1, 1_000, 0);
    assert_eq!(
        super::publish_due(mounts, sink(), 1_000),
        None,
        "nothing is left pending, so the flusher arms no timer"
    );
    assert_eq!(flushes(mounts, volumes[0]), 1);
    // The fired deadline is gone: a second pass at the same instant must not
    // re-publish, or the flusher would spin on a deadline stuck in the past.
    assert_eq!(super::publish_due(mounts, sink(), 1_000), None);
    assert_eq!(flushes(mounts, volumes[0]), 1);
}

#[test]
fn only_the_volumes_that_are_due_are_published() {
    let mounts = registry();
    let volumes = dirty_volumes(mounts, 3, 1_000, 1_000);
    assert_eq!(
        super::publish_due(mounts, sink(), 2_000),
        Some(3_000),
        "the third volume's deadline is what the flusher parks until"
    );
    assert_eq!(flushes(mounts, volumes[0]), 1);
    assert_eq!(flushes(mounts, volumes[1]), 1);
    assert_eq!(flushes(mounts, volumes[2]), 0);
}

#[test]
fn the_due_set_is_returned_in_deadline_order() {
    let mounts = registry();
    // Registration order is deliberately the reverse of deadline order.
    for (raw, deadline) in [(1u64, 3_000u64), (2, 1_000), (3, 2_000)] {
        let volume = handle(raw);
        mounts
            .register(volume, FlushSpy::new(), "vol", "spy", [0u8; 16])
            .expect("register");
        mounts.note_writeback_due(volume, Some(deadline));
    }
    let order: Vec<u64> = mounts
        .take_writeback_due(3_000)
        .into_iter()
        .map(|(volume, _)| volume.as_u64())
        .collect();
    assert_eq!(
        order,
        alloc::vec![2, 3, 1],
        "the volume that has waited longest is published first"
    );
}

#[test]
fn a_refused_publish_is_logged_and_leaves_the_due_set() {
    let mounts = registry();
    let volume = handle(4);
    mounts
        .register(volume, FlushSpy::refusing(), "vol", "spy", [0u8; 16])
        .expect("register");
    mounts.note_writeback_due(volume, Some(500));
    let audit = sink();
    assert_eq!(super::publish_due(mounts, audit, 500), None);
    assert_eq!(flushes(mounts, volume), 1);
    assert!(
        audit
            .snapshot()
            .iter()
            .any(|e| e.id == AuditEvent::VolumeWritebackFailed.id()),
        "a background durability failure no caller awaits must reach the log"
    );
    // Not retried in a loop: the driver's own failure path abandons the
    // transaction, so the volume has nothing left to publish.
    assert_eq!(super::publish_due(mounts, audit, 500), None);
    assert_eq!(flushes(mounts, volume), 1);
}

#[test]
fn a_volume_reporting_nothing_open_leaves_no_deadline_armed() {
    let mounts = registry();
    let volumes = dirty_volumes(mounts, 1, 1_000, 0);
    assert_eq!(mounts.earliest_writeback_due(), Some(1_000));
    mounts.note_writeback_due(volumes[0], None);
    assert_eq!(
        mounts.earliest_writeback_due(),
        None,
        "a published transaction leaves nothing for the timer to fire"
    );
    assert_eq!(super::publish_due(mounts, sink(), u64::MAX - 1), None);
    assert_eq!(flushes(mounts, volumes[0]), 0);
}

#[test]
fn a_report_against_an_unregistered_handle_records_nothing() {
    let mounts = registry();
    mounts.note_writeback_due(handle(99), Some(10));
    assert_eq!(
        mounts.earliest_writeback_due(),
        None,
        "a handle this registry does not hold has no mount here to publish"
    );
}

#[test]
fn an_unregistered_volume_takes_its_deadline_with_it() {
    let mounts = registry();
    let volumes = dirty_volumes(mounts, 2, 1_000, 1_000);
    mounts.unregister(volumes[0]);
    assert_eq!(
        mounts.earliest_writeback_due(),
        Some(2_000),
        "a detached volume's deadline cannot keep the flusher waking"
    );
}

#[test]
fn an_absurd_deadline_cannot_spell_nothing_open() {
    let mounts = registry();
    let volume = handle(1);
    mounts
        .register(volume, FlushSpy::new(), "vol", "spy", [0u8; 16])
        .expect("register");
    mounts.note_writeback_due(volume, Some(u64::MAX));
    assert_eq!(
        mounts.earliest_writeback_due(),
        Some(u64::MAX - 1),
        "the sentinel is reserved, so a saturated deadline is clamped below it"
    );
    assert_eq!(
        super::publish_due(mounts, sink(), super::EVERYTHING_DUE),
        None
    );
    assert_eq!(flushes(mounts, volume), 1);
}

#[test]
fn every_dirty_volume_of_a_crowded_machine_is_published_exactly_once() {
    // The combined floor: several volumes dirty at the same time, each with
    // its own deadline. One pass publishes each exactly once, in order, and
    // leaves nothing armed — the flusher's cost is the dirty volumes, never
    // the volumes' size.
    let mounts = registry();
    let volumes = dirty_volumes(mounts, 8, 1_000, 100);
    assert_eq!(mounts.earliest_writeback_due(), Some(1_000));
    assert_eq!(
        super::publish_due(mounts, sink(), 1_700),
        None,
        "with every deadline reached nothing is left for the timer"
    );
    for volume in volumes {
        assert_eq!(flushes(mounts, volume), 1);
    }
}
