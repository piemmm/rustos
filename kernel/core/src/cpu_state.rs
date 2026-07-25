//! Discovered-CPU-sized scheduler continuation state.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
// `AtomicU32` backs only the debug-diagnostics pre-silence-backtrace length,
// and `AtomicUsize` the debug-diagnostics per-CPU lock-site stack, so both
// are imported only when that facility is compiled in.
#[cfg(feature = "watchdog-diagnostics")]
use core::sync::atomic::{AtomicU32, AtomicUsize};

use tairix_kernel_mem::LiveUserSpace;
use tairix_kernel_sched_api::TaskAction;
use tairix_sync::{OnceCell, SpinLock};

/// Type-erased continuation handle for the task currently running on a CPU.
#[derive(Copy, Clone)]
pub(crate) struct ResumeHandle {
    pub(crate) data: usize,
    pub(crate) thunk: unsafe fn(usize, TaskAction),
}

/// Published live user-space pointer for the task currently running on a CPU.
#[derive(Copy, Clone)]
pub(crate) struct LiveSpacePtr(pub(crate) *mut (dyn LiveUserSpace + Send));

/// Maximum stack frames the watchdog records per liveness sample.
///
/// A fixed diagnostic depth (a *bound*, not a scaling capacity), like the
/// random output reserve: deep enough to bridge the interrupted context
/// into the caller nest that names a wedge, shallow enough that capturing
/// it on every ~1 Hz sample and rendering it in one log line stays cheap.
///
/// Part of the debug-diagnostics facility, so it is compiled in only with
/// the `watchdog-diagnostics` feature (the shippable image has no
/// pre-silence backtrace at all).
#[cfg(feature = "watchdog-diagnostics")]
pub(crate) const WD_BT_MAX: usize = 12;

