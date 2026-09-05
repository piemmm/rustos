//! Shared host-test stand-ins for the process-global publications a real
//! boot makes exactly once.
//!
//! The unit-test binary runs many independent boots in one process, so
//! sharing a set-once boot publication lets whichever test reached it first
//! decide what every later one observes (`plans/OPEN-DEFECTS.md` D90). Each
//! stand-in here is therefore either identical for every caller (the per-boot
//! hash key) or private to the calling test — libtest runs each test on a
//! thread of its own — so no start order changes what a test sees.

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

use tairix_kernel_sched_api::{CpuId, TaskId};

use crate::waitq::WaitQueueArch;

/// The key every test publishes, standing in for the boot path's draw from
/// the CSPRNG output reserve.
const TEST_HASH_KEY: tairix_hash::HashSeed =
    tairix_hash::HashSeed::from_words(0x4C41_554E_4348_0001, 0x4C41_554E_4348_0002);

/// Publish the per-boot hash key, so a container whose index is keyed under
/// it (the semantic launch cache) is keyed here exactly as on a booted
/// system rather than born poisoned.
///
/// Every caller publishes the same key and the cell refuses a second write,
/// so what a container files its keys under does not depend on which test
/// reached this first.
pub(crate) fn publish_hash_key() {
    let _ = tairix_hash::publish(TEST_HASH_KEY);
}

/// The scheduler wait hook a test claims for itself: a monotonic clock it
/// advances, plus the CPU and task the in-kernel wait loops resolve through
/// [`crate::waitq::wait_arch`].
///
/// A stateless handle; every answer comes from the calling thread's own
/// claim, so two tests running in parallel never observe each other's. It
/// answers [`Self::current_task`] from that claim regardless of the CPU
/// asked about, which is all a wait loop's parkability decision reads.
struct HostWaitArch;

static HOST_WAIT_ARCH: HostWaitArch = HostWaitArch;

/// One test's claim: the CPU and task the kernel's process-wide,
/// task-id-keyed state sees it as.
#[derive(Copy, Clone)]
struct Claim {
    cpu: CpuId,
    task: TaskId,
}

std::thread_local! {
    /// The calling test's claim, `None` until it makes one.
    static CLAIM: Cell<Option<Claim>> = const { Cell::new(None) };
    /// Whether the calling test also claimed the wait hook.
    static HOOK: Cell<bool> = const { Cell::new(false) };
    /// The calling test's wait clock, advanced only by its own
    /// [`advance_clock`] so a concurrent test's ticks can never shorten or
    /// lengthen this one's deadlines.
    static CLOCK: Cell<u64> = const { Cell::new(0) };
}

/// Serial number of the next claim, which both its CPU and its task id are
/// derived from.
static NEXT_CLAIM: AtomicU32 = AtomicU32::new(0);

/// Task ids are issued far above every id the suite spells by hand or draws
/// from a test scheduler, so a claimed task's entries in the process-wide
/// signal kill gate and wait queues can neither be mistaken for nor cleared
/// by another test's.
const CLAIM_TASK_BASE: TaskId = 1 << 56;

/// This test's own task id.
///
/// A test whose call path reads kernel state keyed by task id alone — the
/// signal kill gate, the stopped-task overlay, a wait queue — needs an id no
/// other test can name. A scheduler-minted one will not do: every test
/// builds its own scheduler and each mints the same low ids, so a
/// termination one test legitimately defers against *its* task 1 is
/// indistinguishable from one against another's.
///
/// Idempotent: a second call returns the same id.
pub(crate) fn claim_task() -> TaskId {
    claim().task
}

/// Claim the wait hook for the calling test as well, and report the CPU and
/// task it will resolve through it.
///
/// The CPU is issued from beyond the per-CPU state table, so a scheduler
/// park against it fails closed at the missing slot — the production
/// unconfigured-CPU answer, which is what a host test with no live dispatch
/// loop must get — whatever resume handle another test publishes for a CPU
/// of its own.
pub(crate) fn claim_scheduler() -> (CpuId, TaskId) {
    let claim = claim();
    HOOK.with(|hook| hook.set(true));
    (claim.cpu, claim.task)
}

