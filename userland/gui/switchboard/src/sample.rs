//! System sampling: gathers a compact [`Sample`] of the live system state
//! through the System Information API, tracking the prior-sample state a
//! delta computation needs.
//!
//! Every field degrades to its honest empty form on a query failure or a
//! refused capability — never a fabricated value. `sysinfo` refusals are
//! observed as the typed [`CallError::PermissionDenied`] returned by
//! `lib/procinfo`'s helpers; the optional global-process, memory-pressure
//! and hardware scopes are probed exactly once, at startup
//! ([`probe_scopes`]), since a process's capability set is fixed at spawn
//! and re-probing per sample would only spam the audited memory-pressure
//! query for an answer that can never change.
//!
//! # Absence is explainable
//!
//! A reading that is absent carries *why*: the resolved [`ScopeVerdicts`]
//! travel on every [`Sample`], and [`Sample::absence`] turns an absent
//! reading into [`Absence::NotPermitted`] (the capability gating it is
//! outside this process's ceiling, so the query was never issued) or
//! [`Absence::Unavailable`] (it was issued and the service could not
//! answer, or its cadence has not yet produced a first reading). "You may
//! not see this" and "this is broken" are different statements, and the
//! surface must be able to make the right one.
//!
//! # Cadence
//!
//! Seventeen queries per sample would be absurd for a service that samples
//! every [`SAMPLE_PERIOD_NS`], so each
//! reading is classified under exactly one [`Cadence`] tier and the tier
//! decides when it is issued. Which tier a reading belongs to is stated on
//! its [`Sample`] field; the tiers themselves are defined once, in
//! [`crate::schedule`].
//!
//! # Bounded accumulation
//!
//! Every paged reading walks its pages to completion under a per-reading
//! cap, so a service answering an implausibly long (or hostile) list can
//! make the sampler allocate only up to that cap and no further. Reaching
//! the cap ends the walk deterministically — the retained records are the
//! first `cap` in the service's own stable order, not an arbitrary subset —
//! and is not a failure: the walk answers
//! [`WalkStep::Stop`], which is the
//! shared walker's honest early end rather than an error to be caught back.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use tairix_abi::display_ipc::DisplayStats;
use tairix_abi::hwtree::HwNode;
use tairix_abi::net_ipc::{
    NetInterfaceCountersRecord, NetInterfaceFactsRecord, NetInterfaceRatesRecord,
    NetInterfaceStateRecord, NetServerAddr, NetSockState, NetStackDefenceCounters,
};
use tairix_abi::sysinfo::{
    CacheLedgerRecord, CpuInfoListRequest, CpuInfoRecord, CpuLoadRecord, CpuLoadRequest,
    CpuTimeRecord, CrashRecord, CrashRecordRequest, DeviceStatsRequest, KernelMemoryStats,
    LoadAverage, MemoryPressureBand, MemoryTotal, MountListRequest, MountRecord,
    NetInterfaceListRequest, NetInterfaceRatesRequest, ProcessListRequest, ProcessRecord,
    ProcessState, RamzipStats, ReclaimClassRecord, ResourceLimitRecord, SeatListRequest,
    SeatRecord, SysinfoQueryId, SystemIdentity, Uptime, VolumeIoHealthRecord, VolumeIoQueueRecord,
    VolumeIoRequest, VolumeIoStatsRecord, RESOURCE_LIMITS_REPORT_LEN,
};
use tairix_abi::{Duration64, Errno, ProcId, SchedPriority};
use tairix_procinfo::{
    call, fetch_tree, for_each_cpu_time, for_each_net_socket, for_each_process, memory_pressure,
    memory_pressure_band, net_stack_defence, ramzip_stats, walk_pages, CallError, CpuTotals,
    ListError, Transport, WalkStep,
};

use crate::schedule::{Cadence, SAMPLE_PERIOD_NS};

/// The busiest task observed over the last sample interval, before its
/// display name has been validated against the wire's bounded-text rules
/// ([`crate::derive::derive_summary`] performs that validation).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopTask {
    /// The process's raw name bytes, exactly as `sysinfod` reported them
    /// (at most [`tairix_abi::sysinfo::PROCESS_NAME_MAX`] bytes; may not be
    /// valid UTF-8 or may contain control characters — [`crate::derive`] is
    /// what turns this into a wire-valid name, or drops it honestly).
    pub name: Vec<u8>,
    /// Its CPU share over the sample interval, in permille (`0..=1000`).
    pub cpu_permille: u16,
}

/// One process observed in a sample, carrying enough of its state for the
/// live panel's task and recovery rows ([`crate::model`]).
///
/// `pid` is [`ProcessRecord::pid`] — the scheduler task id — which is the
/// same numeric identity the desktop session's [`SeatReport`] and the
/// window/command mailbox rendezvous address a process by
/// ([`tairix_abi::switchboard_ipc`]); it is display/convenience only and is
/// reused across process lifetimes, so it is never used as a map key across
/// samples (that is [`proc_id`](Self::proc_id)'s job).
///
/// [`SeatReport`]: tairix_abi::switchboard_ipc::SeatReport
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSummary {
    /// The scheduler task id.
    pub pid: u64,
    /// The stable, never-reused process-instance identity.
    pub proc_id: ProcId,
    /// The process's raw name bytes, exactly as `sysinfod` reported them.
    pub name: Vec<u8>,
    /// Lifecycle state.
    pub state: ProcessState,
    /// The owning user id, kernel-attested through the record.
    ///
    /// The pressure and activity actions compare it against the service's
    /// own uid so a control renders denied exactly where the kernel's
    /// same-principal rule would refuse it.
    pub uid: u32,
    /// The CPU the scheduler last dispatched this task on, so a busy core in
    /// the CPU pane can be traced to the task sitting on it.
    pub cpu: u8,
    /// Bytes of memory currently mapped in the process's address space
    /// ([`ProcessRecord::mem_bytes`]) — what names the memory-pressure
    /// culprit.
    pub mem_bytes: u64,
    /// The process's time-shared scheduling service level, read from the
    /// scheduler's own record — what lets an already-lowered process render
    /// its "lower priority" action spent instead of re-offering it.
    pub priority: SchedPriority,
    /// Its CPU share over the sample interval, in permille. `None` on the
    /// very first sample or an unmeasurable interval, exactly like
    /// [`TopTask::cpu_permille`] for the busiest task.
    pub cpu_permille: Option<u16>,
    /// Bytes this process has read through the storage path since it
    /// started ([`ProcessRecord::io_bytes_read`]).
    ///
    /// Carried because the process record already measures it: dropping a
    /// figure the service has already paid to produce would leave the
    /// surface unable to name the process behind a busy disk.
    pub io_bytes_read: u64,
    /// Bytes this process has written through the storage path since it
    /// started ([`ProcessRecord::io_bytes_written`]).
    pub io_bytes_written: u64,
}

/// A memory-pressure reading, carried forward between samples since the
/// audited query is issued only on the [`Cadence::Memory`] cadence.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemoryPressureSample {
    /// The current pressure band depth (`0` = normal).
    pub band: u8,
    /// The honest used-memory fraction at the sample, in permille:
    /// `(total_bytes - free_bytes) * 1000 / total_bytes`, clamped to
    /// `1000`, or `0` when `total_bytes` is zero.
    pub used_permille: u16,
    /// The machine's total usable memory in bytes, as the same reading
    /// reported it — what a per-process `mem_bytes` is a fraction *of* in
    /// the pressure card's cause text.
    pub total_bytes: u64,
}

