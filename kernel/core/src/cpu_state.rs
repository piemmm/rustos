//! Discovered-CPU-sized scheduler continuation state.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustos_kernel_mem::LiveUserSpace;
use rustos_kernel_sched_api::TaskAction;
use rustos_sync::{OnceCell, SpinLock};

/// Type-erased continuation handle for the task currently running on a CPU.
#[derive(Copy, Clone)]
pub(crate) struct ResumeHandle {
    pub(crate) data: usize,
    pub(crate) thunk: unsafe fn(usize, TaskAction),
}

/// Published live user-space pointer for the task currently running on a CPU.
#[derive(Copy, Clone)]
pub(crate) struct LiveSpacePtr(pub(crate) *mut (dyn LiveUserSpace + Send));

// SAFETY: the kthread publication protocol makes the pointee exclusive to
// the CPU whose slot contains it; consumers borrow it only while that task is
// synchronously trapped and cannot run or migrate.
unsafe impl Send for LiveSpacePtr {}

/// All kernel-core state indexed by a dense discovered CPU id.
pub(crate) struct CpuState {
    pub(crate) resume: SpinLock<Option<ResumeHandle>>,
    pub(crate) live_space: SpinLock<Option<LiveSpacePtr>>,
    pub(crate) preempt_pending: AtomicBool,
    pub(crate) preemptions: AtomicU64,
}

impl CpuState {
    const fn new() -> Self {
        Self {
            resume: SpinLock::new(None),
            live_space: SpinLock::new(None),
            preempt_pending: AtomicBool::new(false),
            preemptions: AtomicU64::new(0),
        }
    }
}

/// Failure to publish the per-CPU state table during scheduler init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuStateInitError {
    /// A scheduler cannot operate without at least one CPU.
    ZeroCpus,
    /// The discovered-sized state table could not be allocated.
    AllocationFailed,
    /// This boot already published its immutable state table.
    AlreadyInstalled,
}

#[cfg(not(feature = "test-arch"))]
static CPU_STATES: OnceCell<Box<[CpuState]>> = OnceCell::new();

#[cfg(any(test, feature = "test-arch"))]
pub(crate) const TEST_CPUS: usize = 64;
#[cfg(any(test, feature = "test-arch"))]
static TEST_STATE: [CpuState; TEST_CPUS] = [const { CpuState::new() }; TEST_CPUS];

fn allocate(cpus: u32) -> Result<Box<[CpuState]>, CpuStateInitError> {
    let count = usize::try_from(cpus).map_err(|_| CpuStateInitError::AllocationFailed)?;
    if count == 0 {
        return Err(CpuStateInitError::ZeroCpus);
    }
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(count)
        .map_err(|_| CpuStateInitError::AllocationFailed)?;
    for _ in 0..count {
        slots.push(CpuState::new());
    }
    Ok(slots.into_boxed_slice())
}

fn install_into(cell: &OnceCell<Box<[CpuState]>>, cpus: u32) -> Result<(), CpuStateInitError> {
    let slots = allocate(cpus)?;
    cell.set(slots)
        .map_err(|_| CpuStateInitError::AlreadyInstalled)
}

#[cfg(any(test, not(feature = "test-arch")))]
fn contains(cell: &OnceCell<Box<[CpuState]>>, cpu: u32) -> bool {
    let Some(index) = usize::try_from(cpu).ok() else {
        return false;
    };
    cell.get()
        .ok()
        .flatten()
        .is_some_and(|slots| slots.get(index).is_some())
}

#[cfg(any(test, not(feature = "test-arch")))]
fn ensure_in(cell: &OnceCell<Box<[CpuState]>>, cpus: u32, cpu: u32) -> bool {
    if contains(cell, cpu) {
        return true;
    }
    match install_into(cell, cpus) {
        Ok(()) | Err(CpuStateInitError::AlreadyInstalled) => contains(cell, cpu),
        Err(CpuStateInitError::ZeroCpus | CpuStateInitError::AllocationFailed) => false,
    }
}

