//! The production [`IntrospectSource`] over the live kernel state
//! (`PREREQUISITES.md` P-C).
//!
//! [`KernelIntrospectSource`] is the real implementation of the
//! [`crate::introspect::IntrospectSource`] seam: it reads the authoritative
//! `CapTable`, scheduler, frame allocator, per-task limits, mount table, and
//! monotonic/wall clocks the running kernel already owns, and serialises each
//! domain into the `lib/abi` wire form the `sysinfo_introspect` syscall copies
//! out. It is built in [`crate::init`] once `KernelState` is lifted to
//! `'static`, and installed through
//! [`crate::syscalls::KernelSyscallHandlers::with_introspect`].
//!
//! **Every field is filled from kernel-attested state, never a caller claim,
//! and the source always answers with the whole system's state.** The
//! `sysinfo_introspect` syscall it backs is held only by the user-space
//! `sysinfod` broker, which re-derives every per-client scope against each
//! requester's attested `Origin`; keeping this primitive global-only holds the
//! ring-0 attack surface down while the kernel stays the identity authority.
//!
//! A read never panics and never blocks: it takes only the reader side of the
//! registries' `RwLock`s, allocates the encoded answer, and returns it.

use alloc::vec::Vec;

use rustos_abi::sysinfo::{
    CpuTimeRecord, KernelMemoryStats, LoadAverage, ProcessRecord, ProcessState,
    ResourceLimitRecord, SystemIdentity, Uptime, UserDirectoryRecord, PROCESS_CPU_NONE,
    RESOURCE_LIMITS_REPORT_LEN,
};
use rustos_abi::{Duration64, Errno, LimitKind, ProcId, Time64};
use rustos_kernel_mem::PAGE_SIZE;
use rustos_kernel_sched_api::{SchedulerPolicy, TaskId, TaskState};
use rustos_kernel_sec::TaskId as SecTaskId;

use crate::bootinfo::KernelArch;
use crate::fs::FilesystemService;
use crate::init::KernelState;
use crate::introspect::IntrospectSource;
use crate::loadavg::LoadTracker;
use crate::sched::SchedulerArch;
use crate::users::UsersDbSource;
use crate::wallclock::WallClockSource;

/// The OS version reported in the [`SystemIdentity`] domain, taken from the
/// crate's own package version at build time so the reported version never
/// drifts from the built artefact.
const fn version_component(s: &str) -> u16 {
    // A `const` decimal parser: the Cargo-provided version components are
    // always well-formed decimal integers, so any non-digit is a build-time
    // impossibility that saturates rather than panicking.
    let bytes = s.as_bytes();
    let mut acc: u16 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < b'0' || b > b'9' {
            return acc;
        }
        acc = acc.saturating_mul(10).saturating_add((b - b'0') as u16);
        i += 1;
    }
    acc
}

const OS_VERSION_MAJOR: u16 = version_component(env!("CARGO_PKG_VERSION_MAJOR"));
const OS_VERSION_MINOR: u16 = version_component(env!("CARGO_PKG_VERSION_MINOR"));
const OS_VERSION_PATCH: u16 = version_component(env!("CARGO_PKG_VERSION_PATCH"));

/// Whether `task` in `state` counts toward the runnable census `observer`
/// is taking.
///
/// A task counts when it is ready or running — except the observer itself.
/// The census is read inside the observing broker's own
/// `sysinfo_introspect` syscall, so the observer is always `Running` at
/// the sample *because of* the observation, never because the system has
/// work; counting it would floor every sample at one runnable task and
/// drive an idle machine's damped averages toward the size of the
/// query-wake burst instead of zero (the measurement perturbing the
/// measured quantity). Every *other* awake task is real load and counts.
fn counts_toward_load(state: TaskState, task: TaskId, observer: Option<TaskId>) -> bool {
    matches!(state, TaskState::Ready | TaskState::Running) && Some(task) != observer
}

/// The unprovisioned machine-id sentinel: all zero, meaning "no per-install
/// identity has been generated yet".
///
/// The installer (not yet built) mints a real per-installation machine id and
/// hostname; until it does, reporting the all-zero sentinel is the honest
/// answer — exactly as `BootId::UNSET` is honest for an unseeded boot id —
/// rather than fabricating an identity the system does not have.
const UNPROVISIONED_MACHINE_ID: [u8; 16] = [0u8; 16];

