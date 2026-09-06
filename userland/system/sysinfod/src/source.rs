//! The data-provider seam: everything `sysinfod` knows about live system
//! state is read through [`SysinfoSource`].
//!
//! The dispatcher in [`crate::service`] owns *policy* — request decoding,
//! capability enforcement, paging bounds, and audit
//! logging — and nothing else. The *data* is supplied by an implementation
//! of [`SysinfoSource`] injected by `init` when it starts the service: on a
//! running kernel this is a thin shim over the kernel's process table and
//! memory accounting; in tests it is an in-memory fixture. Splitting the
//! two keeps the security-relevant code free of any particular kernel
//! plumbing.

use alloc::vec::Vec;

use tairix_abi::net_ipc::{
    NetBondMemberRecord, NetInterfaceCountersRecord, NetInterfaceFactsRecord,
    NetInterfaceRatesRecord, NetInterfaceStateRecord, NetServerAddr, NetSocketRecord,
    NetStackDefenceCounters,
};
use tairix_abi::raid_admin::{RaidArrayRecord, RaidMemberRecord};
use tairix_abi::sysinfo::{
    CacheLedgerRecord, CpuInfoRecord, CpuLoadRecord, CpuTimeRecord, CrashRecord, IrqRecord,
    KernelMemoryStats, LoadAverage, MemoryPressureBand, MemoryPressureStats, MemoryTotal,
    MountRecord, ProcessRecord, RamzipStats, ResourceLimitRecord, SeatRecord, SystemIdentity,
    Uptime, UserDirectoryRecord, VolumeIoHealthRecord, VolumeIoQueueRecord, VolumeIoStatsRecord,
};
use tairix_abi::time::Duration64;
use tairix_abi::{CapabilityQuery, Errno, LimitKind, Origin, ProcId};

/// The authenticated principal on whose behalf a request is served.
///
/// The identity is the kernel-attested [`Origin`] of the requesting task —
/// obtained from the IPC layer (`call_peer_origin`), never from the caller's
/// own payload — so `sysinfod` trusts the kernel's view, not bytes on the
/// wire. The owning uid and the effective capability summary the dispatcher
/// gates on are read from that one attested record, so they cannot drift
/// apart.
pub struct Caller {
    origin: Origin,
}

impl Caller {
    /// Wrap a kernel-attested [`Origin`] as the serving principal.
    #[must_use]
    pub fn new(origin: Origin) -> Self {
        Self { origin }
    }

    /// The caller's kernel-attested origin.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Owning user identifier of the requesting task.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.origin.uid()
    }

    /// The caller's effective capability set, as the object-safe
    /// [`CapabilityQuery`] seam the dispatcher gates on — backed by the
    /// non-secret membership summary the attested origin carries, so
    /// `sysinfod` never names a concrete `CapabilitySet` type.
    #[must_use]
    pub fn capabilities(&self) -> &dyn CapabilityQuery {
        self.origin.capabilities()
    }
}

/// Which processes a process-list query should observe.
///
/// The scope is decided by the dispatcher from the query identifier — never
/// by the caller — so a self-scoped request can never be widened into a
/// global one without the `CAP_SYSINFO_GLOBAL` gate that the global query
/// carries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProcessScope {
    /// Only processes owned by [`Caller::uid`]. The self-scoped, ungated
    /// observer.
    Caller,
    /// Every process on the system. Reached only after the
    /// `CAP_SYSINFO_GLOBAL` check has passed.
    Global,
}

/// Read-only access to the live system state exposed by the `sysinfo` API.
///
/// Every method is fallible and returns a [`tairix_abi::Errno`]; a source
/// must never panic. Methods that answer a query whose
/// response is a sequence — the process and mount lists — return an owned
/// `Vec` and leave paging to the dispatcher, so the bounds logic lives in
/// exactly one place.
pub trait SysinfoSource {
    /// Return the records visible to `caller` under `scope`.
    ///
    /// The owned list is returned whole; [`crate::serve`] applies the
    /// `offset`/`limit` paging from the request. Ordering is the source's
    /// responsibility and must be stable across paged calls so a client
    /// walking the list never skips or repeats a record. An **owned** `Vec`
    /// (not a borrowed slice) because a syscall-backed source materialises
    /// the records freshly on each call — it holds no persistent table to
    /// lend — and a fixture simply clones its own.
    fn process_records(
        &self,
        caller: &Caller,
        scope: ProcessScope,
    ) -> Result<Vec<ProcessRecord>, Errno>;