/// Which kind of measurement degraded to its honest empty value this
/// sample — used to log a one-time stderr notice per field kind rather
/// than spamming one on every subsequent failure of the same kind.
///
/// A field kind is listed here only when the query for it was actually
/// *issued* and could not be answered: a reading the granted ceiling never
/// permitted is not a degradation to report, it is a standing absence the
/// sample states through [`Sample::absence`].
///
/// Ordered so the sampler can hold the set of kinds it has already
/// reported in a [`BTreeSet`], rather than one bool per kind.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DegradedField {
    /// The process list could not be read (denied, or a transport
    /// failure).
    ProcessList,
    /// The aggregate CPU-time totals could not be read.
    CpuTime,
    /// The memory-pressure query could not be read.
    MemoryPressure,
    /// The system identity (hostname, machine id, version) could not be
    /// read.
    Identity,
    /// The uptime and boot instant could not be read.
    Uptime,
    /// The load average could not be read.
    LoadAverage,
    /// The per-CPU inventory (core class, model, frequency) could not be
    /// read.
    CpuInfo,
    /// The per-CPU scheduler load could not be read.
    CpuLoad,
    /// The kernel's own memory accounting could not be read.
    KernelMemory,
    /// The machine's installed-memory total could not be read.
    MemoryTotal,
    /// The mount table could not be read.
    Mounts,
    /// Per-volume I/O health could not be read.
    VolumeHealth,
    /// Per-volume cumulative I/O service counters could not be read.
    VolumeIoStats,
    /// Per-volume queue occupancy could not be read.
    VolumeIoQueue,
    /// Per-graphics-device statistics could not be read.
    GpuDeviceStats,
    /// The network interface inventory could not be read.
    NetInterfaceFacts,
    /// Live network interface link/address state could not be read.
    NetInterfaceState,
    /// Network interface throughput rates could not be read.
    NetInterfaceRates,
    /// The seat list could not be read.
    Seats,
    /// The caller's effective resource limits could not be read.
    ResourceLimits,
    /// The crash-record list could not be read.
    CrashRecords,
    /// The ungated memory-pressure band could not be read.
    MemoryPressureBand,
    /// The reclaimable-memory ledger could not be read.
    ReclaimStats,
    /// The compressed-memory tier's statistics could not be read.
    RamzipStats,
    /// The bounded-cache ledger could not be read.
    CacheLedgers,
    /// Per-interface cumulative counters could not be read.
    NetInterfaceCounters,
    /// The socket table could not be read.
    NetSockets,
    /// The configured resolver servers could not be read.
    NetResolverServers,
    /// The configured time servers could not be read.
    NetTimeServers,
    /// The network stack's connection-defence counters could not be read.
    NetStackDefence,
    /// The hardware tree could not be read.
    HardwareTree,
}

/// Why a reading is absent from a [`Sample`].
///
/// The surface must say the honest thing about a missing figure, and "you
/// are not permitted to see this" is a different statement from "the system
/// could not tell me": the first is a property of this session's authority
/// that will not change while the process runs, the second is a fault worth
/// showing as one. [`Sample::absence`] answers which.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Absence {
    /// The capability gating the reading is outside this process's granted
    /// ceiling, so the query was never issued — and never will be, since a
    /// capability set is fixed at spawn.
    NotPermitted,
    /// The reading is permitted but not available: the query was issued and
    /// the service could not answer it, or its cadence has not yet produced
    /// a first successful reading.
    Unavailable,
}

/// Reply-size budget one page of a paged reading may occupy, in bytes.
///
/// A page exists to bound a single reply, so the page *record count* is
/// derived from this budget and the record's own wire size ([`page_for`])
/// rather than hand-picked per query: a large record simply pages more
/// often. Four kibibytes is comfortably under the reply sizes the existing
/// sysinfo walks already produce (sixty-four process records exceed seven),
/// so no reading trades a bounded reply for extra round trips it does not
/// need.
const PAGE_BYTES: usize = 4096;

/// Most records one page may request, whatever the record size, so a tiny
/// record does not turn one page into thousands of records the caller must
/// hold at once. It matches the page size the shared sysinfo walks use.
const PAGE_RECORDS_MAX: u16 = 64;

/// Records the sampler retains from the per-CPU readings (the CPU
/// inventory and the per-CPU scheduler load).
///
/// A machine's CPU count is real hardware: the largest systems TAIRiX
/// targets are in the hundreds of cores, so 512 accepts every plausible
/// machine while refusing to grow without limit for a service that claims
/// an implausible one.
const CPU_RECORD_CAP: usize = 512;

/// Records the sampler retains from the per-volume readings (the mount
/// table and per-volume I/O health).
///
/// A few hundred mounted volumes is already far past a desktop's handful or
/// a server's disk shelves, and the panel names only the volumes a user can
/// act on.
const VOLUME_RECORD_CAP: usize = 256;

/// Records the sampler retains from the per-graphics-device reading.
///
/// A machine has one display path per seat, and a seat is a physical place a
/// person sits: eight covers every multi-head and multi-seat machine this
/// desktop is built for, while refusing to grow for a service claiming a
/// device count no hardware has.
const GPU_RECORD_CAP: usize = 8;

/// Records the sampler retains from each of the three network interface
/// readings.
///
/// Sixty-four covers a heavily virtualised host's physical and virtual
/// interfaces together; a machine does not have thousands, and a service
/// claiming so is not describing hardware.
const NET_INTERFACE_CAP: usize = 64;

/// Reclaim-ledger rows the sampler retains, across the kernel's own reclaim
/// classes and every process-reported bounded cache.
///
/// A bounded cache is a declared thing, not an unbounded list: the reclaim
/// model itself caps what one process may report, so a few hundred rows
/// covers every cache a busy desktop declares while bounding what a service
/// claiming more can make one sample allocate.
const CACHE_ROW_CAP: usize = 256;

/// Socket records the sampler walks.
///
/// The interface pane states *how many* sockets are established and
/// listening, never which, so this bounds the walk that counts them: a
/// machine serving more connections than this reports the count it read,
/// which is exactly what the sampler saw.
const SOCKET_RECORD_CAP: usize = 4096;

/// Configured resolver or time servers the sampler retains.
///
/// A stub resolver and an NTP client each consult a handful; a list longer
/// than this is not a configuration a reader is looking at.
const SERVER_RECORD_CAP: usize = 16;

/// Hardware-tree nodes the sampler retains.
///
/// One node per detected bus or device function, so a densely populated
/// server with several root complexes and their whole PCIe fan-out stays
/// well inside this while a service claiming more cannot page without limit.
const HW_NODE_CAP: usize = 1024;

/// Seat records the sampler retains.
///
/// A seat is a physical workstation position at the machine (its
/// keyboard/pointer/display grouping), so even a shared multi-seat system
/// has a handful.
const SEAT_CAP: usize = 16;

/// Process records the sampler reads per sample.
///
/// A busy multi-user machine really does run thousands of tasks, so this
/// accepts the whole task table of any plausible system while bounding what
/// one sample can allocate. It bounds the *walk*, not merely what is kept:
/// on reaching the cap the sampler ends the page walk, so a service that
/// answers full pages indefinitely cannot make one sample page without
/// limit. Everything the sample then says about processes — the summaries,
/// the busiest task, and the stopped-task count — describes the first
/// [`PROCESS_RECORD_CAP`] the service reported, which is exactly what the
/// sampler read.
const PROCESS_RECORD_CAP: usize = 4096;

/// Crash records the sampler retains.
///
/// [`CrashRecord`] is by far the largest record the sampler decodes — it
/// carries a backtrace and the faulting register anchors — and the surface
/// shows only the most recent handful, so this bounds the whole reading to
/// tens of kilobytes rather than however many faults the service has kept.
const CRASH_RECORD_CAP: usize = 32;

/// How many records of `record_len` bytes one page requests: as many as fit
/// [`PAGE_BYTES`], never fewer than one nor more than [`PAGE_RECORDS_MAX`].
///
/// A record larger than the whole budget still pages one at a time rather
/// than requesting zero records, which would page forever.
fn page_for(record_len: usize) -> u16 {
    if record_len == 0 {
        return 1;
    }
    let fits = PAGE_BYTES / record_len;
    u16::try_from(fits)
        .unwrap_or(PAGE_RECORDS_MAX)
        .clamp(1, PAGE_RECORDS_MAX)
}

/// One core's busy share over the sample interval.
///
/// `None` for a core first seen this sample: a cumulative total is not a
/// share, and reporting one would plot the whole of boot as this interval's
/// reading.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CoreBusy {
    /// The CPU index this share belongs to.
    pub cpu: u32,
    /// Its busy share, in permille.
    pub permille: Option<u16>,
}

/// How many sockets the stack holds, by the two states a reader asks about.
///
/// The interface pane states counts, never a socket list, so the walk that
/// reads the table folds it into these two totals rather than retaining
/// thousands of records nothing would draw.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SocketCensus {
    /// Sockets with an established connection.
    pub established: u64,
    /// Sockets accepting connections.
    pub listening: u64,
}

