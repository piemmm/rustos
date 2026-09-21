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

use tairix_abi::sysinfo::SYSTEM_CONFIG_MAX_LEN;
use tairix_abi::sysinfo::{
    CacheLedgerRecord, CpuCoreClass, CpuInfoRecord, CpuLoadRecord, CpuTimeRecord,
    GroupDirectoryRecord, KernelMemoryStats, LoadAverage, MemoryPressureBand, MemoryPressureStats,
    MemoryTotal, MountRecord, ProcessRecord, ProcessState, ResourceLimitRecord, SelfAccountRecord,
    SystemIdentity, Uptime, UserDirectoryRecord, VolumeIoHealthRecord, VolumeIoQueueRecord,
    VolumeIoStatsRecord, CPU_INFO_FLAG_FREQ_MEASURED, CPU_MODEL_NAME_MAX, PRESSURE_BAND_COUNT,
    PROCESS_CPU_NONE, RESOURCE_LIMITS_REPORT_LEN,
};
use tairix_abi::{
    CapabilityId, CapabilityQuery, Duration64, Errno, LimitKind, ProcId, Time64, MEMORY_CLASS_COUNT,
};
use tairix_kalloc::FreeListAllocator;
use tairix_kernel_mem::PAGE_SIZE;
use tairix_kernel_sched_api::{Priority, SchedulerPolicy, TaskId, TaskState};
use tairix_kernel_sec::ProcessId;
use tairix_reclaim::PressureBand;

use crate::aspace::AddressSpaceRegistry;
use crate::bootinfo::KernelArch;
use crate::fs::FilesystemService;
use crate::init::KernelState;
use crate::introspect::IntrospectSource;
use crate::loadavg::LoadTracker;
use crate::sched::{level_of_priority, SchedulerArch};
use tairix_users::NO_PATH_MARKER;

use crate::groups::GroupsDbSource;
use crate::users::UsersDbSource;
use crate::wallclock::WallClockSource;

/// How much of the configuration document one read asks for.
///
/// A whole page: the store is far smaller than one, so in practice this is
/// a single read, and the loop above it exists because the filesystem seam
/// promises only "up to" this many bytes.
const CONFIG_READ_CHUNK: usize = 4096;

/// The kernel's own bootstrap principal holds no capability at all, so a
/// read it makes is admitted by the per-inode policy or not at all.
struct NoCapabilities;

impl CapabilityQuery for NoCapabilities {
    fn holds(&self, _cap: CapabilityId) -> bool {
        false
    }
}

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

/// Total usable physical RAM in bytes, scaled from the frame allocator's
/// `usable_frames` census.
///
/// Usable frames, not the allocator's address-space extent: the extent
/// spans from physical address zero to the highest mapped address, so on a
/// platform whose RAM sits above an MMIO window (e.g. a 1 GiB hole below
/// the RAM base) it would overstate the machine's memory and make
/// `total - free` look almost exhausted on a fresh boot.
///
/// This is the one place the figure is derived. Both the gated
/// [`KernelMemoryStats::total_bytes`] view and the ungated
/// [`MemoryTotal`] view read it from here, so the two can never disagree.
/// The multiply saturates: a frame census that could overflow a `u64` of
/// bytes is physically impossible, and reporting `u64::MAX` is still an
/// over-report a budget-sizing caller can bound, where a wrapped product
/// would look like a nearly-empty machine.
fn usable_ram_bytes(usable_frames: usize) -> u64 {
    (usable_frames as u64).saturating_mul(PAGE_SIZE as u64)
}