    /// Return kernel memory statistics.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed.
    fn kernel_memory_stats(&self, caller: &Caller) -> Result<KernelMemoryStats, Errno>;

    /// Return the encoded detected hardware tree, exactly as `hw_tree_read`
    /// supplies it: one `HwTreeHeader` followed by whole `HwNode` records.
    ///
    /// Reached only after the `CAP_SYSINFO_HW` gate has passed. The
    /// snapshot is returned whole; [`crate::serve`] validates the header
    /// against the body (fail closed) and applies the request's
    /// `offset`/`limit` record window, repeating the header on every page.
    /// Returned as an **owned** `Vec` for the same reason as
    /// [`process_records`](Self::process_records) — a syscall-backed source
    /// materialises the bytes on each call.
    fn hardware_tree(&self, caller: &Caller) -> Result<Vec<u8>, Errno>;

    /// Return the machine identity (machine ID, OS version, hostname).
    fn system_identity(&self, caller: &Caller) -> Result<SystemIdentity, Errno>;

    /// Return system uptime and boot wall-clock time.
    fn uptime(&self, caller: &Caller) -> Result<Uptime, Errno>;

    /// Return the scheduler load averages and the logged-in-user census.
    ///
    /// System-wide, secret-free figures (the `uptime(1)` line), so the
    /// query is ungated — exactly like [`uptime`](Self::uptime).
    fn load_average(&self, caller: &Caller) -> Result<LoadAverage, Errno>;

    /// Return the current mount table.
    ///
    /// The mount table is system-wide and secret-free, so the query is
    /// ungated: unlike [`process_records`](Self::process_records)
    /// there is no per-principal scope to narrow. As with the process list
    /// the owned list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging; ordering must be stable across paged calls.
    fn mount_records(&self, caller: &Caller) -> Result<Vec<MountRecord>, Errno>;

    /// Return `caller`'s effective resource limits and current live usage,
    /// one record per [`LimitKind`] in discriminant order. The query is self-scoped — the answer describes the caller's
    /// own task only — so it carries no capability gate.
    ///
    /// The fixed-length array (one entry per kind) is returned whole; the
    /// dispatcher packs it. A source that cannot read a particular usage
    /// figure reports it conservatively rather than omitting the record, so
    /// the array is always [`LimitKind::COUNT`] long and positional.
    fn resource_limits(
        &self,
        caller: &Caller,
    ) -> Result<[ResourceLimitRecord; LimitKind::COUNT], Errno>;

    /// Return the account directory: every account's uid + username pair,
    /// and nothing else — no credential material (password records stay
    /// behind the capability-gated `users_db_read` syscall).
    ///
    /// The `/etc/passwd`-class public pairing is secret-free, so the query
    /// is ungated like [`mount_records`](Self::mount_records). The owned
    /// list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging; ordering must be stable across paged calls.
    fn user_directory(&self, caller: &Caller) -> Result<Vec<UserDirectoryRecord>, Errno>;

    /// Return the per-CPU execution-time accounting, one record per online
    /// CPU in ascending CPU order.
    ///
    /// The aggregate busy/idle split is the `top`-class utilisation figure
    /// every user may see and exposes strictly less than the ungated
    /// [`load_average`](Self::load_average) census, so the query is ungated.
    /// The owned list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging; ordering must be stable across paged calls.
    fn cpu_times(&self, caller: &Caller) -> Result<Vec<CpuTimeRecord>, Errno>;

    /// Return the live memory-pressure gauge snapshot: the current band,
    /// the derived watermarks in force, the reserve floor, the free/total
    /// readings, and the per-band entry counters since boot.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed — the
    /// same boundary as [`kernel_memory_stats`](Self::kernel_memory_stats)
    /// (`plans/STRESSTEST.md` ST1).
    fn memory_pressure(&self, caller: &Caller) -> Result<MemoryPressureStats, Errno>;