/// One sample of the live system, gathered by [`Sampler::sample`].
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct Sample {
    /// Count of processes observed in the [`ProcessState::Stopped`] state
    /// (recovery candidates).
    pub stopped_count: u16,
    /// Each core's own busy share over the sample interval, in CPU-index
    /// order. Empty when the per-CPU accounting could not be read.
    pub core_busy: Vec<CoreBusy>,
    /// Overall CPU busy fraction, in permille. `None` when the aggregate
    /// CPU-time query failed or reported no CPUs.
    pub cpu_busy_permille: Option<u16>,
    /// The task with the highest CPU-time delta since the previous sample.
    /// `None` on the very first sample (nothing to delta against), when
    /// the process list could not be read, or when the interval since the
    /// previous sample is unmeasurable.
    pub top_task: Option<TopTask>,
    /// The most recently known memory-pressure reading, carried forward
    /// between the sparser memory-pressure queries. `None` when the
    /// capability was never granted or no attempt has yet succeeded.
    pub memory_pressure: Option<MemoryPressureSample>,
    /// The whole process list observed this sample, in the order the
    /// System Information API reported it. Empty when the process list
    /// could not be read this sample (the same honest-empty rule as
    /// [`Self::stopped_count`] and [`Self::top_task`]).
    pub processes: Vec<ProcessSummary>,
    /// Field kinds that degraded to their honest empty value *for the
    /// first time* this sample (for a one-time stderr notice).
    pub degradations: Vec<DegradedField>,
    /// The machine's identity: hostname, machine id, and OS version.
    ///
    /// Read once ([`Cadence::Static`]) — a hostname change is a
    /// re-provisioning, not a live figure. `None` means the reading is
    /// absent for the reason [`Self::absence`] gives.
    pub identity: Option<SystemIdentity>,
    /// How long the system has been up, with the boot instant it is
    /// measured from ([`Cadence::EverySample`], since it advances between
    /// every sample).
    pub uptime: Option<Uptime>,
    /// The fixed-point 1/5/15-minute load averages with the runnable and
    /// total task counts and the logged-in user count
    /// ([`Cadence::EverySample`]).
    pub load_average: Option<LoadAverage>,
    /// The per-CPU inventory: each core's class, model name, and measured
    /// frequency.
    ///
    /// Read on every sample ([`Cadence::EverySample`]) because
    /// `current_freq_hz` is a live reading and a clock a reader watches move
    /// is the point of the field; the immutable part (model, class, feature
    /// bits, reference clock) is re-read with it because the query is not
    /// field-selective. `Some(empty)` is a service that reported no CPUs;
    /// `None` is an absent reading (see [`Self::absence`]) — the distinction
    /// the surface needs to avoid showing "no CPUs" for "may not look".
    pub cpu_info: Option<Vec<CpuInfoRecord>>,
    /// Per-CPU scheduler load ([`Cadence::EverySample`]), which needs the
    /// kernel-statistics scope; absent without it.
    pub cpu_load: Option<Vec<CpuLoadRecord>>,
    /// The kernel's own memory accounting, on the audited
    /// [`Cadence::Memory`] cadence and needing the kernel-statistics
    /// scope.
    pub kernel_memory: Option<KernelMemoryStats>,
    /// The machine's installed and usable memory totals
    /// ([`Cadence::Static`] — installed RAM does not change under a
    /// running kernel).
    pub memory_total: Option<MemoryTotal>,
    /// The mount table, each entry carrying its volume's real capacity
    /// (block size and total/free/available blocks) — the storage figures
    /// the panel would otherwise have to guess at.
    ///
    /// [`Cadence::Inventory`]: a volume is mounted or removed on an event,
    /// not from moment to moment.
    pub mounts: Option<Vec<MountRecord>>,
    /// Per-volume I/O health ([`Cadence::Inventory`], kernel-statistics
    /// scope): the fault/latency counters that say a disk is failing.
    pub volume_health: Option<Vec<VolumeIoHealthRecord>>,
    /// Per-volume cumulative I/O service counters ([`Cadence::EverySample`],
    /// ungated): bytes, completed requests, device-busy time and summed
    /// waits, from which the pane derives throughput, IOPS, utilisation and
    /// await over its own interval.
    pub volume_io_stats: Option<Vec<VolumeIoStatsRecord>>,
    /// Per-volume queue occupancy and the budget bounding it
    /// ([`Cadence::EverySample`], kernel-statistics scope).
    pub volume_io_queue: Option<Vec<VolumeIoQueueRecord>>,
    /// Per-graphics-device statistics ([`Cadence::EverySample`], hardware
    /// scope): the occupancy the display service measured over its own
    /// present path, the memory the driver reports the device owns, what its
    /// compositor can do, and the mode it scans out. Cumulative, so the
    /// pane's utilisation is a delta over its own interval.
    pub gpu_stats: Option<Vec<DisplayStats>>,
    /// The network interface inventory — the interfaces that exist and
    /// their fixed properties ([`Cadence::Static`], hardware scope).
    pub net_facts: Option<Vec<NetInterfaceFactsRecord>>,
    /// Live per-interface link and address state
    /// ([`Cadence::EverySample`], global scope).
    pub net_state: Option<Vec<NetInterfaceStateRecord>>,
    /// Per-interface throughput averaged over the sample period
    /// ([`Cadence::EverySample`], global scope). Each record states the
    /// window actually used, which may be shorter than requested when an
    /// interface's history is younger.
    pub net_rates: Option<Vec<NetInterfaceRatesRecord>>,
    /// The seats configured at this machine ([`Cadence::Inventory`],
    /// hardware scope).
    pub seats: Option<Vec<SeatRecord>>,
    /// This process's own effective resource limits and live usage, one
    /// record per limit kind in discriminant order
    /// ([`Cadence::Inventory`] — a limit changes on an administrative
    /// act).
    pub resource_limits: Option<Vec<ResourceLimitRecord>>,
    /// The ungated memory-pressure band ([`Cadence::EverySample`]), which a
    /// session without the kernel-statistics scope can still read — so the
    /// pressure banner works on an unprivileged ceiling.
    pub pressure_band: Option<MemoryPressureBand>,
    /// The kernel's reclaimable-memory ledger, one row per reclaim class
    /// ([`Cadence::Memory`], kernel-statistics scope).
    pub reclaim: Option<Vec<ReclaimClassRecord>>,
    /// The compressed-memory tier's statistics ([`Cadence::Memory`],
    /// kernel-statistics scope).
    pub ramzip: Option<RamzipStats>,
    /// Every declared bounded cache and what it currently holds
    /// ([`Cadence::Memory`], kernel-statistics scope).
    pub cache_ledgers: Option<Vec<CacheLedgerRecord>>,
    /// Per-interface cumulative counters ([`Cadence::EverySample`], global
    /// scope).
    pub net_counters: Option<Vec<NetInterfaceCountersRecord>>,
    /// How many sockets are established and listening
    /// ([`Cadence::EverySample`], global scope).
    pub sockets: Option<SocketCensus>,
    /// The configured resolver servers ([`Cadence::Inventory`]).
    pub resolver_servers: Option<Vec<NetServerAddr>>,
    /// The configured time servers ([`Cadence::Inventory`]).
    pub time_servers: Option<Vec<NetServerAddr>>,
    /// The stack's connection-defence counters ([`Cadence::EverySample`],
    /// global scope).
    pub stack_defence: Option<NetStackDefenceCounters>,
    /// The discovered hardware tree ([`Cadence::Inventory`], hardware
    /// scope), which names the graphics device the display path runs on.
    pub hardware: Option<Vec<HwNode>>,
    /// Recent user-fault crash records ([`Cadence::Inventory`],
    /// kernel-statistics scope). `Some(empty)` is the healthy system that
    /// has crashed nothing — which is why this is not a bare [`Vec`].
    pub crashes: Option<Vec<CrashRecord>>,
    /// The optional scopes this process's ceiling resolved to at startup.
    ///
    /// Carried on the sample so the surface can explain an absent reading
    /// without re-probing (or guessing): [`Self::absence`] reads it.
    pub scopes: ScopeVerdicts,
    /// The monotonic nanoseconds actually elapsed since the previous
    /// sample, or `None` on the very first one (nothing to measure from).
    ///
    /// Carried because a consumer deriving a *rate* from two successive
    /// counter readings needs the interval those readings actually span,
    /// which is not the nominal sample period: a cycle delayed by input,
    /// a command, or a busy machine spans longer, and dividing by the
    /// nominal period would report a rate the disk never sustained. This
    /// is the same interval the per-process CPU share is measured over,
    /// so a row's CPU and disk figures describe one window.
    pub elapsed_ns: Option<u64>,
}

impl Sample {
    /// Why `field`'s reading is absent — the honest statement a surface
    /// makes instead of leaving a blank.
    ///
    /// [`Absence::NotPermitted`] when the capability gating that reading is
    /// outside this process's ceiling: the query was never issued, and the
    /// figure will not appear later in this session.
    /// [`Absence::Unavailable`] otherwise: the reading is permitted, so its
    /// absence means the service could not answer (this sample, or the last
    /// time its cadence came round). Callers ask this only about a field
    /// they have already found absent; a permitted, present reading is not
    /// an absence to explain.
    #[must_use]
    pub fn absence(&self, field: DegradedField) -> Absence {
        if self.scopes.permits(field) {
            Absence::Unavailable
        } else {
            Absence::NotPermitted
        }
    }
}

