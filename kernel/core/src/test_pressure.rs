//! Shared host-test fixture for the reclaimable-cache pressure gauge
//! (`plans/SMARTRAM.md`): a controllable [`FreeMemorySource`] plus the
//! per-band free readings the cache suites steer it with, defined once
//! for the filesystem-cache, launch-cache, and cross-cache integration
//! tests.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_reclaim::{FreeMemorySource, MemoryPressure, PressureBand};
use tairix_sync::once::Once;

/// The test gauge's backing size (1 GiB), so the band watermarks land
/// on readable byte counts.
pub(crate) const TEST_TOTAL: usize = 1 << 30;

/// A controllable memory reading backing a test pressure gauge.
pub(crate) struct TestSource {
    free: AtomicUsize,
}

impl TestSource {
    /// Store a new free reading; the gauge folds it on its next sample.
    pub(crate) fn set_free(&self, free: usize) {
        self.free.store(free, Ordering::Relaxed);
    }
}

impl FreeMemorySource for TestSource {
    fn free_bytes(&self) -> usize {
        self.free.load(Ordering::Relaxed)
    }

    fn total_bytes(&self) -> usize {
        TEST_TOTAL
    }
}

/// A gauge plus its adjustable source, starting with `free` bytes free.
pub(crate) fn pressured(free: usize) -> (&'static TestSource, &'static MemoryPressure) {
    let source: &'static TestSource = Box::leak(Box::new(TestSource {
        free: AtomicUsize::new(free),
    }));
    (source, Box::leak(Box::new(MemoryPressure::over(source))))
}

/// A gauge pinned at plentiful free memory: normal pressure.
pub(crate) fn unpressured() -> &'static MemoryPressure {
    pressured(TEST_TOTAL / 2).1
}

/// A free reading that folds to `band` from any shallower state.
pub(crate) fn free_for(band: PressureBand) -> usize {
    match band {
        PressureBand::Normal => TEST_TOTAL / 2,
        PressureBand::Mild => TEST_TOTAL / 5 - 4096,
        PressureBand::Moderate => TEST_TOTAL / 10 - 4096,
        PressureBand::Severe => TEST_TOTAL / 16 - 4096,
        PressureBand::Critical => TEST_TOTAL / 32 - 4096,
    }
}

/// The process-global system gauge every host test shares, installed
/// over a controllable source the first time this is called.
///
/// The published band the wait-set readiness scan and the ungated band
/// query read comes from `crate::memstats::MEM_STATS`, which creates its
/// one gauge on first request and returns that same gauge forever after.
/// A test steering a *private* gauge therefore could not move the
/// published band at all, and two tests installing different sources
/// would race. Both problems are closed by routing every host test that
/// needs the published band through this one installer.
///
/// Exactly one test may *move* the band, because the band is process-wide
/// and the test binary runs its tests concurrently: a second steering
/// test would see the first's writes. That test is
/// `waitset_pressure_member_reports_a_band_change_and_consumes_the_edge`
/// in `crate::syscalls`; anything else may read the band but must not
/// write it.
pub(crate) fn global_pressure_source() -> &'static TestSource {
    static INSTALLED: Once<&'static TestSource> = Once::new();
    INSTALLED
        .call_once_infallible(|| {
            let source: &'static TestSource = Box::leak(Box::new(TestSource {
                free: AtomicUsize::new(free_for(PressureBand::Normal)),
            }));
            crate::memstats::MEM_STATS.system_pressure(source);
            source
        })
        .copied()
        .expect("the installer closure cannot panic, so the cell cannot poison")
}