    /// Return the published memory-pressure band alone.
    ///
    /// Ungated: a process must be able to learn that the machine is
    /// short of memory in order to give its own caches back, and the
    /// band carries no bytes and nothing per-principal. The detailed
    /// [`memory_pressure`](Self::memory_pressure) view stays behind
    /// `CAP_SYSINFO_KERNEL`.
    fn memory_pressure_band(&self, caller: &Caller) -> Result<MemoryPressureBand, Errno>;

    /// Return the machine's total usable physical RAM alone, in bytes.
    ///
    /// Ungated: installed RAM is a static hardware fact — the figure on
    /// the machine's spec sheet — carrying no per-process, per-user, or
    /// byte-level runtime state, so it discloses strictly less than the
    /// already-ungated [`load_average`](Self::load_average). It lets a
    /// process size its own caches against the real machine instead of a
    /// hand-picked constant. The detailed
    /// [`memory_pressure`](Self::memory_pressure) view — free bytes,
    /// watermarks, the reserve, transition history — stays behind
    /// `CAP_SYSINFO_KERNEL`.
    ///
    /// The same figure
    /// [`kernel_memory_stats`](Self::kernel_memory_stats) reports as
    /// `KernelMemoryStats::total_bytes`; a source must thread both from
    /// one reading so the two can never disagree.
    fn memory_total(&self, caller: &Caller) -> Result<MemoryTotal, Errno>;

    /// Return the kernel's own per-cache ledger rows: one
    /// [`CacheLedgerRecord`] per cache the kernel measures directly, in the
    /// kernel's stable registration order.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed, exactly
    /// like the reclaim-class aggregate this now backs. The owned list is
    /// returned whole and unpaged: [`crate::serve`] folds it with the
    /// self-reported rows the reporter registry holds before paging either
    /// the per-class totals or the per-cache list a client asked for.
    fn cache_ledger_records(&self, caller: &Caller) -> Result<Vec<CacheLedgerRecord>, Errno>;

    /// Return the unforgeable process-instance id of every live process on
    /// the machine, in no particular order.
    ///
    /// Caller-independent — it takes no [`Caller`] — because the answer is a
    /// fact of the machine, not of the requester: it exists solely so
    /// [`crate::serve`] can expire a reporter registry entry whose process
    /// has since exited, never to answer a client query directly.
    fn live_process_instances(&self) -> Result<Vec<ProcId>, Errno>;

    /// Return the `ramzip` compressed-tier accounting: counters only,
    /// never page contents or key material; an undriven tier truthfully
    /// reports idle zeros.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed.
    fn ramzip_stats(&self, caller: &Caller) -> Result<RamzipStats, Errno>;

    /// Return the per-CPU scheduler load figures (run-queue depth sample,
    /// context-switch and preemption counters), one record per online CPU
    /// in ascending CPU order — the busy/idle time split stays in
    /// [`cpu_times`](Self::cpu_times), so no figure is served twice.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed: queue
    /// depths and preemption counters are kernel-wide scheduler internals,
    /// unlike the ungated utilisation split. The owned list is returned
    /// whole and [`crate::serve`] applies the `offset`/`limit` paging;
    /// ordering must be stable across paged calls.
    fn cpu_load(&self, caller: &Caller) -> Result<Vec<CpuLoadRecord>, Errno>;

    /// Return the per-CPU processor information (`/proc/cpuinfo`-class
    /// facts): one record per online CPU in ascending CPU order —
    /// performance class, ISA-extension feature bits, the raw identity
    /// register, the model/vendor name, the fixed reference/timebase
    /// frequency, and the live measured core-clock frequency.
    ///
    /// Ungated (needs no capability): vendor, model, features, topology,
    /// and clock speed are public hardware facts, like
    /// [`cpu_times`](Self::cpu_times). The owned list is returned whole and
    /// [`crate::serve`] applies the `offset`/`limit` paging; ordering must
    /// be stable across paged calls.
    fn cpu_info(&self, caller: &Caller) -> Result<Vec<CpuInfoRecord>, Errno>;