/// Which optional System Information API scopes this Switchboard
/// instance's ceiling grants, established once at startup
/// ([`probe_scopes`]) and held for the process's life.
///
/// One verdict per *capability*, not per query: the readings that share a
/// capability share its verdict, so a single probe settles every reading it
/// gates. [`Self::permits`] maps a reading onto the verdict that governs
/// it. The [`Default`] is the fail-closed reading — no optional scope
/// granted — so a sample that was never given real verdicts claims no
/// authority it cannot show.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ScopeVerdicts {
    /// Whether the system-wide process list
    /// ([`SysinfoQueryId::GLOBAL_PROCESS_LIST`]) and the other global-scope
    /// readings (live network interface state and throughput) are available
    /// (`CAP_SYSINFO_GLOBAL` granted).
    pub global_process_scope: bool,
    /// Whether the memory-pressure gauge
    /// ([`SysinfoQueryId::MEMORY_PRESSURE`]) and the other kernel-internal
    /// readings that share its capability (per-CPU scheduler load, kernel
    /// memory accounting, per-volume I/O health, crash records) are
    /// available (`CAP_SYSINFO_KERNEL` granted).
    pub memory_pressure: bool,
    /// Whether the hardware-inventory readings (the network interface
    /// inventory and the seat list) are available (`CAP_SYSINFO_HW`
    /// granted).
    pub hardware_scope: bool,
}

impl ScopeVerdicts {
    /// Whether this ceiling permits `field`'s reading at all.
    ///
    /// The ungated readings — the process list in its self scope, CPU time,
    /// identity, uptime, load average, the CPU inventory, installed-memory
    /// totals, the mount table, per-volume service counters, and this
    /// process's own resource limits — are always permitted, so their
    /// absence is always a failure to report rather than a refusal.
    #[must_use]
    pub const fn permits(self, field: DegradedField) -> bool {
        match field {
            DegradedField::ProcessList
            | DegradedField::CpuTime
            | DegradedField::Identity
            | DegradedField::Uptime
            | DegradedField::LoadAverage
            | DegradedField::CpuInfo
            | DegradedField::MemoryTotal
            | DegradedField::MemoryPressureBand
            | DegradedField::Mounts
            | DegradedField::VolumeIoStats
            | DegradedField::NetResolverServers
            | DegradedField::NetTimeServers
            | DegradedField::ResourceLimits => true,
            DegradedField::MemoryPressure
            | DegradedField::CpuLoad
            | DegradedField::KernelMemory
            | DegradedField::ReclaimStats
            | DegradedField::RamzipStats
            | DegradedField::CacheLedgers
            | DegradedField::VolumeHealth
            | DegradedField::VolumeIoQueue
            | DegradedField::CrashRecords => self.memory_pressure,
            DegradedField::NetInterfaceState
            | DegradedField::NetInterfaceRates
            | DegradedField::NetInterfaceCounters
            | DegradedField::NetSockets
            | DegradedField::NetStackDefence => self.global_process_scope,
            DegradedField::NetInterfaceFacts
            | DegradedField::HardwareTree
            | DegradedField::GpuDeviceStats
            | DegradedField::Seats => self.hardware_scope,
        }
    }
}

/// Probe, once, whether the granted ceiling includes the global process
/// scope, the memory-pressure gauge, and the hardware inventory.
///
/// Issued exactly once, at startup: repeating a denied, audited query
/// (memory pressure) every sample would spam the audit log for an answer
/// that cannot change mid-process — capability sets are fixed at spawn. A
/// verdict of "granted" on any outcome other than an explicit
/// [`CallError::PermissionDenied`] is deliberate: a transient service
/// failure at probe time does not condemn a field to its degraded form for
/// the rest of the process's life — a later real failure degrades that one
/// sample honestly instead.
///
/// Three probes settle all three scopes because a verdict belongs to a
/// capability rather than a query: each probe asks for a single record of
/// the cheapest reading its capability gates.
#[must_use]
pub fn probe_scopes(transport: &dyn Transport) -> ScopeVerdicts {
    let probe_payload = ProcessListRequest {
        offset: 0,
        limit: 1,
        flags: 0,
    }
    .to_le_bytes();
    let global_process_scope = !matches!(
        call(
            transport,
            SysinfoQueryId::GLOBAL_PROCESS_LIST,
            &probe_payload,
        ),
        Err(CallError::PermissionDenied)
    );
    let memory_pressure_scope =
        !matches!(memory_pressure(transport), Err(CallError::PermissionDenied));
    let hardware_scope = !matches!(
        call(
            transport,
            SysinfoQueryId::NET_INTERFACE_FACTS,
            &page_payload(0, 1),
        ),
        Err(CallError::PermissionDenied)
    );
    ScopeVerdicts {
        global_process_scope,
        memory_pressure: memory_pressure_scope,
        hardware_scope,
    }
}

/// Encode the `{offset, limit, reserved}` paging header every paged reading
/// the sampler issues shares.
///
/// `sysinfo-v1` gives the mount, CPU-info, CPU-load, volume-health, seat,
/// crash-record and network-interface list queries the identical eight-byte
/// paging payload (each type's own documentation says so), so it is spelled
/// once here rather than once per query; the assertion below pins that
/// agreement at compile time, and the rates request is the one that differs
/// — it appends the averaging window and encodes itself.
fn page_payload(offset: u32, limit: u16) -> Vec<u8> {
    MountListRequest {
        offset,
        limit,
        flags: 0,
    }
    .to_le_bytes()
    .to_vec()
}

const _: () = assert!(
    MountListRequest::WIRE_LEN == CpuInfoListRequest::WIRE_LEN
        && MountListRequest::WIRE_LEN == CpuLoadRequest::WIRE_LEN
        && MountListRequest::WIRE_LEN == VolumeIoRequest::WIRE_LEN
        && MountListRequest::WIRE_LEN == DeviceStatsRequest::WIRE_LEN
        && MountListRequest::WIRE_LEN == SeatListRequest::WIRE_LEN
        && MountListRequest::WIRE_LEN == CrashRecordRequest::WIRE_LEN
        && MountListRequest::WIRE_LEN == NetInterfaceListRequest::WIRE_LEN,
    "the paged sysinfo list queries must share one paging-header layout",
);

/// The busy share of `delta_ns` over `interval_ns`, in permille
/// (`0..=1000`), or `None` when `interval_ns` is zero (an unmeasurable
/// interval — the honest absence, never a fabricated rate).
pub(crate) fn permille_of(delta_ns: u64, interval_ns: u64) -> Option<u16> {
    if interval_ns == 0 {
        return None;
    }
    let permille = (u128::from(delta_ns) * 1000 / u128::from(interval_ns)).min(1000);
    Some(u16::try_from(permille).unwrap_or(1000))
}

/// The honest used-memory fraction, in permille, given the reported totals.
fn used_permille(total_bytes: u64, free_bytes: u64) -> u16 {
    if total_bytes == 0 {
        return 0;
    }
    let used = total_bytes.saturating_sub(free_bytes);
    let permille = (u128::from(used) * 1000 / u128::from(total_bytes)).min(1000);
    u16::try_from(permille).unwrap_or(1000)
}

/// One paged reading's fixed description: which query answers it, which
/// field kind to blame when it cannot be read, the wire size of one record,
/// and how many records the sampler will retain.
///
/// Bundled into one value so the shared bounded walk
/// ([`Sampler::read_paged`]) takes a reading's whole policy as a single
/// argument instead of a long, order-sensitive parameter list.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct PagedRead {
    query: SysinfoQueryId,
    field: DegradedField,
    record_len: usize,
    cap: usize,
}

