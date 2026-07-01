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
    KernelMemoryStats, ProcessRecord, ProcessState, ResourceLimitRecord, SystemIdentity, Uptime,
    PROCESS_CPU_NONE, RESOURCE_LIMITS_REPORT_LEN,
};
use rustos_abi::{Duration64, Errno, LimitKind, ProcId, Time64};
use rustos_kernel_mem::PAGE_SIZE;
use rustos_kernel_sched_api::{SchedulerPolicy, TaskState};
use rustos_kernel_sec::TaskId as SecTaskId;

use crate::bootinfo::KernelArch;
use crate::fs::FilesystemService;
use crate::init::KernelState;
use crate::introspect::IntrospectSource;
use crate::sched::SchedulerArch;
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
        kernel_heap_bytes: u64,
    ) -> Self {
        Self {
            state,
            filesystem,
            wall_clock,
            kernel_heap_bytes,
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
            let process = ProcessRecord::new(
                task_id,
                resolve_parent(record.parent_proc_id()),
                record.proc_id(),
                record.parent_proc_id(),
                record.owner().0,
                record.primary_gid().0,
                state,
                cpu,
                record.name().as_bytes(),
            )?;
            out.extend_from_slice(&process.to_le_bytes());
        }
        Ok(out)
    }

    fn kernel_memory(&self) -> Result<Vec<u8>, Errno> {
        let total_frames = self.state.frame_allocator.total_frames() as u64;
        let free_frames = self.state.frame_allocator.free_frames() as u64;
        let page = PAGE_SIZE as u64;
        let stats = KernelMemoryStats {
            total_bytes: total_frames.saturating_mul(page),
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