    /// Return the seat inventory: one record per seat, in ascending seat-id
    /// order (`plans/DISPLAY.md` D3).
    ///
    /// Reached only after the `CAP_SYSINFO_HW` gate has passed: like
    /// [`hardware_tree`](Self::hardware_tree), the inventory names which
    /// task owns each physical display — cross-principal surface topology,
    /// not a self-scoped observer. The owned list is returned whole and
    /// [`crate::serve`] applies the `offset`/`limit` paging; ordering must
    /// be stable across paged calls.
    fn seats(&self, caller: &Caller) -> Result<Vec<SeatRecord>, Errno>;

    /// Return every managed network interface's static facts, in the
    /// stack's stable table order (`plans/NETWORK.md` §5).
    ///
    /// Reached only after the `CAP_SYSINFO_HW` gate has passed: the
    /// record carries the device's MAC address — stable hardware
    /// identity, like [`hardware_tree`](Self::hardware_tree). On a
    /// running system the source forwards to the `netstack` service's
    /// broker read; the owned list is returned whole and
    /// [`crate::serve`] applies the `offset`/`limit` paging.
    fn net_interface_facts(&self, caller: &Caller) -> Result<Vec<NetInterfaceFactsRecord>, Errno>;

    /// Return every managed network interface's live link/address
    /// state, in the stack's stable table order (`plans/NETWORK.md`
    /// §5).
    ///
    /// Reached only after the `CAP_SYSINFO_GLOBAL` gate has passed: the
    /// address book is system-wide network state, not a self-scoped
    /// observer. On a running system the source forwards to the
    /// `netstack` service's broker read; the owned list is returned
    /// whole and [`crate::serve`] applies the `offset`/`limit` paging.
    fn net_interface_state(&self, caller: &Caller) -> Result<Vec<NetInterfaceStateRecord>, Errno>;

    /// Return every managed network interface's live stack counters, in
    /// the stack's stable table order (`plans/NETWORK.md` §5).
    ///
    /// Reached only after the `CAP_SYSINFO_GLOBAL` gate has passed: the
    /// counters are system-wide network metrics — the same boundary as
    /// [`net_interface_state`](Self::net_interface_state), and the
    /// surface a defence-in-progress becomes visible on. On a running
    /// system the source forwards to the `netstack` service's broker
    /// read; the owned list is returned whole and [`crate::serve`]
    /// applies the `offset`/`limit` paging.
    fn net_interface_counters(
        &self,
        caller: &Caller,
    ) -> Result<Vec<NetInterfaceCountersRecord>, Errno>;

    /// Return the network stack's stack-wide TCP connection-defence
    /// counters (`plans/NETWORK.md` §5: `stats:net/stack/…`).
    ///
    /// Reached only after the `CAP_SYSINFO_GLOBAL` gate has passed: these
    /// are system-wide, cross-principal figures — the same boundary as
    /// [`net_interface_counters`](Self::net_interface_counters), and the
    /// surface a SYN flood in progress becomes visible on. One record, not
    /// a list: the counters belong to the stack's socket table as a whole
    /// and name no interface, so there is nothing to page.
    fn net_stack_defence(&self, caller: &Caller) -> Result<NetStackDefenceCounters, Errno>;

    /// Return every managed network interface's live throughput rates over
    /// `window`, in the stack's stable table order (`plans/NETWORK.md` §5:
    /// `stats:net/<iface>/{rx,tx}.{pps,bps}`).
    ///
    /// Reached only after the `CAP_SYSINFO_GLOBAL` gate has passed: the
    /// rates derive from the same system-wide counters as
    /// [`net_interface_counters`](Self::net_interface_counters). Each
    /// record reports the window it was *actually* averaged over. On a
    /// running system the source forwards to the `netstack` service's
    /// broker read; the owned list is returned whole and [`crate::serve`]
    /// applies the `offset`/`limit` paging.
    fn net_interface_rates(
        &self,
        caller: &Caller,
        window: Duration64,
    ) -> Result<Vec<NetInterfaceRatesRecord>, Errno>;