/// Gathers successive [`Sample`]s of the live system, tracking the prior
/// per-process CPU time, the prior aggregate CPU totals, the
/// carried-forward memory-pressure reading needed to compute each sample's
/// deltas, and the cached static and slow-moving readings their cadences
/// carry between samples.
#[derive(Debug)]
pub struct Sampler {
    scopes: ScopeVerdicts,
    prev_proc_times: BTreeMap<ProcId, u64>,
    prev_core_times: BTreeMap<u32, CpuTotals>,
    prev_sample_ns: Option<u64>,
    prev_totals: Option<CpuTotals>,
    last_memory: Option<MemoryPressureSample>,
    sample_index: u64,
    /// The field kinds whose degradation has already been reported, so a
    /// persistent failure states itself once rather than every sample. One
    /// set rather than a bool per kind, since there are eighteen kinds.
    warned: BTreeSet<DegradedField>,
    identity: Option<SystemIdentity>,
    memory_total: Option<MemoryTotal>,
    net_facts: Option<Vec<NetInterfaceFactsRecord>>,
    kernel_memory: Option<KernelMemoryStats>,
    mounts: Option<Vec<MountRecord>>,
    volume_health: Option<Vec<VolumeIoHealthRecord>>,
    volume_io_stats: Option<Vec<VolumeIoStatsRecord>>,
    volume_io_queue: Option<Vec<VolumeIoQueueRecord>>,
    gpu_stats: Option<Vec<DisplayStats>>,
    seats: Option<Vec<SeatRecord>>,
    resource_limits: Option<Vec<ResourceLimitRecord>>,
    crashes: Option<Vec<CrashRecord>>,
    reclaim: Option<Vec<ReclaimClassRecord>>,
    ramzip: Option<RamzipStats>,
    cache_ledgers: Option<Vec<CacheLedgerRecord>>,
    resolver_servers: Option<Vec<NetServerAddr>>,
    time_servers: Option<Vec<NetServerAddr>>,
    hardware: Option<Vec<HwNode>>,
}

impl Sampler {
    /// Build a sampler with no prior state, remembering `scopes` (probed
    /// once by [`probe_scopes`]) for the life of the process.
    #[must_use]
    pub fn new(scopes: ScopeVerdicts) -> Self {
        Self {
            scopes,
            prev_proc_times: BTreeMap::new(),
            prev_core_times: BTreeMap::new(),
            prev_sample_ns: None,
            prev_totals: None,
            last_memory: None,
            sample_index: 0,
            warned: BTreeSet::new(),
            identity: None,
            memory_total: None,
            net_facts: None,
            kernel_memory: None,
            mounts: None,
            volume_health: None,
            volume_io_stats: None,
            volume_io_queue: None,
            gpu_stats: None,
            seats: None,
            resource_limits: None,
            crashes: None,
            reclaim: None,
            ramzip: None,
            cache_ledgers: None,
            resolver_servers: None,
            time_servers: None,
            hardware: None,
        }
    }

    /// Gather one [`Sample`] of the live system through `transport`.
    ///
    /// `now_ns` is the caller's monotonic clock reading for this sample,
    /// used only to measure the actual elapsed interval since the previous
    /// call (never assumed to be exactly the nominal sample period).
    ///
    /// Which queries this actually issues depends on the sample index and
    /// the granted scopes: a reading is issued only when its [`Cadence`]
    /// says it is due and its capability permits it, so a steady sample
    /// costs the fast readings alone. Everything slower is carried forward
    /// from the last time it was read.
    pub fn sample(&mut self, transport: &dyn Transport, now_ns: u64) -> Sample {
        let elapsed_ns = self.prev_sample_ns.map(|prev| now_ns.saturating_sub(prev));
        let mut degradations = Vec::new();

        let (stopped_count, top_task, processes) =
            self.sample_processes(transport, elapsed_ns, &mut degradations);
        let (cpu_busy_permille, core_busy) = self.sample_cpu_totals(transport, &mut degradations);
        self.sample_memory_pressure(transport, &mut degradations);
        let uptime = self.read_uptime(transport, &mut degradations);
        let load_average = self.read_load_average(transport, &mut degradations);
        let cpu_info = self.read_cpu_info(transport, &mut degradations);
        let cpu_load = self.read_cpu_load(transport, &mut degradations);
        let pressure_band = self.read_pressure_band(transport, &mut degradations);
        let net_state = self.read_net_state(transport, &mut degradations);
        let net_rates = self.read_net_rates(transport, &mut degradations);
        let net_counters = self.read_net_counters(transport, &mut degradations);
        let sockets = self.read_sockets(transport, &mut degradations);
        let stack_defence = self.read_stack_defence(transport, &mut degradations);
        self.refresh_cached(transport, &mut degradations);

        self.prev_sample_ns = Some(now_ns);
        self.sample_index = self.sample_index.wrapping_add(1);

        Sample {
            stopped_count,
            cpu_busy_permille,
            core_busy,
            top_task,
            memory_pressure: self.last_memory,
            processes,
            degradations,
            identity: self.identity,
            uptime,
            load_average,
            cpu_info,
            cpu_load,
            kernel_memory: self.kernel_memory,
            memory_total: self.memory_total,
            mounts: self.mounts.clone(),
            volume_health: self.volume_health.clone(),
            volume_io_stats: self.volume_io_stats.clone(),
            volume_io_queue: self.volume_io_queue.clone(),
            gpu_stats: self.gpu_stats.clone(),
            net_facts: self.net_facts.clone(),
            net_state,
            net_rates,
            seats: self.seats.clone(),
            resource_limits: self.resource_limits.clone(),
            crashes: self.crashes.clone(),
            pressure_band,
            reclaim: self.reclaim.clone(),
            ramzip: self.ramzip,
            cache_ledgers: self.cache_ledgers.clone(),
            net_counters,
            sockets,
            resolver_servers: self.resolver_servers.clone(),
            time_servers: self.time_servers.clone(),
            stack_defence,
            hardware: self.hardware.clone(),
            scopes: self.scopes,
            elapsed_ns,
        }
    }

    /// Walk the process list, counting stopped processes, picking the task
    /// with the highest CPU-time delta since the previous sample, and
    /// building each process's [`ProcessSummary`] for the live panel.
    fn sample_processes(
        &mut self,
        transport: &dyn Transport,
        elapsed_ns: Option<u64>,
        degradations: &mut Vec<DegradedField>,
    ) -> (u16, Option<TopTask>, Vec<ProcessSummary>) {
        let mut stopped_count: u16 = 0;
        let mut records: Vec<ProcessRecord> = Vec::new();
        let outcome = for_each_process(transport, self.scopes.global_process_scope, |record| {
            if record.state == ProcessState::Stopped {
                stopped_count = stopped_count.saturating_add(1);
            }
            records.push(*record);
            if records.len() >= PROCESS_RECORD_CAP {
                return Ok(WalkStep::Stop);
            }
            Ok(WalkStep::Continue)
        });

        if outcome.is_err() {
            self.note_failure(DegradedField::ProcessList, degradations);
            // Prior-sample state is left untouched: a transient failure
            // must not erase history a later successful sample could still
            // use to compute an honest delta.
            return (0, None, Vec::new());
        }
        self.note_success(DegradedField::ProcessList);

        // Keyed on the stable, never-reused `proc_id`, so a numeric-pid
        // reuse across two process lifetimes can never be mistaken for one
        // continuously-running task. A process not seen in the previous
        // sample (first sight) contributes an honest zero delta rather than
        // a fabricated rate over an interval it was never observed across.
        let mut current = BTreeMap::new();
        let mut top: Option<(usize, u64)> = None;
        let mut deltas: Vec<u64> = Vec::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            let prev_time = self.prev_proc_times.get(&record.proc_id).copied();
            current.insert(record.proc_id, record.cpu_time_ns);
            let delta = prev_time.map_or(0, |prev| record.cpu_time_ns.saturating_sub(prev));
            deltas.push(delta);
            let is_new_best = match top {
                Some((_, best_delta)) => delta > best_delta,
                None => true,
            };
            if is_new_best {
                top = Some((index, delta));
            }
        }
        self.prev_proc_times = current;

        let top_task = match elapsed_ns {
            // No prior sample time to delta against: honestly no top task,
            // never one measured over a fabricated interval.
            None => None,
            Some(interval) => top.and_then(|(index, delta)| {
                let record = &records[index];
                let cpu_permille = permille_of(delta, interval)?;
                Some(TopTask {
                    name: record.name_bytes().to_vec(),
                    cpu_permille,
                })
            }),
        };

        let processes = records
            .iter()
            .zip(deltas)
            .map(|(record, delta)| ProcessSummary {
                pid: record.pid,
                proc_id: record.proc_id,
                name: record.name_bytes().to_vec(),
                state: record.state,
                uid: record.uid,
                cpu: record.cpu,
                mem_bytes: record.mem_bytes,
                priority: record.priority,
                cpu_permille: elapsed_ns.and_then(|interval| permille_of(delta, interval)),
                io_bytes_read: record.io_bytes_read,
                io_bytes_written: record.io_bytes_written,
            })
            .collect();