/// Maximum nested lock records the debug-diagnostics lock-site stack keeps
/// per CPU.
///
/// A fixed diagnostic *bound*, not a scaling capacity: kernel lock nesting
/// is shallow by design (holding a spinlock across another lock is rare and
/// discouraged), so this is ample. A deeper nesting simply stops recording
/// past the cap and still balances on release (fail-safe) — it never grows,
/// allocates, or faults. Compiled in only with the `watchdog-diagnostics`
/// feature; a shippable image records no lock sites at all.
#[cfg(feature = "watchdog-diagnostics")]
pub(crate) const LOCK_STACK_MAX: usize = 8;

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
    /// Watchdog **scheduler-progress** heartbeat: the monotonic-ns
    /// timestamp of the last observed scheduler progress on this CPU — the
    /// dispatch loop stamps it once per iteration (`crate::watchdog`). `0`
    /// means "not yet armed" — no dispatch has run here, so the watchdog
    /// makes no judgement (fail closed). Basis for **soft**-lockup
    /// detection (a CPU still taking its watchdog interrupt but no longer
    /// returning to the scheduler).
    pub(crate) last_progress_ns: AtomicU64,
    /// Whether an active **soft**-lockup episode has already been reported
    /// on this CPU, so detection reports each episode exactly once and its
    /// recovery exactly once (cross-CPU-safe via atomic swap).
    pub(crate) stall_reported: AtomicBool,
    /// Watchdog **liveness** heartbeat: the monotonic-ns timestamp of the
    /// last non-maskable watchdog sample taken *on* this CPU (the aarch64
    /// FIQ watchdog stamps it every cadence tick). `0` means "not yet
    /// armed". Basis for **hard**-lockup detection: a CPU that stops
    /// advancing this while it is `WatchdogActivity::Active` is no longer
    /// taking even the pseudo-NMI — wedged — and only another CPU can
    /// observe it.
    pub(crate) last_seen_ns: AtomicU64,
    /// This CPU's watchdog activity class (a `WatchdogActivity` encoded
    /// as `u8`), published by the dispatch loop so a cross-CPU check can
    /// tell a legitimately parked (idle) or not-yet-online CPU apart from
    /// one that is genuinely running work and therefore *owes* progress.
    pub(crate) wd_activity: AtomicU8,
    /// Whether an active **hard**-lockup episode has already been reported
    /// for this CPU, so a buddy reports each episode exactly once and its
    /// recovery exactly once.
    pub(crate) hard_reported: AtomicBool,
    /// Last-known interrupted program counter captured on this CPU by the
    /// non-maskable watchdog sample — the "where" of a lockup diagnosis.
    pub(crate) wd_ctx_pc: AtomicU64,
    /// Last-known running task id captured alongside [`Self::wd_ctx_pc`]
    /// ([`u64::MAX`] = none/kernel), so a lockup names the culprit task.
    pub(crate) wd_ctx_task: AtomicU64,
    /// Last-known architecture auxiliary word captured alongside
    /// [`Self::wd_ctx_pc`] (aarch64 `SPSR_EL1`, so the diagnosis can decode
    /// the interrupted exception level and mask state); `0` when unset.
    pub(crate) wd_ctx_aux: AtomicU64,
    /// Whether [`Self::wd_ctx_pc`] captured **kernel** code. A cross-CPU
    /// soft-lockup check flags an Active CPU that stops making scheduler
    /// progress only when it was last seen in the kernel: a CPU running a
    /// lone, preemptible user task legitimately makes no scheduler progress
    /// and must never be flagged.
    pub(crate) wd_ctx_in_kernel: AtomicBool,
    /// A pending **forced** reschedule for a lone user task that has
    /// monopolised this CPU: the watchdog cadence sets it when it samples an
    /// `Active` CPU running a user task that has withheld the CPU from the
    /// scheduler past the monopoly-guard window, and the return-to-user
    /// preempt point honours it by suspending the task back to the
    /// dispatcher — unconditionally, without a runnable competitor. This is
    /// how a CPU-bound user task that never yields is still returned to the
    /// scheduler (so the dispatch loop's housekeeping and progress stamp run
    /// again) without arming any new periodic timer: it rides the watchdog
    /// cadence that is already firing on the CPU. Distinct from
    /// [`Self::preempt_pending`], which is gated on a runnable competitor.
    pub(crate) force_yield: AtomicBool,
    /// Watchdog **kernel-activity breadcrumb** — the site tag of the last
    /// in-kernel region this CPU *itself* entered (a
    /// [`crate::watchdog::KernelBreadcrumb`] encoded as `u8`), published by
    /// the CPU as it runs rather than by the watchdog sample. This is the
    /// one diagnosis a hard-locked CPU can still give: on a GICv2
    /// non-secure board there is no non-maskable channel, so a CPU wedged
    /// with maskable interrupts off cannot be sampled by its own watchdog
    /// IRQ or interrupted by a buddy's IPI — its [`Self::wd_ctx_pc`] goes
    /// stale (`sampled=pre_silence`). The breadcrumb, written just before
    /// the CPU entered the region it is now stuck in, is fresh, so the buddy
    /// observer's report names the real in-kernel activity (a syscall, a
    /// user-fault resolver phase, or the scheduler) instead of the innocent
    /// pre-silence PC.
    ///
    /// Part of the debug-diagnostics facility (`watchdog-diagnostics`): a
    /// shippable image compiles this field, its siblings below, and every
    /// write to them out entirely, so the syscall/dispatch/fault hot paths
    /// pay nothing for a breadcrumb they can never emit.
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) kbc_site: AtomicU8,
    /// The datum accompanying [`Self::kbc_site`] — the syscall number for a
    /// syscall breadcrumb, the faulting virtual address for a fault-resolver
    /// breadcrumb, `0` otherwise. Written before the sequence bump so a
    /// reader that observes a fresh [`Self::kbc_seq`] sees a matching datum.
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) kbc_detail: AtomicU64,
    /// Monotonic breadcrumb sequence, bumped (release) on every breadcrumb
    /// write after the site and detail are stored. A reader loads it
    /// (acquire) first so it sees a consistent site+detail; a human reading
    /// two successive reports can also tell a frozen breadcrumb (the CPU is
    /// stuck in exactly this region) from an advancing one.
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) kbc_seq: AtomicU64,
    /// Watchdog **pre-silence backtrace** — the frame-pointer-unwound
    /// return-address chain of the context this CPU's last non-maskable
    /// watchdog sample interrupted (innermost first, starting at the
    /// interrupted PC). The port captures it from the saved exception
    /// frame on every cadence sample, so on a hard lockup — where the
    /// sampled PC is a stale `pre_silence` single word — the observer's
    /// report can still name the whole call nest the CPU was in ~1 s
    /// before it went silent, not one ambiguous address. Only the first
    /// [`Self::wd_bt_len`] entries are valid.
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) wd_bt: [AtomicU64; WD_BT_MAX],
    /// Number of valid frames in [`Self::wd_bt`] (`0` when the port
    /// captured none — the field is then omitted from the report, never
    /// fabricated). Written (release) *after* the frames, so a reader that
    /// loads it (acquire) first sees a consistent set.
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) wd_bt_len: AtomicU32,
    /// Watchdog **lock-site stack** — the `&'static Location` (as a
    /// `usize`, `0` = empty) of each spinlock this CPU is currently
    /// holding, innermost at index `lock_depth - 1`, recorded by the
    /// `tairix_sync` lock-diagnostics observer as the CPU acquires and
    /// releases IRQ-masking spinlocks. On a GICv2 hard lockup the maskable
    /// liveness sample can no longer observe a CPU wedged with interrupts
    /// off inside a spinlock section, so this self-published record is what
    /// names the exact lock: the report renders the innermost entry as
    /// `k_lock=<file>:<line>`. The stored value is a source `Location`
    /// pointer to `'static` rodata whose `file`/`line` are rendered — never
    /// a runtime code address, so it discloses no KASLR base.
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) lock_sites: [AtomicUsize; LOCK_STACK_MAX],
    /// Current lock-nesting depth (number of valid [`Self::lock_sites`]
    /// entries, saturating at [`LOCK_STACK_MAX`] for recording while still
    /// counting true depth so release stays balanced). Written last
    /// (release) after the site, so a reader loading it (acquire) first
    /// sees a consistent (site, depth) pair.
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) lock_depth: AtomicUsize,
    /// Whether the innermost record is still *spinning to acquire* (`true`)
    /// rather than *held* (`false`). Only the top entry can be acquiring —
    /// a deeper lock is only taken from inside an already-held section — so
    /// one flag suffices. Renders the `k_lock` state as `acquiring`
    /// (contended/deadlocked on that lock) vs `held` (wedged inside its
    /// critical section).
    #[cfg(feature = "watchdog-diagnostics")]
    pub(crate) lock_top_acquiring: AtomicBool,
}