    /// Return every open socket the stack owns, system-wide, in the
    /// stack's stable table order (`plans/NETWORK.md` §5: the
    /// `ss`/`netstat` socket table).
    ///
    /// Reached only after the `CAP_SYSINFO_GLOBAL` gate has passed: the
    /// records name every principal's sockets and every connection's peer
    /// address — the most privileged of the `stats:net` surfaces. On a
    /// running system the source forwards to the `netstack` service's
    /// broker read; the owned list is returned whole and [`crate::serve`]
    /// applies the `offset`/`limit` paging.
    fn net_sockets(&self, caller: &Caller) -> Result<Vec<NetSocketRecord>, Errno>;

    /// Return every bond interface's members and their live health, one
    /// record per (bond, member) pair, in the stack's stable table order
    /// (`plans/NETWORK.md` §5, §6.3: `info:net/<bond>/members`,
    /// `state:net/<bond>/active-member`, per-member health).
    ///
    /// Reached only after the `CAP_SYSINFO_GLOBAL` gate has passed: the
    /// link-aggregation topology and its live failover state are
    /// system-wide network state — the same boundary as
    /// [`net_interface_state`](Self::net_interface_state). On a running
    /// system the source forwards to the `netstack` service's broker read;
    /// the owned list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging.
    fn net_bond_members(&self, caller: &Caller) -> Result<Vec<NetBondMemberRecord>, Errno>;

    /// Return the host's active recursive-resolver server set: the
    /// aggregated, deduplicated DHCP-learned ∪ statically-configured DNS
    /// servers, in the stack's stable order (`plans/DNS.md` DNS2).
    ///
    /// Ungated at this broker: the recursive DNS servers a host queries are
    /// public host configuration (the resolv.conf analogue), exposing no
    /// per-principal secret — like [`cpu_info`](Self::cpu_info). On a
    /// running system the source forwards to the `netstack` service's
    /// broker read (itself gated on this broker's `CAP_SYSINFO_INTROSPECT`
    /// grant); the owned list is returned whole and [`crate::serve`]
    /// applies the `offset`/`limit` paging (the small set fits one page).
    fn net_resolver_servers(&self, caller: &Caller) -> Result<Vec<NetServerAddr>, Errno>;

    /// Return the network time servers the host's DHCP client(s) learned, in
    /// the stack's stable order (`plans/TIMESYNC.md` §3).
    ///
    /// Ungated for the same reason as
    /// [`net_resolver_servers`](Self::net_resolver_servers): which time
    /// server the network offers is public network configuration and confers
    /// no authority. The source forwards to the `netstack` broker read and
    /// returns the owned list whole; [`crate::serve`] applies the paging.
    fn net_time_servers(&self, caller: &Caller) -> Result<Vec<NetServerAddr>, Errno>;

    /// Return the kernel IRQ table: one record per bound interrupt line,
    /// in ascending line order.
    ///
    /// Reached only after the `CAP_SYSINFO_HW` gate has passed: like
    /// [`seats`](Self::seats) and [`hardware_tree`](Self::hardware_tree),
    /// the table names which task owns each physical interrupt line —
    /// cross-principal surface topology, not a self-scoped observer. The
    /// owned list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging; ordering must be stable across paged calls
    /// (ascending line order is).
    fn irqs(&self, caller: &Caller) -> Result<Vec<IrqRecord>, Errno>;

    /// Return the post-mortem crash-record store: one record per recorded
    /// user-fault kill, newest first.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed: the
    /// record carries absolute general-purpose register values — the
    /// privileged-debugger datum, the same boundary as
    /// [`kernel_memory_stats`](Self::kernel_memory_stats). The owned list is
    /// returned whole and [`crate::serve`] applies the `offset`/`limit`
    /// paging; ordering (newest first) is stable across paged calls.
    fn crashes(&self, caller: &Caller) -> Result<Vec<CrashRecord>, Errno>;