        (stopped_count, top_task, processes)
    }

    /// Fetch the aggregate CPU-time totals and derive the busy-fraction
    /// delta against the previous sample (an all-zero previous total on
    /// the first sample yields the honest cumulative since-boot ratio).
    fn sample_cpu_totals(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> (Option<u16>, Vec<CoreBusy>) {
        let mut totals = CpuTotals::default();
        let mut cores: Vec<CpuTimeRecord> = Vec::new();
        let outcome = for_each_cpu_time(transport, |record| {
            totals.busy_ns = totals.busy_ns.saturating_add(record.busy_ns);
            totals.idle_ns = totals.idle_ns.saturating_add(record.idle_ns);
            cores.push(*record);
            Ok(if cores.len() >= CPU_RECORD_CAP {
                WalkStep::Stop
            } else {
                WalkStep::Continue
            })
        });
        if outcome.is_err() {
            self.note_failure(DegradedField::CpuTime, degradations);
            // Prior-sample state is left untouched: a transient failure must
            // not erase totals a later sample could still delta against.
            return (None, Vec::new());
        }
        self.note_success(DegradedField::CpuTime);
        // No CPUs reported is an honest empty, not a failure to warn about,
        // and there is nothing to delta.
        if cores.is_empty() {
            return (None, Vec::new());
        }
        let per_core = self.core_busy(&cores);
        let prev = self.prev_totals.unwrap_or_default();
        self.prev_totals = Some(totals);
        (CpuTotals::busy_permille(prev, totals), per_core)
    }

    /// Each core's busy share over the interval, from the delta against its
    /// own previous reading.
    ///
    /// Keyed on the CPU index the record names rather than its position, so
    /// a service that reports its CPUs in a different order between samples
    /// cannot attribute one core's delta to another.
    fn core_busy(&mut self, cores: &[CpuTimeRecord]) -> Vec<CoreBusy> {
        let mut shares = Vec::with_capacity(cores.len());
        let mut current = BTreeMap::new();
        for record in cores {
            let now = CpuTotals {
                busy_ns: record.busy_ns,
                idle_ns: record.idle_ns,
            };
            let permille = self
                .prev_core_times
                .get(&record.cpu)
                .and_then(|prev| CpuTotals::busy_permille(*prev, now));
            current.insert(record.cpu, now);
            shares.push(CoreBusy {
                cpu: record.cpu,
                permille,
            });
        }
        self.prev_core_times = current;
        shares
    }

    /// On the memory-pressure query's own slower cadence (and only when the
    /// capability was granted), refresh the carried-forward reading.
    fn sample_memory_pressure(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) {
        // Not permitted (never granted, so never issued) or not this
        // cycle: carry forward whatever reading is already known.
        if !self.due(DegradedField::MemoryPressure, self.last_memory.is_some()) {
            return;
        }
        match memory_pressure(transport) {
            Ok(stats) => {
                self.note_success(DegradedField::MemoryPressure);
                self.last_memory = Some(MemoryPressureSample {
                    band: stats.band,
                    used_permille: used_permille(stats.total_bytes, stats.free_bytes),
                    total_bytes: stats.total_bytes,
                });
            }
            Err(_) => {
                self.note_failure(DegradedField::MemoryPressure, degradations);
                // Leave `last_memory` at its last known value: a single
                // transient failure does not erase a recent honest reading.
            }
        }
    }

    /// Whether `field`'s reading is issued on this sample: the ceiling
    /// permits it at all, and its cadence tier says a fresh read would
    /// tell the surface something it does not already hold.
    ///
    /// `cached` is whether a value for it is already held (it decides only a
    /// [`Cadence::Static`] reading, which is retried until it succeeds and
    /// then never again).
    fn due(&self, field: DegradedField, cached: bool) -> bool {
        self.scopes.permits(field) && cadence_of(field).due(self.sample_index, cached)
    }

    /// Record that `field` could not be read, reporting it to the caller
    /// only the first time since it was last read successfully.
    fn note_failure(&mut self, field: DegradedField, degradations: &mut Vec<DegradedField>) {
        if self.warned.insert(field) {
            degradations.push(field);
        }
    }

    /// Record that `field` was read, so a later failure of the same kind is
    /// news again rather than a repeat.
    fn note_success(&mut self, field: DegradedField) {
        self.warned.remove(&field);
    }

    /// Record `field`'s outcome — the pair of the two above, for a reading
    /// fetched through a `lib/procinfo` helper rather than through this
    /// module's own scalar and paged readers.
    fn note(&mut self, field: DegradedField, read: bool, degradations: &mut Vec<DegradedField>) {
        if read {
            self.note_success(field);
        } else {
            self.note_failure(field, degradations);
        }
    }

    /// Issue a fixed-size reading and decode it, blaming `field` if either
    /// the query or the decode fails.
    ///
    /// The queries reached this way take no payload: they answer one
    /// whole-system fact rather than a window of a list.
    fn read_scalar<T>(
        &mut self,
        transport: &dyn Transport,
        query: SysinfoQueryId,
        field: DegradedField,
        degradations: &mut Vec<DegradedField>,
        decode: impl Fn(&[u8]) -> Result<T, Errno>,
    ) -> Option<T> {
        // A structurally invalid reply is as unusable as no reply, and is
        // never partially believed.
        let decoded = call(transport, query, &[])
            .ok()
            .and_then(|reply| decode(&reply).ok());
        if decoded.is_some() {
            self.note_success(field);
        } else {
            self.note_failure(field, degradations);
        }
        decoded
    }

    /// Walk a paged reading to completion under its own retention cap,
    /// decoding each record and blaming `spec.field` if the walk fails.
    ///
    /// Reaching the cap is not a failure: the walk stops there, the reading
    /// keeps the first `spec.cap` records in the service's own stable order
    /// (so truncation is deterministic, never an arbitrary subset), and no
    /// further page is requested — which is what bounds the allocation a
    /// hostile or implausibly long list can provoke.
    fn read_paged<T>(
        &mut self,
        transport: &dyn Transport,
        spec: PagedRead,
        degradations: &mut Vec<DegradedField>,
        make_request: impl Fn(u32, u16) -> Vec<u8>,
        decode: impl Fn(&[u8]) -> Result<T, Errno>,
    ) -> Option<Vec<T>> {
        let page = page_for(spec.record_len);
        let mut records: Vec<T> = Vec::with_capacity(usize::from(page).min(spec.cap));
        let outcome = walk_pages(
            transport,
            spec.query,
            spec.record_len,
            page,
            make_request,
            |chunk| {
                let record =
                    decode(chunk).map_err(|errno| ListError::Call(CallError::Service(errno)))?;
                records.push(record);
                Ok(if records.len() >= spec.cap {
                    WalkStep::Stop
                } else {
                    WalkStep::Continue
                })
            },
        );
        if outcome.is_err() {
            self.note_failure(spec.field, degradations);
            return None;
        }
        self.note_success(spec.field);
        Some(records)
    }

    /// The time since boot and the instant it is measured from.
    fn read_uptime(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<Uptime> {
        if !self.due(DegradedField::Uptime, false) {
            return None;
        }
        self.read_scalar(
            transport,
            SysinfoQueryId::UPTIME,
            DegradedField::Uptime,
            degradations,
            Uptime::from_bytes,
        )
    }

    /// The 1/5/15-minute load averages with the task and user counts.
    fn read_load_average(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<LoadAverage> {
        if !self.due(DegradedField::LoadAverage, false) {
            return None;
        }
        self.read_scalar(
            transport,
            SysinfoQueryId::LOAD_AVERAGE,
            DegradedField::LoadAverage,
            degradations,
            LoadAverage::from_bytes,
        )
    }

    /// The per-CPU inventory: each core's class, model and measured clock.
    ///
    /// Read fresh every sample and never carried forward, exactly as the
    /// other per-sample readings are: the record's `current_freq_hz` is a
    /// live clock, so a retained record could only report a stale one as
    /// live.
    fn read_cpu_info(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<Vec<CpuInfoRecord>> {
        if !self.due(DegradedField::CpuInfo, false) {
            return None;
        }
        self.read_paged(
            transport,
            PagedRead {
                query: SysinfoQueryId::CPU_INFO,
                field: DegradedField::CpuInfo,
                record_len: CpuInfoRecord::WIRE_LEN,
                cap: CPU_RECORD_CAP,
            },
            degradations,
            page_payload,
            CpuInfoRecord::from_bytes,
        )
    }

    /// The ungated memory-pressure band.
    ///
    /// Read separately from the audited pressure query because it needs no
    /// capability: a session without the kernel-statistics scope still gets
    /// the band the banner is about.
    fn read_pressure_band(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<MemoryPressureBand> {
        if !self.due(DegradedField::MemoryPressureBand, false) {
            return None;
        }
        let read = memory_pressure_band(transport).ok();
        self.note(
            DegradedField::MemoryPressureBand,
            read.is_some(),
            degradations,
        );
        read
    }

    /// The stack's connection-defence counters.
    fn read_stack_defence(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<NetStackDefenceCounters> {
        if !self.due(DegradedField::NetStackDefence, false) {
            return None;
        }
        let read = net_stack_defence(transport).ok();
        self.note(DegradedField::NetStackDefence, read.is_some(), degradations);
        read
    }

    /// How many sockets are established and listening.
    ///
    /// The table is folded into two counts as it is walked: the pane states
    /// how many, never which, so retaining the records would hold thousands
    /// nothing draws.
    fn read_sockets(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<SocketCensus> {
        if !self.due(DegradedField::NetSockets, false) {
            return None;
        }
        let mut census = SocketCensus::default();
        let mut seen = 0usize;
        let outcome = for_each_net_socket(transport, |record| {
            match record.state {
                NetSockState::Established => {
                    census.established = census.established.saturating_add(1);
                }
                NetSockState::Listen => {
                    census.listening = census.listening.saturating_add(1);
                }
                _ => {}
            }
            seen = seen.saturating_add(1);
            Ok(if seen >= SOCKET_RECORD_CAP {
                WalkStep::Stop
            } else {
                WalkStep::Continue
            })
        });
        self.note(DegradedField::NetSockets, outcome.is_ok(), degradations);
        outcome.is_ok().then_some(census)
    }

    /// Per-interface cumulative counters.
    fn read_net_counters(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<Vec<NetInterfaceCountersRecord>> {
        if !self.due(DegradedField::NetInterfaceCounters, false) {
            return None;
        }
        self.read_paged(
            transport,
            PagedRead {
                query: SysinfoQueryId::NET_INTERFACE_COUNTERS,
                field: DegradedField::NetInterfaceCounters,
                record_len: NetInterfaceCountersRecord::WIRE_LEN,
                cap: NET_INTERFACE_CAP,
            },
            degradations,
            page_payload,
            NetInterfaceCountersRecord::from_bytes,
        )
    }

    /// Per-CPU scheduler load.
    fn read_cpu_load(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<Vec<CpuLoadRecord>> {
        if !self.due(DegradedField::CpuLoad, false) {
            return None;
        }
        self.read_paged(
            transport,
            PagedRead {
                query: SysinfoQueryId::CPU_LOAD,
                field: DegradedField::CpuLoad,
                record_len: CpuLoadRecord::WIRE_LEN,
                cap: CPU_RECORD_CAP,
            },
            degradations,
            page_payload,
            CpuLoadRecord::from_bytes,
        )
    }

    /// Live per-interface link and address state.
    fn read_net_state(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<Vec<NetInterfaceStateRecord>> {
        if !self.due(DegradedField::NetInterfaceState, false) {
            return None;
        }
        self.read_paged(
            transport,
            PagedRead {
                query: SysinfoQueryId::NET_INTERFACE_STATE,
                field: DegradedField::NetInterfaceState,
                record_len: NetInterfaceStateRecord::WIRE_LEN,
                cap: NET_INTERFACE_CAP,
            },
            degradations,
            page_payload,
            NetInterfaceStateRecord::from_bytes,
        )
    }

    /// Per-interface throughput, averaged over the nominal sample period.
    ///
    /// The requested window is the sample period rather than the measured
    /// interval: it is what the *next* sample will be a period away from,
    /// and the service answers with the window it actually used, so a
    /// younger interface's shorter average is stated rather than assumed.
    fn read_net_rates(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<Vec<NetInterfaceRatesRecord>> {
        if !self.due(DegradedField::NetInterfaceRates, false) {
            return None;
        }
        let window = Duration64::from_nanos(SAMPLE_PERIOD_NS);
        self.read_paged(
            transport,
            PagedRead {
                query: SysinfoQueryId::NET_INTERFACE_RATES,
                field: DegradedField::NetInterfaceRates,
                record_len: NetInterfaceRatesRecord::WIRE_LEN,
                cap: NET_INTERFACE_CAP,
            },
            degradations,
            move |offset, limit| {
                NetInterfaceRatesRequest {
                    offset,
                    limit,
                    flags: 0,
                    window,
                }
                .to_le_bytes()
                .to_vec()
            },
            NetInterfaceRatesRecord::from_bytes,
        )
    }

    /// Refresh the readings that are cached between samples — the facts that
    /// are fixed for the boot and the slow-moving inventory — each on its own
    /// cadence.
    ///
    /// A failed refresh leaves the previously held value in place rather
    /// than blanking it: a reading that was true a cadence ago is better
    /// than nothing, and the degradation is reported once either way.
    fn refresh_cached(&mut self, transport: &dyn Transport, degradations: &mut Vec<DegradedField>) {
        self.refresh_boot_facts(transport, degradations);
        self.refresh_inventory(transport, degradations);
    }

    /// Refresh the readings that cannot change while the machine is up — the
    /// installation's identity, its RAM total, and the network hardware it
    /// presents.
    ///
    /// Each is read once and kept, and re-read only while it is still
    /// missing, so a reading that was refused or unavailable at start-up is
    /// picked up when it becomes answerable rather than being written off for
    /// the boot.
    fn refresh_boot_facts(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) {
        if self.due(DegradedField::Identity, self.identity.is_some()) {
            if let Some(identity) = self.read_scalar(
                transport,
                SysinfoQueryId::SYSTEM_IDENTITY,
                DegradedField::Identity,
                degradations,
                SystemIdentity::from_bytes,
            ) {
                self.identity = Some(identity);
            }
        }
        if self.due(DegradedField::MemoryTotal, self.memory_total.is_some()) {
            if let Some(total) = self.read_scalar(
                transport,
                SysinfoQueryId::MEMORY_TOTAL,
                DegradedField::MemoryTotal,
                degradations,
                MemoryTotal::from_bytes,
            ) {
                self.memory_total = Some(total);
            }
        }
        if self.due(DegradedField::NetInterfaceFacts, self.net_facts.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::NET_INTERFACE_FACTS,
                    field: DegradedField::NetInterfaceFacts,
                    record_len: NetInterfaceFactsRecord::WIRE_LEN,
                    cap: NET_INTERFACE_CAP,
                },
                degradations,
                page_payload,
                NetInterfaceFactsRecord::from_bytes,
            ) {
                self.net_facts = Some(records);
            }
        }
    }

    /// Refresh the slow-moving inventory: what is mounted and how healthy it
    /// is, which seats exist, this process's resource limits, and the crash
    /// records the recovery surface reads.
    ///
    /// These change, but far more slowly than a reader watches them, so they
    /// are read on the sparser cadences rather than every sample — a list of
    /// mounts re-read once a second would cost the service and the kernel
    /// real work for a reading that is almost always identical.
    fn refresh_inventory(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) {
        if self.due(DegradedField::KernelMemory, self.kernel_memory.is_some()) {
            if let Some(stats) = self.read_scalar(
                transport,
                SysinfoQueryId::KERNEL_MEMORY_STATS,
                DegradedField::KernelMemory,
                degradations,
                KernelMemoryStats::from_bytes,
            ) {
                self.kernel_memory = Some(stats);
            }
        }
        if self.due(
            DegradedField::ResourceLimits,
            self.resource_limits.is_some(),
        ) {
            if let Some(limits) = self.read_scalar(
                transport,
                SysinfoQueryId::RESOURCE_LIMITS,
                DegradedField::ResourceLimits,
                degradations,
                decode_resource_limits,
            ) {
                self.resource_limits = Some(limits);
            }
        }
        if self.due(DegradedField::Mounts, self.mounts.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::MOUNT_LIST,
                    field: DegradedField::Mounts,
                    record_len: MountRecord::WIRE_LEN,
                    cap: VOLUME_RECORD_CAP,
                },
                degradations,
                page_payload,
                MountRecord::from_bytes,
            ) {
                self.mounts = Some(records);
            }
        }
        if self.due(DegradedField::VolumeHealth, self.volume_health.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::VOLUME_IO_HEALTH,
                    field: DegradedField::VolumeHealth,
                    record_len: VolumeIoHealthRecord::WIRE_LEN,
                    cap: VOLUME_RECORD_CAP,
                },
                degradations,
                page_payload,
                VolumeIoHealthRecord::from_bytes,
            ) {
                self.volume_health = Some(records);
            }
        }
        if self.due(DegradedField::Seats, self.seats.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::SEAT_LIST,
                    field: DegradedField::Seats,
                    record_len: SeatRecord::WIRE_LEN,
                    cap: SEAT_CAP,
                },
                degradations,
                page_payload,
                SeatRecord::from_bytes,
            ) {
                self.seats = Some(records);
            }
        }
        if self.due(DegradedField::CrashRecords, self.crashes.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::CRASH_RECORD,
                    field: DegradedField::CrashRecords,
                    record_len: CrashRecord::WIRE_LEN,
                    cap: CRASH_RECORD_CAP,
                },
                degradations,
                page_payload,
                CrashRecord::from_bytes,
            ) {
                self.crashes = Some(records);
            }
        }
        self.refresh_memory_detail(transport, degradations);
        self.refresh_volume_io(transport, degradations);
        self.refresh_net_config(transport, degradations);
        if self.due(DegradedField::HardwareTree, self.hardware.is_some()) {
            let read = fetch_tree(transport).ok();
            self.note(DegradedField::HardwareTree, read.is_some(), degradations);
            if let Some(mut nodes) = read {
                nodes.truncate(HW_NODE_CAP);
                self.hardware = Some(nodes);
            }
        }
    }

    /// Refresh the per-volume service and queue counters every sample: each
    /// is a rate source whose delta between two samples *is* the reading, so
    /// unlike the mount table beside it neither can be read on a sparse
    /// cadence and still yield a rate.
    ///
    /// Both are keyed and ordered like the health list the inventory refresh
    /// holds, so the volume pane joins the three by volume id.
    fn refresh_volume_io(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) {
        if self.due(DegradedField::VolumeIoStats, self.volume_io_stats.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::VOLUME_IO_STATS,
                    field: DegradedField::VolumeIoStats,
                    record_len: VolumeIoStatsRecord::WIRE_LEN,
                    cap: VOLUME_RECORD_CAP,
                },
                degradations,
                page_payload,
                VolumeIoStatsRecord::from_bytes,
            ) {
                self.volume_io_stats = Some(records);
            }
        }
        if self.due(DegradedField::VolumeIoQueue, self.volume_io_queue.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::VOLUME_IO_QUEUE,
                    field: DegradedField::VolumeIoQueue,
                    record_len: VolumeIoQueueRecord::WIRE_LEN,
                    cap: VOLUME_RECORD_CAP,
                },
                degradations,
                page_payload,
                VolumeIoQueueRecord::from_bytes,
            ) {
                self.volume_io_queue = Some(records);
            }
        }
        // The graphics device's occupancy is a rate source on the same
        // footing: cumulative busy and idle nanoseconds whose delta is the
        // reading, so a sparse cadence would leave the pane with a total it
        // cannot divide.
        if self.due(DegradedField::GpuDeviceStats, self.gpu_stats.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::GPU_DEVICE_STATS,
                    field: DegradedField::GpuDeviceStats,
                    record_len: DisplayStats::WIRE_LEN,
                    cap: GPU_RECORD_CAP,
                },
                degradations,
                page_payload,
                DisplayStats::from_bytes,
            ) {
                self.gpu_stats = Some(records);
            }
        }
    }

    /// Refresh the memory readings behind the composition bar and the
    /// reclaim ledger, on the audited memory cadence their capability sets.
    fn refresh_memory_detail(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) {
        if self.due(DegradedField::RamzipStats, self.ramzip.is_some()) {
            let read = ramzip_stats(transport).ok();
            self.note(DegradedField::RamzipStats, read.is_some(), degradations);
            if let Some(stats) = read {
                self.ramzip = Some(stats);
            }
        }
        if self.due(DegradedField::ReclaimStats, self.reclaim.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::RECLAIM_STATS,
                    field: DegradedField::ReclaimStats,
                    record_len: ReclaimClassRecord::WIRE_LEN,
                    cap: CACHE_ROW_CAP,
                },
                degradations,
                page_payload,
                ReclaimClassRecord::from_bytes,
            ) {
                self.reclaim = Some(records);
            }
        }
        if self.due(DegradedField::CacheLedgers, self.cache_ledgers.is_some()) {
            if let Some(records) = self.read_paged(
                transport,
                PagedRead {
                    query: SysinfoQueryId::CACHE_LEDGERS,
                    field: DegradedField::CacheLedgers,
                    record_len: CacheLedgerRecord::WIRE_LEN,
                    cap: CACHE_ROW_CAP,
                },
                degradations,
                page_payload,
                CacheLedgerRecord::from_bytes,
            ) {
                self.cache_ledgers = Some(records);
            }
        }
    }

    /// Refresh the configured resolver and time servers, which change on an
    /// administrative action rather than moment to moment.
    fn refresh_net_config(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) {
        if self.due(
            DegradedField::NetResolverServers,
            self.resolver_servers.is_some(),
        ) {
            if let Some(servers) = self.read_servers(
                transport,
                SysinfoQueryId::NET_RESOLVER_SERVERS,
                DegradedField::NetResolverServers,
                degradations,
            ) {
                self.resolver_servers = Some(servers);
            }
        }
        if self.due(DegradedField::NetTimeServers, self.time_servers.is_some()) {
            if let Some(servers) = self.read_servers(
                transport,
                SysinfoQueryId::NET_TIME_SERVERS,
                DegradedField::NetTimeServers,
                degradations,
            ) {
                self.time_servers = Some(servers);
            }
        }
    }

    /// A configured-server list, whichever of the two `query` names: both
    /// page the same [`NetServerAddr`] record, so they share one read.
    fn read_servers(
        &mut self,
        transport: &dyn Transport,
        query: SysinfoQueryId,
        field: DegradedField,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<Vec<NetServerAddr>> {
        self.read_paged(
            transport,
            PagedRead {
                query,
                field,
                record_len: NetServerAddr::WIRE_LEN,
                cap: SERVER_RECORD_CAP,
            },
            degradations,
            page_payload,
            NetServerAddr::from_bytes,
        )
    }
}

