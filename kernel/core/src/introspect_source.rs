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

use tairix_abi::sysinfo::{
    CpuCoreClass, CpuInfoRecord, CpuLoadRecord, CpuTimeRecord, KernelMemoryStats, LoadAverage,
    MemoryPressureBand, MemoryPressureStats, ProcessRecord, ProcessState, ReclaimClassRecord,
    ResourceLimitRecord, SystemIdentity, Uptime, UserDirectoryRecord, CPU_INFO_FLAG_FREQ_MEASURED,
    CPU_MODEL_NAME_MAX, PRESSURE_BAND_COUNT, PROCESS_CPU_NONE, RESOURCE_LIMITS_REPORT_LEN,
};
use tairix_abi::{Duration64, Errno, LimitKind, ProcId, Time64};
use tairix_kernel_mem::PAGE_SIZE;
use tairix_kernel_sched_api::{Priority, SchedulerPolicy, TaskId, TaskState};
use tairix_kernel_sec::TaskId as SecTaskId;
use tairix_reclaim::{PressureBand, ReclaimClass};

use crate::bootinfo::KernelArch;
use crate::fs::FilesystemService;
use crate::init::KernelState;
use crate::introspect::IntrospectSource;
use crate::loadavg::LoadTracker;
use crate::sched::{level_of_priority, SchedulerArch};
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
    /// `tairix_kalloc::HEAP_BYTES`).
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
            // The task's service level from the scheduler's own record. A
            // record the scheduler has already drained no longer competes
            // for CPU at any level; the admission default is the honest
            // reading for it, never an error that would fail the whole page.
            let priority = level_of_priority(
                self.state
                    .scheduler
                    .priority(task_id)
                    .unwrap_or(Priority::Normal),
            );
            let process = ProcessRecord::new(
                task_id,
                resolve_parent(record.parent_proc_id()),
                record.proc_id(),
                record.parent_proc_id(),
                record.owner().0,
                record.primary_gid().0,
                state,
                cpu,
                priority,
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

    fn volume_io_health(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        let records = self.filesystem.volume_io_health_snapshot();
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
        user_directory_page(self.users_db, offset, max_records)
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

    fn memory_pressure(&self) -> Result<Vec<u8>, Errno> {
        // The one system gauge, created over this kernel's frame
        // allocator if the boot path has not already done so — either
        // way there is a single hysteresis history. Reading it takes a
        // fresh sample, exactly as every cache consumer reads it.
        let gauge = crate::memstats::MEM_STATS.system_pressure(self.state.frame_allocator);
        let band = gauge.sample();
        let thresholds = gauge.thresholds();
        let to_u64 = |v: usize| v as u64;
        let mut band_entries = [0u64; PRESSURE_BAND_COUNT];
        for (depth, slot) in band_entries.iter_mut().enumerate() {
            // Depth indexes are the closed five-band set by construction.
            let band = PressureBand::from_depth(u8::try_from(depth).unwrap_or(0));
            *slot = gauge.band_entries(band);
        }
        let stats = MemoryPressureStats {
            band: band.depth(),
            reserved: [0u8; 7],
            total_bytes: to_u64(gauge.total_bytes()),
            free_bytes: to_u64(gauge.free_bytes()),
            reserve_bytes: to_u64(thresholds.reserve()),
            enter_bytes: thresholds.enter_watermarks().map(to_u64),
            exit_bytes: thresholds.exit_watermarks().map(to_u64),
            band_entries,
        };
        Ok(stats.to_le_bytes().to_vec())
    }

    fn memory_pressure_band(&self) -> Result<Vec<u8>, Errno> {
        // The published band, with no reading taken: this backs the
        // ungated query, so an unprivileged caller must not be able to
        // drive a free-memory sample. Before boot brings the gauge
        // online the registry truthfully reports the shallowest band.
        let report = MemoryPressureBand {
            band: crate::memstats::MEM_STATS.published_band().depth(),
            reserved: [0u8; 7],
        };
        Ok(report.to_le_bytes().to_vec())
    }

    fn reclaim(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        // One record per reclaim class, aggregated across every
        // registered live cache ledger; the wire class id is the class's
        // index, pinned equal across `lib/abi` and `kernel/mem` by the
        // `reclaim_classes_match_the_abi_registry` test below.
        let classes = ReclaimClass::ALL;
        let count = classes.len() as u64;
        let first = offset.min(count);
        let last = first.saturating_add(max_records as u64).min(count);
        let mut out = Vec::new();
        for index in first..last {
            let class = classes[usize::try_from(index).unwrap_or(0)];
            let stats = crate::memstats::MEM_STATS.reclaim_class_stats(class);
            let record = ReclaimClassRecord {
                class: u8::try_from(index).unwrap_or(0),
                reserved: [0u8; 7],
                payload_bytes: stats.payload_bytes,
                metadata_bytes: stats.metadata_bytes,
                entries: stats.entries,
                refusals: stats.refusals,
                pressure_shrinks: stats.pressure_shrinks,
                teardowns: stats.teardowns,
                failures: stats.failures,
                hits: stats.hits,
                misses: stats.misses,
            };
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }

    fn ramzip(&self) -> Result<Vec<u8>, Errno> {
        // Counters only — never page contents or key material. An
        // undriven tier truthfully reports idle zeros. The pinned
        // aggregate rides the same record: it is the registry's live
        // pinned footprint (`mem_pin`), composed here rather than inside
        // a tier source because the exemption exists — and is worth
        // observing — whether or not a tier is running.
        let mut stats = crate::memstats::MEM_STATS.ramzip_stats();
        stats.pinned_bytes = self.state.aspaces.read().pinned_total_bytes();
        Ok(stats.to_le_bytes().to_vec())
    }

    fn cpu_load(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        // The busy/idle split stays in `cpu_times`; these records carry
        // only the remainder. A torn-down CPU truthfully reports zero
        // rather than erroring the whole page.
        let cpu_count = u64::from(self.state.scheduler.cpu_count());
        let first = offset.min(cpu_count);
        let last = first.saturating_add(max_records as u64).min(cpu_count);
        let mut out = Vec::new();
        for cpu in first..last {
            let cpu_id = u32::try_from(cpu).unwrap_or(u32::MAX);
            let record = CpuLoadRecord {
                cpu: cpu_id,
                reserved: 0,
                queue_depth: self.state.scheduler.queue_depth(cpu_id).unwrap_or(0),
                switches: self.state.scheduler.cpu_switches(cpu_id).unwrap_or(0),
                // Real involuntary preemptions performed by the kernel's
                // preemption mechanism — not the scheduler policy's
                // internal timer-tick observation, which is always zero
                // for a tickless policy (EEVDF) and so never reflected
                // the preemptions actually taken under load.
                preemptions: crate::preempt::preemption_count(cpu_id),
            };
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }

    fn cpu_info(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        let cpu_count = u64::from(self.state.scheduler.cpu_count());
        let first = offset.min(cpu_count);
        let last = first.saturating_add(max_records as u64).min(cpu_count);
        let features = self.state.arch.cpu_features();
        // The fixed reference/timebase frequency is one value for the whole
        // machine; `0` when the port drives no core-clock source.
        let reference_hz = crate::cpufreq::reference_hz();
        let mut out = Vec::new();
        for cpu in first..last {
            let cpu_id = u32::try_from(cpu).unwrap_or(u32::MAX);
            // ISA feature bits and per-core identity read through the Arch
            // HAL. `detect`/`core_type` read the *executing* core's ID
            // registers, so on a heterogeneous machine they describe the CPU
            // running this read rather than `cpu_id`; the per-CPU frequency
            // below is genuinely per-target (sampled on each core's own
            // tick). A port with no CPU-feature slice honestly reports no
            // bits and an unknown core (fail closed, never fabricated).
            let feature_bits = features.map_or(0, |f| f.detect(cpu_id).bits());
            let (class, raw_id, model) = match features.map(|f| f.core_type(cpu_id)) {
                Some(core) => {
                    let class = match core.class {
                        tairix_arch_api::CoreClass::Efficiency => CpuCoreClass::Efficiency,
                        tairix_arch_api::CoreClass::Performance => CpuCoreClass::Performance,
                    };
                    (class, core.raw_id, core.model.unwrap_or(""))
                }
                None => (CpuCoreClass::Performance, 0, ""),
            };
            // The live measured core-clock frequency (`0` = not measured on
            // this CPU yet, or the port drives no core-clock source), and the
            // flag that says which it is — never a fabricated rate.
            let current_freq_hz = crate::cpufreq::current_freq_hz(cpu_id);
            let flags = if current_freq_hz != 0 {
                CPU_INFO_FLAG_FREQ_MEASURED
            } else {
                0
            };
            // The model name is a short static ASCII string; cap it to the
            // record's fixed field rather than error a whole page.
            let model_bytes = model.as_bytes();
            let model_bytes = &model_bytes[..model_bytes.len().min(CPU_MODEL_NAME_MAX)];
            let record = CpuInfoRecord::new(
                cpu_id,
                class,
                flags,
                feature_bits,
                raw_id,
                current_freq_hz,
                reference_hz,
                model_bytes,
            )?;
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
                .map(tairix_kernel_sec::TaskCapabilities::task);
            id
        };
        let task_id = found.ok_or(Errno::NotFound)?;

        // Read the task's effective limit set plus the live accounting
        // behind each kind under one registry read, and build the
        // positional per-kind report. A kind with no live accounter yet
        // reports zero — the honest "none measured" answer, never a
        // fabricated count (the array stays `LimitKind::COUNT` long and
        // positional, never omitting a kind).
        let (limits, aspace_usage, stack_usage, pinned_usage) = {
            let aspaces = self.state.aspaces.read();
            let task = SecTaskId(task_id.0);
            // Pinned usage is the whole footprint while the task is
            // pinned and zero otherwise — the budget is only consumed by
            // a live pin, so an unpinned task honestly reports none.
            let pinned_usage = if aspaces.is_pinned(task) {
                aspaces.pinned_footprint_bytes(task)
            } else {
                0
            };
            (
                aspaces.limits(task),
                aspaces.mapped_aspace_bytes(task),
                aspaces.stack_committed_bytes(task),
                pinned_usage,
            )
        };
        let mut out = Vec::with_capacity(RESOURCE_LIMITS_REPORT_LEN);
        for kind in LimitKind::ALL {
            let usage = match kind {
                LimitKind::AddressSpaceBytes => aspace_usage,
                LimitKind::StackBytes => stack_usage,
                LimitKind::PinnedMemoryBytes => pinned_usage,
                _ => 0,
            };
            let record = ResourceLimitRecord::new(kind, limits.get(kind), usage);
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }
}

/// Encode one page of the account directory: the concatenation of the two
/// identity halves, in stable order — the compiled-in system accounts
/// first (kernel policy, always present, no volume required), then the
/// on-disk human records.
///
/// A kernel holding no human database (the root volume is not yet
/// mounted/unlocked, or none is installed) truthfully lists just the
/// compiled half — never an error the broker would refuse ungated clients
/// over and never a fabricated account. The held text was validated by
/// the same fail-closed parser at load, so a re-parse failure equally
/// yields no human rows.
///
/// Only the uid + username pairing crosses this boundary; password
/// records, homes, shells, and grants stay behind the capability-gated
/// `users_db_read` syscall. Row order is stable across paged calls (the
/// held text only changes through the audited admin path).
fn user_directory_page(
    users_db: &dyn UsersDbSource,
    offset: u64,
    max_records: usize,
) -> Result<Vec<u8>, Errno> {
    let humans = users_db.text().ok().and_then(|text| {
        core::str::from_utf8(&text)
            .ok()
            .and_then(|text| tairix_users::UsersDb::parse(text).ok())
    });
    // Page across the concatenation with shared skip/take counters: the
    // two halves borrow with different lifetimes, so a single chained
    // iterator cannot express them.
    let mut skip = usize::try_from(offset).unwrap_or(usize::MAX);
    let mut remaining = max_records;
    let mut out = Vec::new();
    for (uid, username) in tairix_users::system_account_directory() {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        if remaining == 0 {
            break;
        }
        let entry = UserDirectoryRecord::new(uid, username.as_bytes())?;
        out.extend_from_slice(&entry.to_le_bytes());
        remaining -= 1;
    }
    if let Some(db) = &humans {
        for record in db.records() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if remaining == 0 {
                break;
            }
            let entry = UserDirectoryRecord::new(record.uid().0, record.username().as_bytes())?;
            out.extend_from_slice(&entry.to_le_bytes());
            remaining -= 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::counts_toward_load;
    use tairix_abi::sysinfo::{PRESSURE_BAND_COUNT, RECLAIM_CLASS_COUNT};
    use tairix_kernel_sched_api::TaskState;
    use tairix_reclaim::ReclaimClass;

    /// The wire class ids the reclaim export emits are the kernel
    /// taxonomy's own indexes; the two closed sets must stay the same
    /// size (their name correspondence is pinned beside the names in
    /// `lib/abi`).
    #[test]
    fn reclaim_classes_match_the_abi_registry() {
        assert_eq!(ReclaimClass::ALL.len(), RECLAIM_CLASS_COUNT);
        for (index, class) in ReclaimClass::ALL.iter().enumerate() {
            assert_eq!(class.index(), index);
        }
    }

    /// The pressure export's band vocabulary is the kernel gauge's own
    /// five-band set.
    #[test]
    fn pressure_bands_match_the_abi_count() {
        use tairix_reclaim::PressureBand;
        for depth in 0..PRESSURE_BAND_COUNT {
            let band = PressureBand::from_depth(u8::try_from(depth).unwrap());
            assert_eq!(usize::from(band.depth()), depth);
        }
    }

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

    use super::user_directory_page;
    use crate::users::{HeldUsersDbSource, LateUsersDb, NullUsersDbSource};
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::sysinfo::UserDirectoryRecord;

    /// Decode a page's packed records into owned `(uid, name)` rows.
    fn rows(bytes: &[u8]) -> Vec<(u32, String)> {
        assert_eq!(bytes.len() % UserDirectoryRecord::WIRE_LEN, 0);
        bytes
            .as_chunks::<{ UserDirectoryRecord::WIRE_LEN }>()
            .0
            .iter()
            .map(|chunk| {
                let record = UserDirectoryRecord::from_bytes(chunk).expect("record decodes");
                (
                    record.uid,
                    String::from(core::str::from_utf8(record.name_bytes()).expect("utf8")),
                )
            })
            .collect()
    }

    /// A users cell holding one human account, mirroring the unlock's
    /// install of the on-disk half.
    fn human_db() -> LateUsersDb {
        let record = tairix_users::UserRecord::with_password(
            tairix_users::Identity {
                username: "root",
                uid: tairix_users::Uid(1000),
                primary_gid: tairix_users::Gid(1000),
                supplementary_gids: &[],
                display_name: "",
                home: Some("/Users/root"),
                shell: Some("/System/Apps/elsh.app/Run"),
                capabilities: tairix_caps::CapabilitySet::empty(),
                state: tairix_users::AccountState::Active,
            },
            b"pw",
            [0x42; 16],
            tairix_users::MIN_ITERATIONS,
        )
        .expect("valid record");
        let db = tairix_users::UsersDb::new(alloc::vec![record]).expect("valid db");
        let cell = LateUsersDb::new();
        cell.install(HeldUsersDbSource::new(db.serialise().into_bytes()))
            .expect("fresh cell installs");
        cell
    }

    #[test]
    fn the_user_directory_lists_the_compiled_accounts_without_a_database() {
        // No volume, no database: the compiled-in system identity still
        // lists in full — the /etc/passwd-class public pairing exists from
        // first boot, and nothing is fabricated beyond it.
        let page = user_directory_page(&NullUsersDbSource, 0, 64).expect("page encodes");
        let rows = rows(&page);
        let expected: Vec<(u32, String)> = tairix_users::system_account_directory()
            .map(|(uid, name)| (uid, String::from(name)))
            .collect();
        assert_eq!(rows, expected);
    }

    #[test]
    fn the_user_directory_pages_across_the_compiled_and_human_halves() {
        let cell = human_db();
        // The whole directory: compiled rows first, then the human half.
        let all = rows(&user_directory_page(&cell, 0, 64).expect("page encodes"));
        assert_eq!(all.len(), 8);
        assert_eq!(all[0], (0, String::from("system")));
        assert_eq!(all[7], (1000, String::from("root")));
        // A page straddling the seam carries the tail of the compiled half
        // and the head of the human half.
        let seam = rows(&user_directory_page(&cell, 6, 2).expect("page encodes"));
        assert_eq!(
            seam,
            alloc::vec![
                (tairix_users::FONTD_UID.0, String::from("fontd")),
                (1000, String::from("root")),
            ]
        );
        // An offset past the end is the empty paging terminator.
        assert!(rows(&user_directory_page(&cell, 8, 64).expect("page encodes")).is_empty());
    }
}
