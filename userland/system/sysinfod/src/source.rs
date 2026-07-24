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
    NetInterfaceRatesRecord, NetInterfaceStateRecord, NetSocketRecord,
};
use tairix_abi::sysinfo::{
    CpuLoadRecord, CpuTimeRecord, CrashRecord, IrqRecord, KernelMemoryStats, LoadAverage,
    MemoryPressureStats, MountRecord, ProcessRecord, RamzipStats, ReclaimClassRecord,
    ResourceLimitRecord, SeatRecord, SystemIdentity, Uptime, UserDirectoryRecord,
};
use tairix_abi::time::Duration64;
use tairix_abi::{CapabilityQuery, Errno, LimitKind, Origin};

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

    /// Return the reclaimable-cache ledger: one record per reclaim class,
    /// in class-id order, aggregated across every live cache.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed. The
    /// owned list is returned whole and [`crate::serve`] applies the
    /// `offset`/`limit` paging; ordering must be stable across paged
    /// calls (class-id order is).
    fn reclaim_records(&self, caller: &Caller) -> Result<Vec<ReclaimClassRecord>, Errno>;

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
}