/// The live [`IntrospectSource`] backed by the running kernel's authoritative
/// state.
///
/// Holds only `'static` borrows of state the kernel already owns and adds no
/// authority of its own; every read is capability-gated at the
/// `sysinfo_introspect` syscall the source backs.
pub struct KernelIntrospectSource<A: KernelArch + 'static> {
    /// The leaked kernel state: the authoritative `CapTable`, scheduler,
    /// frame allocator, per-task address-space/limit registry, and arch
    /// handle.
    state: &'static KernelState<A>,
    /// The mounted filesystem service, for the mount-table snapshot.
    filesystem: &'static (dyn FilesystemService + 'static),
    /// The kernel wall clock, for the uptime domain's boot wall-instant.
    wall_clock: &'static (dyn WallClockSource + 'static),
    /// Committed size of the kernel heap region in bytes, reported as
    /// `KernelMemoryStats::kernel_heap_bytes` (the heap is committed at this
    /// fixed size at boot; the value is threaded from the binding kernel's
    /// `rustos_kalloc::HEAP_BYTES`).
    kernel_heap_bytes: u64,
    /// The kernel-held user database the account directory is derived
    /// from. Only the uid + username pairing is ever exposed; credential
    /// material stays behind the capability-gated `users_db_read` syscall.
    users_db: &'static (dyn UsersDbSource + 'static),
    /// The damped run-queue averages, advanced at each load-average read
    /// (the tickless observation model — see [`crate::loadavg`]).
    load: LoadTracker,
}

impl<A: KernelArch + 'static> KernelIntrospectSource<A> {
    /// Build the source over the leaked kernel state and the boot-installed
    /// filesystem service and wall clock.
    ///
    /// Crate-internal because `KernelState` is a private hand-off type: only
    /// [`crate::init`] (which owns the leaked state) constructs the source.
    #[must_use]
    pub(crate) const fn new(
        state: &'static KernelState<A>,
        filesystem: &'static (dyn FilesystemService + 'static),
        wall_clock: &'static (dyn WallClockSource + 'static),
        users_db: &'static (dyn UsersDbSource + 'static),
        kernel_heap_bytes: u64,
    ) -> Self {
        Self {
            state,
            filesystem,
            wall_clock,
            users_db,
            kernel_heap_bytes,
            load: LoadTracker::new(),
        }
    }

    /// Map a scheduler [`TaskState`] to the ABI [`ProcessState`].
    ///
    /// `Ready` (queued, runnable) reports `Runnable`; `Running` reports
    /// `Running`; `Parked` (blocked on a wait) reports `Blocked`; `Exited`
    /// reports `Zombie` (terminated, record not yet reaped).
    fn process_state(state: TaskState) -> ProcessState {
        match state {
            TaskState::Ready => ProcessState::Runnable,
            TaskState::Running => ProcessState::Running,
            TaskState::Parked => ProcessState::Blocked,
            TaskState::Exited => ProcessState::Zombie,
        }
    }

    /// Read the monotonic clock on the issuing CPU.
    fn monotonic_ns(&self) -> u64 {
        let cpu = SchedulerArch::current_cpu(&*self.state.arch);
        self.state.arch.monotonic_ns(cpu)
    }
}

