//! Shared host-test fixture for the reclaimable-cache pressure gauge
//! (`plans/SMARTRAM.md`): a controllable [`FreeMemorySource`] plus the
//! per-band free readings the cache suites steer it with, defined once
//! for the filesystem-cache, launch-cache, and cross-cache integration
//! tests.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_kernel_mem::{FreeMemorySource, MemoryPressure, PressureBand};

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