impl CpuState {
    const fn new() -> Self {
        Self {
            resume: SpinLock::new(None),
            live_space: SpinLock::new(None),
            preempt_pending: AtomicBool::new(false),
            preemptions: AtomicU64::new(0),
            last_progress_ns: AtomicU64::new(0),
            stall_reported: AtomicBool::new(false),
            last_seen_ns: AtomicU64::new(0),
            wd_activity: AtomicU8::new(0),
            hard_reported: AtomicBool::new(false),
            wd_ctx_pc: AtomicU64::new(0),
            wd_ctx_task: AtomicU64::new(u64::MAX),
            wd_ctx_aux: AtomicU64::new(0),
            wd_ctx_in_kernel: AtomicBool::new(false),
            force_yield: AtomicBool::new(false),
            #[cfg(feature = "watchdog-diagnostics")]
            kbc_site: AtomicU8::new(0),
            #[cfg(feature = "watchdog-diagnostics")]
            kbc_detail: AtomicU64::new(0),
            #[cfg(feature = "watchdog-diagnostics")]
            kbc_seq: AtomicU64::new(0),
            #[cfg(feature = "watchdog-diagnostics")]
            wd_bt: [const { AtomicU64::new(0) }; WD_BT_MAX],
            #[cfg(feature = "watchdog-diagnostics")]
            wd_bt_len: AtomicU32::new(0),
            #[cfg(feature = "watchdog-diagnostics")]
            lock_sites: [const { AtomicUsize::new(0) }; LOCK_STACK_MAX],
            #[cfg(feature = "watchdog-diagnostics")]
            lock_depth: AtomicUsize::new(0),
            #[cfg(feature = "watchdog-diagnostics")]
            lock_top_acquiring: AtomicBool::new(false),
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

/// The installed per-CPU state slots, or an empty slice before install.
///
/// The cross-CPU watchdog scan walks this to inspect every other CPU's
/// heartbeats and activity; the slice is immutable for the boot lifetime
/// (set-once), so reading it lock-free from the non-maskable watchdog path
/// is sound.
pub(crate) fn states() -> &'static [CpuState] {
    #[cfg(any(test, feature = "test-arch"))]
    {
        &TEST_STATE
    }
    #[cfg(not(any(test, feature = "test-arch")))]
    {
        installed().unwrap_or(&[])
    }
}

/// Sum the monotonic preemption counters across installed CPUs.
pub(crate) fn total_preemptions() -> u64 {
    states()
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