/// Which cadence tier each reading belongs to — the one place a reading's
/// read frequency is stated, so the policy reads as a table instead of a
/// modulus test repeated at each fetch.
const fn cadence_of(field: DegradedField) -> Cadence {
    match field {
        DegradedField::ProcessList
        | DegradedField::CpuTime
        | DegradedField::Uptime
        | DegradedField::LoadAverage
        | DegradedField::CpuInfo
        | DegradedField::CpuLoad
        | DegradedField::MemoryPressureBand
        | DegradedField::NetInterfaceState
        | DegradedField::NetInterfaceRates
        | DegradedField::NetInterfaceCounters
        | DegradedField::NetSockets
        | DegradedField::NetStackDefence
        | DegradedField::VolumeIoStats
        | DegradedField::VolumeIoQueue
        | DegradedField::GpuDeviceStats => Cadence::EverySample,
        DegradedField::MemoryPressure
        | DegradedField::KernelMemory
        | DegradedField::ReclaimStats
        | DegradedField::RamzipStats
        | DegradedField::CacheLedgers => Cadence::Memory,
        DegradedField::Mounts
        | DegradedField::VolumeHealth
        | DegradedField::Seats
        | DegradedField::ResourceLimits
        | DegradedField::CrashRecords
        | DegradedField::NetResolverServers
        | DegradedField::NetTimeServers
        | DegradedField::HardwareTree => Cadence::Inventory,
        DegradedField::Identity | DegradedField::MemoryTotal | DegradedField::NetInterfaceFacts => {
            Cadence::Static
        }
    }
}

/// Decode the fixed [`SysinfoQueryId::RESOURCE_LIMITS`] report: one
/// [`ResourceLimitRecord`] per limit kind, in discriminant order.
///
/// The report is a fixed ABI length rather than a paged list, so it needs no
/// retention cap: the format itself is the bound. A reply that cannot hold
/// the whole report is rejected rather than half-read.
fn decode_resource_limits(reply: &[u8]) -> Result<Vec<ResourceLimitRecord>, Errno> {
    if reply.len() < RESOURCE_LIMITS_REPORT_LEN {
        return Err(Errno::BufferTooSmall);
    }
    reply[..RESOURCE_LIMITS_REPORT_LEN]
        .as_chunks::<{ ResourceLimitRecord::WIRE_LEN }>()
        .0
        .iter()
        .map(|record| ResourceLimitRecord::from_bytes(record.as_slice()))
        .collect()
}

#[cfg(test)]
#[path = "sample_tests.rs"]
mod tests;