    /// Return the per-volume storage I/O health: one record per fault-aware
    /// block-backed volume the kernel serves (its durable volume id, the
    /// serving block-service endpoint, its current availability, and the
    /// cumulative outcome counters the kernel filesystem client folded)
    /// (`plans/FIX-IO.md` IO5).
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed: the
    /// per-device outcome tallies are kernel-wide storage operational state —
    /// the same boundary as [`memory_pressure`](Self::memory_pressure) and
    /// [`cache_ledger_records`](Self::cache_ledger_records), not the ungated
    /// mount table. The owned list is returned whole and [`crate::serve`] applies
    /// the `offset`/`limit` paging; ordering must be stable across paged
    /// calls.
    fn volume_io_health(&self, caller: &Caller) -> Result<Vec<VolumeIoHealthRecord>, Errno>;

    /// Return the per-volume storage **service** counters: one record per
    /// fault-aware block-backed volume the kernel serves, in the same order
    /// and keyed the same way as
    /// [`volume_io_health`](Self::volume_io_health), carrying the cumulative
    /// bytes, completed requests, device-busy time and summed waits the
    /// kernel block client folded (`plans/FIX-IO.md` IO5).
    ///
    /// **Ungated**, exactly like [`cpu_times`](Self::cpu_times) and for the
    /// same reason: a machine-wide throughput and utilisation figure is one
    /// every user may see, and it exposes strictly less than the ungated
    /// mount table. Nothing is served pre-derived, so no consumer inherits
    /// another's averaging window. The owned list is returned whole and
    /// [`crate::serve`] applies the `offset`/`limit` paging.
    fn volume_io_stats(&self, caller: &Caller) -> Result<Vec<VolumeIoStatsRecord>, Errno>;

    /// Return the per-volume storage **queue** occupancy: one record per
    /// fault-aware block-backed volume, in the same order and keyed the same
    /// way as [`volume_io_health`](Self::volume_io_health), carrying the live
    /// in-flight count, the mean-depth accumulators, and the per-device
    /// budget bounding them.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed: a queue
    /// depth is a driver and scheduler internal — the same boundary
    /// [`cpu_load`](Self::cpu_load) draws — not the utilisation split
    /// [`volume_io_stats`](Self::volume_io_stats) serves ungated. The owned
    /// list is returned whole and [`crate::serve`] applies the paging.
    fn volume_io_queue(&self, caller: &Caller) -> Result<Vec<VolumeIoQueueRecord>, Errno>;

    /// Return the live RAID arrays the composer serves: one
    /// [`RaidArrayRecord`] per array (its identity, level, health, width,
    /// geometry, the endpoint and node it is published on, and how far a
    /// running verification pass or rebuild has reached).
    ///
    /// Reached only after the `CAP_SYSINFO_HW` gate has passed: how a
    /// machine's storage is composed is hardware topology, not a
    /// per-principal fact, the same boundary as
    /// [`hardware_tree`](Self::hardware_tree) and [`seats`](Self::seats). On
    /// a running system the source forwards to the RAID composer's control
    /// endpoint; a machine with no running array composer fails closed with
    /// the transport's typed error, never a fabricated empty table. The
    /// owned list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging; ordering must be stable across paged calls.
    fn raid_arrays(&self, caller: &Caller) -> Result<Vec<RaidArrayRecord>, Errno>;

    /// Return every device the RAID composer holds: one [`RaidMemberRecord`]
    /// per array member *and* per unaffiliated candidate a new array could
    /// be created over.
    ///
    /// Reached only after the `CAP_SYSINFO_HW` gate has passed, for the same
    /// reason as [`raid_arrays`](Self::raid_arrays). A device with no
    /// filesystem on it has no volume to appear as, so this is how an
    /// administrator names it when composing an array. On a running system
    /// the source forwards to the RAID composer's control endpoint; a
    /// machine with no running array composer fails closed with the
    /// transport's typed error, never a fabricated empty table. The owned
    /// list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging; ordering must be stable across paged calls.
    fn raid_members(&self, caller: &Caller) -> Result<Vec<RaidMemberRecord>, Errno>;
}