/// Whole pages currently mapped in `process`'s registered address space, in
/// bytes — image, stack, and anonymous regions; the registry snapshot is
/// re-frozen on every mutating map syscall.
///
/// A process with no registered space (a pure kernel task) truthfully reports
/// zero. One definition, so the per-process rows and the system-wide
/// user-residency aggregate cannot drift apart.
fn resident_bytes(aspaces: &AddressSpaceRegistry, process: ProcessId) -> u64 {
    aspaces
        .resolve(process)
        .map_or(0, |(space, _)| space.mapped_pages() as u64)
        .saturating_mul(PAGE_SIZE as u64)
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
    /// The binary's kernel heap, read live for
    /// `KernelMemoryStats::kernel_heap_bytes`. The heap grows and shrinks by
    /// whole regions, so its size is a reading and not a boot-time constant.
    heap: &'static FreeListAllocator,
    /// The kernel-held user database the account directory is derived
    /// from. Only the uid + username pairing is ever exposed; credential
    /// material stays behind the capability-gated `users_db_read` syscall.
    users_db: &'static (dyn UsersDbSource + 'static),
    /// The kernel-held group registry the group directory is derived
    /// from: the gid + group-name pairing and nothing else.
    groups_db: &'static (dyn GroupsDbSource + 'static),
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
        groups_db: &'static (dyn GroupsDbSource + 'static),
        heap: &'static FreeListAllocator,
    ) -> Self {
        Self {
            state,
            filesystem,
            wall_clock,
            users_db,
            groups_db,
            heap,
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

    /// The frame allocator's usable-frame census, the input
    /// [`usable_ram_bytes`] scales.
    fn usable_frames(&self) -> usize {
        self.state.frame_allocator.usable_frames()
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
            pid_by_proc.push((record.proc_id(), record.process().0));
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
            let task_id = record.process().0;
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
            let mem_bytes = resident_bytes(&aspaces, ProcessId(task_id));
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
                record.io_bytes_read(),
                record.io_bytes_written(),
                record.name().as_bytes(),
            )?;
            out.extend_from_slice(&process.to_le_bytes());
        }
        Ok(out)
    }

    fn kernel_memory(&self) -> Result<Vec<u8>, Errno> {
        // One acquisition for the whole, the free pool and every class, so
        // the parts and the whole describe the same instant and
        // `free + Σ class == total` holds of what the caller reads.
        let frames = self.state.frame_allocator.snapshot();
        let page = PAGE_SIZE as u64;
        let bytes_of = |count: usize| (count as u64).saturating_mul(page);
        // Summed over the same records and through the same derivation the
        // per-process view reports, so the aggregate and the rows can never
        // disagree. It reveals nothing `total_bytes - free_bytes` does not
        // already, and this query is capability-gated regardless.
        let user_resident_bytes = {
            let caps = self.state.caps.read();
            let aspaces = self.state.aspaces.read();
            caps.iter().fold(0u64, |sum, record| {
                sum.saturating_add(resident_bytes(&aspaces, record.process()))
            })
        };
        let mut class_bytes = [0u64; MEMORY_CLASS_COUNT];
        for (slot, count) in class_bytes.iter_mut().zip(frames.class) {
            *slot = bytes_of(count);
        }
        let stats = KernelMemoryStats {
            total_bytes: bytes_of(frames.usable),
            free_bytes: bytes_of(frames.free),
            kernel_heap_bytes: self.heap.capacity() as u64,
            user_resident_bytes,
            page_size: u32::try_from(PAGE_SIZE).unwrap_or(u32::MAX),
            reserved: 0,
            class_bytes,
        };
        Ok(stats.to_le_bytes().to_vec())
    }

    fn mounts(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        Ok(record_page(
            &self.filesystem.mount_snapshot(),
            offset,
            max_records,
            MountRecord::to_le_bytes,
        ))
    }

    fn volume_io_health(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        Ok(record_page(
            &self.filesystem.volume_io_health_snapshot(),
            offset,
            max_records,
            VolumeIoHealthRecord::to_le_bytes,
        ))
    }

    fn volume_io_stats(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        Ok(record_page(
            &self.filesystem.volume_io_stats_snapshot(),
            offset,
            max_records,
            VolumeIoStatsRecord::to_le_bytes,
        ))
    }

    fn volume_io_queue(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        Ok(record_page(
            &self.filesystem.volume_io_queue_snapshot(),
            offset,
            max_records,
            VolumeIoQueueRecord::to_le_bytes,
        ))
    }

    fn system_config(&self) -> Result<Vec<u8>, Errno> {
        // The kernel's own uid-0 bootstrap identity holds no capability, so
        // the read passes the per-inode owner/mode/ACL check on its merits:
        // the document is the machine's public configuration and world-
        // readable, and nothing here could reach a node that is not.
        let cred = NoCapabilities;
        let mut document = Vec::new();
        let mut buf = [0u8; CONFIG_READ_CHUNK];
        loop {
            let read = match self.filesystem.read(
                0,
                &cred,
                tairix_sysconfig::CONFIG_PATH,
                document.len() as u64,
                &mut buf,
            ) {
                Ok(read) => read,
                // No store is the fresh-installation case: the documented
                // defaults apply, and answering nothing says exactly that.
                Err(Errno::NotFound) => return Ok(Vec::new()),
                Err(err) => return Err(err),
            };
            if read == 0 {
                return Ok(document);
            }
            if document.len().saturating_add(read) > SYSTEM_CONFIG_MAX_LEN {
                // The store's own parser refuses an over-long document
                // whole, so serving a prefix would only turn a refusal into
                // data that reads as the truth.
                return Err(Errno::LengthOutOfRange);
            }
            document.extend_from_slice(buf.get(..read).ok_or(Errno::OutOfRange)?);
        }
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
                let state = self.state.scheduler.state_of(record.process().0);
                if state == TaskState::Exited {
                    continue;
                }
                total = total.saturating_add(1);
                if counts_toward_load(state, record.process().0, observer) {
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

    fn group_directory(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        group_directory_page(self.groups_db, offset, max_records)
    }

    fn account(&self, uid: u32) -> Result<Vec<u8>, Errno> {
        account_record(self.users_db, uid)
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

    fn memory_total_bytes(&self) -> Result<Vec<u8>, Errno> {
        // The same figure `kernel_memory` reports, derived by the one
        // shared helper so the ungated and gated views can never disagree.
        // Only the installed-RAM census is read: an unprivileged caller
        // must not be able to drive a free-memory sample, which is why
        // this does not project the gated record.
        let total = MemoryTotal {
            total_bytes: usable_ram_bytes(self.usable_frames()),
        };
        Ok(total.to_le_bytes().to_vec())
    }

    fn cache_ledgers(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno> {
        // One row per cache this kernel measures, each carrying the
        // cache's own identity; the wire class id is the class's index,
        // pinned equal across `lib/abi` and `kernel/mem` by the
        // `reclaim_classes_match_the_abi_registry` test below.
        Ok(record_page(
            &crate::memstats::MEM_STATS.cache_ledger_records(),
            offset,
            max_records,
            CacheLedgerRecord::to_le_bytes,
        ))
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
                .map(tairix_kernel_sec::TaskCapabilities::process);
            id
        };
        let task_id = found.ok_or(Errno::NotFound)?;

        // Read the task's effective limit set plus the live accounting
        // behind each kind under one registry read, and build the
        // positional per-kind report. A kind with no live accounter yet
        // reports zero — the honest "none measured" answer, never a
        // fabricated count (the array stays `LimitKind::COUNT` long and
        // positional, never omitting a kind).
        // The thread count is the `Threads` limit's live usage, read from the
        // authoritative thread-group table rather than counted anywhere else.
        let thread_usage = self.state.caps.read().thread_count(ProcessId(task_id.0)) as u64;
        // The live advisory-lock record count, read from the lock registry
        // that charges it rather than recounted here.
        let lock_usage = crate::filelock::usage(ProcessId(task_id.0));
        let (limits, aspace_usage, stack_usage, pinned_usage) = {
            let aspaces = self.state.aspaces.read();
            let process = ProcessId(task_id.0);
            // Pinned usage is the whole footprint while the process is
            // pinned and zero otherwise — the budget is only consumed by
            // a live pin, so an unpinned process honestly reports none.
            let pinned_usage = if aspaces.is_pinned(process) {
                aspaces.pinned_footprint_bytes(process)
            } else {
                0
            };
            (
                aspaces.limits(process),
                aspaces.mapped_aspace_bytes(process),
                aspaces.stack_committed_bytes(process),
                pinned_usage,
            )
        };
        let mut out = Vec::with_capacity(RESOURCE_LIMITS_REPORT_LEN);
        for kind in LimitKind::ALL {
            let usage = match kind {
                LimitKind::AddressSpaceBytes => aspace_usage,
                LimitKind::StackBytes => stack_usage,
                LimitKind::PinnedMemoryBytes => pinned_usage,
                LimitKind::Threads => thread_usage,
                LimitKind::FileLocks => lock_usage,
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
/// Only the uid + username pairing crosses this boundary: password
/// records stay behind the capability-gated `users_db_read` syscall, and
/// an account's grants behind the `CAP_USER_ADMIN` listing. A principal
/// reads its own home and shell through the self-scoped account read,
/// never here — the directory answers about *every* account, so it
/// carries only what rendering a uid needs. Row order is stable across
/// paged calls (the held text only changes through the audited admin
/// path).
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
    let mut page = Page::new(offset, max_records);
    for (uid, username) in tairix_users::system_account_directory() {
        page.push(|| {
            UserDirectoryRecord::new(uid, username.as_bytes()).map(|entry| entry.to_le_bytes())
        })?;
    }
    if let Some(db) = &humans {
        for record in db.records() {
            page.push(|| {
                UserDirectoryRecord::new(record.uid().0, record.username().as_bytes())
                    .map(|entry| entry.to_le_bytes())
            })?;
        }
    }
    Ok(page.finish())
}

/// Encode one page of the group directory: the compiled-in system groups
/// first, then the on-disk registry's, each as a
/// [`GroupDirectoryRecord`].
///
/// The group sibling of [`user_directory_page`], on the same terms: a
/// kernel with no registry held (the root is not mounted/unlocked, or none
/// is published) truthfully lists just the compiled half, and a held text
/// the shared parser will not take equally yields no on-disk rows — never
/// an error the broker would refuse ungated clients over, and never a
/// fabricated group.
///
/// Only the gid + name pairing crosses this boundary: membership is the
/// account record's, and the grant ceiling is the `CAP_USER_ADMIN`
/// listing's.
fn group_directory_page(
    groups_db: &dyn GroupsDbSource,
    offset: u64,
    max_records: usize,
) -> Result<Vec<u8>, Errno> {
    let held = groups_db.text().ok().and_then(|text| {
        core::str::from_utf8(&text)
            .ok()
            .and_then(|text| tairix_users::GroupsDb::parse(text).ok())
    });
    let mut page = Page::new(offset, max_records);
    for group in tairix_users::system_groups().unwrap_or_default() {
        page.push(|| {
            GroupDirectoryRecord::new(group.gid().0, group.name().as_bytes())
                .map(|record| record.to_le_bytes())
        })?;
    }
    if let Some(db) = &held {
        for record in db.records() {
            page.push(|| {
                GroupDirectoryRecord::new(record.gid().0, record.name().as_bytes())
                    .map(|entry| entry.to_le_bytes())
            })?;
        }
    }
    Ok(page.finish())
}

/// The wire image of one account's display fields, or no bytes at all for
/// a uid neither the compiled-in table nor the on-disk database holds.
///
/// Absence is an empty answer rather than an error, exactly as an
/// out-of-range directory page is: the broker's client asked about an
/// account that is not there, which is a fact rather than a failure.
fn account_record(users_db: &dyn UsersDbSource, uid: u32) -> Result<Vec<u8>, Errno> {
    if let Some(record) = tairix_users::system_accounts()
        .unwrap_or_default()
        .iter()
        .find(|record| record.uid().0 == uid)
    {
        return Ok(encode_account(record)?.to_le_bytes().to_vec());
    }
    let held = users_db.text().ok().and_then(|text| {
        core::str::from_utf8(&text)
            .ok()
            .and_then(|text| tairix_users::UsersDb::parse(text).ok())
    });
    let Some(db) = held else {
        return Ok(Vec::new());
    };
    let Some(record) = db.records().iter().find(|record| record.uid().0 == uid) else {
        return Ok(Vec::new());
    };
    Ok(encode_account(record)?.to_le_bytes().to_vec())
}

/// One account's display fields as a [`SelfAccountRecord`].
///
/// An absent home or shell is reported as the database's own `none`
/// marker rather than an empty string, so a no-login account states the
/// intent it actually carries instead of looking like a missing reading.
fn encode_account(record: &tairix_users::UserRecord) -> Result<SelfAccountRecord, Errno> {
    let gids: Vec<u32> = record
        .supplementary_gids()
        .iter()
        .map(|gid| gid.0)
        .collect();
    SelfAccountRecord::new(
        record.uid().0,
        record.primary_gid().0,
        &gids,
        tairix_abi::sysinfo::SelfAccountText {
            name: record.username().as_bytes(),
            display_name: record.display_name().as_bytes(),
            home: record.home().unwrap_or(NO_PATH_MARKER).as_bytes(),
            shell: record.shell().unwrap_or(NO_PATH_MARKER).as_bytes(),
        },
    )
}

/// The skip/take cursor a two-half directory page is built through.
///
/// Both directories concatenate a compiled-in half with an on-disk one
/// whose records borrow with a different lifetime, so a single chained
/// iterator cannot express them; this holds the shared counters instead of
/// each page re-deriving them.
struct Page {
    skip: usize,
    remaining: usize,
    out: Vec<u8>,
}

impl Page {
    /// A cursor that drops the first `offset` records and takes at most
    /// `max_records` of the rest.
    fn new(offset: u64, max_records: usize) -> Self {
        Self {
            skip: usize::try_from(offset).unwrap_or(usize::MAX),
            remaining: max_records,
            out: Vec::new(),
        }
    }

    /// Offer one record, encoding it only where the cursor is inside the
    /// window — a skipped or past-the-window row costs nothing.
    fn push<const N: usize>(
        &mut self,
        encode: impl FnOnce() -> Result<[u8; N], Errno>,
    ) -> Result<(), Errno> {
        if self.skip > 0 {
            self.skip -= 1;
            return Ok(());
        }
        if self.remaining == 0 {
            return Ok(());
        }
        self.out.extend_from_slice(&encode()?);
        self.remaining -= 1;
        Ok(())
    }

    /// The encoded page.
    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Encode one page of a snapshot-backed record list: at most `max_records`
/// of `records` starting at record index `offset`, each through `encode`,
/// packed little-endian back-to-back.
///
/// The one definition of that paging, shared by every domain whose answer is
/// a snapshot slice, so none of them can diverge in how an out-of-range
/// offset behaves: an `offset` past the last row — including one too large
/// for this target's `usize` — yields the empty page every paged domain
/// terminates with, never an error. Each caller's snapshot arrives in a
/// stable order of its own, so a client paging the list sees each row once.
fn record_page<R, const N: usize>(
    records: &[R],
    offset: u64,
    max_records: usize,
    encode: impl Fn(&R) -> [u8; N],
) -> Vec<u8> {
    let total = records.len();
    let first = usize::try_from(offset).unwrap_or(total).min(total);
    let last = first.saturating_add(max_records).min(total);
    let mut out = Vec::new();
    for record in &records[first..last] {
        out.extend_from_slice(&encode(record));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{counts_toward_load, record_page};
    use tairix_abi::sysinfo::{
        CacheLedgerRecord, CacheOwnerKind, PRESSURE_BAND_COUNT, RECLAIM_CLASS_COUNT,
    };
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

    /// The labels of the three rows [`ledger_rows`] builds, in order — what
    /// a decoded page is checked against.
    const LEDGER_LABELS: [&str; 3] = ["clean_fs.data", "clean_fs.metadata", "launch"];

    /// Three rows whose labels, owners, and figures all differ, so a
    /// decoded page proves *which* rows it carries, not merely how many.
    fn ledger_rows() -> Vec<CacheLedgerRecord> {
        let mut rows = Vec::new();
        for (index, label) in LEDGER_LABELS.iter().enumerate() {
            let owner_id = u64::try_from(index).expect("three rows fit");
            let mut record = CacheLedgerRecord::new(
                label.as_bytes(),
                CacheOwnerKind::FilesystemVolume,
                owner_id,
                0,
            )
            .expect("renderable label");
            record.payload_bytes = 4096 * (owner_id + 1);
            rows.push(record);
        }
        rows
    }

    /// Decode a page back into the rows it carries, refusing a page whose
    /// length is not a whole number of records — a client indexes by the
    /// fixed stride, so a partial tail row would be read as garbage.
    fn decode(page: &[u8]) -> Vec<CacheLedgerRecord> {
        assert_eq!(
            page.len() % CacheLedgerRecord::WIRE_LEN,
            0,
            "whole records only"
        );
        page.chunks(CacheLedgerRecord::WIRE_LEN)
            .map(|chunk| CacheLedgerRecord::from_bytes(chunk).expect("decodes"))
            .collect()
    }

    #[test]
    fn the_cache_ledger_record_page_honours_offset_and_limit() {
        let rows = ledger_rows();

        // A limit covering the registry carries every row unchanged: the
        // identity, owner, and figures a client reads are the ones the
        // registry holds.
        assert_eq!(
            decode(&record_page(
                &rows,
                0,
                rows.len(),
                CacheLedgerRecord::to_le_bytes
            )),
            rows
        );

        // A limit truncates from the front; the offset then resumes from
        // exactly where the previous page stopped, so a paging client sees
        // every row once and none twice.
        let first = decode(&record_page(&rows, 0, 2, CacheLedgerRecord::to_le_bytes));
        let second = decode(&record_page(&rows, 2, 2, CacheLedgerRecord::to_le_bytes));
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 1);
        let paged: Vec<&str> = first
            .iter()
            .chain(&second)
            .map(CacheLedgerRecord::label)
            .collect();
        assert_eq!(paged, LEDGER_LABELS);

        // A limit of nothing asks for nothing, and is not an error.
        assert!(record_page(&rows, 0, 0, CacheLedgerRecord::to_le_bytes).is_empty());
    }

    #[test]
    fn the_cache_ledger_record_page_terminates_past_the_last_row() {
        let rows = ledger_rows();
        let total = u64::try_from(rows.len()).expect("three rows fit");
        // At the end, one past it, and an offset no `usize` on a 32-bit
        // target could hold: each is the empty terminator every paged
        // domain ends with, never an error and never a wrapped row.
        for offset in [total, total + 1, u64::MAX] {
            assert!(
                record_page(&rows, offset, rows.len(), CacheLedgerRecord::to_le_bytes).is_empty()
            );
        }
        // An empty registry is the same terminator from the first read.
        let empty: [CacheLedgerRecord; 0] = [];
        assert!(record_page(&empty, 0, 8, CacheLedgerRecord::to_le_bytes).is_empty());
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

    use super::{usable_ram_bytes, PAGE_SIZE};
    use tairix_abi::sysinfo::{KernelMemoryStats, MemoryTotal};
    use tairix_abi::MEMORY_CLASS_COUNT;

    /// The ungated total and the gated kernel-memory view report one
    /// number for one machine: both scale the same usable-frame census
    /// through [`usable_ram_bytes`], so which capability a caller holds
    /// can never change the size it is told.
    #[test]
    fn the_ungated_total_matches_the_gated_kernel_memory_total() {
        for usable_frames in [0usize, 1, 512, 1 << 20] {
            let bytes = usable_ram_bytes(usable_frames);
            let gated = KernelMemoryStats {
                total_bytes: bytes,
                free_bytes: 0,
                kernel_heap_bytes: 0,
                user_resident_bytes: 0,
                page_size: u32::try_from(PAGE_SIZE).unwrap_or(u32::MAX),
                reserved: 0,
                class_bytes: [0; MEMORY_CLASS_COUNT],
            };
            let ungated = MemoryTotal { total_bytes: bytes };
            let gated =
                KernelMemoryStats::from_bytes(&gated.to_le_bytes()).expect("gated round trip");
            let ungated =
                MemoryTotal::from_bytes(&ungated.to_le_bytes()).expect("ungated round trip");
            assert_eq!(ungated.total_bytes, gated.total_bytes);
        }
    }

    #[test]
    fn usable_ram_bytes_scales_by_page_and_never_wraps() {
        assert_eq!(usable_ram_bytes(0), 0);
        assert_eq!(usable_ram_bytes(1), PAGE_SIZE as u64);
        assert_eq!(usable_ram_bytes(512), 512 * PAGE_SIZE as u64);

        // A census large enough to overflow the byte count reports the
        // largest representable size, never a wrapped one that would look
        // like a nearly-empty machine to a caller sizing a cache against
        // it. Unreachable where `usize` is too narrow to hold such a
        // census, so the arm is skipped rather than asserted there.
        let overflowing = u64::MAX / PAGE_SIZE as u64 + 1;
        if let Ok(frames) = usize::try_from(overflowing) {
            assert_eq!(usable_ram_bytes(frames), u64::MAX);
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

    use super::{account_record, group_directory_page, user_directory_page};
    use crate::groups::{LateGroupsDb, NullGroupsDbSource};
    use crate::users::{HeldUsersDbSource, LateUsersDb, NullUsersDbSource};
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::sysinfo::{GroupDirectoryRecord, SelfAccountRecord, UserDirectoryRecord};
    use tairix_users::NO_PATH_MARKER;

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
    ///
    /// The account carries the shared stored password rather than deriving
    /// its own: the directory lists the identity half alone and never any
    /// password material.
    fn human_db() -> LateUsersDb {
        let record = tairix_users::UserRecord::new(
            tairix_users::Identity {
                username: "root",
                uid: tairix_users::Uid(1000),
                primary_gid: tairix_users::Gid(1000),
                supplementary_gids: &[],
                display_name: "",
                home: Some("/Users/root"),
                shell: Some("/System/Commands/elsh.app/Run"),
                capabilities: tairix_caps::CapabilitySet::empty(),
                state: tairix_users::AccountState::Active,
            },
            crate::test_identity::shared_password(),
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
        assert_eq!(all.len(), 12);
        assert_eq!(all[0], (0, String::from("system")));
        assert_eq!(all[11], (1000, String::from("root")));
        // A page straddling the seam carries the tail of the compiled half
        // and the head of the human half.
        let seam = rows(&user_directory_page(&cell, 10, 2).expect("page encodes"));
        assert_eq!(
            seam,
            alloc::vec![
                (tairix_users::AUDIOD_UID.0, String::from("audiod")),
                (1000, String::from("root")),
            ]
        );
        // An offset past the end is the empty paging terminator.
        assert!(rows(&user_directory_page(&cell, 12, 64).expect("page encodes")).is_empty());
    }

    /// Decode a group page's packed records into owned `(gid, name)` rows.
    fn group_rows(bytes: &[u8]) -> Vec<(u32, String)> {
        assert_eq!(bytes.len() % GroupDirectoryRecord::WIRE_LEN, 0);
        bytes
            .as_chunks::<{ GroupDirectoryRecord::WIRE_LEN }>()
            .0
            .iter()
            .map(|chunk| {
                let record = GroupDirectoryRecord::from_bytes(chunk).expect("record decodes");
                (
                    record.gid,
                    String::from(core::str::from_utf8(record.name_bytes()).expect("utf8")),
                )
            })
            .collect()
    }

    /// A group cell holding one on-disk group, mirroring the unlock's
    /// publish of the loaded registry.
    fn human_groups() -> LateGroupsDb {
        let record =
            tairix_users::GroupRecord::new("staff", tairix_users::Gid(1000)).expect("valid group");
        let db = tairix_users::GroupsDb::new(alloc::vec![record]).expect("valid registry");
        let cell = LateGroupsDb::new();
        cell.publish(db.serialise().into_bytes());
        cell
    }

    #[test]
    fn the_group_directory_lists_the_compiled_groups_without_a_registry() {
        // No volume, no registry: the compiled-in system groups still
        // list in full, and nothing is fabricated beyond them.
        let page = group_directory_page(&NullGroupsDbSource, 0, 64).expect("page encodes");
        let expected: Vec<(u32, String)> = tairix_users::system_groups()
            .expect("compiled groups build")
            .iter()
            .map(|group| (group.gid().0, String::from(group.name())))
            .collect();
        assert_eq!(group_rows(&page), expected);
    }

    #[test]
    fn the_group_directory_pages_across_the_compiled_and_on_disk_halves() {
        let cell = human_groups();
        let compiled = tairix_users::system_groups()
            .expect("compiled groups build")
            .len();
        let all = group_rows(&group_directory_page(&cell, 0, 64).expect("page encodes"));
        assert_eq!(all.len(), compiled + 1);
        assert_eq!(all[compiled], (1000, String::from("staff")));
        // A page straddling the seam carries the tail of the compiled half
        // and the head of the on-disk half, so a paging client sees every
        // row once and none twice.
        let seam = group_rows(
            &group_directory_page(&cell, u64::try_from(compiled - 1).expect("fits"), 2)
                .expect("page encodes"),
        );
        assert_eq!(seam.len(), 2);
        assert_eq!(seam[1], (1000, String::from("staff")));
        // An offset past the end is the empty paging terminator.
        assert!(group_rows(
            &group_directory_page(&cell, u64::try_from(compiled + 1).expect("fits"), 64)
                .expect("page encodes")
        )
        .is_empty());
        assert!(
            group_rows(&group_directory_page(&cell, u64::MAX, 64).expect("page encodes"))
                .is_empty()
        );
    }

    #[test]
    fn the_account_read_answers_a_held_record_and_nothing_for_an_unknown_uid() {
        let cell = human_db();
        let bytes = account_record(&cell, 1000).expect("the read answers");
        let record = SelfAccountRecord::from_bytes(&bytes).expect("a whole record");
        assert_eq!(record.uid, 1000);
        assert_eq!(record.name_bytes(), b"root");
        assert_eq!(record.home_bytes(), b"/Users/root");
        assert_eq!(record.shell_bytes(), b"/System/Commands/elsh.app/Run");
        assert_eq!(record.primary_gid, 1000);
        assert!(record.supplementary_gids().is_empty());

        // A uid no database holds is an empty answer, not an error: the
        // account is simply not there.
        assert!(account_record(&cell, 4242)
            .expect("the read answers")
            .is_empty());
    }

    #[test]
    fn the_account_read_serves_a_compiled_account_before_any_volume() {
        // A system account resolves from the compiled-in table, so a
        // service reads its own record from first boot; its absent home
        // and shell state the database's own marker rather than looking
        // like a missing reading.
        let bytes = account_record(&NullUsersDbSource, 0).expect("the read answers");
        let record = SelfAccountRecord::from_bytes(&bytes).expect("a whole record");
        assert_eq!(record.name_bytes(), b"system");
        assert_eq!(record.home_bytes(), NO_PATH_MARKER.as_bytes());
        assert_eq!(record.shell_bytes(), NO_PATH_MARKER.as_bytes());
        // With no database there is no human account to answer for.
        assert!(account_record(&NullUsersDbSource, 1000)
            .expect("the read answers")
            .is_empty());
    }
}