/// Allocate and publish one state slot per validated scheduler CPU.
///
/// Kernel boot and minimal kernel harnesses call this exactly once after
/// validating the scheduler CPU count and before admitting any task or
/// enabling any interrupt that can reach preemption state.
///
/// # Errors
///
/// Returns [`CpuStateInitError`] when `cpus` is zero, allocation fails, or a
/// table was already published for this boot.
pub fn install(cpus: u32) -> Result<(), CpuStateInitError> {
    #[cfg(feature = "test-arch")]
    {
        // Host tests invoke boot initialization independently and in
        // parallel. Exercise the exact set-once allocation and publication
        // path through an isolated boot cell rather than contaminating the
        // next independent test boot with process-global state.
        install_into(&OnceCell::new(), cpus)
    }
    #[cfg(not(feature = "test-arch"))]
    {
        install_into(&CPU_STATES, cpus)
    }
}

/// Ensure the table exists and contains `cpu` before admitting a task there.
///
/// Production boot installs eagerly so allocation failures retain their
/// precise [`CpuStateInitError`]. This defensive path makes the public
/// kthread runtime complete for minimal kernels that construct a scheduler
/// directly: concurrent first admissions may race to install, but every
/// caller accepts success only after the requested immutable slot is visible.
pub(crate) fn ensure(cpus: u32, cpu: u32) -> bool {
    #[cfg(any(test, feature = "test-arch"))]
    {
        let _ = cpus;
        TEST_STATE.get(cpu as usize).is_some()
    }
    #[cfg(not(any(test, feature = "test-arch")))]
    {
        ensure_in(&CPU_STATES, cpus, cpu)
    }
}

#[cfg(not(feature = "test-arch"))]
fn installed() -> Option<&'static [CpuState]> {
    CPU_STATES.get().ok().flatten().map(Box::as_ref)
}

/// State for dense `cpu`, or `None` before install / outside discovery.
#[inline]
pub(crate) fn get(cpu: u32) -> Option<&'static CpuState> {
    let index = usize::try_from(cpu).ok()?;
    #[cfg(any(test, feature = "test-arch"))]
    {
        TEST_STATE.get(index)
    }
    #[cfg(not(any(test, feature = "test-arch")))]
    {
        installed()?.get(index)
    }
}

/// Sum the monotonic preemption counters across installed CPUs.
pub(crate) fn total_preemptions() -> u64 {
    #[cfg(any(test, feature = "test-arch"))]
    let slots: &[CpuState] = &TEST_STATE;
    #[cfg(not(any(test, feature = "test-arch")))]
    let slots = installed().unwrap_or(&[]);
    slots
        .iter()
        .map(|slot| slot.preemptions.load(Ordering::Relaxed))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_cpu_install_is_rejected() {
        assert_eq!(install(0), Err(CpuStateInitError::ZeroCpus));
    }

    #[test]
    fn allocation_scales_to_large_discovered_topologies() {
        for count in [1, 4, 128, 1024] {
            let slots = allocate(count).expect("valid discovered CPU count");
            assert_eq!(slots.len(), count as usize);
            assert!(slots
                .iter()
                .all(|slot| !slot.preempt_pending.load(Ordering::Relaxed)));
        }
    }

    #[test]
    fn publication_is_atomic_set_once_and_bounds_checked() {
        let cell = OnceCell::new();
        assert!(cell.get().expect("fresh cell is not poisoned").is_none());
        install_into(&cell, 4).expect("valid discovered CPU count");

        let slots = cell
            .get()
            .expect("set does not poison")
            .expect("published table");
        assert_eq!(slots.len(), 4);
        assert!(slots.get(3).is_some());
        assert!(slots.get(4).is_none());
        assert_eq!(
            install_into(&cell, 4),
            Err(CpuStateInitError::AlreadyInstalled)
        );
    }

    #[test]
    fn first_task_admission_initializes_exact_scheduler_extent() {
        let cell = OnceCell::new();
        assert!(ensure_in(&cell, 4, 3));
        assert!(contains(&cell, 0));
        assert!(contains(&cell, 3));
        assert!(!ensure_in(&cell, 4, 4));
    }

    #[test]
    fn test_slots_fail_closed_outside_their_bound() {
        let count = u32::try_from(TEST_CPUS).expect("test CPU count fits u32");
        assert!(get(count - 1).is_some());
        assert!(get(count).is_none());
    }
}