/// The calling test's claim, minting one on first use.
fn claim() -> Claim {
    if let Some(claim) = CLAIM.with(Cell::get) {
        return claim;
    }
    let serial = NEXT_CLAIM.fetch_add(1, Ordering::Relaxed);
    let table = u32::try_from(crate::cpu_state::TEST_CPUS).expect("the test table is small");
    let claim = Claim {
        cpu: table
            .checked_add(serial)
            .expect("a claim per test stays inside the CPU id space"),
        task: CLAIM_TASK_BASE + TaskId::from(serial),
    };
    CLAIM.with(|c| c.set(Some(claim)));
    claim
}

/// The calling test's claimed hook, or `None` while it has claimed none —
/// which is what every test that never asks for one sees, exactly as before
/// any boot publication. Read by [`crate::waitq::wait_arch`].
pub(crate) fn claimed_wait_arch() -> Option<&'static (dyn WaitQueueArch + 'static)> {
    HOOK.with(Cell::get)
        .then_some(&HOST_WAIT_ARCH as &(dyn WaitQueueArch + 'static))
}

/// Advance the calling test's wait clock, so a bounded wait it drives
/// reaches its deadline.
pub(crate) fn advance_clock(ns: u64) {
    CLOCK.with(|c| c.set(c.get().saturating_add(ns)));
}

impl WaitQueueArch for HostWaitArch {
    fn unpark(&self, _id: TaskId) {}

    fn now_ns(&self) -> u64 {
        CLOCK.with(Cell::get)
    }

    fn set_wakeup(&self, _deadline_ns: Option<u64>) {}

    fn current_cpu(&self) -> Option<CpuId> {
        CLAIM.with(Cell::get).map(|claim| claim.cpu)
    }

    fn current_task(&self, _cpu: CpuId) -> Option<TaskId> {
        CLAIM.with(Cell::get).map(|claim| claim.task)
    }
}

#[cfg(test)]
mod tests {
    use super::{advance_clock, claim_scheduler, claim_task, publish_hash_key};

    #[test]
    fn an_unclaimed_test_sees_no_wait_hook() {
        assert!(crate::waitq::wait_arch().is_none());
        assert_eq!(crate::waitq::wait_now_ns(), None);
    }

    #[test]
    fn a_claim_is_idempotent_and_answers_from_this_thread() {
        let (cpu, task) = claim_scheduler();
        assert_eq!(claim_scheduler(), (cpu, task));
        assert_eq!(claim_task(), task);
        let hook = crate::waitq::wait_arch().expect("the claim published a hook");
        assert_eq!(hook.current_cpu(), Some(cpu));
        assert_eq!(hook.current_task(0), Some(task));
    }

    #[test]
    fn a_task_claim_alone_publishes_no_wait_hook() {
        // A test that only needs an id of its own must not gain a scheduler
        // it never asked for: that would change what its park loops do.
        let task = claim_task();
        assert!(
            task >= super::CLAIM_TASK_BASE,
            "reserved above every hand-written id"
        );
        assert!(crate::waitq::wait_arch().is_none());
    }

    #[test]
    fn a_claimed_cpu_has_no_per_cpu_slot_so_a_park_fails_closed() {
        // What makes the fallback park deterministic: the claim names a CPU
        // the state table does not cover, so no other test's resume handle
        // can be found for it.
        let (cpu, _task) = claim_scheduler();
        assert!(crate::cpu_state::get(cpu).is_none());
    }

    #[test]
    fn the_clock_starts_at_zero_and_only_this_test_advances_it() {
        let _ = claim_scheduler();
        assert_eq!(crate::waitq::wait_now_ns(), Some(0));
        advance_clock(700);
        assert_eq!(crate::waitq::wait_now_ns(), Some(700));
    }

    #[test]
    fn the_hash_key_publication_is_the_same_for_every_caller() {
        publish_hash_key();
        publish_hash_key();
        assert_eq!(tairix_hash::published(), Some(super::TEST_HASH_KEY));
    }
}