impl<A: KernelArch + 'static> IntrospectSource for KernelIntrospectSource<A> {
    fn processes(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        let caps = self.state.caps.read();

        // First pass: the (proc_id -> numeric pid) map, so a child's parent
        // numeric pid can be resolved from its attested parent proc-id even
        // though the parent may sit anywhere in the ordered table. Using the
        // unforgeable proc-id (not the reusable numeric id) means parentage
        // survives PID reuse.
        let mut pid_by_proc: Vec<(ProcId, u64)> = Vec::new();
        for record in caps.iter() {
            pid_by_proc.push((record.proc_id(), record.task().0));
        }
        let resolve_parent = |parent: ProcId| -> u64 {
            pid_by_proc
                .iter()
                .find(|(pid, _)| *pid == parent)
                .map_or(0, |(_, numeric)| *numeric)
        };

        // Second pass: encode the requested window in the stable ascending
        // `TaskId` order `CapTable::iter` guarantees. An offset past the end
        // yields an empty answer (the paging terminator), never an error.
        let mut out = Vec::new();
        let aspaces = self.state.aspaces.read();
        for record in caps
            .iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(max_records)
        {
            let task_id = record.task().0;
            let state = Self::process_state(self.state.scheduler.state_of(task_id));
            let cpu = match SchedulerPolicy::running_cpu(&self.state.scheduler, task_id) {
                Some(cpu) => u8::try_from(cpu).unwrap_or(PROCESS_CPU_NONE),
                None => PROCESS_CPU_NONE,
            };
            // The scheduler accounts on-CPU time in raw arch ticks; convert
            // at this read point through the port's calibrated frequency. A
            // task the scheduler has already drained (a reaped record)
            // truthfully reports zero rather than erroring the whole page.
            let cpu_time_ns = self
                .state
                .arch
                .ticks_to_ns(self.state.scheduler.cpu_ticks_of(task_id).unwrap_or(0));
            // Whole pages currently mapped in the task's registered address
            // space (image + stack + anonymous regions; the registry snapshot
            // is re-frozen on every mutating map syscall). A task with no
            // registered space (a pure kernel task) truthfully reports zero.
            let mem_bytes = aspaces
                .resolve(SecTaskId(task_id))
                .map_or(0, |(space, _)| space.mapped_pages() as u64)
                .saturating_mul(PAGE_SIZE as u64);
            let process = ProcessRecord::new(
                task_id,
                resolve_parent(record.parent_proc_id()),
                record.proc_id(),
                record.parent_proc_id(),
                record.owner().0,
                record.primary_gid().0,
                state,
                cpu,
                cpu_time_ns,
                mem_bytes,
                record.name().as_bytes(),
            )?;
            out.extend_from_slice(&process.to_le_bytes());
        }
        Ok(out)
    }

    fn kernel_memory(&self) -> Result<Vec<u8>, Errno> {
        // Usable frames, not the allocator's address-space extent: the
        // extent spans from physical address zero to the highest mapped
        // address, so on a platform whose RAM sits above an MMIO window
        // (e.g. a 1 GiB hole below the RAM base) it would overstate the
        // machine's memory and make `total - free` look almost exhausted
        // on a fresh boot.
        let usable_frames = self.state.frame_allocator.usable_frames() as u64;
        let free_frames = self.state.frame_allocator.free_frames() as u64;
        let page = PAGE_SIZE as u64;
        let stats = KernelMemoryStats {
            total_bytes: usable_frames.saturating_mul(page),
            free_bytes: free_frames.saturating_mul(page),
            kernel_heap_bytes: self.kernel_heap_bytes,
            // Per-space resident accounting has no live accounter yet; report
            // it conservatively as zero rather than a guess, exactly as the
            // `SysinfoSource` contract permits for an unavailable usage figure.
            user_resident_bytes: 0,
            page_size: u32::try_from(PAGE_SIZE).unwrap_or(u32::MAX),
            reserved: 0,
        };
        Ok(stats.to_le_bytes().to_vec())
    }

    fn mounts(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        let records = self.filesystem.mount_snapshot();
        let mut out = Vec::new();
        for record in records
            .iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(max_records)
        {
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }

    fn identity(&self) -> Result<Vec<u8>, Errno> {
        // Machine id / hostname are the honest "unprovisioned" sentinel until
        // the installer mints them; the OS version is the real build version.
        let identity = SystemIdentity::new(
            UNPROVISIONED_MACHINE_ID,
            OS_VERSION_MAJOR,
            OS_VERSION_MINOR,
            OS_VERSION_PATCH,
            b"",
        )?;
        Ok(identity.to_le_bytes().to_vec())
    }

    fn uptime(&self) -> Result<Vec<u8>, Errno> {
        let mono_ns = self.monotonic_ns();
        let since_boot = Duration64::from_nanos(mono_ns);
        let reading = self.wall_clock.read(mono_ns);
        // Project the current wall reading back to the boot instant. When no
        // wall time has been established the reading is the Unix epoch tagged
        // `Unset`; report the epoch as the boot instant rather than inventing
        // one (honest, like the unset boot id).
        let boot_time = if reading.state().is_set() {
            reading.time().saturating_sub(since_boot)
        } else {
            Time64::UNIX_EPOCH
        };
        let uptime = Uptime {
            since_boot,
            boot_time,
        };
        Ok(uptime.to_le_bytes().to_vec())
    }

    fn load_average(&self) -> Result<Vec<u8>, Errno> {
        // One walk of the authoritative CapTable yields all three
        // censuses: runnable (ready or running), live tasks, and the
        // distinct non-system uids with at least one live task — the
        // logged-in-user count.
        //
        // The observer is excluded from the runnable census — see
        // [`counts_toward_load`] for why.
        let observer = self
            .state
            .scheduler
            .current_task(SchedulerArch::current_cpu(&*self.state.arch));
        let mut runnable: u32 = 0;
        let mut total: u32 = 0;
        let mut uids: Vec<u32> = Vec::new();
        {
            let caps = self.state.caps.read();
            for record in caps.iter() {
                let state = self.state.scheduler.state_of(record.task().0);
                if state == TaskState::Exited {
                    continue;
                }
                total = total.saturating_add(1);
                if counts_toward_load(state, record.task().0, observer) {
                    runnable = runnable.saturating_add(1);
                }
                let uid = record.owner().0;
                if uid != 0 && !uids.contains(&uid) {
                    uids.push(uid);
                }
            }
        }
        let [one, five, fifteen] = self.load.observe(self.monotonic_ns(), u64::from(runnable));
        let load = LoadAverage {
            load1: one,
            load5: five,
            load15: fifteen,
            runnable,
            total_tasks: total,
            users: u32::try_from(uids.len()).unwrap_or(u32::MAX),
        };
        Ok(load.to_le_bytes().to_vec())
    }

    fn user_directory(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        // A kernel holding no database (the root volume is not yet
        // mounted/unlocked, or none is installed) answers with an empty
        // directory — a truthful "no accounts known", never an error the
        // broker would refuse ungated clients over and never a fabricated
        // account. The held text was validated by the same fail-closed
        // parser at load, so a re-parse failure is equally an empty answer.
        let Ok(text) = self.users_db.text() else {
            return Ok(Vec::new());
        };
        let Ok(text) = core::str::from_utf8(&text) else {
            return Ok(Vec::new());
        };
        let Ok(db) = rustos_users::UsersDb::parse(text) else {
            return Ok(Vec::new());
        };
        // Only the uid + username pairing crosses this boundary; password
        // records, homes, shells, and grants stay behind the capability-
        // gated `users_db_read` syscall. File order is stable across paged
        // calls (the held text only changes through the audited admin path).
        let mut out = Vec::new();
        for record in db
            .records()
            .iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(max_records)
        {
            let entry = UserDirectoryRecord::new(record.uid().0, record.username().as_bytes())?;
            out.extend_from_slice(&entry.to_le_bytes());
        }
        Ok(out)
    }

    fn cpu_times(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        // One monotonic sample shared by every record so the busy/idle
        // split of each CPU describes the same instant; idle is the
        // remainder of uptime the dispatch bracket did not account.
        let now_ns = self.monotonic_ns();
        let cpu_count = u64::from(self.state.scheduler.cpu_count());
        let mut out = Vec::new();
        let first = offset.min(cpu_count);
        let last = first.saturating_add(max_records as u64).min(cpu_count);
        for cpu in first..last {
            // The CPU index is in range by construction; a scheduler
            // refusal (a torn-down CPU) truthfully reports zero rather
            // than erroring the whole page.
            let cpu_id = u32::try_from(cpu).unwrap_or(u32::MAX);
            let busy_ns = self
                .state
                .arch
                .ticks_to_ns(self.state.scheduler.cpu_busy_ticks(cpu_id).unwrap_or(0));
            let record = CpuTimeRecord {
                cpu: cpu_id,
                reserved: 0,
                busy_ns,
                idle_ns: now_ns.saturating_sub(busy_ns),
            };
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }

    fn task_limits(&self, proc_id: ProcId) -> Result<Vec<u8>, Errno> {
        // Resolve the target task by its unforgeable proc-id against the
        // authoritative CapTable; a proc-id with no live task fails closed.
        let found = {
            let caps = self.state.caps.read();
            let id = caps
                .iter()
                .find(|record| record.proc_id() == proc_id)
                .map(rustos_kernel_sec::TaskCapabilities::task);
            id
        };
        let task_id = found.ok_or(Errno::NotFound)?;

        // Read the task's effective limit set and build the positional
        // per-kind report. Live usage has no accounter yet, so it is reported
        // conservatively as zero (the array stays `LimitKind::COUNT` long and
        // positional, never omitting a kind).
        let limits = self.state.aspaces.read().limits(SecTaskId(task_id.0));
        let mut out = Vec::with_capacity(RESOURCE_LIMITS_REPORT_LEN);
        for kind in LimitKind::ALL {
            let record = ResourceLimitRecord::new(kind, limits.get(kind), 0);
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::counts_toward_load;
    use rustos_kernel_sched_api::TaskState;

    #[test]
    fn ready_and_running_tasks_count_toward_load() {
        for state in [TaskState::Ready, TaskState::Running] {
            assert!(counts_toward_load(state, 7, None));
            assert!(counts_toward_load(state, 7, Some(9)));
        }
    }

    #[test]
    fn parked_and_exited_tasks_never_count() {
        for state in [TaskState::Parked, TaskState::Exited] {
            assert!(!counts_toward_load(state, 7, None));
            assert!(!counts_toward_load(state, 7, Some(7)));
        }
    }

    #[test]
    fn the_observer_never_counts_itself() {
        // The regression this pins: the broker reading the census is
        // always `Running` inside its own syscall, so counting it floored
        // every sample at one runnable task and an idle machine's load
        // crept toward the query burst's size instead of zero.
        assert!(!counts_toward_load(TaskState::Running, 7, Some(7)));
        assert!(!counts_toward_load(TaskState::Ready, 7, Some(7)));
    }
}
