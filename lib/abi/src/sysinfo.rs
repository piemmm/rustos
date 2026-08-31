//! System Information API (`sysinfo`) — the ABI surface.
//!
//! TAIRiX has no `/proc` and no `/sys`. Every piece of live system
//! information that would have lived under those trees is exposed through
//! this single, versioned, capability-checked API. Each query is a *typed*
//! request returning a *typed* response — there is no free-form text
//! scraping interface — and every query declares the capability a caller
//! must hold to invoke it.
//!
//! Adding a query carries the same discipline as adding a syscall: the registry is versioned, its canonical encoding is
//! hashed, and it is frozen on release. Existing [`SysinfoQueryId`] numbers
//! and [`SysinfoQuerySpec`] rows must never be re-numbered or removed; new
//! queries take the next free identifier and ship in `sysinfo-v2`.
//!
//! This module defines only the wire types and the frozen registry. The
//! user-space service that answers the queries lives at
//! `/System/Services/sysinfod.app/Run` (`userland/system/sysinfod`); the kernel has
//! no privileged path that bypasses the capability check.

use crate::blkio::{
    BlkDeviceClass, BlkHealthCounters, BlkHealthState, BlkStatus, BLK_HEALTH_COUNTERS_LEN,
};
use crate::driver::filesystem::{MountFlags, VolumeStats};
use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::origin::ProcId;
use crate::process::SchedPriority;
use crate::rlimit::{LimitKind, ResourceLimit};
use crate::time::{Duration64, Time64};
use crate::{CapabilityId, Errno};

/// Version tag for the frozen `sysinfo-v1` request/response surface.
///
/// Carried in every [`SysinfoRequestHeader`]; a service receiving a header
/// whose version it does not understand refuses the request rather than
/// guessing at the layout.
pub const SYSINFO_VERSION_V1: u16 = 1;

/// The current `sysinfo` version served by this ABI revision.
///
/// Equal to [`SYSINFO_VERSION_V1`] today; when `sysinfo-v2` is introduced
/// this constant is re-pointed and `sysinfo-v1` moves to a compatibility
/// path rather than mutating these types in place.
pub const SYSINFO_VERSION_CURRENT: u16 = SYSINFO_VERSION_V1;

/// Stable identifier for one System Information query.
///
/// Wraps a `u16` so it cannot be confused with raw integer arguments at
/// call sites. Identifiers are dense; the registry [`SYSINFO_QUERIES`] is
/// indexed directly by this value.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SysinfoQueryId(u16);

impl SysinfoQueryId {
    /// List the calling principal's own processes. Requires no capability.
    pub const SELF_PROCESS_LIST: Self = Self(0);
    /// List every process on the system. Requires `CAP_SYSINFO_GLOBAL`.
    pub const GLOBAL_PROCESS_LIST: Self = Self(1);
    /// Read kernel memory statistics. Requires `CAP_SYSINFO_KERNEL`.
    pub const KERNEL_MEMORY_STATS: Self = Self(2);
    /// Read the detected hardware tree, paged by a [`HardwareTreeRequest`].
    /// Requires `CAP_SYSINFO_HW`.
    ///
    /// The reply is one [`HwTreeHeader`](crate::hwtree::HwTreeHeader) —
    /// whose `node_count` is the **total** node count of the snapshot and
    /// whose `generation` identifies it — followed by up to
    /// [`HardwareTreeRequest::limit`] whole
    /// [`HwNode`](crate::hwtree::HwNode) records starting at
    /// [`HardwareTreeRequest::offset`]. A tree is larger than one framed
    /// reply can carry (one [`HwNode`](crate::hwtree::HwNode) is hundreds
    /// of bytes), so a client pages until it holds `node_count` records,
    /// checking that `generation` stayed constant across pages and
    /// restarting the walk when the tree changed under it.
    pub const HARDWARE_TREE: Self = Self(3);
    /// Read machine identity (machine ID, OS version). Requires none.
    pub const SYSTEM_IDENTITY: Self = Self(4);
    /// Read system uptime and boot wall-clock time. Requires none.
    pub const UPTIME: Self = Self(5);
    /// List the currently-mounted filesystems. Requires no capability:
    /// the mount table is system-wide rather than scoped to a principal,
    /// and exposes no per-process secret, so — like [`Self::UPTIME`] and
    /// [`Self::SYSTEM_IDENTITY`] — any task may read it. The privileged *act* of mounting is gated separately by
    /// `CAP_FS_MOUNT`; this query only reports.
    pub const MOUNT_LIST: Self = Self(6);
    /// Read the calling principal's own effective resource limits together
    /// with its current live usage of each.
    ///
    /// Requires no capability: the answer is scoped to the caller's own
    /// task, exposing no other principal's state — like
    /// [`Self::SELF_PROCESS_LIST`]. Observing *another* principal's limits
    /// would be a separate, capability-gated query.
    pub const RESOURCE_LIMITS: Self = Self(7);
    /// Read the calling principal's own kernel-attested [`Origin`](crate::Origin).
    ///
    /// Self-scoped and ungated: the answer describes only the caller — its
    /// trust domain, uid, pid, unforgeable [`ProcId`], and a
    /// non-secret capability summary — all filled by the kernel from the
    /// caller's own task state, never from the request payload. A principal
    /// observing *its own* attested identity exposes no other principal's
    /// state, so — like [`Self::SELF_PROCESS_LIST`] and
    /// [`Self::RESOURCE_LIMITS`] — it carries no capability gate. Reading
    /// *another* principal's origin would be a separate, capability-gated
    /// query.
    pub const PROCESS_IDENTITY: Self = Self(8);

    /// Read the scheduler load averages and the logged-in-user census.
    ///
    /// Ungated: the answer is the classic `uptime(1)` line — three
    /// exponentially-damped load averages, the runnable/total task counts,
    /// and the number of distinct logged-in users — system-wide figures
    /// that expose no per-process secret, exactly like [`Self::UPTIME`].
    pub const LOAD_AVERAGE: Self = Self(9);

    /// List the account directory: every account's uid and username, one
    /// [`UserDirectoryRecord`] per account.
    ///
    /// Ungated: the pairing of a numeric uid with its account name is the
    /// `/etc/passwd`-class public directory every `ls -l`- or `top`-style
    /// display needs, and it carries **no** credential material — password
    /// records stay behind the capability-gated `users_db_read` syscall.
    /// A system whose user database is not loaded answers with an empty
    /// directory, never a fabricated account.
    pub const USER_DIRECTORY: Self = Self(10);

    /// List per-CPU execution-time accounting: one [`CpuTimeRecord`] per
    /// online CPU, paged by a [`CpuTimeListRequest`].
    ///
    /// Ungated: the aggregate busy/idle split is the `top`/`uptime`-class
    /// utilisation figure every user may see, and it exposes strictly less
    /// than the ungated [`SysinfoQueryId::LOAD_AVERAGE`] census — no
    /// per-task, per-user, or kernel-internal detail crosses it.
    pub const CPU_TIME_STATS: Self = Self(11);

    /// List the kernel's seats: one [`SeatRecord`] per seat (seat id, live
    /// owner, lease generation, foreground console), paged by a
    /// [`SeatListRequest`].
    ///
    /// Requires `CAP_SYSINFO_HW` and is audited: like
    /// [`Self::HARDWARE_TREE`], the seat inventory names which task owns
    /// each physical display — cross-principal, security-relevant surface
    /// topology, not a self-scoped observer (`plans/DISPLAY.md` D3).
    pub const SEAT_LIST: Self = Self(12);

    /// Read the live memory-pressure gauge: the current band, the
    /// derived enter/exit watermarks actually in force, the reserve
    /// floor, the free/total readings, and the per-band entry counters
    /// since boot — a single [`MemoryPressureStats`].
    ///
    /// Requires `CAP_SYSINFO_KERNEL` and is audited: like
    /// [`Self::KERNEL_MEMORY_STATS`], the gauge is kernel-wide
    /// operational state, not a self-scoped observer
    /// (`plans/STRESSTEST.md` ST1).
    pub const MEMORY_PRESSURE: Self = Self(13);

    /// Read the reclaimable-cache ledger: one [`ReclaimClassRecord`]
    /// per reclaim class with live payload/metadata bytes, entry count,
    /// and the per-class event counters, paged by a
    /// [`ReclaimListRequest`].
    ///
    /// Requires `CAP_SYSINFO_KERNEL` and is audited, exactly like its
    /// sibling [`Self::KERNEL_MEMORY_STATS`] (`plans/STRESSTEST.md` ST1).
    pub const RECLAIM_STATS: Self = Self(14);

    /// Read the `ramzip` compressed-tier accounting: a single
    /// [`RamzipStats`] carrying the tier's stored/logical/metadata
    /// bytes, the derived min/soft/hard caps, and every monotonic event
    /// counter — counters only, never page contents or key material
    /// (`plans/SWAPSWAPSWAP.md` §16).
    ///
    /// Requires `CAP_SYSINFO_KERNEL` and is audited.
    pub const RAMZIP_STATS: Self = Self(15);

    /// Read per-CPU scheduler load figures: one [`CpuLoadRecord`] per
    /// online CPU (run-queue depth sample, context-switch and
    /// preemption counters), paged by a [`CpuLoadRequest`].
    ///
    /// The cumulative busy/idle time split lives in
    /// [`Self::CPU_TIME_STATS`]; this query carries only the remainder,
    /// so the same figure is never served twice. Requires
    /// `CAP_SYSINFO_KERNEL` and is audited: queue depths and
    /// preemption counters are kernel-wide scheduler internals, not the
    /// utilisation split every user may see.
    pub const CPU_LOAD: Self = Self(16);

    /// List every managed network interface's static facts: one
    /// [`NetInterfaceFactsRecord`](crate::net_ipc::NetInterfaceFactsRecord)
    /// per interface (alias, kind, MAC, MTU, negotiated offloads,
    /// receive-queue count), paged by a [`NetInterfaceListRequest`].
    ///
    /// Requires `CAP_SYSINFO_HW` and is audited: the record carries the
    /// device's MAC address — stable hardware identity, the same class of
    /// surface topology as [`Self::HARDWARE_TREE`] (`plans/NETWORK.md`
    /// §5: `info:net` sits behind hardware-identity policy review).
    pub const NET_INTERFACE_FACTS: Self = Self(17);

    /// List every managed network interface's live link/address state:
    /// one
    /// [`NetInterfaceStateRecord`](crate::net_ipc::NetInterfaceStateRecord)
    /// per interface (link, bound v4/v6 addresses with their SLAAC/DAD
    /// state), paged by a [`NetInterfaceListRequest`].
    ///
    /// Requires `CAP_SYSINFO_GLOBAL` and is audited: the address book is
    /// system-wide, cross-principal network state, not a self-scoped
    /// observer (`plans/NETWORK.md` §5: `state:net`).
    pub const NET_INTERFACE_STATE: Self = Self(18);

    /// List the kernel IRQ table: one [`IrqRecord`] per bound interrupt
    /// line (line id, the owning driver task, the monotonic fire count
    /// since boot, and whether the line is quarantined), paged by an
    /// [`IrqListRequest`].
    ///
    /// Requires `CAP_SYSINFO_HW` and is audited: like
    /// [`Self::HARDWARE_TREE`] and [`Self::SEAT_LIST`], the table names
    /// which task owns each physical interrupt line — cross-principal
    /// surface topology, not a self-scoped observer. The per-line fire
    /// count is the counter a driver's own device asserts; it exposes no
    /// per-principal secret beyond the ownership the hardware view already
    /// carries.
    pub const IRQ_LIST: Self = Self(19);

    /// Read the post-mortem crash record of each user task killed by an
    /// unresolvable memory fault: one [`CrashRecord`] per recorded crash
    /// (faulting identity, cause class, the load-relative `pc` and
    /// backtrace, the register file), paged by a [`CrashRecordRequest`].
    ///
    /// Requires `CAP_SYSINFO_KERNEL` and is audited. The record is the
    /// privileged-debugger analogue of a Linux kernel oops: it carries the
    /// absolute general-purpose register *values* — the one datum expressed
    /// absolute anywhere in this diagnostics surface — so it is gated on
    /// the same kernel-introspection capability as
    /// [`Self::KERNEL_MEMORY_STATS`], never handed to an unprivileged
    /// reader. Even behind the gate the faulting `pc`, the backtrace
    /// frames, and the fault address are expressed as **program-relative /
    /// region-relative offsets**, so the record symbolicates offline
    /// against the unstripped binary without becoming an address-space
    /// layout oracle.
    pub const CRASH_RECORD: Self = Self(20);

    /// List every managed network interface's live stack counters: one
    /// [`NetInterfaceCountersRecord`](crate::net_ipc::NetInterfaceCountersRecord)
    /// per interface (received/transmitted frames and bytes, receive
    /// drops, transmit resolution drops, and the stack-wide ICMP-error
    /// and reassembly-eviction defence counters), paged by a
    /// [`NetInterfaceListRequest`].
    ///
    /// Requires `CAP_SYSINFO_GLOBAL` and is audited: the counters are
    /// system-wide, cross-principal network metrics — the same class of
    /// state as [`Self::NET_INTERFACE_STATE`] and the surface a
    /// defence-in-progress (a SYN flood, a reassembly-eviction storm)
    /// becomes visible on (`plans/NETWORK.md` §5: `stats:net`).
    pub const NET_INTERFACE_COUNTERS: Self = Self(21);

    /// List every managed network interface's live throughput rates over a
    /// caller-supplied window: one
    /// [`NetInterfaceRatesRecord`](crate::net_ipc::NetInterfaceRatesRecord)
    /// per interface (received/transmitted packets- and bits-per-second and
    /// the window each was actually averaged over), paged by a
    /// [`NetInterfaceRatesRequest`].
    ///
    /// Requires `CAP_SYSINFO_GLOBAL` and is audited: rates are derived from
    /// the same system-wide, cross-principal counters as
    /// [`Self::NET_INTERFACE_COUNTERS`] and are the surface a
    /// denial-of-service in progress (a traffic flood) becomes visible on
    /// (`plans/NETWORK.md` §5: `stats:net/<iface>/{rx,tx}.{pps,bps}`).
    pub const NET_INTERFACE_RATES: Self = Self(22);

    /// List every open socket the stack owns, system-wide: one
    /// [`NetSocketRecord`](crate::net_ipc::NetSocketRecord) per socket
    /// (protocol, state, local and peer addresses, the owning process,
    /// and the receive/send queue depths), paged by a
    /// [`NetInterfaceListRequest`] — the `ss`/`netstat` socket table.
    ///
    /// Requires `CAP_SYSINFO_GLOBAL` and is audited: the records name
    /// every principal's sockets and every connection's peer address, so
    /// this is the most privileged of the `stats:net` surfaces — never
    /// open by default (`plans/NETWORK.md` §5).
    pub const NET_SOCKETS: Self = Self(23);

    /// List every bond interface's members and their live health: one
    /// [`NetBondMemberRecord`](crate::net_ipc::NetBondMemberRecord) per
    /// (bond, member) pair (the owning bond, the member alias, whether the
    /// member is the bond's currently-active transmitting member, and its
    /// link/eligibility health), paged by a [`NetInterfaceListRequest`].
    /// The surface `info:net/<bond>/members`,
    /// `state:net/<bond>/active-member`, and per-member health read.
    ///
    /// Requires `CAP_SYSINFO_GLOBAL` and is audited: it exposes the
    /// system-wide link-aggregation topology and its live failover state —
    /// the same cross-principal state class as [`Self::NET_INTERFACE_STATE`]
    /// (`plans/NETWORK.md` §5, §6.3).
    pub const NET_BOND_MEMBERS: Self = Self(24);

    /// List per-CPU processor information: one [`CpuInfoRecord`] per online
    /// CPU (core index, performance class, ISA-extension feature bits, raw
    /// identity register, the fixed reference/timebase frequency, and the
    /// live measured core-clock frequency), paged by a [`CpuInfoListRequest`].
    ///
    /// Ungated: vendor, model, ISA features, topology, and clock speed are
    /// the classic `/proc/cpuinfo` public hardware facts every user may read
    /// — like [`Self::CPU_TIME_STATS`] they expose no per-task, per-user, or
    /// kernel-internal secret. The live frequency is a measured hardware
    /// property, not an address-space or credential oracle. (This differs
    /// from [`Self::CPU_LOAD`], whose queue depths and preemption counters
    /// are scheduler internals gated on `CAP_SYSINFO_KERNEL`.)
    pub const CPU_INFO: Self = Self(25);

    /// Report the host's active recursive-resolver server set: one
    /// [`NetServerAddr`](crate::net_ipc::NetServerAddr) per server
    /// (family and address), paged by a [`NetInterfaceListRequest`]. The
    /// aggregated, deduplicated DHCP-learned ∪ statically-configured DNS
    /// servers the stack maintains (`plans/DNS.md` DNS2), the one source
    /// both a userland resolver client and this read share.
    ///
    /// Ungated: the recursive DNS servers a host queries are public host
    /// configuration — the TAIRiX analogue of a world-readable
    /// `/etc/resolv.conf` — and expose no per-principal secret, so like
    /// [`Self::CPU_INFO`] any principal may read them. (The `netstack`
    /// broker still gates its own `ResolverServers` read on the sysinfo
    /// broker's `CAP_SYSINFO_INTROSPECT` grant; this ungated query is the
    /// *client* surface the broker fronts.)
    pub const NET_RESOLVER_SERVERS: Self = Self(26);

    /// List per-volume storage I/O health: one [`VolumeIoHealthRecord`] per
    /// mounted block-backed volume (its durable volume id, the block-service
    /// endpoint serving it, its current live availability, and the cumulative
    /// [`BlkHealthCounters`] the kernel
    /// filesystem client folded from every completion), paged by a
    /// [`VolumeIoHealthRequest`].
    ///
    /// Requires `CAP_SYSINFO_KERNEL` and is audited: the per-device outcome
    /// tallies (resets, timeouts, reissues, medium errors) are kernel-wide
    /// storage operational state — the surface a failing or flapping disk
    /// becomes visible on (`plans/FIX-IO.md` IO5), the same class of
    /// kernel-internal operational metric as [`Self::MEMORY_PRESSURE`] and
    /// [`Self::RECLAIM_STATS`], not the ungated mount table
    /// ([`Self::MOUNT_LIST`]) every user may read. The current availability
    /// reported here is the same live health the mount table's
    /// [`MountAvailability`] surfaces; this query adds the counters and names
    /// the serving endpoint. It exposes no per-principal secret and no
    /// capability token.
    pub const VOLUME_IO_HEALTH: Self = Self(27);

    /// The current system memory-pressure band alone: a single
    /// [`MemoryPressureBand`].
    ///
    /// Ungated and unaudited, and deliberately so. A process cannot
    /// manage its own memory well without knowing whether the machine
    /// is short of it: a desktop session holding megabytes of
    /// rasterised glyphs and icons must give them back as pressure
    /// rises, in the same order and at the same bands as the kernel's
    /// own caches (`plans/SMARTRAM.md` SMART5). Denying it that would
    /// not protect anything — it would simply make cooperative reclaim
    /// impossible and leave the process to be reclaimed *against*.
    ///
    /// What it exposes is one hysteresis-damped five-level indicator of
    /// the whole machine — no bytes, no watermarks, no per-task,
    /// per-user, or per-address-space figure, and nothing that varies
    /// with another principal's individual allocations. That is a
    /// coarser disclosure than the already-ungated
    /// [`Self::LOAD_AVERAGE`] (which reports the live task census and
    /// the logged-in user count). The detailed view —
    /// [`Self::MEMORY_PRESSURE`], with free and total bytes, every
    /// watermark, and the per-band transition history — remains gated
    /// by `CAP_SYSINFO_KERNEL` and audited, unchanged.
    ///
    /// This is the drain for the edge-triggered
    /// [`WaitSourceKind::MemoryPressure`](crate::WaitSourceKind::MemoryPressure)
    /// wait source: a process parks until the band moves, then reads it
    /// here. It is never a polling surface.
    pub const MEMORY_PRESSURE_BAND: Self = Self(28);

    /// The machine's total usable physical RAM alone, in bytes: a single
    /// [`MemoryTotal`].
    ///
    /// Ungated and unaudited. Installed RAM is a static hardware fact —
    /// the same figure printed on the machine's spec sheet — and carries
    /// no per-process, per-user, or byte-level runtime state; it is a
    /// strictly coarser disclosure than the already-ungated
    /// [`Self::LOAD_AVERAGE`] (which reports the live task census and the
    /// logged-in user count), exactly as [`Self::MEMORY_PRESSURE_BAND`]
    /// is. A userland cache needs this figure to derive a real budget
    /// from the actual machine instead of a hand-picked constant, and
    /// denying it that would not protect anything the gated, audited
    /// [`Self::MEMORY_PRESSURE`] view (free bytes, watermarks, the
    /// reserve, transition history) does not already guard.
    pub const MEMORY_TOTAL: Self = Self(29);

    /// List the live RAID arrays: one
    /// [`RaidArrayRecord`](crate::raid_admin::RaidArrayRecord) per array the
    /// composer serves (its identity, level, health, width, geometry, the
    /// endpoint and node it is published on, and how far a running
    /// verification pass or rebuild has reached), paged by a
    /// [`RaidListRequest`].
    ///
    /// Requires `CAP_SYSINFO_HW` and is audited, like [`Self::HARDWARE_TREE`]
    /// and [`Self::IRQ_LIST`]: how a machine's storage is composed is
    /// hardware topology, not a per-principal fact, and it names the raw
    /// devices a filesystem actually rests on. The composer gates its own
    /// read at the identical bar, so this query is the broker's view of that
    /// answer rather than a way around it. It exposes no secret and no
    /// capability token.
    pub const RAID_ARRAYS: Self = Self(30);

    /// List the devices the RAID composer holds: one
    /// [`RaidMemberRecord`](crate::raid_admin::RaidMemberRecord) per array
    /// member *and* per unaffiliated candidate a new array could be created
    /// over, paged by a [`RaidListRequest`].
    ///
    /// Requires `CAP_SYSINFO_HW` and is audited, for the same reason as
    /// [`Self::RAID_ARRAYS`]. This is the surface that names a bare disk: a
    /// device with no filesystem on it has no volume to appear as, so the
    /// hardware-tree node id reported here is how an administrator names it
    /// when composing an array.
    pub const RAID_MEMBERS: Self = Self(31);

    /// Read the reclaimable-cache ledger one *cache at a time*: one
    /// [`CacheLedgerRecord`] per registered cache — its label, its owner,
    /// its class, whether the figures are kernel-measured or self-reported,
    /// and the same nine figures [`Self::RECLAIM_STATS`] aggregates — paged
    /// by a [`CacheLedgerListRequest`].
    ///
    /// This is the breakdown behind the class totals: a class row says
    /// "disposable UI holds 12 MiB", and these rows say which caches hold
    /// it — the one system-wide glyph-rasterisation cache in the font
    /// service, the per-process glyph client caches, the desktop's icon
    /// artwork, the kernel's block and filesystem caches. Summing every row
    /// of a class reproduces that class's [`ReclaimClassRecord`] exactly.
    ///
    /// Requires `CAP_SYSINFO_KERNEL` and is audited, exactly like
    /// [`Self::RECLAIM_STATS`]: naming every cache in the machine, with the
    /// process holding it, is cross-principal operational state.
    pub const CACHE_LEDGERS: Self = Self(32);

    /// Publish the calling process's **own** cache ledgers so they appear
    /// in [`Self::CACHE_LEDGERS`] and in the [`Self::RECLAIM_STATS`]
    /// totals: a [`CacheReportRequest`] followed by its
    /// [`CacheLedgerRecord`] rows. The reply carries no payload.
    ///
    /// This is the one *submission* in an otherwise read-only API, and it
    /// exists because a userland cache is invisible otherwise. The reclaim
    /// model is deliberately two-sided — the kernel's caches and a
    /// process's rasterised glyphs and decoded artwork obey the same
    /// classes and the same pressure bands — but only the kernel's side can
    /// be measured from outside. Without this the `disposable-ui` class,
    /// the one reclaim *starts* with, reads zero on a desktop holding
    /// megabytes of it.
    ///
    /// Ungated and unaudited per call, for the same reason
    /// [`Self::SELF_PROCESS_LIST`] is: a process describes only itself,
    /// grants nothing, and reads nothing. The submitted rows replace that
    /// process's previous rows rather than accumulating, the service stamps
    /// the kernel-attested identity itself (a caller cannot name another
    /// process), and every row is marked self-reported wherever it is
    /// shown. Self-reported figures are diagnostics: they never feed a
    /// kernel decision, because they never enter the kernel.
    pub const CACHE_REPORT: Self = Self(33);

    /// Read the network stack's **stack-wide** TCP connection-defence
    /// counters: one
    /// [`NetStackDefenceCounters`](crate::net_ipc::NetStackDefenceCounters)
    /// carrying the SYN-backlog, stateless-SYN-cookie, accept-queue, and
    /// reset totals summed over every listener the stack has ever held.
    ///
    /// A single record, not a page: the counters belong to the stack's
    /// socket table as a whole, not to any one interface, so they have no
    /// per-interface home the way [`Self::NET_INTERFACE_COUNTERS`] does.
    ///
    /// Requires `CAP_SYSINFO_GLOBAL` and is audited: these are system-wide,
    /// cross-principal figures, and they are the surface a SYN flood in
    /// progress becomes visible on (`plans/NETWORK.md` §5:
    /// `stats:net/stack/syn-cookies`).
    pub const NET_STACK_DEFENCE: Self = Self(34);

    /// Publish the calling process's **own** compositor frame accounting —
    /// one [`DesktopFrameTotals`] — so it appears in
    /// [`Self::DESKTOP_FRAME_STATS`].
    ///
    /// The second *submission* in an otherwise read-only API, and it exists
    /// for the same reason [`Self::CACHE_REPORT`] does: only the process
    /// that owns the compositor can count pixels, and the kernel — which
    /// every other query reads — knows nothing about them. Without it "the
    /// desktop repaints the screen to move a cursor" is unmeasurable from
    /// outside the desktop's own monitor.
    ///
    /// Ungated and unaudited per call, exactly like [`Self::CACHE_REPORT`]:
    /// a process describes only itself, grants nothing, and reads nothing.
    /// The submitted totals replace that process's previous totals rather
    /// than accumulating, the service stamps the kernel-attested identity
    /// itself (a caller cannot name another process), and the figures are
    /// self-reported diagnostics that no kernel decision reads.
    pub const DESKTOP_FRAME_REPORT: Self = Self(35);

    /// Read the composited-frame accounting every live desktop session has
    /// published: one [`DesktopFrameRecord`] per publishing process, paged
    /// by a [`DesktopFrameStatsRequest`].
    ///
    /// Requires `CAP_SYSINFO_GLOBAL` and is audited: the answer names
    /// another principal — the session process — and its work, which is
    /// cross-principal operational state exactly as
    /// [`Self::GLOBAL_PROCESS_LIST`] and [`Self::NET_STACK_DEFENCE`] are,
    /// not a self-scoped observer.
    pub const DESKTOP_FRAME_STATS: Self = Self(36);

    /// Report the network time servers the host's DHCP client(s) learned:
    /// one [`NetServerAddr`](crate::net_ipc::NetServerAddr) per server,
    /// paged by a [`NetInterfaceListRequest`]. The aggregated, deduplicated
    /// DHCPv4 option 42 / DHCPv6 option 56 servers of every managed
    /// interface's current lease (`plans/TIMESYNC.md` §3).
    ///
    /// Ungated for the same reason as [`Self::NET_RESOLVER_SERVERS`]:
    /// which time server the network *offers* a host is public network
    /// configuration and exposes no per-principal secret. It confers no
    /// authority — the answer is a set of addresses, and only the clock
    /// service (the sole `CAP_TIME_SET` holder) can act on a sample from
    /// one, after validating it.
    pub const NET_TIME_SERVERS: Self = Self(37);

    /// Inclusive upper bound on the query identifier space in `sysinfo-v1`.
    ///
    /// Sized identically to the syscall table so a future query explosion
    /// is accommodated without an ABI break.
    pub const MAX: u16 = 1023;

    /// Wrap a raw value, validating that it falls inside the identifier
    /// space.
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` exceeds [`SysinfoQueryId::MAX`].
    pub const fn from_raw(raw: u16) -> Result<Self, Errno> {
        if raw > Self::MAX {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

/// The slice of the **unfiltered, global** system view a
/// [`sysinfo_introspect`](crate::SyscallNumber::SYSINFO_INTROSPECT) syscall
/// selects.
///
/// This is the *kernel primitive's* vocabulary, distinct from the userland
/// [`SysinfoQueryId`] registry: the kernel answers each domain with the whole
/// system's state and never narrows by principal — the `sysinfod` broker maps
/// its clients' [`SysinfoQueryId`] queries onto these domains and enforces all
/// per-client scoping. The set is closed and fail-closed: an unknown
/// discriminant is rejected rather than guessed.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IntrospectDomain {
    /// The live process table: every task, one packed [`ProcessRecord`], with
    /// the syscall's `arg` naming the record offset to page from.
    Processes = 0,
    /// Kernel memory accounting: a single [`KernelMemoryStats`].
    KernelMemory = 1,
    /// The mount table: every mount, one packed [`MountRecord`], with the
    /// syscall's `arg` naming the record offset to page from.
    Mounts = 2,
    /// Machine identity: a single [`SystemIdentity`].
    Identity = 3,
    /// System uptime and boot wall-clock time: a single [`Uptime`].
    Uptime = 4,
    /// One task's effective resource limits and live usage: the
    /// [`RESOURCE_LIMITS_REPORT_LEN`]-byte positional
    /// `[ResourceLimitRecord; LimitKind::COUNT]` array. The target task is
    /// named by the 128-bit [`ProcId`] the caller writes into the output
    /// buffer on entry (a `u64` `arg` cannot carry it), which the kernel
    /// resolves against the capability table so the answer survives PID reuse.
    TaskLimits = 5,
    /// Scheduler load averages and the logged-in-user census: a single
    /// [`LoadAverage`].
    LoadAverage = 6,
    /// The account directory: every account, one packed
    /// [`UserDirectoryRecord`] (uid + username, no credential material),
    /// with the syscall's `arg` naming the record offset to page from.
    UserDirectory = 7,
    /// Per-CPU execution-time accounting: every online CPU, one packed
    /// [`CpuTimeRecord`], with the syscall's `arg` naming the record offset
    /// to page from.
    CpuTimes = 8,
    /// The seat registry: every seat, one packed [`SeatRecord`], with the
    /// syscall's `arg` naming the record offset to page from.
    Seats = 9,
    /// The live memory-pressure gauge: a single [`MemoryPressureStats`].
    MemoryPressure = 10,
    /// The reclaimable-cache ledger: every registered kernel cache, one
    /// packed [`CacheLedgerRecord`], with the syscall's `arg` naming the
    /// record offset to page from.
    ///
    /// Per *cache*, not per class: the broker folds these rows into the
    /// per-class [`ReclaimClassRecord`] totals its clients see, and does so
    /// in the one place that also holds the self-reported userland rows, so
    /// the two views can never be summed differently.
    CacheLedgers = 11,
    /// The `ramzip` compressed-tier accounting: a single [`RamzipStats`].
    Ramzip = 12,
    /// Per-CPU scheduler load figures: every online CPU, one packed
    /// [`CpuLoadRecord`], with the syscall's `arg` naming the record offset
    /// to page from.
    CpuLoad = 13,
    /// The kernel IRQ table: every bound interrupt line, one packed
    /// [`IrqRecord`], with the syscall's `arg` naming the record offset to
    /// page from.
    Irqs = 14,
    /// The post-mortem crash-record store: every recorded user-fault kill,
    /// one packed [`CrashRecord`], with the syscall's `arg` naming the
    /// record offset to page from.
    Crashes = 15,
    /// Per-CPU processor information: every online CPU, one packed
    /// [`CpuInfoRecord`] (class, ISA features, identity, reference and live
    /// core-clock frequency), with the syscall's `arg` naming the record
    /// offset to page from.
    CpuInfo = 16,
    /// Per-volume storage I/O health: every mounted block-backed volume, one
    /// packed [`VolumeIoHealthRecord`] (durable volume id, serving
    /// block-service endpoint, current availability, and the folded
    /// [`BlkHealthCounters`]), with the
    /// syscall's `arg` naming the record offset to page from.
    VolumeIoHealth = 17,
    /// The published memory-pressure band alone, without taking a fresh
    /// reading of free memory: a single [`MemoryPressureBand`]. Distinct
    /// from [`Self::MemoryPressure`], which samples the gauge and
    /// returns the whole watermark and transition picture.
    MemoryPressureBand = 18,
    /// The machine's total usable physical RAM alone, in bytes: a single
    /// [`MemoryTotal`]. The same figure [`Self::KernelMemory`]'s
    /// `KernelMemoryStats::total_bytes` reports, threaded from the one
    /// frame-allocator source so the two views can never disagree.
    MemoryTotalBytes = 19,
}

impl IntrospectDomain {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a raw discriminant, failing closed on an unknown value.
    ///
    /// Returns [`Errno::OutOfRange`] for any value outside the closed set.
    pub const fn from_u32(raw: u32) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Processes),
            1 => Ok(Self::KernelMemory),
            2 => Ok(Self::Mounts),
            3 => Ok(Self::Identity),
            4 => Ok(Self::Uptime),
            5 => Ok(Self::TaskLimits),
            6 => Ok(Self::LoadAverage),
            7 => Ok(Self::UserDirectory),
            8 => Ok(Self::CpuTimes),
            9 => Ok(Self::Seats),
            10 => Ok(Self::MemoryPressure),
            11 => Ok(Self::CacheLedgers),
            12 => Ok(Self::Ramzip),
            13 => Ok(Self::CpuLoad),
            14 => Ok(Self::Irqs),
            15 => Ok(Self::Crashes),
            16 => Ok(Self::CpuInfo),
            17 => Ok(Self::VolumeIoHealth),
            18 => Ok(Self::MemoryPressureBand),
            19 => Ok(Self::MemoryTotalBytes),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Maximum length, in bytes, of the ASCII `name` of any [`SysinfoQuerySpec`].
///
/// Pinned so that [`ENCODED_QUERY_TABLE`] uses a fixed stride per record and
/// the encoding is computable in a `const fn` without an allocator.
pub const SYSINFO_QUERY_NAME_MAX: usize = 20;

/// Stride, in bytes, of one record inside [`ENCODED_QUERY_TABLE`].
///
/// Layout per record, in [`SysinfoQueryId`] ascending order:
///
/// | Offset | Size | Field |
/// |-------:|-----:|-------|
/// |   0    |  2   | `id` as little-endian `u16` |
/// |   2    |  1   | `required_capability.is_some()` (`0` or `1`) |
/// |   3    |  2   | `required_capability` as little-endian `u16` (`0` when absent) |
/// |   5    |  1   | `audit` (`0` or `1`) |
/// |   6    | 20   | `name`, ASCII, right-padded with `0x00` to 20 bytes |
pub const SYSINFO_QUERY_RECORD_LEN: usize = 6 + SYSINFO_QUERY_NAME_MAX;

/// One row of the frozen `sysinfo-v1` query registry.
///
/// Fields are public and `const`-constructible so the registry can be a
/// `&'static [SysinfoQuerySpec]`. Existing rows must never change; see the
/// module-level frozen-ABI note.
#[derive(Copy, Clone, Debug)]
pub struct SysinfoQuerySpec {
    /// Stable identifier.
    pub id: SysinfoQueryId,
    /// ASCII name. `len <= SYSINFO_QUERY_NAME_MAX`.
    pub name: &'static str,
    /// Capability required to invoke this query, if any.
    ///
    /// `None` means any task may issue the query (its answer is scoped to
    /// the caller's own principal); `Some(cap)` means the serving service
    /// refuses with [`Errno::PermissionDenied`] unless the caller's
    /// effective set contains `cap`.
    pub required_capability: Option<CapabilityId>,
    /// Whether the service must emit an audit record for every invocation.
    ///
    /// Privileged, cross-principal queries are audited; self-scoped
    /// observers are not, to avoid drowning the audit log.
    pub audit: bool,
}

/// The frozen `sysinfo-v1` query registry.
///
/// Indexed by [`SysinfoQueryId::as_u16`]; every entry's array index equals
/// its `id` field (verified by `registry_is_dense_and_ordered`).
pub const SYSINFO_QUERIES: &[SysinfoQuerySpec] = &[
    SysinfoQuerySpec {
        id: SysinfoQueryId::SELF_PROCESS_LIST,
        name: "self_process_list",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::GLOBAL_PROCESS_LIST,
        name: "global_process_list",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::KERNEL_MEMORY_STATS,
        name: "kernel_memory_stats",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::HARDWARE_TREE,
        name: "hardware_tree",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::SYSTEM_IDENTITY,
        name: "system_identity",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::UPTIME,
        name: "uptime",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::MOUNT_LIST,
        name: "mount_list",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::RESOURCE_LIMITS,
        name: "resource_limits",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::PROCESS_IDENTITY,
        name: "process_identity",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::LOAD_AVERAGE,
        name: "load_average",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::USER_DIRECTORY,
        name: "user_directory",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::CPU_TIME_STATS,
        name: "cpu_time_stats",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::SEAT_LIST,
        name: "seat_list",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::MEMORY_PRESSURE,
        name: "memory_pressure",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::RECLAIM_STATS,
        name: "reclaim_stats",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::RAMZIP_STATS,
        name: "ramzip_stats",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::CPU_LOAD,
        name: "cpu_load",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_INTERFACE_FACTS,
        name: "net_interface_facts",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_INTERFACE_STATE,
        name: "net_interface_state",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::IRQ_LIST,
        name: "irq_list",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::CRASH_RECORD,
        name: "crash_record",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_INTERFACE_COUNTERS,
        name: "net_interface_stats",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_INTERFACE_RATES,
        name: "net_interface_rates",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_SOCKETS,
        name: "net_sockets",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_BOND_MEMBERS,
        name: "net_bond_members",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::CPU_INFO,
        name: "cpu_info",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_RESOLVER_SERVERS,
        name: "net_resolver_servers",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::VOLUME_IO_HEALTH,
        name: "volume_io_health",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::MEMORY_PRESSURE_BAND,
        name: "memory_pressure_band",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::MEMORY_TOTAL,
        name: "memory_total",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::RAID_ARRAYS,
        name: "raid_arrays",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::RAID_MEMBERS,
        name: "raid_members",
        required_capability: Some(CapabilityId::SYSINFO_HW),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::CACHE_LEDGERS,
        name: "cache_ledgers",
        required_capability: Some(CapabilityId::SYSINFO_KERNEL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::CACHE_REPORT,
        name: "cache_report",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_STACK_DEFENCE,
        name: "net_stack_defence",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::DESKTOP_FRAME_REPORT,
        name: "desktop_frame_report",
        required_capability: None,
        audit: false,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::DESKTOP_FRAME_STATS,
        name: "desktop_frame_stats",
        required_capability: Some(CapabilityId::SYSINFO_GLOBAL),
        audit: true,
    },
    SysinfoQuerySpec {
        id: SysinfoQueryId::NET_TIME_SERVERS,
        name: "net_time_servers",
        required_capability: None,
        audit: false,
    },
];

/// Length, in bytes, of the canonical encoding in [`ENCODED_QUERY_TABLE`].
///
/// Derived from the registry length so adding a query updates it
/// automatically; the encoder asserts every name fits the fixed stride.
pub const ENCODED_QUERY_TABLE_LEN: usize = SYSINFO_QUERY_RECORD_LEN * SYSINFO_QUERIES.len();

/// Canonical byte representation of [`SYSINFO_QUERIES`].
///
/// Computed in a `const fn` so the encoding is fully determined at compile
/// time. It is the hashable image that pins the `sysinfo-v1` registry: a
/// service and a client built against different registries produce
/// different digests over this buffer. See the [`SYSINFO_QUERY_RECORD_LEN`]
/// layout table.
pub const ENCODED_QUERY_TABLE: [u8; ENCODED_QUERY_TABLE_LEN] = encode_query_table();

const fn encode_query_table() -> [u8; ENCODED_QUERY_TABLE_LEN] {
    let mut out = [0u8; ENCODED_QUERY_TABLE_LEN];
    let mut i = 0;
    while i < SYSINFO_QUERIES.len() {
        let spec = &SYSINFO_QUERIES[i];
        let base = i * SYSINFO_QUERY_RECORD_LEN;
        let [id_lo, id_hi] = spec.id.as_u16().to_le_bytes();
        out[base] = id_lo;
        out[base + 1] = id_hi;
        let (present, cap_id) = match spec.required_capability {
            Some(c) => (1u8, c.as_u16()),
            None => (0u8, 0u16),
        };
        out[base + 2] = present;
        let [c_lo, c_hi] = cap_id.to_le_bytes();
        out[base + 3] = c_lo;
        out[base + 4] = c_hi;
        out[base + 5] = spec.audit as u8;
        let name = spec.name.as_bytes();
        // Reject overlong names at compile time rather than truncate.
        assert!(
            name.len() <= SYSINFO_QUERY_NAME_MAX,
            "sysinfo query name exceeds SYSINFO_QUERY_NAME_MAX"
        );
        let mut n = 0;
        while n < name.len() {
            out[base + 6 + n] = name[n];
            n += 1;
        }
        i += 1;
    }
    out
}

/// Look up the [`SysinfoQuerySpec`] for a given identifier.
///
/// Returns `None` if `id` is not assigned in `sysinfo-v1`.
#[must_use]
pub const fn spec_for(id: SysinfoQueryId) -> Option<&'static SysinfoQuerySpec> {
    let raw = id.as_u16() as usize;
    if raw < SYSINFO_QUERIES.len() {
        let spec = &SYSINFO_QUERIES[raw];
        if spec.id.as_u16() as usize == raw {
            return Some(spec);
        }
    }
    None
}

/// Borrow the canonical registry encoding as a byte slice.
#[must_use]
pub const fn encoded_query_table() -> &'static [u8] {
    &ENCODED_QUERY_TABLE
}

/// Magic word identifying a `sysinfo-v1` request (`"SYI1"` little-endian).
pub const SYSINFO_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"SYI1");

/// Maximum request/response payload length (bytes) advertised by a
/// [`SysinfoRequestHeader`].
///
/// Bounded so a caller cannot trick a service into expecting a payload it
/// cannot represent; far larger than any sensible query argument block.
pub const SYSINFO_MAX_PAYLOAD_LEN: u32 = 1 << 20;

/// Well-known synchronous call-endpoint id the `sysinfod` service binds and
/// clients name in [`crate::SyscallNumber::IPC_CALL`].
///
/// One OS-wide contract, like [`crate::driver_store::DRIVER_STORE_ENDPOINT`]:
/// `sysinfod` publishes this endpoint at startup (an unrestricted-sender
/// endpoint — any process may query, and per-query scoping is enforced by
/// `sysinfod` against each caller's attested origin), and every `sysinfo`
/// client (`lib/procinfo`, the `sysinfo` CLI, `ps`, `top`) posts its
/// framed request here.
pub const SYSINFO_ENDPOINT: u64 = 0x5953_1001;

/// Maximum request payload, in bytes, the [`SYSINFO_ENDPOINT`] accepts: a
/// [`SysinfoRequestHeader`] plus the largest typed argument block any query
/// carries.
///
/// One OS-wide contract shared by the `sysinfod` server (which sizes its
/// endpoint's per-call request capacity by it) and every client (which frames
/// its request within it), so neither carries a private copy that could drift
/// from the other.
///
/// Derived from that largest block rather than hand-picked, so adding a query
/// with a bigger argument cannot silently outgrow the endpoint: every other
/// query's payload is a handful of bytes, and the ceiling is set by a
/// [`SysinfoQueryId::CACHE_REPORT`] carrying its full complement of rows.
pub const SYSINFO_MAX_REQUEST: usize = SysinfoRequestHeader::WIRE_LEN
    + CacheReportRequest::WIRE_LEN
    + MAX_CACHE_REPORT_ENTRIES * CacheLedgerRecord::WIRE_LEN;

/// Maximum reply, in bytes, the [`SYSINFO_ENDPOINT`] delivers: the framed
/// status word ([`SYSINFO_REPLY_STATUS_LEN`]) plus one page of records.
///
/// A client sizes its reply buffer by this bound so a served answer always
/// fits; the server pages a larger list across successive requests (a query
/// that would exceed it fails closed with [`Errno::BufferTooSmall`] so the
/// client shrinks its `limit`/advances its `offset`). One OS-wide contract
/// shared by the server and every client, so neither keeps a private copy.
pub const SYSINFO_MAX_REPLY: usize = 8192;

/// Length, in bytes, of the status word every `sysinfo` reply is prefixed
/// with (see [`encode_reply_ok`] / [`decode_reply`]).
pub const SYSINFO_REPLY_STATUS_LEN: usize = 4;

// The endpoint's message bounds are self-consistent: a framed reply leaves
// room past its status word for a payload, the request bound holds a full
// request header, and both stay within the header's advertised payload
// ceiling — the one contract the `sysinfod` server and every client size
// buffers by. Checked at compile time so a future edit that breaks it fails
// the build rather than a running system.
const _: () = assert!(SYSINFO_MAX_REPLY > SYSINFO_REPLY_STATUS_LEN);
const _: () = assert!(SYSINFO_MAX_REQUEST >= SysinfoRequestHeader::WIRE_LEN);
const _: () = assert!(SYSINFO_MAX_REPLY <= SYSINFO_MAX_PAYLOAD_LEN as usize);
const _: () = assert!(SYSINFO_MAX_REQUEST <= SYSINFO_MAX_PAYLOAD_LEN as usize);
// A full page of the widest record must fit a reply, or the breakdown query
// could never serve even one row.
const _: () = assert!(SYSINFO_MAX_REPLY >= SYSINFO_REPLY_STATUS_LEN + CacheLedgerRecord::WIRE_LEN);
// Every submission must frame within the request bound the cache report
// sizes, or a publisher could not send one.
const _: () =
    assert!(SYSINFO_MAX_REQUEST >= SysinfoRequestHeader::WIRE_LEN + DesktopFrameTotals::WIRE_LEN);

/// Frame a `sysinfo-v1` request: the [`SysinfoRequestHeader`] envelope for
/// `query` followed by its already-encoded `payload`, written into `out`.
///
/// Every client frames its request this way — the command-line tools through
/// the allocating helper in `lib/procinfo`, the runtime's cache reporter
/// directly into a buffer it owns — so the envelope is built in one place
/// and a client cannot invent a variant the service would refuse.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] if `payload` exceeds
///   [`SYSINFO_MAX_PAYLOAD_LEN`].
/// * [`Errno::BufferTooSmall`] if `out` cannot hold the header plus
///   `payload`.
pub fn encode_request(
    query: SysinfoQueryId,
    payload: &[u8],
    out: &mut [u8],
) -> Result<usize, Errno> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| Errno::LengthOutOfRange)?;
    if payload_len > SYSINFO_MAX_PAYLOAD_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    let total = SysinfoRequestHeader::WIRE_LEN + payload.len();
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    let header = SysinfoRequestHeader {
        magic: SYSINFO_REQUEST_MAGIC,
        version: SYSINFO_VERSION_CURRENT,
        flags: 0,
        query,
        reserved: 0,
        payload_len,
        request_id: 0,
    };
    out[..SysinfoRequestHeader::WIRE_LEN].copy_from_slice(&header.to_le_bytes());
    out[SysinfoRequestHeader::WIRE_LEN..total].copy_from_slice(payload);
    Ok(total)
}

/// Frame a **successful** `sysinfo` reply: a zero status word followed by
/// `payload`, written into `out`.
///
/// The server (`sysinfod`) frames every reply this way so a client can tell a
/// served answer from a per-query refusal (e.g. a missing
/// `CAP_SYSINFO_GLOBAL`), which the synchronous call transport itself cannot
/// convey — `call_reply` always succeeds at the transport level.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` cannot hold the status word plus
/// `payload`.
pub fn encode_reply_ok(payload: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
    let total = SYSINFO_REPLY_STATUS_LEN
        .checked_add(payload.len())
        .ok_or(Errno::LengthOutOfRange)?;
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    put_u32(out, 0, 0);
    out[SYSINFO_REPLY_STATUS_LEN..total].copy_from_slice(payload);
    Ok(total)
}

/// Frame an **error** `sysinfo` reply: the non-zero [`Errno`] code as the
/// status word, no payload, written into `out`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` is shorter than
/// [`SYSINFO_REPLY_STATUS_LEN`].
pub fn encode_reply_err(err: Errno, out: &mut [u8]) -> Result<usize, Errno> {
    if out.len() < SYSINFO_REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    // Every `Errno` code is a positive `i32`, so it round-trips through the
    // unsigned status word; `0` is reserved for success and is never an
    // `Errno` code.
    #[allow(clippy::cast_sign_loss)]
    put_u32(out, 0, err.as_i32() as u32);
    Ok(SYSINFO_REPLY_STATUS_LEN)
}

/// Decode a framed `sysinfo` reply: the payload slice on success, or the
/// server's per-query [`Errno`] on a non-zero status word.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] if `bytes` is shorter than the status word.
/// * [`Errno::OutOfRange`] if the status word is a non-zero value that is not
///   a defined [`Errno`] code (wire corruption — fail closed).
/// * the server's reported [`Errno`] when the status word names one.
pub fn decode_reply(bytes: &[u8]) -> Result<&[u8], Errno> {
    if bytes.len() < SYSINFO_REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let status = read_u32(bytes, 0);
    if status == 0 {
        return Ok(&bytes[SYSINFO_REPLY_STATUS_LEN..]);
    }
    // The reinterpretation is the exact inverse of the encoder's; `from_i32`
    // then rejects any word that names no `Errno`, so wire corruption fails
    // closed.
    #[allow(clippy::cast_possible_wrap)]
    Err(Errno::from_i32(status as i32).unwrap_or(Errno::OutOfRange))
}

/// Envelope carried in front of every System Information request.
///
/// The header frames a typed request payload travelling over the IPC
/// transport to `sysinfod`. Total wire size is exactly
/// [`SysinfoRequestHeader::WIRE_LEN`] bytes, encoded little-endian
/// regardless of host architecture.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SysinfoRequestHeader {
    /// Must equal [`SYSINFO_REQUEST_MAGIC`].
    pub magic: u32,
    /// `sysinfo` protocol version; see [`SYSINFO_VERSION_CURRENT`].
    pub version: u16,
    /// Implementation-defined flag bits; reserved bits must be zero.
    pub flags: u16,
    /// Identifies which query the payload addresses.
    pub query: SysinfoQueryId,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u16,
    /// Length of the typed request payload that follows the header.
    pub payload_len: u32,
    /// Caller-chosen correlation token echoed in the response.
    pub request_id: u64,
}

impl SysinfoRequestHeader {
    /// Encoded size of a [`SysinfoRequestHeader`] on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.magic);
        put_u16(&mut out, 4, self.version);
        put_u16(&mut out, 6, self.flags);
        put_u16(&mut out, 8, self.query.as_u16());
        put_u16(&mut out, 10, self.reserved);
        put_u32(&mut out, 12, self.payload_len);
        put_u64(&mut out, 16, self.request_id);
        out
    }

    /// Decode `bytes` into a [`SysinfoRequestHeader`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match, or the
    ///   reserved field is non-zero (reserved-must-be-zero violations are
    ///   wire corruption).
    /// * [`Errno::AbiVersionUnsupported`] if `version` is not
    ///   [`SYSINFO_VERSION_CURRENT`].
    /// * [`Errno::OutOfRange`] if `query` exceeds [`SysinfoQueryId::MAX`].
    /// * [`Errno::LengthOutOfRange`] if `payload_len` exceeds
    ///   [`SYSINFO_MAX_PAYLOAD_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let magic = read_u32(bytes, 0);
        if magic != SYSINFO_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        let version = read_u16(bytes, 4);
        if version != SYSINFO_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        let flags = read_u16(bytes, 6);
        let query = SysinfoQueryId::from_raw(read_u16(bytes, 8))?;
        let reserved = read_u16(bytes, 10);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        let payload_len = read_u32(bytes, 12);
        if payload_len > SYSINFO_MAX_PAYLOAD_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let request_id = read_u64(bytes, 16);
        Ok(Self {
            magic,
            version,
            flags,
            query,
            reserved,
            payload_len,
            request_id,
        })
    }
}

/// Request payload for the process-list queries
/// ([`SysinfoQueryId::SELF_PROCESS_LIST`] and
/// [`SysinfoQueryId::GLOBAL_PROCESS_LIST`]).
///
/// The response is a sequence of [`ProcessRecord`]s; the client pages
/// through it with `offset`/`limit` so a fixed-size transport buffer never
/// has to hold every process at once.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct ProcessListRequest {
    /// Number of leading records to skip.
    pub offset: u32,
    /// Maximum number of records the caller will accept in the response.
    pub limit: u16,
    /// Reserved flag bits; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl ProcessListRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short, or
    /// [`Errno::BadMagic`] if a reserved flag bit is set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Maximum bytes of a process name carried in a [`ProcessRecord`].
pub const PROCESS_NAME_MAX: usize = 32;

/// [`ProcessRecord::cpu`] sentinel: the process is not currently executing
/// on any CPU (it is runnable-but-waiting, blocked, or a zombie).
///
/// A truthful record never reports a fabricated CPU for a task that is not
/// running; it reports this sentinel instead.
pub const PROCESS_CPU_NONE: u8 = 0xFF;

/// Lifecycle state of a process as reported by [`ProcessRecord`].
///
/// Discriminants are part of `sysinfo-v1` and must not be re-numbered.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ProcessState {
    /// Eligible to run, waiting for a CPU.
    Runnable = 0,
    /// Currently executing on a CPU.
    Running = 1,
    /// Blocked awaiting an event (IPC, IRQ, timer).
    Blocked = 2,
    /// Exited; its exit status has not yet been reaped by its parent.
    Zombie = 3,
    /// Stopped by a job-control signal.
    Stopped = 4,
}

impl ProcessState {
    /// Numeric value carried on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte into a [`ProcessState`].
    ///
    /// Returns [`Errno::OutOfRange`] for an unknown discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Runnable),
            1 => Ok(Self::Running),
            2 => Ok(Self::Blocked),
            3 => Ok(Self::Zombie),
            4 => Ok(Self::Stopped),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One process entry in a process-list response.
///
/// Allocation-free: the name is stored inline in a fixed buffer and the
/// valid length is carried alongside it.
///
/// Identity is carried on two axes: the numeric [`pid`](Self::pid) /
/// [`parent_pid`](Self::parent_pid) (the scheduler task ids, familiar and
/// convenient for a `ps`-style display but *reused* across process
/// lifetimes) and the kernel-attested, never-reused
/// [`proc_id`](Self::proc_id) / [`parent_proc_id`](Self::parent_proc_id). A
/// consumer that must correlate a process across time — or distinguish two
/// lifetimes that reused a numeric id — keys on the `proc_id` pair, never the
/// numeric ids. Both axes are attested by the kernel from the task's own
/// state, never from a caller's claim.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProcessRecord {
    /// Numeric process identifier (the scheduler task id).
    ///
    /// For human display and convenience only; numeric ids are reused, so
    /// [`proc_id`](Self::proc_id) is the stable identity.
    pub pid: u64,
    /// Numeric parent process identifier (`0` for a kernel-parented process
    /// such as PID 1).
    ///
    /// Display convenience only; [`parent_proc_id`](Self::parent_proc_id) is
    /// the stable parent link.
    pub parent_pid: u64,
    /// Kernel-attested, unforgeable process-instance identity.
    ///
    /// Never reused, so two lifetimes that reused a numeric [`pid`](Self::pid)
    /// remain distinguishable.
    pub proc_id: ProcId,
    /// Process-instance identity of the parent, or [`ProcId::KERNEL`] for a
    /// kernel-parented process (PID 1, storage-floor drivers).
    ///
    /// Like [`proc_id`](Self::proc_id) it survives numeric PID reuse, so the
    /// parent link is unambiguous even after the parent's numeric id has been
    /// recycled.
    pub parent_proc_id: ProcId,
    /// Owning user identifier.
    pub uid: u32,
    /// Owning (primary) group identifier.
    pub gid: u32,
    /// Lifecycle state.
    pub state: ProcessState,
    /// CPU the process is currently executing on, or [`PROCESS_CPU_NONE`]
    /// when it is not presently scheduled on any CPU.
    pub cpu: u8,
    /// Time-shared scheduling service level, read from the scheduler's own
    /// record (never a caller claim).
    ///
    /// Every process is admitted at [`SchedPriority::Normal`]; the value
    /// changes only through the capability-gated
    /// [`crate::SyscallNumber::SCHED_SET_PRIORITY`] rule, so a consumer
    /// (the Switchboard's "lower priority" recovery action) can render an
    /// already-lowered process as such instead of re-offering the change.
    pub priority: SchedPriority,
    /// Cumulative on-CPU time of the process, in nanoseconds.
    ///
    /// Accounted by the scheduler as the task is dispatched (kernel and
    /// user execution in the task's context alike) and reported through the
    /// architecture's monotonic clock; a task that has never run reports
    /// zero. Consumers derive `%CPU` from the delta between two samples
    /// and `TIME+` from the value directly.
    pub cpu_time_ns: u64,
    /// Bytes of memory currently mapped in the process's address space:
    /// its image, stack, and every anonymous region it has mapped, in
    /// whole pages.
    ///
    /// Zero when the process has no registered user address space (a pure
    /// kernel task) — a truthful "nothing mapped", never a guess.
    pub mem_bytes: u64,
    /// Bytes actually transferred by this process's own file-read system
    /// calls (`fs_read` and the descriptors it delegates to), across its
    /// whole lifetime.
    ///
    /// This is the byte count the kernel really moved on the caller's
    /// behalf, never the length a call merely requested: a short read
    /// advances it by only what came back. It does **not** count pipe,
    /// pty, or resource reads, and it is not block-device traffic — a
    /// cached read that never reaches storage still counts, exactly as
    /// Linux's `rchar` does in `/proc/<pid>/io`. Saturates at [`u64::MAX`]
    /// rather than wrapping. Consumers derive a rate from the delta between
    /// two samples, exactly as for [`cpu_time_ns`](Self::cpu_time_ns).
    pub io_bytes_read: u64,
    /// Bytes actually transferred by this process's own file-write system
    /// calls (`fs_write`), across its whole lifetime. Mirrors
    /// [`io_bytes_read`](Self::io_bytes_read) in every respect but
    /// direction.
    pub io_bytes_written: u64,
    /// Valid byte count in the inline name buffer (`<= PROCESS_NAME_MAX`);
    /// read the bytes through [`ProcessRecord::name_bytes`].
    pub name_len: u8,
    name: [u8; PROCESS_NAME_MAX],
}

impl ProcessRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 92 + PROCESS_NAME_MAX;

    /// Construct a record, copying up to [`PROCESS_NAME_MAX`] bytes of
    /// `name`.
    ///
    /// Returns [`Errno::LengthOutOfRange`] if `name` is longer than
    /// [`PROCESS_NAME_MAX`]; the name is never silently truncated.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pid: u64,
        parent_pid: u64,
        proc_id: ProcId,
        parent_proc_id: ProcId,
        uid: u32,
        gid: u32,
        state: ProcessState,
        cpu: u8,
        priority: SchedPriority,
        cpu_time_ns: u64,
        mem_bytes: u64,
        io_bytes_read: u64,
        io_bytes_written: u64,
        name: &[u8],
    ) -> Result<Self, Errno> {
        if name.len() > PROCESS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; PROCESS_NAME_MAX];
        buf[..name.len()].copy_from_slice(name);
        let name_len = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            pid,
            parent_pid,
            proc_id,
            parent_proc_id,
            uid,
            gid,
            state,
            cpu,
            priority,
            cpu_time_ns,
            mem_bytes,
            io_bytes_read,
            io_bytes_written,
            name_len,
            name: buf,
        })
    }

    /// Borrow the valid prefix of the name buffer.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.pid);
        put_u64(&mut out, 8, self.parent_pid);
        out[16..32].copy_from_slice(&self.proc_id.to_le_bytes());
        out[32..48].copy_from_slice(&self.parent_proc_id.to_le_bytes());
        put_u32(&mut out, 48, self.uid);
        put_u32(&mut out, 52, self.gid);
        out[56] = self.state.as_u8();
        out[57] = self.cpu;
        out[58] = self.name_len;
        out[59] = self.priority as u8;
        put_u64(&mut out, 60, self.cpu_time_ns);
        put_u64(&mut out, 68, self.mem_bytes);
        put_u64(&mut out, 76, self.io_bytes_read);
        put_u64(&mut out, 84, self.io_bytes_written);
        out[92..92 + PROCESS_NAME_MAX].copy_from_slice(&self.name);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short,
    /// [`Errno::OutOfRange`] for an unknown [`ProcessState`] or
    /// [`SchedPriority`] (a zeroed level byte fails closed), or
    /// [`Errno::LengthOutOfRange`] if `name_len` exceeds
    /// [`PROCESS_NAME_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let proc_id = ProcId::from_bytes(&bytes[16..32])?;
        let parent_proc_id = ProcId::from_bytes(&bytes[32..48])?;
        let state = ProcessState::from_u8(bytes[56])?;
        let priority = SchedPriority::from_u32(u32::from(bytes[59]))?;
        let name_len = bytes[58];
        if name_len as usize > PROCESS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut name = [0u8; PROCESS_NAME_MAX];
        name.copy_from_slice(&bytes[92..92 + PROCESS_NAME_MAX]);
        Ok(Self {
            pid: read_u64(bytes, 0),
            parent_pid: read_u64(bytes, 8),
            proc_id,
            parent_proc_id,
            uid: read_u32(bytes, 48),
            gid: read_u32(bytes, 52),
            state,
            cpu: bytes[57],
            priority,
            cpu_time_ns: read_u64(bytes, 60),
            mem_bytes: read_u64(bytes, 68),
            io_bytes_read: read_u64(bytes, 76),
            io_bytes_written: read_u64(bytes, 84),
            name_len,
            name,
        })
    }
}

/// Response payload for [`SysinfoQueryId::KERNEL_MEMORY_STATS`].
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct KernelMemoryStats {
    /// Total usable physical memory (RAM) managed by the kernel, in bytes.
    ///
    /// Counts only frames the kernel can ever allocate: firmware-reserved
    /// regions and physical-address holes (MMIO windows, space below the
    /// RAM base) are excluded, so `total_bytes - free_bytes` is memory
    /// genuinely in use.
    pub total_bytes: u64,
    /// Currently free physical memory, in bytes.
    pub free_bytes: u64,
    /// Memory committed to the kernel's own heaps and slabs, in bytes.
    pub kernel_heap_bytes: u64,
    /// Memory resident in user address spaces, in bytes.
    pub user_resident_bytes: u64,
    /// Page size in bytes for the reporting architecture.
    pub page_size: u32,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
}

impl KernelMemoryStats {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 40;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.total_bytes);
        put_u64(&mut out, 8, self.free_bytes);
        put_u64(&mut out, 16, self.kernel_heap_bytes);
        put_u64(&mut out, 24, self.user_resident_bytes);
        put_u32(&mut out, 32, self.page_size);
        put_u32(&mut out, 36, self.reserved);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`]
    /// if the reserved field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reserved = read_u32(bytes, 36);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            total_bytes: read_u64(bytes, 0),
            free_bytes: read_u64(bytes, 8),
            kernel_heap_bytes: read_u64(bytes, 16),
            user_resident_bytes: read_u64(bytes, 24),
            page_size: read_u32(bytes, 32),
            reserved,
        })
    }
}

/// Response payload for [`SysinfoQueryId::UPTIME`].
///
/// Time is carried with the 64-bit-native ABI types: the
/// monotonic span since boot as a [`Duration64`] and the wall-clock boot
/// instant as a [`Time64`]. Absolute time is never a seconds-only scalar.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Uptime {
    /// Monotonic span since boot.
    pub since_boot: Duration64,
    /// Wall-clock boot instant.
    pub boot_time: Time64,
}

impl Uptime {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = Duration64::WIRE_LEN + Time64::WIRE_LEN;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..Duration64::WIRE_LEN].copy_from_slice(&self.since_boot.to_le_bytes());
        out[Duration64::WIRE_LEN..].copy_from_slice(&self.boot_time.to_le_bytes());
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or
    /// [`Errno::TimestampOutOfRange`] if an encoded sub-second field is not
    /// canonical.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            since_boot: Duration64::from_bytes(&bytes[0..Duration64::WIRE_LEN])?,
            boot_time: Time64::from_bytes(&bytes[Duration64::WIRE_LEN..])?,
        })
    }
}

/// Binary point of the fixed-point load-average values in [`LoadAverage`]:
/// a load of 1.00 is `1 << LOAD_FIXED_SHIFT`.
pub const LOAD_FIXED_SHIFT: u32 = 11;

/// Response payload for [`SysinfoQueryId::LOAD_AVERAGE`] — the `uptime(1)`
/// figures: exponentially-damped 1/5/15-minute run-queue averages, the
/// live task census, and the number of distinct logged-in users.
///
/// The averages are fixed-point with [`LOAD_FIXED_SHIFT`] fractional bits;
/// [`LoadAverage::whole`] and [`LoadAverage::centis`] render the
/// conventional `W.CC` form from one shared definition.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct LoadAverage {
    /// One-minute damped average of the runnable-task count (fixed-point).
    pub load1: u32,
    /// Five-minute damped average (fixed-point).
    pub load5: u32,
    /// Fifteen-minute damped average (fixed-point).
    pub load15: u32,
    /// Tasks currently runnable or running.
    pub runnable: u32,
    /// Live (non-zombie) tasks in total.
    pub total_tasks: u32,
    /// Distinct non-system uids owning at least one live task — the
    /// logged-in-user census.
    pub users: u32,
}

impl LoadAverage {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Whole part of a fixed-point load value.
    #[must_use]
    pub const fn whole(fixed: u32) -> u32 {
        fixed >> LOAD_FIXED_SHIFT
    }

    /// Hundredths part of a fixed-point load value, `0..=99`.
    #[must_use]
    pub const fn centis(fixed: u32) -> u32 {
        ((fixed & ((1 << LOAD_FIXED_SHIFT) - 1)) * 100) >> LOAD_FIXED_SHIFT
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.load1.to_le_bytes());
        out[4..8].copy_from_slice(&self.load5.to_le_bytes());
        out[8..12].copy_from_slice(&self.load15.to_le_bytes());
        out[12..16].copy_from_slice(&self.runnable.to_le_bytes());
        out[16..20].copy_from_slice(&self.total_tasks.to_le_bytes());
        out[20..24].copy_from_slice(&self.users.to_le_bytes());
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if `bytes` is shorter than
    /// [`Self::WIRE_LEN`]. Every field value is representable, so no other
    /// shape check applies.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        Ok(Self {
            load1: word(0),
            load5: word(4),
            load15: word(8),
            runnable: word(12),
            total_tasks: word(16),
            users: word(20),
        })
    }
}

/// Maximum bytes of a username carried in a [`UserDirectoryRecord`] — the
/// same bound the `users-v1` database enforces on account names.
pub const USER_DIRECTORY_NAME_MAX: usize = 32;

/// Request payload for [`SysinfoQueryId::USER_DIRECTORY`]: the record
/// window to return, mirroring the process-list paging shape.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct UserDirectoryRequest {
    /// Zero-based index of the first record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl UserDirectoryRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short, or
    /// [`Errno::BadMagic`] if a reserved flag bit is set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One account entry in a [`SysinfoQueryId::USER_DIRECTORY`] response: the
/// numeric uid and the account's username, and **nothing else** — no
/// password material, home, shell, or grant set. The `/etc/passwd`-class
/// public pairing a `top`/`ls -l`-style display renders.
///
/// Allocation-free: the name is stored inline in a fixed buffer with its
/// valid length alongside.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UserDirectoryRecord {
    /// The account's numeric user identifier.
    pub uid: u32,
    /// Valid byte count in the inline name buffer
    /// (`<= USER_DIRECTORY_NAME_MAX`); read the bytes through
    /// [`UserDirectoryRecord::name_bytes`].
    pub name_len: u8,
    name: [u8; USER_DIRECTORY_NAME_MAX],
}

impl UserDirectoryRecord {
    /// Encoded size on the wire: uid + name length + three reserved bytes
    /// + the inline name buffer.
    pub const WIRE_LEN: usize = 8 + USER_DIRECTORY_NAME_MAX;

    /// Construct a record, copying up to [`USER_DIRECTORY_NAME_MAX`] bytes
    /// of `name`.
    ///
    /// Returns [`Errno::LengthOutOfRange`] if `name` is longer than
    /// [`USER_DIRECTORY_NAME_MAX`]; the name is never silently truncated.
    pub fn new(uid: u32, name: &[u8]) -> Result<Self, Errno> {
        if name.len() > USER_DIRECTORY_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; USER_DIRECTORY_NAME_MAX];
        buf[..name.len()].copy_from_slice(name);
        let name_len = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            uid,
            name_len,
            name: buf,
        })
    }

    /// Borrow the valid prefix of the name buffer.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.uid);
        out[4] = self.name_len;
        // out[5..8] reserved, already zero.
        out[8..8 + USER_DIRECTORY_NAME_MAX].copy_from_slice(&self.name);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if the slice is short, or
    /// [`Errno::LengthOutOfRange`] if `name_len` exceeds
    /// [`USER_DIRECTORY_NAME_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let name_len = bytes[4];
        if name_len as usize > USER_DIRECTORY_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut name = [0u8; USER_DIRECTORY_NAME_MAX];
        name.copy_from_slice(&bytes[8..8 + USER_DIRECTORY_NAME_MAX]);
        Ok(Self {
            uid: read_u32(bytes, 0),
            name_len,
            name,
        })
    }
}

/// Request payload for [`SysinfoQueryId::CPU_TIME_STATS`].
///
/// Structurally parallel to [`MountListRequest`] but a distinct frozen
/// payload: each `sysinfo-v1` query owns its argument type. The response is
/// a sequence of [`CpuTimeRecord`]s; the client pages through it with
/// `offset`/`limit` so a fixed-size transport buffer never bounds how many
/// CPUs the machine may have.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuTimeListRequest {
    /// Index of the first CPU to return.
    pub offset: u32,
    /// Maximum number of [`CpuTimeRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl CpuTimeListRequest {
    /// Encoded size of a [`CpuTimeListRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`CpuTimeListRequest`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero
    ///   (reserved-must-be-zero violations are wire corruption).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One CPU's execution-time accounting inside a
/// [`SysinfoQueryId::CPU_TIME_STATS`] response.
///
/// `busy_ns` is the cumulative time this CPU has spent dispatching task
/// bodies since boot, accounted on the scheduler's dispatch path; `idle_ns`
/// is the remainder of the same monotonic sample instant, so
/// `busy_ns + idle_ns` is that CPU's share of uptime at the sample. A
/// consumer derives a utilisation percentage from the *deltas* of two
/// samples, exactly as `top` reads `/proc/stat` on Linux; TAIRiX does not
/// account a user/system/nice/iowait split, so the honest vocabulary is
/// busy and idle only.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuTimeRecord {
    /// The CPU index this record describes.
    pub cpu: u32,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
    /// Cumulative nanoseconds this CPU spent running tasks since boot.
    pub busy_ns: u64,
    /// Nanoseconds of the sample's uptime this CPU was not running tasks.
    pub idle_ns: u64,
}

impl CpuTimeRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.cpu);
        put_u32(&mut out, 4, self.reserved);
        put_u64(&mut out, 8, self.busy_ns);
        put_u64(&mut out, 16, self.idle_ns);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`]
    /// if the reserved field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reserved = read_u32(bytes, 4);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            cpu: read_u32(bytes, 0),
            reserved,
            busy_ns: read_u64(bytes, 8),
            idle_ns: read_u64(bytes, 16),
        })
    }
}

/// Request payload for [`SysinfoQueryId::SEAT_LIST`].
///
/// Identical paging shape to [`CpuTimeListRequest`]: `offset` names the
/// first seat-record index to return and `limit` bounds the page.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct SeatListRequest {
    /// Index of the first seat record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl SeatListRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set (fail closed on an unknown request shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Request payload for [`SysinfoQueryId::HARDWARE_TREE`].
///
/// Identical paging shape to [`SeatListRequest`]: `offset` names the first
/// [`HwNode`](crate::hwtree::HwNode) index to return and `limit` bounds the
/// page. The reply prefixes every page with the snapshot's
/// [`HwTreeHeader`](crate::hwtree::HwTreeHeader) so the client always sees
/// the total node count and the generation the page was served from.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct HardwareTreeRequest {
    /// Index of the first node record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl HardwareTreeRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set (fail closed on an unknown request shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// [`SeatRecord::flags`] bit: the seat is currently held under a lease and
/// [`SeatRecord::owner_task`] names the owning task.
pub const SEAT_FLAG_OWNED: u32 = 1 << 0;

/// One seat's state inside a [`SysinfoQueryId::SEAT_LIST`] response
/// (`plans/DISPLAY.md` D3).
///
/// Every field is filled from the kernel's seat registry — the
/// kernel-attested owner task, never a caller claim. An unowned seat (which
/// includes one whose lease was just revoked) carries no owner: the
/// [`SEAT_FLAG_OWNED`] bit is clear and `owner_task` is zero.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct SeatRecord {
    /// The seat this record describes (the boot seat is id 0; further
    /// seats are minted per discovered display node).
    pub seat_id: u64,
    /// The task holding the seat's lease; valid only when
    /// [`SEAT_FLAG_OWNED`] is set, zero otherwise.
    pub owner_task: u64,
    /// The seat's monotonic lease-grant counter: the generation of the most
    /// recently minted lease, `0` if the seat has never been acquired.
    pub generation: u64,
    /// Index of the text console an unowned seat's input drains to.
    pub foreground_console: u32,
    /// [`SEAT_FLAG_OWNED`] plus reserved bits that must be zero.
    pub flags: u32,
}

impl SeatRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 32;

    /// `true` when the seat is held under a live lease.
    #[must_use]
    pub const fn owned(&self) -> bool {
        self.flags & SEAT_FLAG_OWNED != 0
    }

    /// The owning task, if the seat is held.
    #[must_use]
    pub const fn owner(&self) -> Option<u64> {
        if self.owned() {
            Some(self.owner_task)
        } else {
            None
        }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.seat_id);
        put_u64(&mut out, 8, self.owner_task);
        put_u64(&mut out, 16, self.generation);
        put_u32(&mut out, 24, self.foreground_console);
        put_u32(&mut out, 28, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set or an unowned record carries a non-zero
    /// owner (fail closed on wire corruption rather than fabricating an
    /// owner).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u32(bytes, 28);
        if flags & !SEAT_FLAG_OWNED != 0 {
            return Err(Errno::BadMagic);
        }
        let owner_task = read_u64(bytes, 8);
        if flags & SEAT_FLAG_OWNED == 0 && owner_task != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            seat_id: read_u64(bytes, 0),
            owner_task,
            generation: read_u64(bytes, 16),
            foreground_console: read_u32(bytes, 24),
            flags,
        })
    }
}

/// Bytes of the per-installation machine identifier.
pub const MACHINE_ID_LEN: usize = 16;

/// Maximum bytes of a hostname carried in a [`SystemIdentity`].
pub const HOSTNAME_MAX: usize = 64;

/// Response payload for [`SysinfoQueryId::SYSTEM_IDENTITY`].
///
/// Allocation-free: the hostname is stored inline in a fixed buffer with
/// its valid length alongside.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SystemIdentity {
    /// Per-installation machine identifier minted by the installer.
    pub machine_id: [u8; MACHINE_ID_LEN],
    /// OS major version.
    pub version_major: u16,
    /// OS minor version.
    pub version_minor: u16,
    /// OS patch version.
    pub version_patch: u16,
    /// Valid byte count in the inline hostname buffer (`<= HOSTNAME_MAX`);
    /// read the bytes through [`SystemIdentity::hostname_bytes`].
    pub hostname_len: u8,
    hostname: [u8; HOSTNAME_MAX],
}

impl SystemIdentity {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = MACHINE_ID_LEN + 8 + HOSTNAME_MAX;

    /// Construct an identity, copying up to [`HOSTNAME_MAX`] bytes of
    /// `hostname`.
    ///
    /// Returns [`Errno::LengthOutOfRange`] if `hostname` is too long; the
    /// name is never silently truncated.
    pub fn new(
        machine_id: [u8; MACHINE_ID_LEN],
        version_major: u16,
        version_minor: u16,
        version_patch: u16,
        hostname: &[u8],
    ) -> Result<Self, Errno> {
        if hostname.len() > HOSTNAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; HOSTNAME_MAX];
        buf[..hostname.len()].copy_from_slice(hostname);
        let hostname_len = u8::try_from(hostname.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            machine_id,
            version_major,
            version_minor,
            version_patch,
            hostname_len,
            hostname: buf,
        })
    }

    /// Borrow the valid prefix of the hostname buffer.
    #[must_use]
    pub fn hostname_bytes(&self) -> &[u8] {
        &self.hostname[..self.hostname_len as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..MACHINE_ID_LEN].copy_from_slice(&self.machine_id);
        put_u16(&mut out, MACHINE_ID_LEN, self.version_major);
        put_u16(&mut out, MACHINE_ID_LEN + 2, self.version_minor);
        put_u16(&mut out, MACHINE_ID_LEN + 4, self.version_patch);
        out[MACHINE_ID_LEN + 6] = self.hostname_len;
        // byte MACHINE_ID_LEN + 7 reserved, already zero.
        out[MACHINE_ID_LEN + 8..].copy_from_slice(&self.hostname);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or
    /// [`Errno::LengthOutOfRange`] if `hostname_len` exceeds
    /// [`HOSTNAME_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut machine_id = [0u8; MACHINE_ID_LEN];
        machine_id.copy_from_slice(&bytes[0..MACHINE_ID_LEN]);
        let hostname_len = bytes[MACHINE_ID_LEN + 6];
        if hostname_len as usize > HOSTNAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut hostname = [0u8; HOSTNAME_MAX];
        hostname.copy_from_slice(&bytes[MACHINE_ID_LEN + 8..Self::WIRE_LEN]);
        Ok(Self {
            machine_id,
            version_major: read_u16(bytes, MACHINE_ID_LEN),
            version_minor: read_u16(bytes, MACHINE_ID_LEN + 2),
            version_patch: read_u16(bytes, MACHINE_ID_LEN + 4),
            hostname_len,
            hostname,
        })
    }
}

/// Maximum bytes of the source identifier carried in a [`MountRecord`].
///
/// The source is the backing volume or device name (e.g. a `/Storage`
/// volume label); it shares the path-length ceiling with the target.
pub const MOUNT_SOURCE_MAX: usize = 64;

/// Maximum bytes of the mount-point path carried in a [`MountRecord`].
pub const MOUNT_TARGET_MAX: usize = 64;

/// Maximum bytes of the filesystem-type name carried in a [`MountRecord`].
///
/// Driver type names (`arxfs`, `ext4`, `fat32`, …) are short; the bound
/// keeps a hostile reply from claiming an unbounded type string.
pub const MOUNT_FSTYPE_MAX: usize = 16;

/// Byte length of the stable volume identity a [`MountRecord`] carries —
/// the same 16-byte identity the volume forest publishes for `id::` paths
/// and a `volume_detach` request names. All-zero when the mount has no
/// published volume identity (the in-RAM layout mounts).
pub const MOUNT_VOLUME_ID_LEN: usize = 16;

/// A mounted volume's availability, as reported by
/// [`SysinfoQueryId::MOUNT_LIST`] (`plans/DEVICES.md` D4).
///
/// A surprise-removed volume stays visible in the mount table — its root,
/// alias, and mount point remain published while its retained uncommitted
/// data awaits verified re-insert or an explicit force-unmount — so the
/// listing must say it is not serving I/O rather than show a volume that
/// looks healthy.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MountAvailability {
    /// The backing volume is live and serving I/O.
    Available = 0,
    /// The serving device vanished with uncommitted writes retained for
    /// verified re-insert or an explicit force-unmount; every operation
    /// fails closed with a device fault.
    UnavailableDirty = 1,
    /// The serving device vanished after write retention was abandoned:
    /// uncommitted data existed that is not held.
    UnavailableLost = 2,
    /// The volume was re-inserted but non-mutation could not be proven
    /// (or retention had been abandoned), so it is mounted fresh and
    /// read-only with its retained set still held: the conflict is
    /// resolved only by the audited force-unmount, which discards the
    /// set — never silently (`plans/DEVICES.md` D4c).
    RecoveryConflict = 3,
    /// The backing device is live and still serving I/O, but reports
    /// itself unhealthy (a recovered-error threshold, a pending sector
    /// reallocation): data is served, so the mount stays usable, but a
    /// tool must show it as at-risk rather than healthy. Surfaced from the
    /// device's reported health (`plans/FIX-IO.md` IO3), never a vanish.
    Degraded = 4,
    /// The backing device stalled/reset and is inside its bounded recovery
    /// grace window: its I/O is being ridden out reissuably while it is
    /// given a bounded chance to come back (`plans/FIX-IO.md` IO3). A
    /// transient, live-device state — distinct from the `Unavailable*`
    /// vanish states — that resolves to [`Available`](Self::Available) on
    /// recovery or, if the device never comes back, to a vanish state via
    /// the surprise-removal path.
    Recovering = 5,
}

impl MountAvailability {
    /// The wire byte for this availability state.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover an availability state from its wire byte.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` names no known state (fail
    /// closed — an unknown state is never presumed available).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Available),
            1 => Ok(Self::UnavailableDirty),
            2 => Ok(Self::UnavailableLost),
            3 => Ok(Self::RecoveryConflict),
            4 => Ok(Self::Degraded),
            5 => Ok(Self::Recovering),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The live-device availability a served volume should report given the
    /// most recent device-level health [`BlkStatus`] its block client
    /// observed, or [`None`] when that status carries no volume-availability
    /// signal and the stored availability must stand.
    ///
    /// This is the single definition of how a device's reported I/O health
    /// maps onto the mount table (`plans/FIX-IO.md` IO2/IO3), shared by
    /// every block consumer so they cannot classify a device differently.
    /// The serving driver owns the sticky health state machine and its
    /// recovery grace window ([`crate::blkio::BlkHealth`]); the consumer
    /// only *reflects* the driver's per-request verdict here, so there is no
    /// second, divergent state machine:
    ///
    /// * [`BlkStatus::Ok`] → [`Available`](Self::Available): a valid answer
    ///   demonstrates the device is serving normally (and clears a prior
    ///   recovering/degraded overlay — the driver would not answer `Ok`
    ///   until it genuinely recovered).
    /// * [`BlkStatus::Degraded`] → [`Degraded`](Self::Degraded): served, but
    ///   the device reports itself unhealthy.
    /// * [`BlkStatus::TransientError`] / [`BlkStatus::Timeout`] /
    ///   [`BlkStatus::Reset`] → [`Recovering`](Self::Recovering): the driver
    ///   is riding out a blip inside its grace window.
    /// * [`BlkStatus::MediumError`] → [`None`]: a per-request bad-sector
    ///   verdict says nothing about the *volume's* availability (the device
    ///   itself is reachable), so the overlay is left unchanged.
    /// * [`BlkStatus::Offline`] / [`BlkStatus::Removed`] /
    ///   [`BlkStatus::Fatal`] → [`None`]: a gone or dead device is owned by
    ///   the surprise-removal path (`plans/DEVICES.md` D4), which sets the
    ///   authoritative `Unavailable*` state; the health overlay never
    ///   competes with it.
    #[must_use]
    pub const fn from_block_status(status: BlkStatus) -> Option<Self> {
        match status {
            BlkStatus::Ok => Some(Self::Available),
            BlkStatus::Degraded => Some(Self::Degraded),
            BlkStatus::TransientError | BlkStatus::Timeout | BlkStatus::Reset => {
                Some(Self::Recovering)
            }
            BlkStatus::MediumError | BlkStatus::Offline | BlkStatus::Removed | BlkStatus::Fatal => {
                None
            }
        }
    }

    /// How far from fully available this state is, as a rank two states can be
    /// ordered by: the higher the rank, the less the volume can be relied on.
    /// The order is available → serving-but-unwell → riding-out-a-blip →
    /// serving-read-only-in-conflict → vanished-with-writes-held → vanished:
    /// [`Available`](Self::Available) < [`Degraded`](Self::Degraded) <
    /// [`Recovering`](Self::Recovering) <
    /// [`RecoveryConflict`](Self::RecoveryConflict) <
    /// [`UnavailableDirty`](Self::UnavailableDirty) <
    /// [`UnavailableLost`](Self::UnavailableLost).
    ///
    /// The three live states rank in the order [`crate::raid::ArrayHealth`]
    /// already documents (optimal, degraded, recovering); a state that still
    /// serves reads outranks one that fails every operation closed, so
    /// `RecoveryConflict` — a read-only mount whose retained write set is
    /// still held — sits below the vanish states, and `UnavailableLost`, the
    /// one state that admits data is gone, is last.
    ///
    /// This is the single, explicit definition of "which availability wins"
    /// when a stack of storage layers each report their own
    /// ([`crate::driver::block::Block::backing_availability`]), so
    /// [`worse_of`](Self::worse_of) and every consumer that folds them cannot
    /// order them differently. It is kept deliberately independent of the wire
    /// byte [`as_u8`](Self::as_u8) so the transport encoding and the
    /// fold precedence can never silently couple.
    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Available => 0,
            Self::Degraded => 1,
            Self::Recovering => 2,
            Self::RecoveryConflict => 3,
            Self::UnavailableDirty => 4,
            Self::UnavailableLost => 5,
        }
    }

    /// The state that most conservatively describes a volume both `self` and
    /// `other` apply to: the further-from-available of the two by
    /// [`severity`](Self::severity).
    ///
    /// This is how a composed storage element folds what it can promise with
    /// what the elements beneath it report — a filesystem over an array over
    /// disks — so background work stands down on the worst answer anywhere in
    /// the stack rather than on the topmost layer's optimism. Because the
    /// ranking is total, the fold is associative and commutative: a stack can
    /// be walked in any order for the same result.
    #[must_use]
    pub const fn worse_of(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }

    /// Classify the change from `prev` to `next` as a storage-health audit
    /// transition, or [`None`] when the change carries no such signal.
    ///
    /// This is the single definition of *when a served volume's health has
    /// materially changed* (`plans/FIX-IO.md` IO5), shared by every consumer
    /// that keeps a live availability overlay so they cannot classify a
    /// transition differently and cannot each emit their own idea of a
    /// recovery. It is edge-triggered: an unchanged state yields [`None`], so
    /// a run of identical completions logs one event, not one per request.
    ///
    /// Only the live health overlay's own states — [`Available`](Self::Available),
    /// [`Degraded`](Self::Degraded), and [`Recovering`](Self::Recovering) —
    /// take part. Any transition into or out of a vanish state
    /// ([`UnavailableDirty`](Self::UnavailableDirty) /
    /// [`UnavailableLost`](Self::UnavailableLost) /
    /// [`RecoveryConflict`](Self::RecoveryConflict)) yields [`None`]: those
    /// are owned by the D4 surprise-removal path (`plans/DEVICES.md`), which
    /// logs its own events, and letting them produce a health event here
    /// would double-count a removal or fabricate a "recovery" for a
    /// re-inserted disk that the re-insert path already reports.
    #[must_use]
    pub const fn health_transition(prev: Self, next: Self) -> Option<BlkHealthTransition> {
        match (prev, next) {
            (Self::Available | Self::Recovering, Self::Degraded) => {
                Some(BlkHealthTransition::Degraded)
            }
            (Self::Available | Self::Degraded, Self::Recovering) => {
                Some(BlkHealthTransition::Recovering)
            }
            (Self::Degraded | Self::Recovering, Self::Available) => {
                Some(BlkHealthTransition::Recovered)
            }
            _ => None,
        }
    }
}

/// An edge-triggered change in a served volume's live health, derived by
/// [`MountAvailability::health_transition`] for the storage-health audit
/// trail (`plans/FIX-IO.md` IO5).
///
/// This names *what changed*; the numeric audit-log event id is assigned by
/// the subsystem that emits the record (the kernel block client, a user-space
/// block driver), each in its own reserved event-id range, exactly as the
/// same logical driver/lifecycle events already carry per-subsystem ids.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlkHealthTransition {
    /// A volume that was healthy or recovering began reporting itself
    /// unhealthy while still serving I/O (a recovered-error threshold, a
    /// pending sector reallocation).
    Degraded,
    /// A volume's backing device stalled or reset and entered its bounded
    /// recovery grace window; its I/O is being ridden out reissuably while it
    /// is given a bounded chance to come back.
    Recovering,
    /// A degraded or recovering volume returned to healthy service — the
    /// "the disk came back to life" recovery: a disk that stalled or reported
    /// itself unwell can recover, and that return is logged as a recovery
    /// rather than a fault.
    Recovered,
}

impl BlkHealthTransition {
    /// Classify a *device-level* health-state change — the edge a serving
    /// block driver's own [`BlkHealth`](crate::blkio::BlkHealth) machine makes
    /// from `prev` to `next` — as a storage-health audit transition, or
    /// [`None`] when the change carries no such signal.
    ///
    /// This is the device-side counterpart of the consumer-side
    /// [`MountAvailability::health_transition`], sharing the **same**
    /// vocabulary so a driver process and the kernel block client cannot
    /// classify a recovery or a degrade differently
    /// (`plans/FIX-IO.md` IO5). Both are edge-triggered: an unchanged
    /// state yields [`None`], so a run of identical outcomes logs one event,
    /// not one per request.
    ///
    /// Only the healthy/unwell/recovering edges take part, and they map to the
    /// same three events the mount side emits:
    /// * into [`BlkHealthState::Degraded`] → [`Degraded`](Self::Degraded);
    /// * into [`BlkHealthState::Recovering`] → [`Recovering`](Self::Recovering)
    ///   (the device entered its bounded grace window);
    /// * back to [`BlkHealthState::Healthy`] from any unwell-but-not-removed
    ///   state → [`Recovered`](Self::Recovered) (the disk came back).
    ///
    /// The fail-closed edges (into [`BlkHealthState::Faulted`] /
    /// [`Offline`](BlkHealthState::Offline) / [`Failed`](BlkHealthState::Failed))
    /// yield [`None`]: they are not a Degraded/Recovering/Recovered signal but
    /// the grace window elapsing, which the driver logs as its own distinct
    /// fail-closed event. Any edge touching
    /// [`BlkHealthState::Removed`] also yields [`None`]: a surprise removal and
    /// its verified re-insert are owned by the D4 hotplug path, exactly as the
    /// mount-side classifier excludes its vanish states, so a removal is never
    /// double-counted and a re-insert never fabricates a recovery here.
    #[must_use]
    pub const fn for_device(prev: BlkHealthState, next: BlkHealthState) -> Option<Self> {
        use crate::blkio::BlkHealthState::{
            Degraded, Failed, Faulted, Healthy, Offline, Recovering, Removed,
        };
        match (prev, next) {
            // No health edge is derived here in two cases. A surprise removal
            // and its verified re-insert (either direction) are owned by the
            // D4 hotplug path, which logs its own events, so a removal is never
            // double-counted nor a re-insert turned into a fabricated recovery.
            // And an unchanged live/recovering state is not a transition, so a
            // run of identical outcomes logs one event, not one per request.
            (Removed, _)
            | (_, Removed)
            | (Healthy, Healthy)
            | (Degraded, Degraded)
            | (Recovering, Recovering) => None,
            (_, Degraded) => Some(Self::Degraded),
            (_, Recovering) => Some(Self::Recovering),
            // A return to healthy service from any unwell-but-present state is
            // the "came back" recovery.
            (Degraded | Recovering | Faulted | Offline | Failed, Healthy) => Some(Self::Recovered),
            _ => None,
        }
    }

    /// Classify a *fault-domain* health-state change — the edge an interior
    /// node's [`FaultDomain`](crate::blkio::FaultDomain) machine (a bus, hub,
    /// USB/SAS controller, expander, or PCIe root complex) makes from `prev`
    /// to `next` — as a storage-health audit transition, or [`None`] when the
    /// change carries no such signal.
    ///
    /// This is the interior-node counterpart of the per-device
    /// [`for_device`](Self::for_device) and the consumer-side
    /// [`MountAvailability::health_transition`], sharing the **same**
    /// vocabulary so a hub reset, a leaf-device blip, and a mount overlay
    /// cannot classify a recovery differently (`plans/FIX-IO.md` IO5).
    /// It is edge-triggered exactly like its siblings: an unchanged state
    /// yields [`None`], so an owner that keeps resetting logs one grace-window
    /// event, not one per reset.
    ///
    /// An interior fault domain has no "degraded-but-serving" state of its own
    /// (that is per-device), so this classifier emits only two of the three
    /// events:
    /// * into [`FaultDomainState::Recovering`](crate::blkio::FaultDomainState::Recovering)
    ///   → [`Recovering`](Self::Recovering) — the owner blipped and the whole
    ///   subtree entered its one shared grace window;
    /// * back to [`FaultDomainState::Healthy`](crate::blkio::FaultDomainState::Healthy)
    ///   from a recovering *or an already-failed* subtree →
    ///   [`Recovered`](Self::Recovered) — the owner demonstrably returned (a
    ///   `resume` clears an `Offline` subtree with no reboot), the "the hub
    ///   came back" recovery.
    ///
    /// The fail-closed edge (into
    /// [`FaultDomainState::Offline`](crate::blkio::FaultDomainState::Offline),
    /// the grace window elapsing without the owner returning) yields [`None`]:
    /// like the per-device fail-closed edges, it is not a
    /// Degraded/Recovering/Recovered signal but the subtree failing closed,
    /// which the fault-domain driver logs as its own distinct event.
    #[must_use]
    pub const fn for_fault_domain(
        prev: crate::blkio::FaultDomainState,
        next: crate::blkio::FaultDomainState,
    ) -> Option<Self> {
        use crate::blkio::FaultDomainState::{Healthy, Offline, Recovering};
        match (prev, next) {
            // No recovery-vocabulary edge in two cases. The subtree failing
            // closed (into `Offline`, including the unchanged `Offline` case)
            // is the fault-domain driver's own distinct fail-closed event, not
            // a Recovering/Recovered signal. And an unchanged live/recovering
            // state is not a transition, so a run of identical observations (a
            // continuing owner reset) logs one event.
            (_, Offline) | (Healthy, Healthy) | (Recovering, Recovering) => None,
            // The owner blipped and the subtree entered its shared grace window.
            (_, Recovering) => Some(Self::Recovering),
            // The owner demonstrably returned — from inside the window or from
            // an already-failed subtree — the "came back" recovery.
            (_, Healthy) => Some(Self::Recovered),
        }
    }
}

/// Request payload for [`SysinfoQueryId::MOUNT_LIST`].
///
/// Structurally parallel to [`ProcessListRequest`] but a distinct frozen
/// payload: each `sysinfo-v1` query owns its argument type, exactly as each
/// syscall owns its argument shape. The response is a
/// sequence of [`MountRecord`]s; the client pages through it with
/// `offset`/`limit` so a fixed-size transport buffer never has to hold every
/// mount at once.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct MountListRequest {
    /// Index of the first mount to return.
    pub offset: u32,
    /// Maximum number of [`MountRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl MountListRequest {
    /// Encoded size of a [`MountListRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`MountListRequest`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero
    ///   (reserved-must-be-zero violations are wire corruption).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Request payload for [`SysinfoQueryId::NET_INTERFACE_FACTS`] and
/// [`SysinfoQueryId::NET_INTERFACE_STATE`]: the record window to
/// return, in the stack's stable interface-table order.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetInterfaceListRequest {
    /// Index of the first interface to return.
    pub offset: u32,
    /// Maximum number of records the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl NetInterfaceListRequest {
    /// Encoded size of a [`NetInterfaceListRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`NetInterfaceListRequest`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero
    ///   (reserved-must-be-zero violations are wire corruption).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Request payload for [`SysinfoQueryId::NET_INTERFACE_RATES`]: the record
/// window to return (in the stack's stable interface-table order) plus the
/// rate-averaging window the caller requests.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetInterfaceRatesRequest {
    /// Index of the first interface to return.
    pub offset: u32,
    /// Maximum number of records the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
    /// The rate-averaging window the caller requests.
    pub window: Duration64,
}

impl NetInterfaceRatesRequest {
    /// Encoded size of a [`NetInterfaceRatesRequest`] on the wire: the
    /// paging header (8) followed by the window (12).
    pub const WIRE_LEN: usize = 8 + Duration64::WIRE_LEN;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out[8..8 + Duration64::WIRE_LEN].copy_from_slice(&self.window.to_le_bytes());
        out
    }

    /// Decode `bytes` into a [`NetInterfaceRatesRequest`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero.
    /// * [`Errno::TimestampOutOfRange`] if the window is non-canonical.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        let window = Duration64::from_bytes(&bytes[8..8 + Duration64::WIRE_LEN])?;
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
            window,
        })
    }
}

/// One entry in the system mount table, returned by
/// [`SysinfoQueryId::MOUNT_LIST`].
///
/// Each record names a mounted filesystem by its backing `source`, the
/// `target` path it is mounted at, the driver `fstype`, the
/// [`MountFlags`] policy in force (`ro`/`nosuid`/`nodev`/`noexec`), and the
/// volume's space accounting as a [`VolumeStats`] — the same type the
/// filesystem driver ABI defines, so the numbers cross the wire in the one
/// shape the driver reported them in.
/// The string fields are inline fixed-capacity buffers, so the whole record
/// is a flat, allocation-free `repr(C)` block of [`MountRecord::WIRE_LEN`]
/// bytes encoded little-endian. The policy bits reuse the same
/// [`MountFlags`] type the filesystem driver ABI defines rather than
/// re-declaring the flag algebra.
///
/// A mount with no live backing volume (or one whose driver reports no
/// accounting) carries an all-zero [`VolumeStats`]: zero total blocks is the
/// honest "no capacity known" answer a consumer skips, never a guess.
///
/// Each record also reports the volume's [`MountAvailability`] — so a
/// surprise-removed volume never masquerades as healthy — and its stable
/// 16-byte volume identity (all-zero when none is published), the same
/// identity a `volume_detach` request names, so the unmount tooling can
/// resolve a catalog name to the volume it detaches without a second query
/// surface.
///
/// The record also carries the backing block device's storage
/// [`medium`](MountRecord::medium) ([`BlkDeviceClass`]), so a consumer such
/// as the file manager can show a medium-appropriate drive icon instead of
/// guessing. A mount with no block backing — a synthetic or view mount — and
/// any medium byte the decoder does not understand both report `None`
/// (unknown): the record never fabricates a class it does not know.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MountRecord {
    flags: MountFlags,
    source_len: u8,
    target_len: u8,
    fstype_len: u8,
    availability: MountAvailability,
    medium: u8,
    usage: VolumeStats,
    volume_id: [u8; MOUNT_VOLUME_ID_LEN],
    source: [u8; MOUNT_SOURCE_MAX],
    target: [u8; MOUNT_TARGET_MAX],
    fstype: [u8; MOUNT_FSTYPE_MAX],
}

/// The live state of the volume behind a mount: how much of it is in use,
/// whether it is still answering, and what medium it sits on.
///
/// The three are learned together from one live mount and rendered together
/// by a consumer, so they cross [`MountRecord::new`] as one value rather
/// than as three positional arguments a caller can silently transpose.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MountVolumeState {
    /// The volume's space accounting, all-zero when no backing volume
    /// reported one.
    pub usage: VolumeStats,
    /// Whether the backing volume is still serving I/O.
    pub availability: MountAvailability,
    /// The storage medium of the backing block device, or `None` when the
    /// mount has no block backing (a synthetic or view mount).
    pub medium: Option<BlkDeviceClass>,
}

impl MountRecord {
    /// Encoded size of a [`MountRecord`] on the wire.
    ///
    /// `4` bytes of flags, three length bytes plus the availability byte,
    /// the storage-medium byte and the reserved bytes that align what
    /// follows, the usage block (`block_size(4)` + reserved pad `(4)` + five
    /// `u64` counts), the 16-byte volume identity, then the three
    /// fixed-capacity string buffers.
    pub const WIRE_LEN: usize =
        Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX + MOUNT_TARGET_MAX + MOUNT_FSTYPE_MAX;

    /// The storage-medium byte, which the record owns outright. It sits with
    /// the other per-mount scalars rather than in the usage block, whose pad
    /// word belongs to the shared [`VolumeStats`] the driver ABI defines and
    /// stays reserved-must-be-zero.
    const MEDIUM_OFFSET: usize = 8;
    /// Where the bytes held back after the medium begin. They pad the usage
    /// block that follows to its eight-byte alignment and are
    /// reserved-must-be-zero.
    const RESERVED_OFFSET: usize = Self::MEDIUM_OFFSET + 1;
    const USAGE_OFFSET: usize = 16;
    const VOLUME_ID_OFFSET: usize = Self::USAGE_OFFSET + 48;
    const SOURCE_OFFSET: usize = Self::VOLUME_ID_OFFSET + MOUNT_VOLUME_ID_LEN;
    const TARGET_OFFSET: usize = Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX;
    const FSTYPE_OFFSET: usize = Self::TARGET_OFFSET + MOUNT_TARGET_MAX;

    /// Wire byte for a mount's storage medium.
    ///
    /// `0` is unknown; a known class is its [`BlkDeviceClass`] discriminant
    /// plus one, so unknown stays representable and distinct from
    /// `Rotational` (discriminant `0`). The inverse is
    /// [`medium`](Self::medium), which reads the byte back off a decoded
    /// record.
    ///
    /// The C view of this record publishes these values as its
    /// `TAIRIX_MOUNT_MEDIUM_*` constants, generated by reading this function
    /// rather than re-typing the numbers.
    #[must_use]
    pub const fn medium_to_wire(medium: Option<BlkDeviceClass>) -> u8 {
        match medium {
            None => 0,
            Some(BlkDeviceClass::Rotational) => 1,
            Some(BlkDeviceClass::SolidState) => 2,
            Some(BlkDeviceClass::Removable) => 3,
            Some(BlkDeviceClass::Virtual) => 4,
        }
    }

    /// Recover a storage medium from its wire byte, failing closed to unknown
    /// for any value the ABI does not define rather than guessing a class.
    const fn medium_from_wire(byte: u8) -> Option<BlkDeviceClass> {
        match byte {
            1 => Some(BlkDeviceClass::Rotational),
            2 => Some(BlkDeviceClass::SolidState),
            3 => Some(BlkDeviceClass::Removable),
            4 => Some(BlkDeviceClass::Virtual),
            _ => None,
        }
    }

    /// Build a record from its parts.
    ///
    /// `state` is the backing volume's live state — its usage, availability,
    /// and storage medium. `volume_id` is the volume's stable published
    /// identity, or all-zero when the mount has none (the in-RAM layout
    /// mounts).
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `source` exceeds [`MOUNT_SOURCE_MAX`],
    /// `target` exceeds [`MOUNT_TARGET_MAX`], or `fstype` exceeds
    /// [`MOUNT_FSTYPE_MAX`]; [`Errno::OutOfRange`] if the state's usage is
    /// internally inconsistent (available exceeding free, or free exceeding
    /// total).
    pub fn new(
        source: &[u8],
        target: &[u8],
        fstype: &[u8],
        flags: MountFlags,
        state: MountVolumeState,
        volume_id: [u8; MOUNT_VOLUME_ID_LEN],
    ) -> Result<Self, Errno> {
        if source.len() > MOUNT_SOURCE_MAX
            || target.len() > MOUNT_TARGET_MAX
            || fstype.len() > MOUNT_FSTYPE_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        let usage = state.usage;
        if usage.avail_blocks > usage.free_blocks || usage.free_blocks > usage.total_blocks {
            return Err(Errno::OutOfRange);
        }
        let source_len = u8::try_from(source.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let target_len = u8::try_from(target.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let fstype_len = u8::try_from(fstype.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let mut record = Self {
            flags,
            source_len,
            target_len,
            fstype_len,
            availability: state.availability,
            medium: Self::medium_to_wire(state.medium),
            usage,
            volume_id,
            source: [0u8; MOUNT_SOURCE_MAX],
            target: [0u8; MOUNT_TARGET_MAX],
            fstype: [0u8; MOUNT_FSTYPE_MAX],
        };
        record.source[..source.len()].copy_from_slice(source);
        record.target[..target.len()].copy_from_slice(target);
        record.fstype[..fstype.len()].copy_from_slice(fstype);
        Ok(record)
    }

    /// The mount policy flags in force on this filesystem.
    #[must_use]
    pub fn flags(&self) -> MountFlags {
        self.flags
    }

    /// The backing volume's availability.
    #[must_use]
    pub fn availability(&self) -> MountAvailability {
        self.availability
    }

    /// The storage medium of the block device backing this mount, or `None`
    /// when the medium is unknown — a mount with no block backing (a
    /// synthetic or view mount), or a decoded record whose medium byte the
    /// ABI does not define. `None` is the honest "unknown", never a guessed
    /// class.
    #[must_use]
    pub const fn medium(&self) -> Option<BlkDeviceClass> {
        Self::medium_from_wire(self.medium)
    }

    /// The volume's stable published identity, or all-zero when the mount
    /// has none.
    #[must_use]
    pub fn volume_id(&self) -> [u8; MOUNT_VOLUME_ID_LEN] {
        self.volume_id
    }

    /// The volume's space accounting (all-zero when no backing volume
    /// reported one).
    #[must_use]
    pub fn usage(&self) -> VolumeStats {
        self.usage
    }

    /// The backing source bytes (volume/device identifier).
    #[must_use]
    pub fn source_bytes(&self) -> &[u8] {
        &self.source[..self.source_len as usize]
    }

    /// The mount-point path bytes.
    #[must_use]
    pub fn target_bytes(&self) -> &[u8] {
        &self.target[..self.target_len as usize]
    }

    /// The filesystem-type name bytes.
    #[must_use]
    pub fn fstype_bytes(&self) -> &[u8] {
        &self.fstype[..self.fstype_len as usize]
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.flags.bits());
        out[4] = self.source_len;
        out[5] = self.target_len;
        out[6] = self.fstype_len;
        out[7] = self.availability.as_u8();
        out[Self::MEDIUM_OFFSET] = self.medium;
        put_u32(&mut out, Self::USAGE_OFFSET, self.usage.block_size);
        put_u64(&mut out, Self::USAGE_OFFSET + 8, self.usage.total_blocks);
        put_u64(&mut out, Self::USAGE_OFFSET + 16, self.usage.free_blocks);
        put_u64(&mut out, Self::USAGE_OFFSET + 24, self.usage.avail_blocks);
        put_u64(&mut out, Self::USAGE_OFFSET + 32, self.usage.files);
        put_u64(&mut out, Self::USAGE_OFFSET + 40, self.usage.files_free);
        out[Self::VOLUME_ID_OFFSET..Self::VOLUME_ID_OFFSET + MOUNT_VOLUME_ID_LEN]
            .copy_from_slice(&self.volume_id);
        out[Self::SOURCE_OFFSET..Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX]
            .copy_from_slice(&self.source);
        out[Self::TARGET_OFFSET..Self::TARGET_OFFSET + MOUNT_TARGET_MAX]
            .copy_from_slice(&self.target);
        out[Self::FSTYPE_OFFSET..Self::FSTYPE_OFFSET + MOUNT_FSTYPE_MAX]
            .copy_from_slice(&self.fstype);
        out
    }

    /// Decode `bytes` into a [`MountRecord`].
    ///
    /// A medium byte the ABI does not define decodes to `None` (unknown)
    /// rather than a wrong class or a whole-record refusal — the medium is
    /// advisory, so an unrecognised value fails closed to "unknown", not to
    /// an error.
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::OutOfRange`] if the flags word sets a bit outside the
    ///   known [`MountFlags`] mask, the availability byte names no known
    ///   state, the reserved bytes after the medium or the usage block's
    ///   reserved pad are non-zero, or the usage block is internally
    ///   inconsistent (available exceeding free, or free exceeding total — a
    ///   hostile reply, refused whole).
    /// * [`Errno::LengthOutOfRange`] if any length byte exceeds its buffer.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[Self::RESERVED_OFFSET..Self::USAGE_OFFSET]
            .iter()
            .any(|byte| *byte != 0)
            || read_u32(bytes, Self::USAGE_OFFSET + 4) != 0
        {
            return Err(Errno::OutOfRange);
        }
        // Normalised on the way in, so a byte the ABI does not define is
        // held — and re-emitted — as unknown rather than relayed onward.
        let medium = Self::medium_to_wire(Self::medium_from_wire(bytes[Self::MEDIUM_OFFSET]));
        let availability = MountAvailability::from_u8(bytes[7])?;
        let flags = MountFlags::from_bits(read_u32(bytes, 0)).map_err(|_| Errno::OutOfRange)?;
        let source_len = bytes[4];
        let target_len = bytes[5];
        let fstype_len = bytes[6];
        if source_len as usize > MOUNT_SOURCE_MAX
            || target_len as usize > MOUNT_TARGET_MAX
            || fstype_len as usize > MOUNT_FSTYPE_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        let usage = VolumeStats {
            block_size: read_u32(bytes, Self::USAGE_OFFSET),
            total_blocks: read_u64(bytes, Self::USAGE_OFFSET + 8),
            free_blocks: read_u64(bytes, Self::USAGE_OFFSET + 16),
            avail_blocks: read_u64(bytes, Self::USAGE_OFFSET + 24),
            files: read_u64(bytes, Self::USAGE_OFFSET + 32),
            files_free: read_u64(bytes, Self::USAGE_OFFSET + 40),
        };
        if usage.avail_blocks > usage.free_blocks || usage.free_blocks > usage.total_blocks {
            return Err(Errno::OutOfRange);
        }
        let mut volume_id = [0u8; MOUNT_VOLUME_ID_LEN];
        let mut source = [0u8; MOUNT_SOURCE_MAX];
        let mut target = [0u8; MOUNT_TARGET_MAX];
        let mut fstype = [0u8; MOUNT_FSTYPE_MAX];
        volume_id.copy_from_slice(
            &bytes[Self::VOLUME_ID_OFFSET..Self::VOLUME_ID_OFFSET + MOUNT_VOLUME_ID_LEN],
        );
        source.copy_from_slice(&bytes[Self::SOURCE_OFFSET..Self::SOURCE_OFFSET + MOUNT_SOURCE_MAX]);
        target.copy_from_slice(&bytes[Self::TARGET_OFFSET..Self::TARGET_OFFSET + MOUNT_TARGET_MAX]);
        fstype.copy_from_slice(&bytes[Self::FSTYPE_OFFSET..Self::FSTYPE_OFFSET + MOUNT_FSTYPE_MAX]);
        Ok(Self {
            flags,
            source_len,
            target_len,
            fstype_len,
            availability,
            medium,
            usage,
            volume_id,
            source,
            target,
            fstype,
        })
    }
}

/// One row of the [`SysinfoQueryId::RESOURCE_LIMITS`] response: a resource's
/// effective soft/hard bound and the caller's current live usage of it.
///
/// The full response is exactly [`LimitKind::COUNT`] records packed
/// back-to-back in [`LimitKind`] discriminant order — its byte length is
/// [`RESOURCE_LIMITS_REPORT_LEN`] — so a client reads them positionally and
/// the `kind` field is a self-describing cross-check rather than a sort key.
///
/// `usage` is expressed in the resource's natural unit: bytes for the
/// `*Bytes` kinds and a plain count otherwise. It is informational; unlike
/// `limit` it carries no well-formedness invariant.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ResourceLimitRecord {
    /// Which resource this row describes.
    pub kind: LimitKind,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
    /// The effective limit (soft/hard) currently in force for the caller.
    pub limit: ResourceLimit,
    /// Current live usage of the resource, in its natural unit (see the
    /// type-level note). Informational; carries no invariant.
    pub usage: u64,
}

impl ResourceLimitRecord {
    /// Encoded size on the wire.
    ///
    /// Layout, little-endian: `kind` (`u32`, offset 0), `reserved` (`u32`,
    /// offset 4), `limit` ([`ResourceLimit`], offset 8), `usage` (`u64`,
    /// offset 24).
    pub const WIRE_LEN: usize = 8 + ResourceLimit::WIRE_LEN + 8;

    /// Construct a record for `kind` with effective `limit` and live `usage`.
    #[must_use]
    pub const fn new(kind: LimitKind, limit: ResourceLimit, usage: u64) -> Self {
        Self {
            kind,
            reserved: 0,
            limit,
            usage,
        }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.kind.as_u32());
        put_u32(&mut out, 4, self.reserved);
        out[8..8 + ResourceLimit::WIRE_LEN].copy_from_slice(&self.limit.encode());
        put_u64(&mut out, 24, self.usage);
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than
    ///   [`WIRE_LEN`](Self::WIRE_LEN).
    /// * [`Errno::BadMagic`] if the reserved field is non-zero (a
    ///   reserved-must-be-zero violation is wire corruption).
    /// * [`Errno::OutOfRange`] if `kind` is not an `abi-v1` [`LimitKind`]
    ///   discriminant, or the embedded [`ResourceLimit`] is not well-formed
    ///   (`soft > hard`).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reserved = read_u32(bytes, 4);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        let kind = LimitKind::from_u32(read_u32(bytes, 0))?;
        let limit = ResourceLimit::decode(&bytes[8..8 + ResourceLimit::WIRE_LEN])?;
        Ok(Self {
            kind,
            reserved,
            limit,
            usage: read_u64(bytes, 24),
        })
    }
}

/// Byte length of a full [`SysinfoQueryId::RESOURCE_LIMITS`] response: one
/// [`ResourceLimitRecord`] per [`LimitKind`], in discriminant order.
pub const RESOURCE_LIMITS_REPORT_LEN: usize = ResourceLimitRecord::WIRE_LEN * LimitKind::COUNT;

/// Number of memory-pressure bands in `sysinfo-v1` (normal, mild,
/// moderate, severe, critical — depth order, shallowest first).
pub const PRESSURE_BAND_COUNT: usize = 5;

/// Number of *entered* bands (every band but normal): the deeper four
/// carry enter/exit watermarks in [`MemoryPressureStats`].
pub const PRESSURE_WATERMARK_COUNT: usize = PRESSURE_BAND_COUNT - 1;

/// Stable display names of the five pressure bands, indexed by band depth.
pub const PRESSURE_BAND_NAMES: [&str; PRESSURE_BAND_COUNT] =
    ["normal", "mild", "moderate", "severe", "critical"];

/// Response payload for [`SysinfoQueryId::MEMORY_PRESSURE`].
///
/// Reports the live five-band gauge: the current band, the readings it
/// folded, the derived watermarks actually in force (reported, never
/// promised — a re-derivation after a policy tune simply reports the new
/// values), and the per-band entry counters since boot.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct MemoryPressureStats {
    /// Current band depth: an index into [`PRESSURE_BAND_NAMES`].
    pub band: u8,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: [u8; 7],
    /// Byte size of the backing resource the gauge watches.
    pub total_bytes: u64,
    /// Free bytes at the sample.
    pub free_bytes: u64,
    /// The reserve floor in bytes: below this the system is critical
    /// regardless of band history, and no cache growth may dip into it.
    pub reserve_bytes: u64,
    /// Enter watermarks (bytes free) for mild, moderate, severe,
    /// critical: the band is entered when the free reading drops below
    /// its watermark.
    pub enter_bytes: [u64; PRESSURE_WATERMARK_COUNT],
    /// Exit watermarks (bytes free) for mild, moderate, severe,
    /// critical: the band is left when the free reading rises above its
    /// watermark (the hysteresis gap).
    pub exit_bytes: [u64; PRESSURE_WATERMARK_COUNT],
    /// Times each band has been entered since boot, indexed by depth.
    pub band_entries: [u64; PRESSURE_BAND_COUNT],
}

impl MemoryPressureStats {
    /// Encoded size on the wire.
    ///
    /// Layout, little-endian: `band` (`u8`, offset 0), `reserved`
    /// (7 bytes, offset 1), `total_bytes` (offset 8), `free_bytes`
    /// (offset 16), `reserve_bytes` (offset 24), `enter_bytes`
    /// (4 × `u64`, offset 32), `exit_bytes` (4 × `u64`, offset 64),
    /// `band_entries` (5 × `u64`, offset 96).
    pub const WIRE_LEN: usize =
        8 + 3 * 8 + 2 * PRESSURE_WATERMARK_COUNT * 8 + PRESSURE_BAND_COUNT * 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = self.band;
        out[1..8].copy_from_slice(&self.reserved);
        put_u64(&mut out, 8, self.total_bytes);
        put_u64(&mut out, 16, self.free_bytes);
        put_u64(&mut out, 24, self.reserve_bytes);
        for (i, value) in self.enter_bytes.iter().enumerate() {
            put_u64(&mut out, 32 + i * 8, *value);
        }
        for (i, value) in self.exit_bytes.iter().enumerate() {
            put_u64(&mut out, 64 + i * 8, *value);
        }
        for (i, value) in self.band_entries.iter().enumerate() {
            put_u64(&mut out, 96 + i * 8, *value);
        }
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::BadMagic`] if a reserved byte is non-zero.
    /// * [`Errno::OutOfRange`] if `band` is not a `sysinfo-v1` band depth.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut reserved = [0u8; 7];
        reserved.copy_from_slice(&bytes[1..8]);
        if reserved != [0u8; 7] {
            return Err(Errno::BadMagic);
        }
        let band = bytes[0];
        if usize::from(band) >= PRESSURE_BAND_COUNT {
            return Err(Errno::OutOfRange);
        }
        let mut enter_bytes = [0u64; PRESSURE_WATERMARK_COUNT];
        for (i, slot) in enter_bytes.iter_mut().enumerate() {
            *slot = read_u64(bytes, 32 + i * 8);
        }
        let mut exit_bytes = [0u64; PRESSURE_WATERMARK_COUNT];
        for (i, slot) in exit_bytes.iter_mut().enumerate() {
            *slot = read_u64(bytes, 64 + i * 8);
        }
        let mut band_entries = [0u64; PRESSURE_BAND_COUNT];
        for (i, slot) in band_entries.iter_mut().enumerate() {
            *slot = read_u64(bytes, 96 + i * 8);
        }
        Ok(Self {
            band,
            reserved,
            total_bytes: read_u64(bytes, 8),
            free_bytes: read_u64(bytes, 16),
            reserve_bytes: read_u64(bytes, 24),
            enter_bytes,
            exit_bytes,
            band_entries,
        })
    }
}

/// Response payload for [`SysinfoQueryId::MEMORY_PRESSURE_BAND`] — the
/// published band and nothing else.
///
/// Deliberately the smallest useful answer. A process that must shrink
/// its own caches as the machine tightens needs the band and only the
/// band; free bytes, watermarks, and transition history belong to the
/// gated [`MemoryPressureStats`] view.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct MemoryPressureBand {
    /// Current band depth: an index into [`PRESSURE_BAND_NAMES`].
    pub band: u8,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: [u8; 7],
}

impl MemoryPressureBand {
    /// Encoded size on the wire.
    ///
    /// Layout, little-endian: `band` (`u8`, offset 0), `reserved`
    /// (7 bytes, offset 1).
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = self.band;
        out[1..8].copy_from_slice(&self.reserved);
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::OutOfRange`] if the band is not one of the
    ///   [`PRESSURE_BAND_COUNT`] known depths, or a reserved byte is
    ///   non-zero — an unrecognised encoding is refused, never
    ///   interpreted as the shallowest band.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let band = bytes[0];
        if usize::from(band) >= PRESSURE_BAND_COUNT {
            return Err(Errno::OutOfRange);
        }
        let mut reserved = [0u8; 7];
        reserved.copy_from_slice(&bytes[1..8]);
        if reserved != [0u8; 7] {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { band, reserved })
    }
}

/// Response payload for [`SysinfoQueryId::MEMORY_TOTAL`] — the machine's
/// total usable physical RAM alone, in bytes.
///
/// Carved out of the gated [`KernelMemoryStats::total_bytes`] figure, not
/// measured a second way, so the ungated and gated views can never
/// disagree. A static hardware fact rather than a runtime reading: it
/// changes only when RAM is physically added or removed.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct MemoryTotal {
    /// Total usable physical RAM the kernel manages, in bytes.
    pub total_bytes: u64,
}

impl MemoryTotal {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.total_bytes);
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] unless `bytes` is exactly
    /// [`Self::WIRE_LEN`] long — a fixed-size scalar record has no
    /// legitimate short or trailing-byte encoding, so both are refused
    /// alike rather than silently truncated or zero-extended.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            total_bytes: read_u64(bytes, 0),
        })
    }
}

/// Number of reclaim classes in `sysinfo-v1`.
///
/// Mirrors the kernel reclaim ledger's closed class set; the shared
/// names below are the classification's stable vocabulary, so the
/// resolver's `stats:mem/reclaim/<class>` selectors and the kernel
/// encoder can never spell a class differently.
pub const RECLAIM_CLASS_COUNT: usize = 9;

/// Stable names of the reclaim classes, indexed by class id.
pub const RECLAIM_CLASS_NAMES: [&str; RECLAIM_CLASS_COUNT] = [
    "disposable-ui",
    "predictive-prefetch",
    "background-validation",
    "semantic-app-cache",
    "runtime-cache",
    "clean-file-data",
    "transform-cache",
    "fs-metadata",
    "reliability-assist",
];

/// Class id a [`CacheLedgerRecord`] carries for a **pinned** pool: bytes
/// the reclaim model accounts but can never take, because they exist
/// nowhere else and can only be written out.
///
/// It sits one past the reclaim classes rather than among them. A reclaim
/// class answers "when is this given back under pressure", and for a
/// pinned pool the answer is "never" — so folding one into the taxonomy
/// would make [`RECLAIM_CLASS_NAMES`]'s ordering a fiction and let a
/// reclaim decision count unreclaimable bytes as headroom. A ledger row
/// may name it; the per-class reclaim totals of [`ReclaimClassRecord`]
/// never do.
pub const CACHE_CLASS_PINNED: u8 = 9;

/// Number of class ids a [`CacheLedgerRecord`] may carry: the reclaim
/// classes plus [`CACHE_CLASS_PINNED`].
pub const CACHE_CLASS_COUNT: usize = RECLAIM_CLASS_COUNT + 1;

// The pinned id sits immediately past the last reclaim class, so adding a
// reclaim class without moving it is a compile error rather than a silently
// overlapping vocabulary.
const _: () = assert!(CACHE_CLASS_PINNED as usize == RECLAIM_CLASS_COUNT);

/// Stable name of a cache-ledger row's class id, or `None` for an id this
/// build does not define (fail closed: never a guessed class).
///
/// Reads through [`RECLAIM_CLASS_NAMES`] rather than repeating it, so the
/// reclaim vocabulary has one definition and the pinned row is the single
/// name added beside it.
#[must_use]
pub fn cache_class_name(class: u8) -> Option<&'static str> {
    if class == CACHE_CLASS_PINNED {
        return Some("pinned");
    }
    RECLAIM_CLASS_NAMES.get(usize::from(class)).copied()
}

/// Look up a reclaim class id by its stable name. Fails closed: an
/// unknown name is `None`, never a guessed class.
#[must_use]
pub fn reclaim_class_from_name(name: &str) -> Option<u8> {
    RECLAIM_CLASS_NAMES
        .iter()
        .position(|&candidate| candidate == name)
        .and_then(|index| u8::try_from(index).ok())
}

/// Request payload for [`SysinfoQueryId::RECLAIM_STATS`].
///
/// Structurally parallel to [`CpuTimeListRequest`] but a distinct frozen
/// payload: each `sysinfo-v1` query owns its argument type. The response
/// is a sequence of [`ReclaimClassRecord`]s paged with `offset`/`limit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct ReclaimListRequest {
    /// Index of the first class record to return.
    pub offset: u32,
    /// Maximum number of [`ReclaimClassRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl ReclaimListRequest {
    /// Encoded size of a [`ReclaimListRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`ReclaimListRequest`].
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One reclaim class's ledger figures inside a
/// [`SysinfoQueryId::RECLAIM_STATS`] response.
///
/// Byte figures are live gauges (they rise and fall as caches charge and
/// discharge); the event counters are monotonic since boot. All figures
/// are aggregated across every registered cache ledger, so the record
/// describes the *class*, not any one cache instance — the per-cache
/// breakdown behind it is [`SysinfoQueryId::CACHE_LEDGERS`].
///
/// The aggregate spans both sides of the reclaim model: the kernel's own
/// measured ledgers and the ledgers processes report for their own caches
/// ([`SysinfoQueryId::CACHE_REPORT`]). [`Self::self_reported_bytes`] says
/// how much of the resident total came from the latter, so a reader can
/// see at a glance how much of a class it is taking on trust.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct ReclaimClassRecord {
    /// The class this record describes: an index into
    /// [`RECLAIM_CLASS_NAMES`].
    pub class: u8,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: [u8; 7],
    /// Cached payload bytes currently held for the class.
    pub payload_bytes: u64,
    /// Bookkeeping metadata bytes currently held for the class.
    pub metadata_bytes: u64,
    /// Entries currently held for the class.
    pub entries: u64,
    /// Admissions refused since boot.
    pub refusals: u64,
    /// Pressure-driven shrink passes that hit the class since boot.
    pub pressure_shrinks: u64,
    /// Whole-cache teardown drains that hit the class since boot.
    pub teardowns: u64,
    /// Internal failures (poisoned ledgers) attributed to the class
    /// since boot.
    pub failures: u64,
    /// Lookups of the class served from cache since boot (the cache
    /// avoided the canonical source): the numerator of the class's
    /// hit ratio, the direct measure of the cache's effectiveness.
    pub hits: u64,
    /// Lookups of the class that fell through to the canonical source
    /// since boot: the miss half of the hit ratio.
    pub misses: u64,
    /// How many of `payload_bytes + metadata_bytes` came from ledgers a
    /// process reported for its own caches rather than from a ledger the
    /// kernel measures.
    ///
    /// A self-reported figure is a diagnostic, not evidence: the process
    /// holding the memory is the only thing that can see it, and a
    /// compromised one can lie about it. Kept separable so a reader is
    /// never misled about which part of a class total is attested.
    pub self_reported_bytes: u64,
}

impl ReclaimClassRecord {
    /// Encoded size on the wire: the class byte plus 7 reserved bytes,
    /// then ten `u64` figures.
    pub const WIRE_LEN: usize = 8 + 10 * 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = self.class;
        out[1..8].copy_from_slice(&self.reserved);
        for (i, value) in [
            self.payload_bytes,
            self.metadata_bytes,
            self.entries,
            self.refusals,
            self.pressure_shrinks,
            self.teardowns,
            self.failures,
            self.hits,
            self.misses,
            self.self_reported_bytes,
        ]
        .iter()
        .enumerate()
        {
            put_u64(&mut out, 8 + i * 8, *value);
        }
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::BadMagic`] if a reserved byte is non-zero.
    /// * [`Errno::OutOfRange`] if `class` is not a `sysinfo-v1` class id.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut reserved = [0u8; 7];
        reserved.copy_from_slice(&bytes[1..8]);
        if reserved != [0u8; 7] {
            return Err(Errno::BadMagic);
        }
        let class = bytes[0];
        if usize::from(class) >= RECLAIM_CLASS_COUNT {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            class,
            reserved,
            payload_bytes: read_u64(bytes, 8),
            metadata_bytes: read_u64(bytes, 16),
            entries: read_u64(bytes, 24),
            refusals: read_u64(bytes, 32),
            pressure_shrinks: read_u64(bytes, 40),
            teardowns: read_u64(bytes, 48),
            failures: read_u64(bytes, 56),
            hits: read_u64(bytes, 64),
            misses: read_u64(bytes, 72),
            self_reported_bytes: read_u64(bytes, 80),
        })
    }
}

/// Maximum bytes of a cache label carried in a [`CacheLedgerRecord`].
pub const CACHE_LABEL_MAX: usize = 32;

/// Who is charged for a cache's memory, as carried on the wire.
///
/// Mirrors the reclaim model's closed owner taxonomy. The variant says
/// *what kind* of principal holds the memory; the numeric payload some
/// kinds carry travels alongside in [`CacheLedgerRecord::owner_id`], and
/// the cache's own label names the specific cache.
///
/// Discriminants are part of `sysinfo-v1` and must not be re-numbered.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CacheOwnerKind {
    /// A kernel subsystem. `owner_id` is zero.
    KernelSubsystem = 0,
    /// A mounted filesystem volume. `owner_id` is the volume id.
    FilesystemVolume = 1,
    /// One task. `owner_id` is the task id.
    Task = 2,
    /// A desktop session. `owner_id` is the seat id.
    DesktopSession = 3,
    /// A userland process. `owner_id` is zero; the reporting process is
    /// named by [`CacheLedgerRecord::reporter_pid`].
    UserlandProcess = 4,
}

impl CacheOwnerKind {
    /// Numeric value carried on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte into a [`CacheOwnerKind`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value that is not a defined variant —
    /// never guessing an owner (fail closed).
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::KernelSubsystem),
            1 => Ok(Self::FilesystemVolume),
            2 => Ok(Self::Task),
            3 => Ok(Self::DesktopSession),
            4 => Ok(Self::UserlandProcess),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Where a [`CacheLedgerRecord`]'s figures came from, and therefore how
/// far they can be trusted.
///
/// Discriminants are part of `sysinfo-v1` and must not be re-numbered.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum CacheLedgerOrigin {
    /// Not yet attributed: the only value a
    /// [`SysinfoQueryId::CACHE_REPORT`] submission may carry. The service
    /// stamps the real origin from the caller's kernel-attested identity,
    /// so a process cannot present its own figures as measured ones.
    #[default]
    Unset = 0,
    /// Read from a ledger the kernel measures directly.
    Kernel = 1,
    /// Reported by the process that holds the cache. Nothing outside that
    /// process can see the figure, and a compromised process can lie about
    /// it, so it is a diagnostic and never an input to a decision.
    SelfReported = 2,
}

impl CacheLedgerOrigin {
    /// Numeric value carried on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte into a [`CacheLedgerOrigin`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value that is not a defined variant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Unset),
            1 => Ok(Self::Kernel),
            2 => Ok(Self::SelfReported),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One cache's ledger: the per-cache row behind the per-class totals of
/// [`ReclaimClassRecord`].
///
/// The same record travels in both directions. A
/// [`SysinfoQueryId::CACHE_LEDGERS`] response carries fully attributed
/// rows; a [`SysinfoQueryId::CACHE_REPORT`] submission carries rows a
/// process filled in for its own caches, with [`Self::origin`] left
/// [`CacheLedgerOrigin::Unset`] and [`Self::reporter_pid`] zero for the
/// service to stamp. One record type rather than two near-identical ones:
/// the direction decides which fields the *service* owns, not which fields
/// exist.
///
/// Byte figures are live gauges; the event counters are monotonic since
/// the cache was built.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CacheLedgerRecord {
    /// Valid byte count in the inline label buffer
    /// (`<= CACHE_LABEL_MAX`); read the bytes through
    /// [`Self::label_bytes`].
    pub label_len: u8,
    /// What kind of principal is charged for this cache's memory.
    pub owner_kind: CacheOwnerKind,
    /// Whether the figures are kernel-measured or self-reported.
    pub origin: CacheLedgerOrigin,
    /// The class of every entry: a reclaim class, or
    /// [`CACHE_CLASS_PINNED`] for a pool the model accounts but never
    /// reclaims. Rendered through [`cache_class_name`].
    pub class: u8,
    /// The owner's numeric payload: a volume id, a task id, or a seat id
    /// depending on [`Self::owner_kind`]; zero for the kinds that carry
    /// none.
    pub owner_id: u64,
    /// The reporting process's numeric pid, for display; zero on a
    /// kernel-measured row.
    ///
    /// Numeric pids are reused, so this is a display convenience: the
    /// service keys its registry by the caller's unforgeable
    /// process-instance identity and drops a row the moment that instance
    /// is gone, so a recycled pid can never inherit another process's row.
    pub reporter_pid: u64,
    /// Cached payload bytes currently held.
    pub payload_bytes: u64,
    /// Bookkeeping metadata bytes currently held.
    pub metadata_bytes: u64,
    /// Entries currently held.
    pub entries: u64,
    /// Admissions refused.
    pub refusals: u64,
    /// Pressure-driven shrink passes that reclaimed from this cache.
    pub pressure_shrinks: u64,
    /// Whole-cache teardown drains.
    pub teardowns: u64,
    /// Detected internal failures that poisoned the cache.
    pub failures: u64,
    /// Lookups served from the cache.
    pub hits: u64,
    /// Lookups that fell through to the canonical source.
    pub misses: u64,
    label: [u8; CACHE_LABEL_MAX],
}

impl CacheLedgerRecord {
    /// Encoded size on the wire: four descriptor bytes, four reserved,
    /// the owner and reporter ids, nine `u64` figures, and the label.
    pub const WIRE_LEN: usize = 24 + 9 * 8 + CACHE_LABEL_MAX;

    /// Construct a row for `label`, with every figure zero.
    ///
    /// The figures are set by the caller after construction; a builder for
    /// fifteen numbers would be noise. `label` must be printable ASCII no
    /// longer than [`CACHE_LABEL_MAX`] — it is rendered verbatim in a
    /// monitor and, on a reported row, comes from another process, so a
    /// control character or an over-long name is refused here rather than
    /// left for a renderer to cope with.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `label` is empty or exceeds
    ///   [`CACHE_LABEL_MAX`].
    /// * [`Errno::OutOfRange`] if `label` holds a byte outside printable
    ///   ASCII, or if `class` is not a `sysinfo-v1` class id.
    pub fn new(
        label: &[u8],
        owner_kind: CacheOwnerKind,
        owner_id: u64,
        class: u8,
    ) -> Result<Self, Errno> {
        if label.is_empty() || label.len() > CACHE_LABEL_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if !is_printable_label(label) {
            return Err(Errno::OutOfRange);
        }
        if usize::from(class) >= CACHE_CLASS_COUNT {
            return Err(Errno::OutOfRange);
        }
        let mut buf = [0u8; CACHE_LABEL_MAX];
        buf[..label.len()].copy_from_slice(label);
        let label_len = u8::try_from(label.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            label_len,
            owner_kind,
            origin: CacheLedgerOrigin::Unset,
            class,
            owner_id,
            reporter_pid: 0,
            payload_bytes: 0,
            metadata_bytes: 0,
            entries: 0,
            refusals: 0,
            pressure_shrinks: 0,
            teardowns: 0,
            failures: 0,
            hits: 0,
            misses: 0,
            label: buf,
        })
    }

    /// Borrow the valid prefix of the label buffer.
    #[must_use]
    pub fn label_bytes(&self) -> &[u8] {
        &self.label[..self.label_len as usize]
    }

    /// The label as a string. Never fails: the decoder admits only
    /// printable ASCII, which is always valid UTF-8.
    #[must_use]
    pub fn label(&self) -> &str {
        core::str::from_utf8(self.label_bytes()).unwrap_or("")
    }

    /// Resident bytes: payload plus this cache's own bookkeeping.
    #[must_use]
    pub const fn resident_bytes(&self) -> u64 {
        self.payload_bytes.saturating_add(self.metadata_bytes)
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = self.label_len;
        out[1] = self.owner_kind.as_u8();
        out[2] = self.origin.as_u8();
        out[3] = self.class;
        put_u64(&mut out, 8, self.owner_id);
        put_u64(&mut out, 16, self.reporter_pid);
        for (i, value) in [
            self.payload_bytes,
            self.metadata_bytes,
            self.entries,
            self.refusals,
            self.pressure_shrinks,
            self.teardowns,
            self.failures,
            self.hits,
            self.misses,
        ]
        .iter()
        .enumerate()
        {
            put_u64(&mut out, 24 + i * 8, *value);
        }
        out[96..96 + CACHE_LABEL_MAX].copy_from_slice(&self.label);
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::BadMagic`] if a reserved byte is non-zero, or if the
    ///   label padding past `label_len` carries a hidden payload.
    /// * [`Errno::OutOfRange`] for an unknown owner kind, an unknown
    ///   origin, a `class` outside `sysinfo-v1`, or a label byte outside
    ///   printable ASCII.
    /// * [`Errno::LengthOutOfRange`] if `label_len` is zero or exceeds
    ///   [`CACHE_LABEL_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[4..8] != [0u8; 4] {
            return Err(Errno::BadMagic);
        }
        let owner_kind = CacheOwnerKind::from_u8(bytes[1])?;
        let origin = CacheLedgerOrigin::from_u8(bytes[2])?;
        let class = bytes[3];
        if usize::from(class) >= CACHE_CLASS_COUNT {
            return Err(Errno::OutOfRange);
        }
        let label_len = bytes[0];
        if label_len == 0 || usize::from(label_len) > CACHE_LABEL_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut label = [0u8; CACHE_LABEL_MAX];
        label.copy_from_slice(&bytes[96..96 + CACHE_LABEL_MAX]);
        let (used, padding) = label.split_at(usize::from(label_len));
        if padding.iter().any(|&byte| byte != 0) {
            return Err(Errno::BadMagic);
        }
        if !is_printable_label(used) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            label_len,
            owner_kind,
            origin,
            class,
            owner_id: read_u64(bytes, 8),
            reporter_pid: read_u64(bytes, 16),
            payload_bytes: read_u64(bytes, 24),
            metadata_bytes: read_u64(bytes, 32),
            entries: read_u64(bytes, 40),
            refusals: read_u64(bytes, 48),
            pressure_shrinks: read_u64(bytes, 56),
            teardowns: read_u64(bytes, 64),
            failures: read_u64(bytes, 72),
            hits: read_u64(bytes, 80),
            misses: read_u64(bytes, 88),
            label,
        })
    }
}

/// Whether `label` is a non-empty run of printable ASCII.
///
/// A cache label is rendered verbatim by a monitor and, on a reported row,
/// crosses a process boundary, so anything that could move a cursor,
/// re-colour a terminal, or forge a column is refused at the ABI edge.
fn is_printable_label(label: &[u8]) -> bool {
    !label.is_empty() && label.iter().all(|&byte| (0x20..=0x7e).contains(&byte))
}

/// Request payload for [`SysinfoQueryId::CACHE_LEDGERS`].
///
/// The response is a sequence of [`CacheLedgerRecord`]s paged with
/// `offset`/`limit`. Ordering is stable across paged calls: kernel rows
/// first in registration order, then reported rows ordered by reporter and
/// label, so a client walking the list never skips or repeats a row.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CacheLedgerListRequest {
    /// Index of the first cache row to return.
    pub offset: u32,
    /// Maximum number of [`CacheLedgerRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl CacheLedgerListRequest {
    /// Encoded size of a [`CacheLedgerListRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`CacheLedgerListRequest`].
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Most cache rows one process may report in a single
/// [`SysinfoQueryId::CACHE_REPORT`].
///
/// A fixed validation bound on untrusted input, not a growable capacity: a
/// process holds a handful of reclaimable caches (its glyph client cache,
/// its decoded artwork, its rasterised chrome), so the ceiling exists only
/// to bound what one submission can demand of the service. A process with
/// more caches than this reports its largest; it cannot enlarge its own
/// footprint in the registry by declaring more.
pub const MAX_CACHE_REPORT_ENTRIES: usize = 16;

/// Request payload for [`SysinfoQueryId::CACHE_REPORT`]: this header
/// followed by exactly `count` [`CacheLedgerRecord`]s.
///
/// The rows **replace** the calling process's previous rows rather than
/// adding to them, so a process's footprint in the registry is bounded by
/// [`MAX_CACHE_REPORT_ENTRIES`] however often it reports. A `count` of
/// zero is meaningful and legal: it withdraws the process's rows, which is
/// what a process does when it tears its caches down.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CacheReportRequest {
    /// Number of [`CacheLedgerRecord`]s that follow this header.
    pub count: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
}

impl CacheReportRequest {
    /// Encoded size of a [`CacheReportRequest`] header on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u16(&mut out, 0, self.count);
        put_u16(&mut out, 2, self.flags);
        put_u32(&mut out, 4, self.reserved);
        out
    }

    /// Decode `bytes` into a [`CacheReportRequest`] header.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if a reserved field is non-zero.
    /// * [`Errno::LengthOutOfRange`] if `count` exceeds
    ///   [`MAX_CACHE_REPORT_ENTRIES`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 2);
        let reserved = read_u32(bytes, 4);
        if flags != 0 || reserved != 0 {
            return Err(Errno::BadMagic);
        }
        let count = read_u16(bytes, 0);
        if usize::from(count) > MAX_CACHE_REPORT_ENTRIES {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            count,
            flags,
            reserved,
        })
    }
}

/// Request payload for [`SysinfoQueryId::DESKTOP_FRAME_STATS`].
///
/// The same paging shape as [`SeatListRequest`]: `offset` names the first
/// [`DesktopFrameRecord`] to return and `limit` bounds the page. A machine
/// holds one publisher per compositing session, so a page is small — but the
/// count is a function of how many seats and switched-away sessions exist,
/// never a fixed one, so it is paged like every other list.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct DesktopFrameStatsRequest {
    /// Index of the first record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl DesktopFrameStatsRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than [`Self::WIRE_LEN`].
    /// * [`Errno::BadMagic`] if a reserved flag bit is set (fail closed on an
    ///   unknown request shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// What a compositing session's frames have cost since it started counting:
/// the request payload of [`SysinfoQueryId::DESKTOP_FRAME_REPORT`] and the
/// body of a [`DesktopFrameRecord`].
///
/// Cumulative rather than last-frame, because the two questions asked of
/// these counters need different things and both are answered here: a reader
/// wants rates and ratios over a run (subtract two samples, exactly as an
/// interface's packet counters are read), while a regression gate wants the
/// **worst** frame in a gesture — a hover that repaints one control cannot be
/// told from one that repaints the screen by an average. The desktop's own
/// live per-frame gauge is a separate, push-side channel to its monitor; this
/// is the pull-side accounting anything on the machine can ask for.
///
/// Every field is a count of work, never a duration, so a figure is exactly
/// reproducible for a given sequence of frames and a test may assert it under
/// any machine load. Every addition saturates: a saturated diagnostic is
/// still truthful about "a very large number", where a wrapped one reads as a
/// suspiciously small desktop.
///
/// `screen_px` is the denominator every pixel figure is read against, and it
/// is a property of the whole accounting rather than of one frame: a session
/// whose display mode changes starts a fresh epoch, because counts taken
/// against a different screen answer a different question and would make the
/// bounds [`from_bytes`](Self::from_bytes) enforces inexact.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DesktopFrameTotals {
    /// The screen's pixel count over the counted frames.
    pub screen_px: u64,
    /// Frames composited in this epoch. Zero means the session has composed
    /// nothing yet, and every other counter is then zero too.
    pub frames: u64,
    /// Screen pixels those frames recomposed.
    pub damaged_px: u64,
    /// Layer contributions blended to resolve them. Counts contributions,
    /// not positions, so a stack of windows may blend one damaged pixel many
    /// times and this can exceed `damaged_px` — that ratio is the overdraw
    /// reading.
    pub blended_px: u64,
    /// Damaged pixels resolved by copying a fully opaque run instead,
    /// skipping every layer beneath.
    pub opaque_px: u64,
    /// Pixels rewritten by a *recomputed* backdrop frost. A frost served
    /// from the retained one is copied rather than blurred and counts
    /// nothing, so this is the blur work the frames could not avoid.
    pub blur_px: u64,
    /// Composed pixels converted to scan-out bytes.
    pub encoded_px: u64,
    /// Dirty rectangles those frames recomposed.
    pub dirty_rects: u64,
    /// Calls into the display driver that published them.
    pub present_calls: u64,
    /// Window-furniture lookups served from the retained cache.
    pub chrome_hits: u64,
    /// Window-furniture lookups that had to be rendered.
    pub chrome_misses: u64,
    /// Most pixels any single frame in this epoch recomposed.
    pub peak_damaged_px: u64,
    /// Most layer contributions any single frame in this epoch blended.
    /// An independent maximum: the frame that blended most need not be the
    /// frame that damaged most, so each bounds its own worst case.
    pub peak_blended_px: u64,
}

impl DesktopFrameTotals {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 104;

    /// An epoch that has counted no frame.
    pub const ZERO: Self = Self {
        screen_px: 0,
        frames: 0,
        damaged_px: 0,
        blended_px: 0,
        opaque_px: 0,
        blur_px: 0,
        encoded_px: 0,
        dirty_rects: 0,
        present_calls: 0,
        chrome_hits: 0,
        chrome_misses: 0,
        peak_damaged_px: 0,
        peak_blended_px: 0,
    };

    /// Refuse counts no compositor could have produced.
    ///
    /// The receiver's fail-closed gate, applied where the untrusted
    /// submission is decoded, so nothing renders or asserts on a sender's
    /// arithmetic. Each rule holds of every sequence of frames a compositor
    /// can actually compose:
    ///
    /// * A `frames` of zero admits no work at all — a counter can only move
    ///   in a frame.
    /// * `dirty_rects` is zero exactly when `damaged_px` is: an empty
    ///   rectangle is never recomposed, so each counted rectangle carries at
    ///   least one pixel.
    /// * `opaque_px` cannot exceed `damaged_px`, and `encoded_px` cannot
    ///   either: a copied opaque run and an encoded scan-out byte both
    ///   resolve a damaged pixel, never a pixel outside the damage.
    /// * `damaged_px` cannot exceed `screen_px * frames`: one frame's
    ///   rectangles are clipped to the screen and pairwise disjoint.
    /// * `present_calls` cannot exceed `dirty_rects + frames`: a frame
    ///   publishes at most one driver call per rectangle, and the
    ///   whole-screen, bounding-box, and hardware-layer paths each publish
    ///   exactly one.
    /// * A peak is a maximum over the summed frames, so it cannot exceed its
    ///   own sum, is zero exactly when that sum is, and — for damage, which
    ///   is clipped per frame — cannot exceed one screen.
    ///
    /// `blended_px`, `blur_px` and the furniture counters are deliberately
    /// unbounded: blends count layer contributions, a recomputed frost is
    /// blurred over the whole window rectangle that caused it (and several
    /// windows may recompute in one frame), and a furniture lookup is not a
    /// pixel at all.
    const fn validate(&self) -> Result<(), Errno> {
        let no_frames = self.frames == 0;
        let work = self.damaged_px
            | self.blended_px
            | self.opaque_px
            | self.blur_px
            | self.encoded_px
            | self.dirty_rects
            | self.present_calls
            | self.chrome_hits
            | self.chrome_misses
            | self.peak_damaged_px
            | self.peak_blended_px;
        if no_frames && work != 0 {
            return Err(Errno::OutOfRange);
        }
        if (self.dirty_rects == 0) != (self.damaged_px == 0)
            || self.opaque_px > self.damaged_px
            || self.encoded_px > self.damaged_px
            || self.damaged_px > self.screen_px.saturating_mul(self.frames)
            || self.present_calls > self.dirty_rects.saturating_add(self.frames)
            || self.peak_damaged_px > self.damaged_px
            || self.peak_damaged_px > self.screen_px
            || (self.peak_damaged_px == 0) != (self.damaged_px == 0)
            || self.peak_blended_px > self.blended_px
            || (self.peak_blended_px == 0) != (self.blended_px == 0)
        {
            return Err(Errno::OutOfRange);
        }
        Ok(())
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.screen_px);
        put_u64(&mut out, 8, self.frames);
        put_u64(&mut out, 16, self.damaged_px);
        put_u64(&mut out, 24, self.blended_px);
        put_u64(&mut out, 32, self.opaque_px);
        put_u64(&mut out, 40, self.blur_px);
        put_u64(&mut out, 48, self.encoded_px);
        put_u64(&mut out, 56, self.dirty_rects);
        put_u64(&mut out, 64, self.present_calls);
        put_u64(&mut out, 72, self.chrome_hits);
        put_u64(&mut out, 80, self.chrome_misses);
        put_u64(&mut out, 88, self.peak_damaged_px);
        put_u64(&mut out, 96, self.peak_blended_px);
        out
    }

    /// Decode from `bytes`, refusing counts no compositor could produce.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than [`Self::WIRE_LEN`].
    /// * [`Errno::OutOfRange`] if the counts are ones no composite pass could
    ///   have produced. The rules are stated on the private `validate` this
    ///   calls, and catalogued for a reader in `docs/src/abi/sysinfo.md`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let totals = Self {
            screen_px: read_u64(bytes, 0),
            frames: read_u64(bytes, 8),
            damaged_px: read_u64(bytes, 16),
            blended_px: read_u64(bytes, 24),
            opaque_px: read_u64(bytes, 32),
            blur_px: read_u64(bytes, 40),
            encoded_px: read_u64(bytes, 48),
            dirty_rects: read_u64(bytes, 56),
            present_calls: read_u64(bytes, 64),
            chrome_hits: read_u64(bytes, 72),
            chrome_misses: read_u64(bytes, 80),
            peak_damaged_px: read_u64(bytes, 88),
            peak_blended_px: read_u64(bytes, 96),
        };
        totals.validate()?;
        Ok(totals)
    }
}

/// One response row of [`SysinfoQueryId::DESKTOP_FRAME_STATS`]: a
/// publisher's [`DesktopFrameTotals`] and the process the service attested
/// them to.
///
/// `reporter_pid` is stamped by `sysinfod` from the caller's kernel-attested
/// identity, never carried in the submission, so a publisher can neither
/// attribute its figures to another process nor be mistaken for one. Every
/// figure here is self-reported: it is a diagnostic the desktop states about
/// itself, and no kernel decision reads it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DesktopFrameRecord {
    /// Numeric pid of the publishing process, stamped by the service.
    pub reporter_pid: u64,
    /// What that publisher's frames cost.
    pub totals: DesktopFrameTotals,
}

impl DesktopFrameRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8 + DesktopFrameTotals::WIRE_LEN;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.reporter_pid);
        out[8..].copy_from_slice(&self.totals.to_le_bytes());
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than [`Self::WIRE_LEN`].
    /// * [`Errno::OutOfRange`] if the totals fail the same bounds
    ///   [`DesktopFrameTotals::from_bytes`] applies to a bare submission.
    /// * [`Errno::BadMagic`] if the row names no publisher: a served row is
    ///   always attributed, so a zero pid is wire corruption rather than an
    ///   anonymous desktop.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reporter_pid = read_u64(bytes, 0);
        if reporter_pid == 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            reporter_pid,
            totals: DesktopFrameTotals::from_bytes(&bytes[8..])?,
        })
    }
}

/// Fold per-cache rows into the per-class totals of a
/// [`SysinfoQueryId::RECLAIM_STATS`] response.
///
/// One fold, used wherever the two views must agree: a class row is
/// exactly the sum of the [`CacheLedgerRecord`]s of that class, and the
/// resident bytes of the [`CacheLedgerOrigin::SelfReported`] rows are also
/// carried separately as
/// [`ReclaimClassRecord::self_reported_bytes`]. Every addition saturates —
/// the byte figures are live gauges sampled independently, and a saturated
/// diagnostic is still truthful about "a very large number".
///
/// A class with no cache reports a row of zeros rather than being absent:
/// "nothing is cached in this class" is an answer, and a caller paging the
/// nine classes gets nine records whatever the machine is doing.
///
/// A [`CACHE_CLASS_PINNED`] row is deliberately dropped: its bytes are
/// never reclaimed, so adding them to a reclaim class's total would report
/// unreclaimable memory as reclaimable. Such a row is visible as itself in
/// the per-cache view and nowhere else.
#[must_use]
pub fn fold_cache_ledgers(rows: &[CacheLedgerRecord]) -> [ReclaimClassRecord; RECLAIM_CLASS_COUNT] {
    let mut totals = [ReclaimClassRecord::default(); RECLAIM_CLASS_COUNT];
    for (index, total) in totals.iter_mut().enumerate() {
        // The index is a class id by construction; the array is exactly as
        // long as the class set.
        total.class = u8::try_from(index).unwrap_or(0);
    }
    for row in rows {
        let Some(total) = totals.get_mut(usize::from(row.class)) else {
            continue;
        };
        total.payload_bytes = total.payload_bytes.saturating_add(row.payload_bytes);
        total.metadata_bytes = total.metadata_bytes.saturating_add(row.metadata_bytes);
        total.entries = total.entries.saturating_add(row.entries);
        total.refusals = total.refusals.saturating_add(row.refusals);
        total.pressure_shrinks = total.pressure_shrinks.saturating_add(row.pressure_shrinks);
        total.teardowns = total.teardowns.saturating_add(row.teardowns);
        total.failures = total.failures.saturating_add(row.failures);
        total.hits = total.hits.saturating_add(row.hits);
        total.misses = total.misses.saturating_add(row.misses);
        if row.origin == CacheLedgerOrigin::SelfReported {
            total.self_reported_bytes = total
                .self_reported_bytes
                .saturating_add(row.resident_bytes());
        }
    }
    totals
}

/// Response payload for [`SysinfoQueryId::RAMZIP_STATS`].
///
/// Counters only — never page contents, never key material
/// (`plans/SWAPSWAPSWAP.md` §16). Byte figures are live gauges; the
/// event counters are monotonic since boot. A build whose tier is not
/// yet driven truthfully reports an idle tier (all gauges and counters
/// zero) rather than refusing or fabricating.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct RamzipStats {
    /// Compressed entries currently held.
    pub entries: u64,
    /// Logical (uncompressed) page bytes the tier represents.
    pub logical_bytes: u64,
    /// Compressed bytes before encryption overhead.
    pub compressed_bytes: u64,
    /// Stored ciphertext bytes (after encryption and authentication
    /// overhead), excluding bookkeeping metadata.
    pub stored_bytes: u64,
    /// Bookkeeping metadata bytes.
    pub metadata_bytes: u64,
    /// Derived minimum capacity the tier may always use, in bytes.
    pub min_cap_bytes: u64,
    /// Derived soft capacity target, in bytes.
    pub soft_cap_bytes: u64,
    /// Derived hard capacity ceiling, in bytes.
    pub hard_cap_bytes: u64,
    /// Compression attempts offered to the tier.
    pub attempts: u64,
    /// Attempts accepted and stored.
    pub accepted: u64,
    /// Refused by the pressure policy (handoff gate closed).
    pub rejected_policy: u64,
    /// Refused by the eligibility classifier.
    pub rejected_ineligible: u64,
    /// Refused because compression did not win.
    pub rejected_incompressible: u64,
    /// Refused by the band capacity cap.
    pub rejected_cap: u64,
    /// Refused by the decompression-floor reserve check.
    pub rejected_reserve: u64,
    /// Refused by the per-task fair-share bound.
    pub rejected_task_share: u64,
    /// Refused because the owning task is thrashing.
    pub rejected_thrash: u64,
    /// Compressed-entry pages restored on demand.
    pub fault_ins: u64,
    /// Entries lost to authentication failure (fail closed, audited).
    pub auth_failures: u64,
    /// Entries lost to metadata or decompression corruption (fail
    /// closed, audited).
    pub decode_failures: u64,
    /// Warm-up steps that considered candidates.
    pub warm_attempts: u64,
    /// Pages restored by the warm-up worker.
    pub warm_restored: u64,
    /// Warm-up steps stopped by a pressure or reserve gate.
    pub warm_stopped: u64,
    /// Pages restored by post-fault clustering.
    pub cluster_restored: u64,
    /// Tasks that crossed the thrash threshold.
    pub thrash_detected: u64,
    /// Bytes of anonymous memory currently exempted from the tier by
    /// process pins (`mem_pin`, `plans/STRESSTEST.md` ST2): the aggregate
    /// pinned footprint across every pinned process, so an operator can
    /// see how much memory pressure management may never reclaim. Zero
    /// when nothing is pinned. Took the record's reserved slot by
    /// reserved-field evolution, so the wire length is unchanged.
    pub pinned_bytes: u64,
}

impl RamzipStats {
    /// Encoded size on the wire: twenty-six `u64` fields, field order as
    /// declared, little-endian.
    pub const WIRE_LEN: usize = 26 * 8;

    /// The wire values in declaration order (shared by the encoder and
    /// the decoder so the two can never disagree on field order).
    fn field_values(&self) -> [u64; 26] {
        [
            self.entries,
            self.logical_bytes,
            self.compressed_bytes,
            self.stored_bytes,
            self.metadata_bytes,
            self.min_cap_bytes,
            self.soft_cap_bytes,
            self.hard_cap_bytes,
            self.attempts,
            self.accepted,
            self.rejected_policy,
            self.rejected_ineligible,
            self.rejected_incompressible,
            self.rejected_cap,
            self.rejected_reserve,
            self.rejected_task_share,
            self.rejected_thrash,
            self.fault_ins,
            self.auth_failures,
            self.decode_failures,
            self.warm_attempts,
            self.warm_restored,
            self.warm_stopped,
            self.cluster_restored,
            self.thrash_detected,
            self.pinned_bytes,
        ]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        for (i, value) in self.field_values().iter().enumerate() {
            put_u64(&mut out, i * 8, *value);
        }
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            entries: read_u64(bytes, 0),
            logical_bytes: read_u64(bytes, 8),
            compressed_bytes: read_u64(bytes, 16),
            stored_bytes: read_u64(bytes, 24),
            metadata_bytes: read_u64(bytes, 32),
            min_cap_bytes: read_u64(bytes, 40),
            soft_cap_bytes: read_u64(bytes, 48),
            hard_cap_bytes: read_u64(bytes, 56),
            attempts: read_u64(bytes, 64),
            accepted: read_u64(bytes, 72),
            rejected_policy: read_u64(bytes, 80),
            rejected_ineligible: read_u64(bytes, 88),
            rejected_incompressible: read_u64(bytes, 96),
            rejected_cap: read_u64(bytes, 104),
            rejected_reserve: read_u64(bytes, 112),
            rejected_task_share: read_u64(bytes, 120),
            rejected_thrash: read_u64(bytes, 128),
            fault_ins: read_u64(bytes, 136),
            auth_failures: read_u64(bytes, 144),
            decode_failures: read_u64(bytes, 152),
            warm_attempts: read_u64(bytes, 160),
            warm_restored: read_u64(bytes, 168),
            warm_stopped: read_u64(bytes, 176),
            cluster_restored: read_u64(bytes, 184),
            thrash_detected: read_u64(bytes, 192),
            pinned_bytes: read_u64(bytes, 200),
        })
    }
}

/// Request payload for [`SysinfoQueryId::CPU_LOAD`].
///
/// Structurally parallel to [`CpuTimeListRequest`] but a distinct frozen
/// payload: each `sysinfo-v1` query owns its argument type. The response
/// is a sequence of [`CpuLoadRecord`]s paged with `offset`/`limit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuLoadRequest {
    /// Index of the first CPU to return.
    pub offset: u32,
    /// Maximum number of [`CpuLoadRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl CpuLoadRequest {
    /// Encoded size of a [`CpuLoadRequest`] on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`CpuLoadRequest`].
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One CPU's scheduler load figures inside a
/// [`SysinfoQueryId::CPU_LOAD`] response.
///
/// The cumulative busy/idle time split lives in [`CpuTimeRecord`]
/// ([`SysinfoQueryId::CPU_TIME_STATS`]); this record carries only the
/// remainder, so the same figure is never served twice. `queue_depth`
/// is an instantaneous sample; the two counters are monotonic since
/// boot.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuLoadRecord {
    /// The CPU index this record describes.
    pub cpu: u32,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub reserved: u32,
    /// Runnable tasks queued on this CPU at the sample (excluding the
    /// currently running task).
    pub queue_depth: u64,
    /// Task dispatches (context switches into a task body) on this CPU
    /// since boot.
    pub switches: u64,
    /// Timer-driven involuntary preemptions on this CPU since boot.
    pub preemptions: u64,
}

impl CpuLoadRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 32;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.cpu);
        put_u32(&mut out, 4, self.reserved);
        put_u64(&mut out, 8, self.queue_depth);
        put_u64(&mut out, 16, self.switches);
        put_u64(&mut out, 24, self.preemptions);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::BadMagic`] if the reserved field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let reserved = read_u32(bytes, 4);
        if reserved != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            cpu: read_u32(bytes, 0),
            reserved,
            queue_depth: read_u64(bytes, 8),
            switches: read_u64(bytes, 16),
            preemptions: read_u64(bytes, 24),
        })
    }
}

/// The performance class of a CPU reported in a [`CpuInfoRecord`].
///
/// The `sysinfo-v1` mirror of the kernel's core-class discriminant
/// (`tairix_arch_api::CoreClass`), defined here so the ABI carries no edge
/// to a `kernel/*` crate. A homogeneous machine reports every CPU as
/// [`CpuCoreClass::Performance`].
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum CpuCoreClass {
    /// A high-throughput core (Intel "Core" / ARM "big"), the default on a
    /// homogeneous machine.
    #[default]
    Performance = 0,
    /// A low-power efficiency core (Intel "Atom" / ARM "LITTLE").
    Efficiency = 1,
}

impl CpuCoreClass {
    /// Raw discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a raw discriminant, failing closed on an unknown value.
    ///
    /// Returns [`Errno::OutOfRange`] for any value outside the closed set.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Performance),
            1 => Ok(Self::Efficiency),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Request payload for [`SysinfoQueryId::CPU_INFO`].
///
/// Same paging shape as [`CpuLoadRequest`]: `offset` names the first CPU
/// index to return and `limit` bounds the page. Each `sysinfo-v1` query
/// owns its own frozen payload type.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuInfoListRequest {
    /// Index of the first CPU to return.
    pub offset: u32,
    /// Maximum number of [`CpuInfoRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl CpuInfoListRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Maximum length, in bytes, of the model/vendor name in a [`CpuInfoRecord`].
///
/// A fixed field (a bound, not a scaling capacity): ample for the longest
/// x86 brand string (`Intel(R) Core(TM) …`, 48 bytes) and every ARM/RISC-V
/// part name. A longer discovered name is rejected at construction rather
/// than truncated silently (fail closed).
pub const CPU_MODEL_NAME_MAX: usize = 48;

/// [`CpuInfoRecord::flags`] bit: [`CpuInfoRecord::current_freq_hz`] is a
/// live *measured* frequency. When clear, the core clock could not be
/// measured on this CPU (no core-clock counter, or no sample taken yet) and
/// `current_freq_hz` is `0` — the honest unknown, never a fabricated rate.
pub const CPU_INFO_FLAG_FREQ_MEASURED: u8 = 1 << 0;

/// One CPU's processor information inside a [`SysinfoQueryId::CPU_INFO`]
/// response — the `/proc/cpuinfo`-class hardware facts for one online core.
///
/// Every field is filled from kernel-attested hardware state: the ISA
/// feature bits and identity register read from the silicon, and the live
/// frequency measured by the kernel's per-CPU estimator (the core-clock /
/// reference-counter ratio). `current_freq_hz` is `0` and
/// [`CPU_INFO_FLAG_FREQ_MEASURED`] clear when the core clock could not be
/// measured; `reference_hz` is the fixed reference/timebase frequency (`0`
/// when unknown).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CpuInfoRecord {
    /// The CPU index this record describes.
    pub cpu: u32,
    /// The CPU's performance class ([`CpuCoreClass`]).
    pub class: CpuCoreClass,
    /// State bits ([`CPU_INFO_FLAG_FREQ_MEASURED`]); other bits reserved zero.
    pub flags: u8,
    /// Valid byte length of [`Self::model`] (`<= CPU_MODEL_NAME_MAX`).
    pub model_len: u8,
    /// The raw ISA feature bitset (`tairix_abi::cpufeatures::CpuFeatureSet`
    /// bits) this core implements.
    pub feature_bits: u64,
    /// The raw per-core identity register (aarch64 `MIDR_EL1`, the x86 CPUID
    /// signature, riscv64 `mvendorid:marchid:mimpid`); `0` when the port has
    /// no such register.
    pub raw_id: u64,
    /// Live measured core-clock frequency in Hz, or `0` when unmeasured (see
    /// [`CPU_INFO_FLAG_FREQ_MEASURED`]).
    pub current_freq_hz: u64,
    /// The fixed reference/timebase frequency in Hz, or `0` when unknown.
    pub reference_hz: u64,
    /// The model/vendor name, UTF-8, `model_len` bytes valid.
    pub model: [u8; CPU_MODEL_NAME_MAX],
}

impl CpuInfoRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 40 + CPU_MODEL_NAME_MAX;

    /// Construct a record, validating the model name fits the fixed field.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `model` exceeds [`CPU_MODEL_NAME_MAX`].
    // A CPU record is eight independent scalar/identity fields; threading them
    // through a builder would be gratuitous indirection for a frozen wire type.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cpu: u32,
        class: CpuCoreClass,
        flags: u8,
        feature_bits: u64,
        raw_id: u64,
        current_freq_hz: u64,
        reference_hz: u64,
        model: &[u8],
    ) -> Result<Self, Errno> {
        // `CPU_MODEL_NAME_MAX` is 48, so a fitting length always converts
        // without truncation; a longer name fails closed here.
        let model_len = u8::try_from(model.len()).map_err(|_| Errno::OutOfRange)?;
        if usize::from(model_len) > CPU_MODEL_NAME_MAX {
            return Err(Errno::OutOfRange);
        }
        let mut model_buf = [0u8; CPU_MODEL_NAME_MAX];
        model_buf[..model.len()].copy_from_slice(model);
        Ok(Self {
            cpu,
            class,
            flags,
            model_len,
            feature_bits,
            raw_id,
            current_freq_hz,
            reference_hz,
            model: model_buf,
        })
    }

    /// The valid model-name bytes.
    #[must_use]
    pub fn model_bytes(&self) -> &[u8] {
        &self.model[..self.model_len as usize]
    }

    /// Whether the live frequency was actually measured.
    #[must_use]
    pub const fn freq_measured(&self) -> bool {
        self.flags & CPU_INFO_FLAG_FREQ_MEASURED != 0
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.cpu);
        out[4] = self.class.as_u8();
        out[5] = self.flags;
        out[6] = self.model_len;
        // out[7] reserved zero.
        put_u64(&mut out, 8, self.feature_bits);
        put_u64(&mut out, 16, self.raw_id);
        put_u64(&mut out, 24, self.current_freq_hz);
        put_u64(&mut out, 32, self.reference_hz);
        out[40..40 + CPU_MODEL_NAME_MAX].copy_from_slice(&self.model);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::BadMagic`] if the reserved byte or an unknown `flags` bit
    ///   is set.
    /// * [`Errno::OutOfRange`] if `class` is unknown or `model_len` exceeds
    ///   the field (fail closed on a corrupt wire).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        let flags = bytes[5];
        if flags & !CPU_INFO_FLAG_FREQ_MEASURED != 0 {
            return Err(Errno::BadMagic);
        }
        let class = CpuCoreClass::from_u8(bytes[4])?;
        let model_len = bytes[6];
        if model_len as usize > CPU_MODEL_NAME_MAX {
            return Err(Errno::OutOfRange);
        }
        let mut model = [0u8; CPU_MODEL_NAME_MAX];
        model.copy_from_slice(&bytes[40..40 + CPU_MODEL_NAME_MAX]);
        Ok(Self {
            cpu: read_u32(bytes, 0),
            class,
            flags,
            model_len,
            feature_bits: read_u64(bytes, 8),
            raw_id: read_u64(bytes, 16),
            current_freq_hz: read_u64(bytes, 24),
            reference_hz: read_u64(bytes, 32),
            model,
        })
    }
}

/// Request payload for [`SysinfoQueryId::IRQ_LIST`].
///
/// Identical paging shape to [`SeatListRequest`]: `offset` names the first
/// interrupt-record index to return and `limit` bounds the page.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct IrqListRequest {
    /// Index of the first interrupt record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl IrqListRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set (fail closed on an unknown request shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// [`IrqRecord::flags`] bit: the line is **quarantined** — the kernel's
/// runaway-interrupt safety net disabled it after it fired far faster than
/// any correctly-serviced device could, so it is kept masked and delivers
/// no further wakes until it is re-bound. A set bit is the machine-readable
/// form of the `stuck_owner` attribution a lockup report carries.
pub const IRQ_FLAG_QUARANTINED: u32 = 1 << 0;

/// One bound interrupt line's state inside a [`SysinfoQueryId::IRQ_LIST`]
/// response.
///
/// Every field is filled from the kernel's own IRQ table — the
/// kernel-attested owning task, never a caller claim. The list carries one
/// record per *bound* line, in ascending line order, so a client walking it
/// never skips or repeats a record. `count` is monotonic since boot (it is
/// not reset when a line is re-bound), the classic `/proc/interrupts`-style
/// per-line total; `flags` reports the line's containment state
/// ([`IRQ_FLAG_QUARANTINED`]).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct IrqRecord {
    /// The architecture-defined interrupt line id.
    pub line: u32,
    /// Line state bits ([`IRQ_FLAG_QUARANTINED`]); other bits reserved zero.
    pub flags: u32,
    /// The kernel-attested task id of the driver that bound this line.
    pub owner: u64,
    /// Monotonic count of interrupts delivered on this line since boot.
    pub count: u64,
}

impl IrqRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Whether the line is quarantined (see [`IRQ_FLAG_QUARANTINED`]).
    #[must_use]
    pub const fn is_quarantined(&self) -> bool {
        self.flags & IRQ_FLAG_QUARANTINED != 0
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.line);
        put_u32(&mut out, 4, self.flags);
        put_u64(&mut out, 8, self.owner);
        put_u64(&mut out, 16, self.count);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::BadMagic`] if a reserved `flags` bit is set (fail closed
    ///   on an unknown record shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u32(bytes, 4);
        if flags & !IRQ_FLAG_QUARANTINED != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            line: read_u32(bytes, 0),
            flags,
            owner: read_u64(bytes, 8),
            count: read_u64(bytes, 16),
        })
    }
}

/// Request payload for [`SysinfoQueryId::VOLUME_IO_HEALTH`].
///
/// Identical paging shape to [`IrqListRequest`]: `offset` names the first
/// volume-health-record index to return and `limit` bounds the page.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct VolumeIoHealthRequest {
    /// Index of the first volume-health record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl VolumeIoHealthRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set (fail closed on an unknown request shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// Request payload for [`SysinfoQueryId::RAID_ARRAYS`] and
/// [`SysinfoQueryId::RAID_MEMBERS`].
///
/// One request type serves both because they page identically: `offset` names
/// the first record to return and `limit` bounds the page. A limit above the
/// composer's own page bound is clamped by the broker rather than refused,
/// exactly as the other paged reads behave.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct RaidListRequest {
    /// Index of the first record to return.
    pub offset: u32,
    /// Maximum number of records to return.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl RaidListRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode from `bytes`.
    ///
    /// Returns [`Errno::BufferTooSmall`] if short, or [`Errno::BadMagic`] if
    /// a reserved flag bit is set (fail closed on an unknown request shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// One mounted block-backed volume's live I/O health inside a
/// [`SysinfoQueryId::VOLUME_IO_HEALTH`] response.
///
/// Every field is filled from the kernel's own filesystem-client state — the
/// durable volume identity, the block-service endpoint serving it, its current
/// [`MountAvailability`] (the same live reading the mount table overlays), and
/// the cumulative [`BlkHealthCounters`] the client folded from every
/// completion. The counters are the storage analogue of the per-line
/// `/proc/interrupts` totals [`IrqRecord`] carries: monotonic since the volume
/// was attached, never reset, and named against the serving endpoint rather
/// than any secret. There is one record per attached block-backed volume, in a
/// stable order the source defines, so a client walking the list never skips
/// or repeats a record.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeIoHealthRecord {
    /// The volume's durable 16-byte identity (the mount registry's
    /// `volume_id`), zero when the volume has no published identity.
    volume_id: [u8; 16],
    /// The block-service call-endpoint id serving this volume's device.
    dev: u64,
    /// The volume's current live availability (the overlaid mount health).
    availability: MountAvailability,
    /// The cumulative outcome tallies folded from every completion.
    counters: BlkHealthCounters,
}

impl VolumeIoHealthRecord {
    /// Encoded size on the wire: `volume_id(16) || dev(8) || availability(1) ||
    /// reserved(7) || counters`. The seven reserved bytes keep the counters
    /// block eight-byte aligned and must be zero on the wire.
    pub const WIRE_LEN: usize = 16 + 8 + 8 + BLK_HEALTH_COUNTERS_LEN;

    /// Build a record from its parts.
    #[must_use]
    pub const fn new(
        volume_id: [u8; 16],
        dev: u64,
        availability: MountAvailability,
        counters: BlkHealthCounters,
    ) -> Self {
        Self {
            volume_id,
            dev,
            availability,
            counters,
        }
    }

    /// The volume's durable 16-byte identity.
    #[must_use]
    pub const fn volume_id(&self) -> [u8; 16] {
        self.volume_id
    }

    /// The block-service endpoint id serving the volume's device.
    #[must_use]
    pub const fn dev(&self) -> u64 {
        self.dev
    }

    /// The volume's current live availability.
    #[must_use]
    pub const fn availability(&self) -> MountAvailability {
        self.availability
    }

    /// The cumulative I/O-health counters folded for the volume.
    #[must_use]
    pub const fn counters(&self) -> BlkHealthCounters {
        self.counters
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..16].copy_from_slice(&self.volume_id);
        put_u64(&mut out, 16, self.dev);
        out[24] = self.availability.as_u8();
        // out[25..32] are the reserved padding, left zero.
        out[32..].copy_from_slice(&self.counters.to_le_bytes());
        out
    }

    /// Decode from `bytes`, validating the availability discriminant and the
    /// reserved padding.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes` is shorter than
    ///   [`Self::WIRE_LEN`].
    /// * [`Errno::OutOfRange`] if the availability byte is not a known
    ///   [`MountAvailability`] discriminant.
    /// * [`Errno::BadMagic`] if any reserved padding byte is non-zero (fail
    ///   closed on an unknown record shape).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[25..32].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let mut volume_id = [0u8; 16];
        volume_id.copy_from_slice(&bytes[0..16]);
        let availability = MountAvailability::from_u8(bytes[24])?;
        let counters = BlkHealthCounters::from_bytes(&bytes[32..Self::WIRE_LEN])?;
        Ok(Self {
            volume_id,
            dev: read_u64(bytes, 16),
            availability,
            counters,
        })
    }
}

/// Maximum number of backtrace frames a [`CrashRecord`] carries.
///
/// The user-stack unwinder is hard-capped at 64 frames, but the innermost
/// frames are what a post-mortem needs; the crash record retains the
/// deepest-into-the-fault 32, which is ample for a `ps`/oops-style dump and
/// keeps the fixed record size bounded. A backtrace longer than this is
/// truncated at the *outermost* end (the recorded frames are frame 0
/// upward), which is documented, never silent corruption.
pub const CRASH_MAX_FRAMES: usize = 32;

/// Maximum number of named general-purpose registers a [`CrashRecord`]
/// carries. Sized for the widest Tier-1 register file (riscv64's 31 GP
/// registers plus the pc); a port with fewer fills fewer slots.
pub const CRASH_MAX_REGS: usize = 32;

/// Fixed byte width of a register name inside a [`CrashNamedReg`].
///
/// Every Tier-1 register mnemonic (`rax`, `x30`, `s11`, …) fits in eight
/// ASCII bytes; the name is right-padded with `0x00`.
pub const CRASH_REG_NAME_LEN: usize = 8;

/// Coarse class of *why* the resolver refused the faulting access, as
/// carried in a [`CrashRecord`]. A closed set, decoded fail-closed.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum CrashFaultClass {
    /// A stack-growth fault the resolver could not back (frame exhaustion).
    Stack = 0,
    /// Stack growth refused because the task's `StackBytes` soft bound is
    /// exhausted.
    StackLimit = 1,
    /// A miss inside a live file mapping the resolver refused (past
    /// end-of-file, or a write to a read-only mapping).
    FileRegion = 2,
    /// A miss inside a reserved anonymous region the resolver could not
    /// back (deterministic OOM fatal to this task).
    Anon = 3,
    /// Outside every mapping the task owns.
    #[default]
    Wild = 4,
}

impl CrashFaultClass {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a discriminant, failing closed on an unknown value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value outside the closed set.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Stack),
            1 => Ok(Self::StackLimit),
            2 => Ok(Self::FileRegion),
            3 => Ok(Self::Anon),
            4 => Ok(Self::Wild),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Coarse, non-leaking locality of the faulting address in a
/// [`CrashRecord`] — a distance from a fixed anchor, never an absolute
/// virtual address. A closed set, decoded fail-closed.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum CrashFaultBucket {
    /// Within the first page: a null-pointer dereference. `fault_offset` is
    /// the byte offset from virtual address 0.
    NullPage = 0,
    /// Just below the stack guard page: an overflow past the guard.
    /// `fault_offset` is the distance below the reserved span.
    BelowStackGuard = 1,
    /// A bounded run past the end of an owned mapping. `fault_offset` is the
    /// distance past the region end.
    PastRegion = 2,
    /// Genuinely far from every mapping; `fault_offset` is meaningless (`0`).
    #[default]
    Wild = 3,
    /// Inside a region the task legitimately owns (a reserved anonymous
    /// mapping, a file mapping, or its stack span) but which could not be
    /// resolved — the deterministic out-of-memory case. `fault_offset` is
    /// meaningless (`0`): the address is memory the task reserved, not a
    /// stray pointer, so no distance is leaked.
    InRegion = 4,
}

impl CrashFaultBucket {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a discriminant, failing closed on an unknown value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value outside the closed set.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::NullPage),
            1 => Ok(Self::BelowStackGuard),
            2 => Ok(Self::PastRegion),
            3 => Ok(Self::Wild),
            4 => Ok(Self::InRegion),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One named general-purpose register value inside a [`CrashRecord`].
///
/// The register **value** is absolute — this is the privileged-debugger
/// datum the whole record is capability-gated for. The name is a fixed
/// eight-byte ASCII field, right-padded with `0x00`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CrashNamedReg {
    name_len: u8,
    name: [u8; CRASH_REG_NAME_LEN],
    /// The register's absolute value at fault entry.
    pub value: u64,
}

impl CrashNamedReg {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = CRASH_REG_NAME_LEN + 8;

    /// Build a named register, copying up to [`CRASH_REG_NAME_LEN`] bytes of
    /// `name`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `name` exceeds [`CRASH_REG_NAME_LEN`];
    /// the name is never silently truncated.
    pub fn new(name: &[u8], value: u64) -> Result<Self, Errno> {
        if name.len() > CRASH_REG_NAME_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; CRASH_REG_NAME_LEN];
        buf[..name.len()].copy_from_slice(name);
        let name_len = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            name_len,
            name: buf,
            value,
        })
    }

    /// Borrow the valid prefix of the register name.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..CRASH_REG_NAME_LEN].copy_from_slice(&self.name);
        put_u64(&mut out, CRASH_REG_NAME_LEN, self.value);
        out
    }

    /// Decode from `bytes`, recovering the name length from the trailing
    /// `0x00` padding.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if the slice is short.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut name = [0u8; CRASH_REG_NAME_LEN];
        name.copy_from_slice(&bytes[..CRASH_REG_NAME_LEN]);
        // The name length is the ASCII prefix before the first NUL pad byte;
        // it is at most CRASH_REG_NAME_LEN, so the width conversion is exact.
        let name_len = u8::try_from(
            name.iter()
                .position(|&b| b == 0)
                .unwrap_or(CRASH_REG_NAME_LEN),
        )
        .unwrap_or(0);
        Ok(Self {
            name_len,
            name,
            value: read_u64(bytes, CRASH_REG_NAME_LEN),
        })
    }
}

/// Request payload for [`SysinfoQueryId::CRASH_RECORD`].
///
/// The response is a sequence of [`CrashRecord`]s paged with
/// `offset`/`limit`, exactly like the other list queries.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CrashRecordRequest {
    /// Index of the first crash record to return.
    pub offset: u32,
    /// Maximum number of [`CrashRecord`]s the caller will accept.
    pub limit: u16,
    /// Reserved; must be zero in `sysinfo-v1`.
    pub flags: u16,
}

impl CrashRecordRequest {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.offset);
        put_u16(&mut out, 4, self.limit);
        put_u16(&mut out, 6, self.flags);
        out
    }

    /// Decode `bytes` into a [`CrashRecordRequest`].
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the reserved `flags` field is non-zero.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, 6);
        if flags != 0 {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            offset: read_u32(bytes, 0),
            limit: read_u16(bytes, 4),
            flags,
        })
    }
}

/// [`CrashRecord::flags`] bit: the fatal access was a **store** (`true`) as
/// opposed to a load.
pub const CRASH_FLAG_WRITE: u8 = 1 << 0;
/// [`CrashRecord::flags`] bit: [`CrashRecord::pc`] and every backtrace frame
/// are **program-relative offsets** (the PIE load base was known). When
/// clear, they are absolute addresses — an honest degradation that only
/// affects offline symbolication convenience, still behind the capability
/// gate.
pub const CRASH_FLAG_LOAD_BASE_KNOWN: u8 = 1 << 1;
/// [`CrashRecord::flags`] bit: [`CrashRecord::fp`] was a usable frame
/// pointer, so the backtrace followed the frame-pointer chain. When clear,
/// the port did not save the fp at trap entry and the backtrace is `pc`
/// only.
pub const CRASH_FLAG_FP_VALID: u8 = 1 << 2;
/// The union of all defined [`CrashRecord::flags`] bits; any other bit set
/// on the wire fails the decode closed.
pub const CRASH_FLAG_ALL: u8 = CRASH_FLAG_WRITE | CRASH_FLAG_LOAD_BASE_KNOWN | CRASH_FLAG_FP_VALID;

/// One recorded user-fault kill inside a [`SysinfoQueryId::CRASH_RECORD`]
/// response.
///
/// The privileged-debugger post-mortem of a task killed by an unresolvable
/// memory fault. Identity ([`proc_id`](Self::proc_id) / [`pid`](Self::pid) /
/// name / uid / gid) and cause ([`fault_class`](Self::fault_class),
/// [`fault_bucket`](Self::fault_bucket), the `write` flag) are attested by
/// the kernel from the dying task's own state, never a caller claim.
///
/// # Leak policy
///
/// The faulting [`pc`](Self::pc) and every [`frame`](Self::frames) are
/// **program-relative offsets** when [`CRASH_FLAG_LOAD_BASE_KNOWN`] is set,
/// and [`fault_offset`](Self::fault_offset) is a **distance** from the anchor
/// [`fault_bucket`](Self::fault_bucket) names — never an absolute virtual
/// address. The register **values** ([`sp`](Self::sp), [`fp`](Self::fp), and
/// each [`CrashNamedReg`]) are the one absolute datum, which is why the whole
/// query is gated on `CAP_SYSINFO_KERNEL`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CrashRecord {
    /// Kernel-attested, unforgeable process-instance identity of the crashed
    /// task (survives numeric PID reuse).
    pub proc_id: ProcId,
    /// Numeric task id at crash time (reused across lifetimes; display only).
    pub pid: u64,
    /// Faulting distance from the [`fault_bucket`](Self::fault_bucket)
    /// anchor, or `0` for [`CrashFaultBucket::Wild`].
    pub fault_offset: u64,
    /// Faulting program counter — program-relative when
    /// [`CRASH_FLAG_LOAD_BASE_KNOWN`] is set, else absolute.
    pub pc: u64,
    /// Faulting stack pointer (absolute).
    pub sp: u64,
    /// Faulting frame pointer (absolute); meaningful only when
    /// [`CRASH_FLAG_FP_VALID`] is set.
    pub fp: u64,
    /// Owning user id of the crashed task.
    pub uid: u32,
    /// Owning primary group id of the crashed task.
    pub gid: u32,
    /// State bits: [`CRASH_FLAG_WRITE`] / [`CRASH_FLAG_LOAD_BASE_KNOWN`] /
    /// [`CRASH_FLAG_FP_VALID`].
    pub flags: u8,
    /// Why the resolver refused the access.
    pub fault_class: CrashFaultClass,
    /// Where the faulting address sat, relative to the task's mappings.
    pub fault_bucket: CrashFaultBucket,
    name_len: u8,
    frame_count: u16,
    reg_count: u16,
    name: [u8; PROCESS_NAME_MAX],
    frames: [u64; CRASH_MAX_FRAMES],
    regs: [CrashNamedReg; CRASH_MAX_REGS],
}

impl CrashRecord {
    /// Byte offset of the inline `name` field.
    const NAME_OFF: usize = 72;
    /// Byte offset of the inline `frames` array.
    const FRAMES_OFF: usize = Self::NAME_OFF + PROCESS_NAME_MAX;
    /// Byte offset of the inline `regs` array.
    const REGS_OFF: usize = Self::FRAMES_OFF + CRASH_MAX_FRAMES * 8;
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = Self::REGS_OFF + CRASH_MAX_REGS * CrashNamedReg::WIRE_LEN;

    /// Construct a record for the crashed task's identity, with an empty
    /// backtrace and register set (populate them with [`Self::push_frame`]
    /// and [`Self::push_reg`]).
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `name` exceeds [`PROCESS_NAME_MAX`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        proc_id: ProcId,
        pid: u64,
        uid: u32,
        gid: u32,
        write: bool,
        fault_class: CrashFaultClass,
        fault_bucket: CrashFaultBucket,
        fault_offset: u64,
        name: &[u8],
    ) -> Result<Self, Errno> {
        if name.len() > PROCESS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut name_buf = [0u8; PROCESS_NAME_MAX];
        name_buf[..name.len()].copy_from_slice(name);
        let name_len = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(Self {
            proc_id,
            pid,
            fault_offset,
            pc: 0,
            sp: 0,
            fp: 0,
            uid,
            gid,
            flags: if write { CRASH_FLAG_WRITE } else { 0 },
            fault_class,
            fault_bucket,
            name_len,
            frame_count: 0,
            reg_count: 0,
            name: name_buf,
            frames: [0u64; CRASH_MAX_FRAMES],
            regs: [CrashNamedReg::default(); CRASH_MAX_REGS],
        })
    }

    /// Record the faulting register anchors, marking `pc` program-relative
    /// (`load_base_known`) and `fp` usable (`fp_valid`) honestly.
    pub fn set_registers(
        &mut self,
        pc: u64,
        sp: u64,
        fp: u64,
        load_base_known: bool,
        fp_valid: bool,
    ) {
        self.pc = pc;
        self.sp = sp;
        self.fp = fp;
        if load_base_known {
            self.flags |= CRASH_FLAG_LOAD_BASE_KNOWN;
        }
        if fp_valid {
            self.flags |= CRASH_FLAG_FP_VALID;
        }
    }

    /// Append one backtrace frame offset, returning `false` (dropped) when
    /// the fixed record is already full — never panics, never grows.
    pub fn push_frame(&mut self, frame: u64) -> bool {
        let i = self.frame_count as usize;
        if i >= CRASH_MAX_FRAMES {
            return false;
        }
        self.frames[i] = frame;
        self.frame_count += 1;
        true
    }

    /// Append one named register, returning `false` (dropped) when the fixed
    /// record is already full.
    pub fn push_reg(&mut self, reg: CrashNamedReg) -> bool {
        let i = self.reg_count as usize;
        if i >= CRASH_MAX_REGS {
            return false;
        }
        self.regs[i] = reg;
        self.reg_count += 1;
        true
    }

    /// `true` if the fatal access was a store.
    #[must_use]
    pub const fn is_write(&self) -> bool {
        self.flags & CRASH_FLAG_WRITE != 0
    }

    /// `true` if [`Self::pc`] and the frames are program-relative offsets.
    #[must_use]
    pub const fn load_base_known(&self) -> bool {
        self.flags & CRASH_FLAG_LOAD_BASE_KNOWN != 0
    }

    /// `true` if [`Self::fp`] was a usable frame pointer.
    #[must_use]
    pub const fn fp_valid(&self) -> bool {
        self.flags & CRASH_FLAG_FP_VALID != 0
    }

    /// Borrow the valid prefix of the name buffer.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// Borrow the recorded backtrace frames (frame 0 upward).
    #[must_use]
    pub fn frames(&self) -> &[u64] {
        &self.frames[..self.frame_count as usize]
    }

    /// Borrow the recorded register file.
    #[must_use]
    pub fn regs(&self) -> &[CrashNamedReg] {
        &self.regs[..self.reg_count as usize]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..16].copy_from_slice(&self.proc_id.to_le_bytes());
        put_u64(&mut out, 16, self.pid);
        put_u32(&mut out, 24, self.uid);
        put_u32(&mut out, 28, self.gid);
        out[32] = self.flags;
        out[33] = self.fault_class.as_u8();
        out[34] = self.fault_bucket.as_u8();
        out[35] = self.name_len;
        put_u16(&mut out, 36, self.frame_count);
        put_u16(&mut out, 38, self.reg_count);
        put_u64(&mut out, 40, self.fault_offset);
        put_u64(&mut out, 48, self.pc);
        put_u64(&mut out, 56, self.sp);
        put_u64(&mut out, 64, self.fp);
        out[Self::NAME_OFF..Self::FRAMES_OFF].copy_from_slice(&self.name);
        for (i, &frame) in self.frames.iter().enumerate() {
            put_u64(&mut out, Self::FRAMES_OFF + i * 8, frame);
        }
        for (i, reg) in self.regs.iter().enumerate() {
            let base = Self::REGS_OFF + i * CrashNamedReg::WIRE_LEN;
            out[base..base + CrashNamedReg::WIRE_LEN].copy_from_slice(&reg.to_le_bytes());
        }
        out
    }

    /// Decode from `bytes`, failing closed on any structurally invalid
    /// record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if the slice is short.
    /// * [`Errno::OutOfRange`] for an unknown [`CrashFaultClass`] /
    ///   [`CrashFaultBucket`].
    /// * [`Errno::LengthOutOfRange`] if `name_len` exceeds
    ///   [`PROCESS_NAME_MAX`] or a count exceeds its array bound.
    /// * [`Errno::BadMagic`] if a reserved `flags` bit is set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let proc_id = ProcId::from_bytes(&bytes[0..16])?;
        let flags = bytes[32];
        if flags & !CRASH_FLAG_ALL != 0 {
            return Err(Errno::BadMagic);
        }
        let fault_class = CrashFaultClass::from_u8(bytes[33])?;
        let fault_bucket = CrashFaultBucket::from_u8(bytes[34])?;
        let name_len = bytes[35];
        if name_len as usize > PROCESS_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let frame_count = read_u16(bytes, 36);
        let reg_count = read_u16(bytes, 38);
        if frame_count as usize > CRASH_MAX_FRAMES || reg_count as usize > CRASH_MAX_REGS {
            return Err(Errno::LengthOutOfRange);
        }
        let mut name = [0u8; PROCESS_NAME_MAX];
        name.copy_from_slice(&bytes[Self::NAME_OFF..Self::FRAMES_OFF]);
        let mut frames = [0u64; CRASH_MAX_FRAMES];
        for (i, frame) in frames.iter_mut().enumerate() {
            *frame = read_u64(bytes, Self::FRAMES_OFF + i * 8);
        }
        let mut regs = [CrashNamedReg::default(); CRASH_MAX_REGS];
        for (i, reg) in regs.iter_mut().enumerate() {
            let base = Self::REGS_OFF + i * CrashNamedReg::WIRE_LEN;
            *reg = CrashNamedReg::from_bytes(&bytes[base..base + CrashNamedReg::WIRE_LEN])?;
        }
        Ok(Self {
            proc_id,
            pid: read_u64(bytes, 16),
            fault_offset: read_u64(bytes, 40),
            pc: read_u64(bytes, 48),
            sp: read_u64(bytes, 56),
            fp: read_u64(bytes, 64),
            uid: read_u32(bytes, 24),
            gid: read_u32(bytes, 28),
            flags,
            fault_class,
            fault_bucket,
            name_len,
            frame_count,
            reg_count,
            name,
            frames,
            regs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        encoded_query_table, spec_for, BlkHealthState, BlkHealthTransition, CpuTimeListRequest,
        CpuTimeRecord, HardwareTreeRequest, KernelMemoryStats, LoadAverage, MemoryTotal,
        MountAvailability, MountListRequest, MountRecord, MountVolumeState, ProcessListRequest,
        ProcessRecord, ProcessState, ResourceLimitRecord, SeatListRequest, SeatRecord,
        SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime, UserDirectoryRecord,
        UserDirectoryRequest, VolumeStats, ENCODED_QUERY_TABLE, ENCODED_QUERY_TABLE_LEN,
        HOSTNAME_MAX, LOAD_FIXED_SHIFT, MACHINE_ID_LEN, MOUNT_FSTYPE_MAX, MOUNT_SOURCE_MAX,
        MOUNT_TARGET_MAX, PROCESS_CPU_NONE, PROCESS_NAME_MAX, RESOURCE_LIMITS_REPORT_LEN,
        SYSINFO_MAX_PAYLOAD_LEN, SYSINFO_QUERIES, SYSINFO_QUERY_NAME_MAX, SYSINFO_QUERY_RECORD_LEN,
        SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1,
        USER_DIRECTORY_NAME_MAX,
    };
    use super::{
        CpuCoreClass, CpuInfoListRequest, CpuInfoRecord, CPU_INFO_FLAG_FREQ_MEASURED,
        CPU_MODEL_NAME_MAX,
    };
    use super::{
        CrashFaultBucket, CrashFaultClass, CrashNamedReg, CrashRecord, CrashRecordRequest,
        CRASH_MAX_FRAMES, CRASH_MAX_REGS, CRASH_REG_NAME_LEN,
    };
    use super::{DesktopFrameRecord, DesktopFrameStatsRequest, DesktopFrameTotals};
    use super::{IrqListRequest, IrqRecord, IRQ_FLAG_QUARANTINED};
    use super::{VolumeIoHealthRecord, VolumeIoHealthRequest};
    use crate::blkio::{BlkDeviceClass, BlkHealthCounters, BLK_HEALTH_COUNTERS_LEN};
    use crate::driver::filesystem::MountFlags;
    use crate::origin::ProcId;
    use crate::process::SchedPriority;
    use crate::rlimit::{LimitKind, ResourceLimit};
    use crate::time::{Duration64, Time64};
    use crate::{CapabilityId, Errno};

    #[test]
    fn well_known_query_ids_are_frozen() {
        // Numeric assignments are part of sysinfo-v1; do not renumber.
        assert_eq!(SysinfoQueryId::SELF_PROCESS_LIST.as_u16(), 0);
        assert_eq!(SysinfoQueryId::GLOBAL_PROCESS_LIST.as_u16(), 1);
        assert_eq!(SysinfoQueryId::KERNEL_MEMORY_STATS.as_u16(), 2);
        assert_eq!(SysinfoQueryId::HARDWARE_TREE.as_u16(), 3);
        assert_eq!(SysinfoQueryId::SYSTEM_IDENTITY.as_u16(), 4);
        assert_eq!(SysinfoQueryId::UPTIME.as_u16(), 5);
        assert_eq!(SysinfoQueryId::MOUNT_LIST.as_u16(), 6);
        assert_eq!(SysinfoQueryId::RESOURCE_LIMITS.as_u16(), 7);
        assert_eq!(SysinfoQueryId::PROCESS_IDENTITY.as_u16(), 8);
        assert_eq!(SysinfoQueryId::SEAT_LIST.as_u16(), 12);
        assert_eq!(SysinfoQueryId::MEMORY_PRESSURE.as_u16(), 13);
        assert_eq!(SysinfoQueryId::RECLAIM_STATS.as_u16(), 14);
        assert_eq!(SysinfoQueryId::RAMZIP_STATS.as_u16(), 15);
        assert_eq!(SysinfoQueryId::CPU_LOAD.as_u16(), 16);
        assert_eq!(SysinfoQueryId::NET_INTERFACE_FACTS.as_u16(), 17);
        assert_eq!(SysinfoQueryId::NET_INTERFACE_STATE.as_u16(), 18);
        assert_eq!(SysinfoQueryId::IRQ_LIST.as_u16(), 19);
        assert_eq!(SysinfoQueryId::CRASH_RECORD.as_u16(), 20);
        assert_eq!(SysinfoQueryId::NET_INTERFACE_COUNTERS.as_u16(), 21);
        assert_eq!(SysinfoQueryId::NET_INTERFACE_RATES.as_u16(), 22);
        assert_eq!(SysinfoQueryId::NET_SOCKETS.as_u16(), 23);
        assert_eq!(
            spec_for(SysinfoQueryId::NET_SOCKETS)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_GLOBAL)
        );
        assert!(spec_for(SysinfoQueryId::NET_SOCKETS).unwrap().audit);
        assert_eq!(SysinfoQueryId::NET_BOND_MEMBERS.as_u16(), 24);
        assert_eq!(
            spec_for(SysinfoQueryId::NET_BOND_MEMBERS)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_GLOBAL)
        );
        assert!(spec_for(SysinfoQueryId::NET_BOND_MEMBERS).unwrap().audit);
        assert_eq!(SysinfoQueryId::CPU_INFO.as_u16(), 25);
        // The `/proc/cpuinfo`-class query is ungated and unaudited: public
        // hardware facts, no cross-principal secret.
        assert_eq!(
            spec_for(SysinfoQueryId::CPU_INFO)
                .unwrap()
                .required_capability,
            None
        );
        assert!(!spec_for(SysinfoQueryId::CPU_INFO).unwrap().audit);
        assert_eq!(SysinfoQueryId::NET_RESOLVER_SERVERS.as_u16(), 26);
        // The active resolver-server set is public host configuration (the
        // resolv.conf analogue): ungated and unaudited, no per-principal
        // secret.
        assert_eq!(
            spec_for(SysinfoQueryId::NET_RESOLVER_SERVERS)
                .unwrap()
                .required_capability,
            None
        );
        assert!(
            !spec_for(SysinfoQueryId::NET_RESOLVER_SERVERS)
                .unwrap()
                .audit
        );
        assert_eq!(SysinfoQueryId::NET_TIME_SERVERS.as_u16(), 37);
        // The DHCP-learned time servers are public network configuration on
        // the same footing, and confer no authority: ungated, unaudited.
        assert_eq!(
            spec_for(SysinfoQueryId::NET_TIME_SERVERS)
                .unwrap()
                .required_capability,
            None
        );
        assert!(!spec_for(SysinfoQueryId::NET_TIME_SERVERS).unwrap().audit);
        assert_eq!(SysinfoQueryId::VOLUME_IO_HEALTH.as_u16(), 27);
        // Per-device storage I/O health is kernel-wide operational state:
        // gated on `CAP_SYSINFO_KERNEL` and audited, like the memory-pressure
        // and reclaim gauges.
        assert_eq!(
            spec_for(SysinfoQueryId::VOLUME_IO_HEALTH)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_KERNEL)
        );
        assert!(spec_for(SysinfoQueryId::VOLUME_IO_HEALTH).unwrap().audit);
        assert_eq!(SysinfoQueryId::NET_STACK_DEFENCE.as_u16(), 34);
        // Stack-wide, cross-principal TCP defence totals: gated on
        // `CAP_SYSINFO_GLOBAL` and audited, like the interface counters.
        assert_eq!(
            spec_for(SysinfoQueryId::NET_STACK_DEFENCE)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_GLOBAL)
        );
        assert!(spec_for(SysinfoQueryId::NET_STACK_DEFENCE).unwrap().audit);
        assert_eq!(SYSINFO_VERSION_CURRENT, SYSINFO_VERSION_V1);
    }

    #[test]
    fn reply_frame_round_trips_ok_and_error() {
        use super::{decode_reply, encode_reply_err, encode_reply_ok, SYSINFO_REPLY_STATUS_LEN};
        // OK frame: zero status word then the payload verbatim.
        let payload = [0xAA, 0xBB, 0xCC];
        let mut buf = [0u8; 16];
        let n = encode_reply_ok(&payload, &mut buf).unwrap();
        assert_eq!(n, SYSINFO_REPLY_STATUS_LEN + payload.len());
        assert_eq!(decode_reply(&buf[..n]), Ok(&payload[..]));

        // Error frame: the errno status word, no payload.
        let n = encode_reply_err(Errno::PermissionDenied, &mut buf).unwrap();
        assert_eq!(n, SYSINFO_REPLY_STATUS_LEN);
        assert_eq!(decode_reply(&buf[..n]), Err(Errno::PermissionDenied));

        // Fail closed: a reply shorter than the status word, and a status
        // word that is not a defined errno.
        assert_eq!(decode_reply(&[0u8; 2]), Err(Errno::BufferTooSmall));
        let mut bogus = [0u8; SYSINFO_REPLY_STATUS_LEN];
        super::put_u32(&mut bogus, 0, 0xFFFF_FFFF);
        assert_eq!(decode_reply(&bogus), Err(Errno::OutOfRange));

        // Encoders fail closed on a short output buffer.
        assert_eq!(
            encode_reply_ok(&payload, &mut [0u8; 2]),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(
            encode_reply_err(Errno::NotFound, &mut [0u8; 2]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn introspect_domain_round_trips_and_fails_closed() {
        use super::IntrospectDomain;
        // Discriminants are part of abi-v1; do not renumber.
        for (raw, domain) in [
            (0u32, IntrospectDomain::Processes),
            (1, IntrospectDomain::KernelMemory),
            (2, IntrospectDomain::Mounts),
            (3, IntrospectDomain::Identity),
            (4, IntrospectDomain::Uptime),
            (5, IntrospectDomain::TaskLimits),
            (6, IntrospectDomain::LoadAverage),
            (7, IntrospectDomain::UserDirectory),
            (8, IntrospectDomain::CpuTimes),
            (9, IntrospectDomain::Seats),
            (10, IntrospectDomain::MemoryPressure),
            (11, IntrospectDomain::CacheLedgers),
            (12, IntrospectDomain::Ramzip),
            (13, IntrospectDomain::CpuLoad),
            (14, IntrospectDomain::Irqs),
            (15, IntrospectDomain::Crashes),
            (16, IntrospectDomain::CpuInfo),
            (17, IntrospectDomain::VolumeIoHealth),
            (18, IntrospectDomain::MemoryPressureBand),
            (19, IntrospectDomain::MemoryTotalBytes),
        ] {
            assert_eq!(domain.as_u32(), raw);
            assert_eq!(IntrospectDomain::from_u32(raw), Ok(domain));
        }
        // Any value outside the closed set is rejected, not guessed.
        assert_eq!(IntrospectDomain::from_u32(20), Err(Errno::OutOfRange));
        assert_eq!(IntrospectDomain::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn memory_pressure_band_round_trips_and_fails_closed() {
        use super::{MemoryPressureBand, PRESSURE_BAND_COUNT, PRESSURE_BAND_NAMES};

        let report = MemoryPressureBand {
            band: 2,
            reserved: [0; 7],
        };
        let encoded = report.to_le_bytes();
        assert_eq!(encoded.len(), MemoryPressureBand::WIRE_LEN);
        let decoded = MemoryPressureBand::from_bytes(&encoded).expect("round trip");
        assert_eq!(decoded, report);
        assert_eq!(PRESSURE_BAND_NAMES[usize::from(decoded.band)], "moderate");

        assert_eq!(
            MemoryPressureBand::from_bytes(&encoded[..7]),
            Err(Errno::BufferTooSmall)
        );

        // An unrecognised band depth is refused, never read as the
        // shallowest band: a consumer that mistook it for "normal"
        // would grow its caches while the machine was starving.
        let mut unknown = encoded;
        unknown[0] = u8::try_from(PRESSURE_BAND_COUNT).expect("band count fits a byte");
        assert_eq!(
            MemoryPressureBand::from_bytes(&unknown),
            Err(Errno::OutOfRange)
        );

        let mut dirty = encoded;
        dirty[5] = 1;
        assert_eq!(
            MemoryPressureBand::from_bytes(&dirty),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn the_band_query_is_ungated_while_the_detailed_view_stays_gated() {
        let band = spec_for(SysinfoQueryId::MEMORY_PRESSURE_BAND).expect("registered");
        assert_eq!(band.name, "memory_pressure_band");
        assert_eq!(band.required_capability, None);
        assert!(!band.audit);

        let detail = spec_for(SysinfoQueryId::MEMORY_PRESSURE).expect("registered");
        assert_eq!(
            detail.required_capability,
            Some(CapabilityId::SYSINFO_KERNEL)
        );
        assert!(detail.audit);
    }

    #[test]
    fn memory_total_round_trips_and_fails_closed() {
        let total = MemoryTotal {
            total_bytes: 1 << 30,
        };
        let encoded = total.to_le_bytes();
        assert_eq!(encoded.len(), MemoryTotal::WIRE_LEN);
        assert_eq!(MemoryTotal::from_bytes(&encoded), Ok(total));

        // Short buffer.
        assert_eq!(
            MemoryTotal::from_bytes(&encoded[..MemoryTotal::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Over-long buffer: a fixed-size scalar record has no trailing bytes
        // to ignore, so extra bytes are refused rather than silently dropped.
        let mut over_long = [0u8; MemoryTotal::WIRE_LEN + 1];
        over_long[..MemoryTotal::WIRE_LEN].copy_from_slice(&encoded);
        assert_eq!(
            MemoryTotal::from_bytes(&over_long),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn the_memory_total_query_is_ungated_and_unaudited() {
        use super::IntrospectDomain;
        // The identifiers are part of abi-v1; do not renumber.
        assert_eq!(SysinfoQueryId::MEMORY_TOTAL.as_u16(), 29);
        assert_eq!(IntrospectDomain::MemoryTotalBytes.as_u32(), 19);

        let total = spec_for(SysinfoQueryId::MEMORY_TOTAL).expect("registered");
        assert_eq!(total.name, "memory_total");
        assert_eq!(total.required_capability, None);
        assert!(!total.audit);
    }

    #[test]
    fn load_average_round_trips_and_renders_fixed_point() {
        let load = LoadAverage {
            load1: (3 << LOAD_FIXED_SHIFT) + (1 << LOAD_FIXED_SHIFT) / 2,
            load5: 1 << LOAD_FIXED_SHIFT,
            load15: 0,
            runnable: 4,
            total_tasks: 17,
            users: 2,
        };
        let decoded = LoadAverage::from_bytes(&load.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, load);
        assert_eq!(LoadAverage::whole(load.load1), 3);
        assert_eq!(LoadAverage::centis(load.load1), 50);
        assert_eq!(LoadAverage::whole(load.load15), 0);
        assert_eq!(LoadAverage::centis(load.load15), 0);
        assert_eq!(
            LoadAverage::from_bytes(&[0u8; LoadAverage::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn user_directory_request_and_record_round_trip_and_fail_closed() {
        let req = UserDirectoryRequest {
            offset: 3,
            limit: 16,
            flags: 0,
        };
        assert_eq!(
            UserDirectoryRequest::from_bytes(&req.to_le_bytes()),
            Ok(req)
        );
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1; // reserved flag set
        assert_eq!(
            UserDirectoryRequest::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            UserDirectoryRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );

        let rec = UserDirectoryRecord::new(1000, b"alice").expect("record");
        assert_eq!(rec.name_bytes(), b"alice");
        let decoded = UserDirectoryRecord::from_bytes(&rec.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, rec);
        assert_eq!(decoded.uid, 1000);

        let too_long = [b'x'; USER_DIRECTORY_NAME_MAX + 1];
        assert_eq!(
            UserDirectoryRecord::new(0, &too_long),
            Err(Errno::LengthOutOfRange)
        );
        let mut bytes = rec.to_le_bytes();
        bytes[4] = u8::try_from(USER_DIRECTORY_NAME_MAX + 1).unwrap();
        assert_eq!(
            UserDirectoryRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            UserDirectoryRecord::from_bytes(&[0u8; UserDirectoryRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn query_id_from_raw_enforces_bounds() {
        assert_eq!(
            SysinfoQueryId::from_raw(SysinfoQueryId::MAX).map(SysinfoQueryId::as_u16),
            Ok(1023)
        );
        assert_eq!(
            SysinfoQueryId::from_raw(SysinfoQueryId::MAX + 1),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn registry_is_dense_and_ordered() {
        for (idx, spec) in SYSINFO_QUERIES.iter().enumerate() {
            assert_eq!(spec.id.as_u16() as usize, idx, "{}", spec.name);
            assert!(spec.name.is_ascii(), "{} non-ASCII name", spec.name);
            assert!(
                spec.name.len() <= SYSINFO_QUERY_NAME_MAX,
                "{} exceeds name max",
                spec.name
            );
        }
    }

    #[test]
    fn capability_gates_are_frozen() {
        // Self-scoped observers are ungated; cross-principal / kernel /
        // hardware queries each carry their declared capability.
        assert_eq!(
            spec_for(SysinfoQueryId::SELF_PROCESS_LIST)
                .unwrap()
                .required_capability,
            None
        );
        assert_eq!(
            spec_for(SysinfoQueryId::GLOBAL_PROCESS_LIST)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_GLOBAL)
        );
        assert_eq!(
            spec_for(SysinfoQueryId::KERNEL_MEMORY_STATS)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_KERNEL)
        );
        assert_eq!(
            spec_for(SysinfoQueryId::HARDWARE_TREE)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_HW)
        );
        // The mount table is system-wide but secret-free, so it is ungated
        // like uptime and identity.
        assert_eq!(
            spec_for(SysinfoQueryId::MOUNT_LIST)
                .unwrap()
                .required_capability,
            None
        );
        // Privileged queries are audited; self-scoped observers are not.
        assert!(spec_for(SysinfoQueryId::GLOBAL_PROCESS_LIST).unwrap().audit);
        assert!(!spec_for(SysinfoQueryId::SELF_PROCESS_LIST).unwrap().audit);
        assert!(!spec_for(SysinfoQueryId::UPTIME).unwrap().audit);
        assert!(!spec_for(SysinfoQueryId::MOUNT_LIST).unwrap().audit);
        // A principal reads its own limits + usage; self-scoped, so ungated
        // and unaudited like the other self-scoped observers.
        assert_eq!(
            spec_for(SysinfoQueryId::RESOURCE_LIMITS)
                .unwrap()
                .required_capability,
            None
        );
        assert!(!spec_for(SysinfoQueryId::RESOURCE_LIMITS).unwrap().audit);
        // A principal reads its own attested origin; self-scoped, so ungated
        // and unaudited.
        assert_eq!(
            spec_for(SysinfoQueryId::PROCESS_IDENTITY)
                .unwrap()
                .required_capability,
            None
        );
        assert!(!spec_for(SysinfoQueryId::PROCESS_IDENTITY).unwrap().audit);
        // The seat inventory names which task owns each display: gated like
        // the hardware tree, and audited.
        assert_eq!(
            spec_for(SysinfoQueryId::SEAT_LIST)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_HW)
        );
        assert!(spec_for(SysinfoQueryId::SEAT_LIST).unwrap().audit);
        // The IRQ table names which task owns each interrupt line: gated
        // like the hardware tree and seat inventory, and audited.
        assert_eq!(
            spec_for(SysinfoQueryId::IRQ_LIST)
                .unwrap()
                .required_capability,
            Some(CapabilityId::SYSINFO_HW)
        );
        assert!(spec_for(SysinfoQueryId::IRQ_LIST).unwrap().audit);
        // The kernel-introspection queries share KERNEL_MEMORY_STATS's
        // boundary: gated on CAP_SYSINFO_KERNEL and audited. The crash
        // record joins them because it carries absolute register values.
        for id in [
            SysinfoQueryId::MEMORY_PRESSURE,
            SysinfoQueryId::RECLAIM_STATS,
            SysinfoQueryId::RAMZIP_STATS,
            SysinfoQueryId::CPU_LOAD,
            SysinfoQueryId::CRASH_RECORD,
            SysinfoQueryId::VOLUME_IO_HEALTH,
        ] {
            let spec = spec_for(id).unwrap();
            assert_eq!(
                spec.required_capability,
                Some(CapabilityId::SYSINFO_KERNEL),
                "{}",
                spec.name
            );
            assert!(spec.audit, "{}", spec.name);
        }
    }

    #[test]
    fn seat_list_request_round_trips_and_rejects_reserved() {
        let req = SeatListRequest {
            offset: 1,
            limit: 4,
            flags: 0,
        };
        assert_eq!(SeatListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(SeatListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            SeatListRequest::from_bytes(&[0u8; SeatListRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn hardware_tree_request_round_trips_and_rejects_reserved() {
        let req = HardwareTreeRequest {
            offset: 14,
            limit: 14,
            flags: 0,
        };
        assert_eq!(HardwareTreeRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(
            HardwareTreeRequest::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            HardwareTreeRequest::from_bytes(&[0u8; HardwareTreeRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn seat_record_round_trips_owned_and_unowned() {
        let held = SeatRecord {
            seat_id: 0,
            owner_task: 7,
            generation: 3,
            foreground_console: 1,
            flags: super::SEAT_FLAG_OWNED,
        };
        assert_eq!(SeatRecord::WIRE_LEN, 32);
        assert_eq!(SeatRecord::from_bytes(&held.to_le_bytes()), Ok(held));
        assert!(held.owned());
        assert_eq!(held.owner(), Some(7));

        let unowned = SeatRecord {
            seat_id: 0,
            owner_task: 0,
            generation: 4,
            foreground_console: 0,
            flags: 0,
        };
        assert_eq!(SeatRecord::from_bytes(&unowned.to_le_bytes()), Ok(unowned));
        assert!(!unowned.owned());
        assert_eq!(unowned.owner(), None);
    }

    #[test]
    fn seat_record_fails_closed_on_corrupt_wire() {
        let good = SeatRecord {
            seat_id: 0,
            owner_task: 7,
            generation: 1,
            foreground_console: 0,
            flags: super::SEAT_FLAG_OWNED,
        }
        .to_le_bytes();
        // A reserved flag bit is wire corruption.
        let mut reserved = good;
        reserved[29] = 0x80;
        assert_eq!(SeatRecord::from_bytes(&reserved), Err(Errno::BadMagic));
        // An unowned record must not smuggle an owner.
        let mut phantom_owner = good;
        phantom_owner[28] = 0;
        assert_eq!(SeatRecord::from_bytes(&phantom_owner), Err(Errno::BadMagic));
        // Short buffer.
        assert_eq!(
            SeatRecord::from_bytes(&good[..SeatRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn resource_limit_record_round_trips() {
        let limit = ResourceLimit::new(4096, 1 << 20).expect("well-formed");
        let rec = ResourceLimitRecord::new(LimitKind::AddressSpaceBytes, limit, 2048);
        assert_eq!(ResourceLimitRecord::WIRE_LEN, 32);
        assert_eq!(RESOURCE_LIMITS_REPORT_LEN, 32 * LimitKind::COUNT);
        let bytes = rec.to_le_bytes();
        assert_eq!(bytes.len(), ResourceLimitRecord::WIRE_LEN);
        assert_eq!(ResourceLimitRecord::from_bytes(&bytes), Ok(rec));
    }

    #[test]
    fn resource_limit_record_fails_closed() {
        let rec = ResourceLimitRecord::new(LimitKind::Processes, ResourceLimit::UNLIMITED, 7);
        let good = rec.to_le_bytes();
        // Short buffer.
        assert_eq!(
            ResourceLimitRecord::from_bytes(&good[..ResourceLimitRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Non-zero reserved word is wire corruption.
        let mut reserved = good;
        reserved[4] = 1;
        assert_eq!(
            ResourceLimitRecord::from_bytes(&reserved),
            Err(Errno::BadMagic)
        );
        // Unassigned LimitKind discriminant.
        let mut bad_kind = good;
        bad_kind[0] = 0xFF;
        assert_eq!(
            ResourceLimitRecord::from_bytes(&bad_kind),
            Err(Errno::OutOfRange)
        );
        // Malformed embedded limit (soft > hard).
        let mut bad_limit = good;
        bad_limit[8] = 10; // soft low byte
        bad_limit[16] = 5; // hard low byte
        assert_eq!(
            ResourceLimitRecord::from_bytes(&bad_limit),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn spec_for_rejects_unassigned_id() {
        let past = SysinfoQueryId::from_raw(u16::try_from(SYSINFO_QUERIES.len()).unwrap()).unwrap();
        assert!(spec_for(past).is_none());
    }

    #[test]
    fn encoded_table_length_and_first_record() {
        assert_eq!(
            ENCODED_QUERY_TABLE_LEN,
            SYSINFO_QUERY_RECORD_LEN * SYSINFO_QUERIES.len()
        );
        assert_eq!(encoded_query_table().len(), ENCODED_QUERY_TABLE_LEN);
        // First record: id 0, no capability, no audit, name padded.
        let rec = &ENCODED_QUERY_TABLE[..SYSINFO_QUERY_RECORD_LEN];
        assert_eq!(&rec[0..2], &[0, 0]);
        assert_eq!(rec[2], 0); // capability absent
        assert_eq!(&rec[3..5], &[0, 0]);
        assert_eq!(rec[5], 0); // audit off
        assert_eq!(
            &rec[6..6 + b"self_process_list".len()],
            b"self_process_list"
        );
    }

    #[test]
    fn encoded_table_records_capability_and_audit() {
        let idx = SysinfoQueryId::GLOBAL_PROCESS_LIST.as_u16() as usize;
        let base = idx * SYSINFO_QUERY_RECORD_LEN;
        let rec = &ENCODED_QUERY_TABLE[base..base + SYSINFO_QUERY_RECORD_LEN];
        assert_eq!(rec[2], 1, "capability present flag");
        let cap = u16::from_le_bytes([rec[3], rec[4]]);
        assert_eq!(cap, CapabilityId::SYSINFO_GLOBAL.as_u16());
        assert_eq!(rec[5], 1, "audit flag");
    }

    fn sample_header() -> SysinfoRequestHeader {
        SysinfoRequestHeader {
            magic: SYSINFO_REQUEST_MAGIC,
            version: SYSINFO_VERSION_CURRENT,
            flags: 0,
            query: SysinfoQueryId::GLOBAL_PROCESS_LIST,
            reserved: 0,
            payload_len: u32::try_from(ProcessListRequest::WIRE_LEN).unwrap(),
            request_id: 0xDEAD_BEEF_0000_0001,
        }
    }

    #[test]
    fn request_header_round_trips() {
        let h = sample_header();
        assert_eq!(SysinfoRequestHeader::WIRE_LEN, 24);
        let bytes = h.to_le_bytes();
        assert_eq!(SysinfoRequestHeader::from_bytes(&bytes), Ok(h));
    }

    #[test]
    fn request_header_rejection_paths() {
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
        let mut bytes = sample_header().to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );

        let mut header = sample_header();
        header.version = 99;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&header.to_le_bytes()),
            Err(Errno::AbiVersionUnsupported)
        );

        // Query id out of range: write a raw oversize id into the buffer.
        let mut bytes = sample_header().to_le_bytes();
        bytes[8] = 0xFF;
        bytes[9] = 0xFF;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );

        let mut header = sample_header();
        header.reserved = 1;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&header.to_le_bytes()),
            Err(Errno::BadMagic)
        );

        let mut header = sample_header();
        header.payload_len = SYSINFO_MAX_PAYLOAD_LEN + 1;
        assert_eq!(
            SysinfoRequestHeader::from_bytes(&header.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn process_list_request_round_trips_and_rejects_reserved() {
        let req = ProcessListRequest {
            offset: 10,
            limit: 64,
            flags: 0,
        };
        assert_eq!(ProcessListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1; // reserved flag set
        assert_eq!(ProcessListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            ProcessListRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn process_state_round_trips_and_rejects_unknown() {
        for s in [
            ProcessState::Runnable,
            ProcessState::Running,
            ProcessState::Blocked,
            ProcessState::Zombie,
            ProcessState::Stopped,
        ] {
            assert_eq!(ProcessState::from_u8(s.as_u8()), Ok(s));
        }
        assert_eq!(ProcessState::from_u8(5), Err(Errno::OutOfRange));
    }

    #[test]
    fn process_record_round_trips() {
        let rec = ProcessRecord::new(
            7,
            1,
            ProcId::from_raw([0x11; 16]),
            ProcId::from_raw([0x22; 16]),
            1000,
            1000,
            ProcessState::Running,
            2,
            SchedPriority::Low,
            1_234_567_890,
            5 * 4096,
            65_536,
            8_192,
            b"init",
        )
        .unwrap();
        assert_eq!(rec.name_bytes(), b"init");
        let decoded = ProcessRecord::from_bytes(&rec.to_le_bytes()).unwrap();
        assert_eq!(decoded, rec);
        assert_eq!(decoded.name_bytes(), b"init");
        assert_eq!(decoded.proc_id, ProcId::from_raw([0x11; 16]));
        assert_eq!(decoded.parent_proc_id, ProcId::from_raw([0x22; 16]));
        assert_eq!(decoded.priority, SchedPriority::Low);
        assert_eq!(decoded.cpu_time_ns, 1_234_567_890);
        assert_eq!(decoded.mem_bytes, 5 * 4096);
        assert_eq!(decoded.io_bytes_read, 65_536);
        assert_eq!(decoded.io_bytes_written, 8_192);
    }

    #[test]
    fn process_record_io_counters_round_trip_at_zero_and_max() {
        let zero = ProcessRecord::new(
            1,
            0,
            ProcId::KERNEL,
            ProcId::KERNEL,
            0,
            0,
            ProcessState::Runnable,
            PROCESS_CPU_NONE,
            SchedPriority::Normal,
            0,
            0,
            0,
            0,
            b"idle",
        )
        .unwrap();
        assert_eq!(
            ProcessRecord::from_bytes(&zero.to_le_bytes()).unwrap(),
            zero
        );
        assert_eq!(zero.io_bytes_read, 0);
        assert_eq!(zero.io_bytes_written, 0);

        let maxed = ProcessRecord::new(
            1,
            0,
            ProcId::KERNEL,
            ProcId::KERNEL,
            0,
            0,
            ProcessState::Runnable,
            PROCESS_CPU_NONE,
            SchedPriority::Normal,
            0,
            0,
            u64::MAX,
            u64::MAX,
            b"heavy",
        )
        .unwrap();
        let decoded = ProcessRecord::from_bytes(&maxed.to_le_bytes()).unwrap();
        assert_eq!(decoded, maxed);
        assert_eq!(decoded.io_bytes_read, u64::MAX);
        assert_eq!(decoded.io_bytes_written, u64::MAX);
    }

    #[test]
    fn process_record_rejects_overlong_name_and_bad_state() {
        let too_long = [b'x'; PROCESS_NAME_MAX + 1];
        assert_eq!(
            ProcessRecord::new(
                1,
                0,
                ProcId::KERNEL,
                ProcId::KERNEL,
                0,
                0,
                ProcessState::Runnable,
                PROCESS_CPU_NONE,
                SchedPriority::Normal,
                0,
                0,
                0,
                0,
                &too_long,
            ),
            Err(Errno::LengthOutOfRange)
        );

        let mut bytes = ProcessRecord::new(
            1,
            0,
            ProcId::KERNEL,
            ProcId::KERNEL,
            0,
            0,
            ProcessState::Runnable,
            PROCESS_CPU_NONE,
            SchedPriority::Normal,
            0,
            0,
            0,
            0,
            b"a",
        )
        .unwrap()
        .to_le_bytes();
        bytes[56] = 0xFF; // invalid state discriminant
        assert_eq!(ProcessRecord::from_bytes(&bytes), Err(Errno::OutOfRange));

        let mut bytes = ProcessRecord::new(
            1,
            0,
            ProcId::KERNEL,
            ProcId::KERNEL,
            0,
            0,
            ProcessState::Runnable,
            PROCESS_CPU_NONE,
            SchedPriority::Normal,
            0,
            0,
            0,
            0,
            b"a",
        )
        .unwrap()
        .to_le_bytes();
        bytes[58] = u8::try_from(PROCESS_NAME_MAX + 1).unwrap(); // name_len out of range
        assert_eq!(
            ProcessRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn process_record_rejects_unknown_priority() {
        let mut bytes = ProcessRecord::new(
            1,
            0,
            ProcId::KERNEL,
            ProcId::KERNEL,
            0,
            0,
            ProcessState::Runnable,
            PROCESS_CPU_NONE,
            SchedPriority::Normal,
            0,
            0,
            0,
            0,
            b"a",
        )
        .unwrap()
        .to_le_bytes();
        // The reserved zero and every unknown discriminant fail closed.
        bytes[59] = 0;
        assert_eq!(ProcessRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        bytes[59] = 4;
        assert_eq!(ProcessRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn kernel_memory_stats_round_trips_and_rejects_reserved() {
        let stats = KernelMemoryStats {
            total_bytes: 1 << 32,
            free_bytes: 1 << 30,
            kernel_heap_bytes: 4096,
            user_resident_bytes: 1 << 20,
            page_size: 4096,
            reserved: 0,
        };
        assert_eq!(KernelMemoryStats::WIRE_LEN, 40);
        assert_eq!(
            KernelMemoryStats::from_bytes(&stats.to_le_bytes()),
            Ok(stats)
        );
        let mut bytes = stats.to_le_bytes();
        bytes[36] = 1; // reserved non-zero
        assert_eq!(KernelMemoryStats::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn uptime_round_trips() {
        let up = Uptime {
            since_boot: Duration64::from_nanos(123_456_789),
            boot_time: Time64::from_secs(1_700_000_000),
        };
        assert_eq!(Uptime::WIRE_LEN, 24);
        assert_eq!(Uptime::from_bytes(&up.to_le_bytes()), Ok(up));
        assert_eq!(Uptime::from_bytes(&[0u8; 4]), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn system_identity_round_trips() {
        let machine_id = [7u8; MACHINE_ID_LEN];
        let id = SystemIdentity::new(machine_id, 1, 2, 3, b"tairix-box").unwrap();
        assert_eq!(id.hostname_bytes(), b"tairix-box");
        // Decode tolerates a buffer longer than WIRE_LEN.
        let mut bytes = [0xAAu8; SystemIdentity::WIRE_LEN + 9];
        bytes[..SystemIdentity::WIRE_LEN].copy_from_slice(&id.to_le_bytes());
        let decoded = SystemIdentity::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, id);
        assert_eq!(decoded.hostname_bytes(), b"tairix-box");
    }

    #[test]
    fn cpu_time_list_request_round_trips_and_rejects_reserved() {
        let req = CpuTimeListRequest {
            offset: 2,
            limit: 64,
            flags: 0,
        };
        assert_eq!(CpuTimeListRequest::WIRE_LEN, 8);
        assert_eq!(CpuTimeListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(CpuTimeListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            CpuTimeListRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn cpu_time_record_round_trips_and_rejects_reserved() {
        let record = CpuTimeRecord {
            cpu: 3,
            reserved: 0,
            busy_ns: 5_000_000_007,
            idle_ns: 12_345_678_901,
        };
        assert_eq!(CpuTimeRecord::WIRE_LEN, 24);
        assert_eq!(CpuTimeRecord::from_bytes(&record.to_le_bytes()), Ok(record));
        let mut bytes = record.to_le_bytes();
        bytes[4] = 1;
        assert_eq!(CpuTimeRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            CpuTimeRecord::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn mount_list_request_round_trips_and_rejects_reserved() {
        let req = MountListRequest {
            offset: 3,
            limit: 32,
            flags: 0,
        };
        assert_eq!(MountListRequest::WIRE_LEN, 8);
        assert_eq!(MountListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1; // reserved flag set
        assert_eq!(MountListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            MountListRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn mount_record_round_trips() {
        let flags = MountFlags::READ_ONLY
            .union(MountFlags::NOSUID)
            .union(MountFlags::NODEV)
            .union(MountFlags::NOEXEC);
        let usage = VolumeStats {
            block_size: 4096,
            total_blocks: 1 << 40,
            free_blocks: 1 << 39,
            avail_blocks: (1 << 39) - 32,
            files: 0,
            files_free: 0,
        };
        let volume_id = [0xA5u8; 16];
        let rec = MountRecord::new(
            b"/Storage/data",
            b"/Storage/data",
            b"arxfs",
            flags,
            MountVolumeState {
                usage,
                availability: MountAvailability::UnavailableDirty,
                medium: Some(BlkDeviceClass::SolidState),
            },
            volume_id,
        )
        .unwrap();
        assert_eq!(MountRecord::WIRE_LEN, 224);
        // The record's in-memory image is its wire image: the generated C
        // view mirrors one layout, not two.
        assert_eq!(core::mem::size_of::<MountRecord>(), MountRecord::WIRE_LEN);
        assert_eq!(core::mem::align_of::<MountRecord>(), 8);
        assert_eq!(rec.source_bytes(), b"/Storage/data");
        assert_eq!(rec.target_bytes(), b"/Storage/data");
        assert_eq!(rec.fstype_bytes(), b"arxfs");
        assert_eq!(rec.flags(), flags);
        assert_eq!(rec.usage(), usage);
        assert_eq!(rec.availability(), MountAvailability::UnavailableDirty);
        assert_eq!(rec.volume_id(), volume_id);
        assert_eq!(rec.medium(), Some(BlkDeviceClass::SolidState));
        let decoded = MountRecord::from_bytes(&rec.to_le_bytes()).unwrap();
        assert_eq!(decoded, rec);
    }

    #[test]
    fn mount_record_medium_round_trips_every_class_and_unknown() {
        // Every defined medium survives the wire; a backing-less mount encodes
        // and decodes as unknown, never a fabricated class.
        for medium in [
            None,
            Some(BlkDeviceClass::Rotational),
            Some(BlkDeviceClass::SolidState),
            Some(BlkDeviceClass::Removable),
            Some(BlkDeviceClass::Virtual),
        ] {
            let rec = MountRecord::new(
                b"src",
                b"/",
                b"arxfs",
                MountFlags::default(),
                MountVolumeState {
                    usage: VolumeStats::default(),
                    availability: MountAvailability::Available,
                    medium,
                },
                [0u8; 16],
            )
            .unwrap();
            assert_eq!(rec.medium(), medium);
            let decoded = MountRecord::from_bytes(&rec.to_le_bytes()).unwrap();
            assert_eq!(decoded.medium(), medium);
            assert_eq!(decoded, rec);
        }
    }

    #[test]
    fn mount_record_undefined_medium_byte_decodes_to_unknown() {
        // A medium byte the ABI does not define is read as unknown rather than
        // a wrong class or a whole-record refusal — the medium is advisory and
        // fails closed to "unknown".
        let rec = MountRecord::new(
            b"src",
            b"/",
            b"arxfs",
            MountFlags::default(),
            MountVolumeState {
                usage: VolumeStats::default(),
                availability: MountAvailability::Available,
                medium: Some(BlkDeviceClass::Rotational),
            },
            [0u8; 16],
        )
        .unwrap();
        let mut bytes = rec.to_le_bytes();
        // Byte 8 is the medium; 5 names no class in sysinfo-v1.
        bytes[8] = 5;
        let decoded = MountRecord::from_bytes(&bytes).expect("undefined medium is not a refusal");
        assert_eq!(decoded.medium(), None);
        // The refused value is not relayed onward either: re-encoding says
        // unknown rather than repeating a byte the ABI does not define.
        assert_eq!(decoded.to_le_bytes()[8], 0);
    }

    #[test]
    fn mount_availability_round_trips_and_rejects_unknown() {
        for state in [
            MountAvailability::Available,
            MountAvailability::UnavailableDirty,
            MountAvailability::UnavailableLost,
            MountAvailability::RecoveryConflict,
            MountAvailability::Degraded,
            MountAvailability::Recovering,
        ] {
            assert_eq!(MountAvailability::from_u8(state.as_u8()), Ok(state));
        }
        assert_eq!(MountAvailability::from_u8(6), Err(Errno::OutOfRange));
    }

    #[test]
    fn mount_availability_reflects_reported_block_health() {
        use crate::blkio::BlkStatus;
        // A valid answer (or a recovered device answering `Ok`) reads as
        // available; a device reporting itself unhealthy reads as degraded.
        assert_eq!(
            MountAvailability::from_block_status(BlkStatus::Ok),
            Some(MountAvailability::Available)
        );
        assert_eq!(
            MountAvailability::from_block_status(BlkStatus::Degraded),
            Some(MountAvailability::Degraded)
        );
        // Every reissuable/blip class reads as recovering — the driver is
        // riding out a stall inside its grace window.
        for status in [
            BlkStatus::TransientError,
            BlkStatus::Timeout,
            BlkStatus::Reset,
        ] {
            assert_eq!(
                MountAvailability::from_block_status(status),
                Some(MountAvailability::Recovering)
            );
        }
        // A per-request medium error, and every gone/dead class (owned by the
        // surprise-removal path), carry no volume-availability overlay.
        for status in [
            BlkStatus::MediumError,
            BlkStatus::Offline,
            BlkStatus::Removed,
            BlkStatus::Fatal,
        ] {
            assert_eq!(MountAvailability::from_block_status(status), None);
        }
    }

    #[test]
    fn availability_ranks_from_available_to_gone_and_folds_to_the_worst() {
        const ORDER: [MountAvailability; 6] = [
            MountAvailability::Available,
            MountAvailability::Degraded,
            MountAvailability::Recovering,
            MountAvailability::RecoveryConflict,
            MountAvailability::UnavailableDirty,
            MountAvailability::UnavailableLost,
        ];
        // The rank is total and strictly increasing along the documented
        // order, and deliberately not the wire byte.
        for (rank, state) in ORDER.iter().enumerate() {
            assert_eq!(usize::from(state.severity()), rank);
        }
        assert_ne!(
            MountAvailability::Degraded.severity(),
            MountAvailability::Degraded.as_u8(),
            "the fold precedence and the transport encoding stay independent"
        );
        // A fold over a stack of layers takes the worst answer anywhere in it,
        // and is commutative and idempotent, so the stack may be walked in any
        // order.
        for a in ORDER {
            assert_eq!(a.worse_of(a), a);
            for b in ORDER {
                assert_eq!(a.worse_of(b), b.worse_of(a));
                let worse = a.worse_of(b);
                assert!(worse == a || worse == b);
                assert_eq!(worse.severity(), a.severity().max(b.severity()));
            }
        }
        // Only a stack that is available at every layer is available.
        assert_eq!(
            MountAvailability::Available.worse_of(MountAvailability::Available),
            MountAvailability::Available
        );
        assert_eq!(
            MountAvailability::Available.worse_of(MountAvailability::Recovering),
            MountAvailability::Recovering
        );
    }

    #[test]
    fn health_transition_is_edge_triggered_among_live_states() {
        use MountAvailability::{Available, Degraded, Recovering};
        // A device going unwell, then into recovery, then back to healthy —
        // each is exactly one audit edge; the "came back" recovery is logged.
        assert_eq!(
            MountAvailability::health_transition(Available, Degraded),
            Some(BlkHealthTransition::Degraded)
        );
        assert_eq!(
            MountAvailability::health_transition(Degraded, Recovering),
            Some(BlkHealthTransition::Recovering)
        );
        assert_eq!(
            MountAvailability::health_transition(Available, Recovering),
            Some(BlkHealthTransition::Recovering)
        );
        assert_eq!(
            MountAvailability::health_transition(Recovering, Degraded),
            Some(BlkHealthTransition::Degraded)
        );
        for from in [Degraded, Recovering] {
            assert_eq!(
                MountAvailability::health_transition(from, Available),
                Some(BlkHealthTransition::Recovered),
                "a returning disk is a recovery"
            );
        }
        // An unchanged state is not an edge: a run of identical completions
        // logs one event, not one per request.
        for state in [Available, Degraded, Recovering] {
            assert_eq!(MountAvailability::health_transition(state, state), None);
        }
    }

    #[test]
    fn health_transition_ignores_the_surprise_removal_vanish_states() {
        use MountAvailability::{
            Available, Degraded, Recovering, RecoveryConflict, UnavailableDirty, UnavailableLost,
        };
        // Every transition touching a vanish state carries no health edge:
        // the D4 surprise-removal path owns those, so a removal is never
        // double-counted and a re-insert never fabricates a recovery here.
        let vanish = [UnavailableDirty, UnavailableLost, RecoveryConflict];
        let live = [Available, Degraded, Recovering];
        for &v in &vanish {
            for &other in live.iter().chain(vanish.iter()) {
                assert_eq!(MountAvailability::health_transition(v, other), None);
                assert_eq!(MountAvailability::health_transition(other, v), None);
            }
        }
    }

    #[test]
    fn device_health_transition_maps_the_shared_vocabulary() {
        use BlkHealthState::{Degraded, Failed, Faulted, Healthy, Offline, Recovering};
        // Entering the grace window, reporting degraded, and coming back are
        // exactly the three shared events — the same the mount side emits.
        assert_eq!(
            BlkHealthTransition::for_device(Healthy, Degraded),
            Some(BlkHealthTransition::Degraded)
        );
        assert_eq!(
            BlkHealthTransition::for_device(Healthy, Recovering),
            Some(BlkHealthTransition::Recovering)
        );
        assert_eq!(
            BlkHealthTransition::for_device(Degraded, Recovering),
            Some(BlkHealthTransition::Recovering)
        );
        // A disk that came back from any unwell-but-present state is a
        // recovery, whether it merely blipped or was fully failed closed.
        for from in [Degraded, Recovering, Faulted, Offline, Failed] {
            assert_eq!(
                BlkHealthTransition::for_device(from, Healthy),
                Some(BlkHealthTransition::Recovered),
                "a returning disk is a recovery"
            );
        }
    }

    #[test]
    fn device_health_transition_is_edge_triggered() {
        use BlkHealthState::{Degraded, Failed, Faulted, Healthy, Offline, Recovering, Removed};
        // A run of identical outcomes is one event, not one per request.
        for state in [
            Healthy, Degraded, Recovering, Faulted, Offline, Removed, Failed,
        ] {
            assert_eq!(BlkHealthTransition::for_device(state, state), None);
        }
    }

    #[test]
    fn device_health_transition_excludes_fail_closed_and_removal_edges() {
        use BlkHealthState::{Degraded, Failed, Faulted, Healthy, Offline, Recovering, Removed};
        // Failing closed is the grace window elapsing, logged by the driver's
        // own distinct fail-closed event, not this Degraded/Recovering/
        // Recovered vocabulary.
        for from in [Healthy, Degraded, Recovering] {
            for to in [Faulted, Offline, Failed] {
                assert_eq!(BlkHealthTransition::for_device(from, to), None);
            }
        }
        // Surprise-removal and its verified re-insert are the D4 hotplug
        // path's events: no health edge is fabricated here in either
        // direction.
        for other in [Healthy, Degraded, Recovering, Faulted, Failed] {
            assert_eq!(BlkHealthTransition::for_device(other, Removed), None);
            assert_eq!(BlkHealthTransition::for_device(Removed, other), None);
        }
    }

    #[test]
    fn fault_domain_health_transition_maps_the_shared_vocabulary() {
        use crate::blkio::FaultDomainState::{Healthy, Offline, Recovering};
        // An owner blip opening the shared grace window is a Recovering event —
        // whether the subtree was healthy or (defensively) re-entering from a
        // previously-failed state.
        assert_eq!(
            BlkHealthTransition::for_fault_domain(Healthy, Recovering),
            Some(BlkHealthTransition::Recovering)
        );
        assert_eq!(
            BlkHealthTransition::for_fault_domain(Offline, Recovering),
            Some(BlkHealthTransition::Recovering)
        );
        // The owner demonstrably returning is the "the hub came back" recovery,
        // both from inside the window and by clearing an already-failed subtree
        // (a resume with no reboot).
        assert_eq!(
            BlkHealthTransition::for_fault_domain(Recovering, Healthy),
            Some(BlkHealthTransition::Recovered)
        );
        assert_eq!(
            BlkHealthTransition::for_fault_domain(Offline, Healthy),
            Some(BlkHealthTransition::Recovered)
        );
        // An interior node never emits Degraded: it has no degraded-but-serving
        // state of its own.
        for prev in [Healthy, Recovering, Offline] {
            for next in [Healthy, Recovering, Offline] {
                assert_ne!(
                    BlkHealthTransition::for_fault_domain(prev, next),
                    Some(BlkHealthTransition::Degraded)
                );
            }
        }
    }

    #[test]
    fn fault_domain_health_transition_is_edge_triggered() {
        use crate::blkio::FaultDomainState::{Healthy, Offline, Recovering};
        // A run of identical observations (a continuing owner reset) is one
        // event, not one per reset.
        for state in [Healthy, Recovering, Offline] {
            assert_eq!(BlkHealthTransition::for_fault_domain(state, state), None);
        }
    }

    #[test]
    fn fault_domain_health_transition_excludes_the_fail_closed_edge() {
        use crate::blkio::FaultDomainState::{Healthy, Offline, Recovering};
        // The subtree failing closed (into Offline) is the fault-domain
        // driver's own distinct fail-closed event, not this Recovering/
        // Recovered vocabulary.
        for from in [Healthy, Recovering] {
            assert_eq!(
                BlkHealthTransition::for_fault_domain(from, Offline),
                None,
                "failing a subtree closed is not a recovery-vocabulary edge"
            );
        }
    }

    /// The live state of a healthy mount whose medium is unknown, carrying
    /// `usage`: the shape most of these tests need.
    fn healthy(usage: VolumeStats) -> MountVolumeState {
        MountVolumeState {
            usage,
            availability: MountAvailability::Available,
            medium: None,
        }
    }

    #[test]
    fn mount_record_rejects_overlong_fields() {
        let long_source = [b's'; MOUNT_SOURCE_MAX + 1];
        assert_eq!(
            MountRecord::new(
                &long_source,
                b"/",
                b"arxfs",
                MountFlags::default(),
                healthy(VolumeStats::default()),
                [0u8; 16],
            ),
            Err(Errno::LengthOutOfRange)
        );
        let long_target = [b't'; MOUNT_TARGET_MAX + 1];
        assert_eq!(
            MountRecord::new(
                b"src",
                &long_target,
                b"arxfs",
                MountFlags::default(),
                healthy(VolumeStats::default()),
                [0u8; 16],
            ),
            Err(Errno::LengthOutOfRange)
        );
        let long_type = [b'x'; MOUNT_FSTYPE_MAX + 1];
        assert_eq!(
            MountRecord::new(
                b"src",
                b"/",
                &long_type,
                MountFlags::default(),
                healthy(VolumeStats::default()),
                [0u8; 16],
            ),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn mount_record_rejects_inconsistent_usage() {
        // Free exceeding total is refused at construction…
        let free_over_total = VolumeStats {
            block_size: 512,
            total_blocks: 10,
            free_blocks: 11,
            avail_blocks: 0,
            files: 0,
            files_free: 0,
        };
        assert_eq!(
            MountRecord::new(
                b"src",
                b"/",
                b"arxfs",
                MountFlags::default(),
                healthy(free_over_total),
                [0u8; 16],
            ),
            Err(Errno::OutOfRange)
        );
        // …as is available exceeding free…
        let avail_over_free = VolumeStats {
            block_size: 512,
            total_blocks: 10,
            free_blocks: 5,
            avail_blocks: 6,
            files: 0,
            files_free: 0,
        };
        assert_eq!(
            MountRecord::new(
                b"src",
                b"/",
                b"arxfs",
                MountFlags::default(),
                healthy(avail_over_free),
                [0u8; 16],
            ),
            Err(Errno::OutOfRange)
        );
        // …and a hostile wire image claiming either is refused whole.
        let ok = MountRecord::new(
            b"src",
            b"/",
            b"arxfs",
            MountFlags::default(),
            healthy(VolumeStats::default()),
            [0u8; 16],
        )
        .unwrap();
        let mut bytes = ok.to_le_bytes();
        // free_blocks = 1 while total_blocks stays 0.
        bytes[32] = 1;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        // Every byte held back after the medium byte must be zero.
        for offset in 9..16 {
            let mut bytes = ok.to_le_bytes();
            bytes[offset] = 1;
            assert_eq!(
                MountRecord::from_bytes(&bytes),
                Err(Errno::OutOfRange),
                "reserved byte {offset}"
            );
        }
        // So must the usage block's own reserved pad, which belongs to the
        // shared volume-statistics type rather than to this record.
        let mut bytes = ok.to_le_bytes();
        bytes[20] = 1;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn mount_record_rejects_corrupt_wire() {
        let rec = MountRecord::new(
            b"src",
            b"/",
            b"arxfs",
            MountFlags::default(),
            healthy(VolumeStats::default()),
            [0u8; 16],
        )
        .unwrap();
        assert_eq!(
            MountRecord::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
        // Unknown flag bit outside KNOWN_MASK.
        let mut bytes = rec.to_le_bytes();
        bytes[0] = 0xFF;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        // An availability byte naming no known state (past the last state,
        // `Recovering` = 5).
        let mut bytes = rec.to_le_bytes();
        bytes[7] = 6;
        assert_eq!(MountRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        // A length byte beyond its buffer.
        let mut bytes = rec.to_le_bytes();
        bytes[4] = u8::try_from(MOUNT_SOURCE_MAX + 1).unwrap();
        assert_eq!(
            MountRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn system_identity_rejects_overlong_hostname_and_short_buffer() {
        let too_long = [b'h'; HOSTNAME_MAX + 1];
        assert_eq!(
            SystemIdentity::new([0u8; MACHINE_ID_LEN], 0, 0, 0, &too_long),
            Err(Errno::LengthOutOfRange)
        );
        let mut bytes = SystemIdentity::new([0u8; MACHINE_ID_LEN], 0, 0, 0, b"h")
            .unwrap()
            .to_le_bytes();
        bytes[MACHINE_ID_LEN + 6] = u8::try_from(HOSTNAME_MAX + 1).unwrap();
        assert_eq!(
            SystemIdentity::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            SystemIdentity::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn memory_pressure_stats_round_trips_and_fails_closed() {
        use super::{MemoryPressureStats, PRESSURE_BAND_COUNT, PRESSURE_BAND_NAMES};
        let stats = MemoryPressureStats {
            band: 2,
            reserved: [0u8; 7],
            total_bytes: 1 << 30,
            free_bytes: 90 << 20,
            reserve_bytes: 16 << 20,
            enter_bytes: [200 << 20, 100 << 20, 64 << 20, 32 << 20],
            exit_bytes: [256 << 20, 140 << 20, 80 << 20, 50 << 20],
            band_entries: [1, 4, 3, 2, 1],
        };
        let decoded = MemoryPressureStats::from_bytes(&stats.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, stats);
        assert_eq!(PRESSURE_BAND_NAMES[usize::from(decoded.band)], "moderate");

        // Fail closed: short buffer, reserved byte set, unknown band depth.
        assert_eq!(
            MemoryPressureStats::from_bytes(&[0u8; MemoryPressureStats::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bytes = stats.to_le_bytes();
        bytes[3] = 1;
        assert_eq!(
            MemoryPressureStats::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        let mut bytes = stats.to_le_bytes();
        bytes[0] = u8::try_from(PRESSURE_BAND_COUNT).unwrap();
        assert_eq!(
            MemoryPressureStats::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn reclaim_list_request_round_trips_and_rejects_reserved() {
        use super::ReclaimListRequest;
        let req = ReclaimListRequest {
            offset: 2,
            limit: 9,
            flags: 0,
        };
        assert_eq!(ReclaimListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(ReclaimListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            ReclaimListRequest::from_bytes(&[0u8; ReclaimListRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn reclaim_class_record_round_trips_and_fails_closed() {
        use super::{ReclaimClassRecord, RECLAIM_CLASS_COUNT};
        let record = ReclaimClassRecord {
            class: 5,
            reserved: [0u8; 7],
            payload_bytes: 4096,
            metadata_bytes: 128,
            entries: 3,
            refusals: 1,
            pressure_shrinks: 5,
            teardowns: 1,
            failures: 0,
            hits: 900,
            misses: 100,
            self_reported_bytes: 1024,
        };
        let decoded = ReclaimClassRecord::from_bytes(&record.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, record);

        assert_eq!(
            ReclaimClassRecord::from_bytes(&[0u8; ReclaimClassRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bytes = record.to_le_bytes();
        bytes[5] = 1;
        assert_eq!(ReclaimClassRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        let mut bytes = record.to_le_bytes();
        bytes[0] = u8::try_from(RECLAIM_CLASS_COUNT).unwrap();
        assert_eq!(
            ReclaimClassRecord::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn reclaim_class_names_are_a_closed_bijection() {
        use super::{reclaim_class_from_name, RECLAIM_CLASS_COUNT, RECLAIM_CLASS_NAMES};
        for (index, name) in RECLAIM_CLASS_NAMES.iter().enumerate() {
            assert_eq!(
                reclaim_class_from_name(name),
                Some(u8::try_from(index).unwrap())
            );
        }
        assert_eq!(RECLAIM_CLASS_NAMES.len(), RECLAIM_CLASS_COUNT);
        // Unknown names fail closed, never guessed.
        assert_eq!(reclaim_class_from_name("page-cache"), None);
        assert_eq!(reclaim_class_from_name(""), None);
    }

    /// A fully populated row, for the cache-ledger record tests.
    fn cache_row() -> super::CacheLedgerRecord {
        use super::{CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind};
        let mut row =
            CacheLedgerRecord::new(b"fontd.glyph-raster", CacheOwnerKind::UserlandProcess, 0, 0)
                .expect("a printable in-range label is accepted");
        row.origin = CacheLedgerOrigin::SelfReported;
        row.reporter_pid = 42;
        row.payload_bytes = 4096;
        row.metadata_bytes = 256;
        row.entries = 7;
        row.refusals = 1;
        row.pressure_shrinks = 2;
        row.teardowns = 3;
        row.failures = 4;
        row.hits = 900;
        row.misses = 100;
        row
    }

    #[test]
    fn cache_ledger_record_round_trips() {
        use super::{CacheLedgerRecord, CACHE_LABEL_MAX};
        let row = cache_row();
        let decoded = CacheLedgerRecord::from_bytes(&row.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, row);
        assert_eq!(decoded.label(), "fontd.glyph-raster");
        assert_eq!(decoded.resident_bytes(), 4096 + 256);
        assert_eq!(CacheLedgerRecord::WIRE_LEN, 96 + CACHE_LABEL_MAX);
    }

    #[test]
    fn cache_ledger_record_rejects_a_label_it_cannot_render() {
        use super::{CacheLedgerRecord, CacheOwnerKind, CACHE_LABEL_MAX};
        // Empty, over-long, and control-carrying labels are all refused at
        // construction: the label is rendered verbatim by a monitor and, on
        // a reported row, crosses a process boundary.
        assert_eq!(
            CacheLedgerRecord::new(b"", CacheOwnerKind::KernelSubsystem, 0, 0),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            CacheLedgerRecord::new(
                &[b'a'; CACHE_LABEL_MAX + 1],
                CacheOwnerKind::KernelSubsystem,
                0,
                0
            ),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            CacheLedgerRecord::new(b"tidy\x1b[2Jname", CacheOwnerKind::KernelSubsystem, 0, 0),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            CacheLedgerRecord::new(b"kernel.block", CacheOwnerKind::KernelSubsystem, 0, 99),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn the_pinned_class_is_a_ledger_row_but_never_a_reclaim_class() {
        use super::{
            cache_class_name, reclaim_class_from_name, CacheLedgerRecord, CacheOwnerKind,
            CACHE_CLASS_COUNT, CACHE_CLASS_PINNED, RECLAIM_CLASS_COUNT, RECLAIM_CLASS_NAMES,
        };
        // It sits one past the reclaim classes, so a per-class reclaim view
        // paging `RECLAIM_CLASS_COUNT` rows can never reach it.
        assert_eq!(usize::from(CACHE_CLASS_PINNED), RECLAIM_CLASS_COUNT);
        assert_eq!(CACHE_CLASS_COUNT, RECLAIM_CLASS_COUNT + 1);
        assert!(
            CacheLedgerRecord::new(
                b"arxfs.writeback",
                CacheOwnerKind::FilesystemVolume,
                7,
                CACHE_CLASS_PINNED
            )
            .is_ok(),
            "a pinned pool is a legitimate ledger row"
        );
        // The name vocabulary reads through the reclaim names rather than
        // repeating them, and adds exactly one.
        for (index, name) in RECLAIM_CLASS_NAMES.iter().enumerate() {
            let id = u8::try_from(index).expect("a class id fits a byte");
            assert_eq!(cache_class_name(id), Some(*name));
        }
        assert_eq!(cache_class_name(CACHE_CLASS_PINNED), Some("pinned"));
        assert_eq!(cache_class_name(CACHE_CLASS_PINNED + 1), None);
        // And it is not a reclaim class: no selector resolves to it.
        assert_eq!(reclaim_class_from_name("pinned"), None);
    }

    #[test]
    fn cache_ledger_record_decode_fails_closed() {
        use super::{CacheLedgerRecord, CACHE_CLASS_COUNT, CACHE_CLASS_PINNED};
        let row = cache_row();
        assert_eq!(
            CacheLedgerRecord::from_bytes(&[0u8; CacheLedgerRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A reserved byte carrying anything is wire corruption.
        let mut bytes = row.to_le_bytes();
        bytes[4] = 1;
        assert_eq!(CacheLedgerRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        // A payload hidden in the label padding is refused rather than
        // silently dropped, so the decoded record is the whole record.
        let mut bytes = row.to_le_bytes();
        bytes[CacheLedgerRecord::WIRE_LEN - 1] = b'x';
        assert_eq!(CacheLedgerRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        // Unknown owner kind, unknown origin, and an unassigned class each
        // fail closed rather than being guessed.
        let mut bytes = row.to_le_bytes();
        bytes[1] = 5;
        assert_eq!(
            CacheLedgerRecord::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );
        let mut bytes = row.to_le_bytes();
        bytes[2] = 3;
        assert_eq!(
            CacheLedgerRecord::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );
        // The pinned class sits one past the reclaim classes and is a real
        // row kind, so it decodes; the id past *it* is unassigned.
        let mut bytes = row.to_le_bytes();
        bytes[3] = CACHE_CLASS_PINNED;
        assert_eq!(
            CacheLedgerRecord::from_bytes(&bytes).map(|decoded| decoded.class),
            Ok(CACHE_CLASS_PINNED)
        );
        let mut bytes = row.to_le_bytes();
        bytes[3] = u8::try_from(CACHE_CLASS_COUNT).unwrap();
        assert_eq!(
            CacheLedgerRecord::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );
        // A zero-length label would render as an anonymous row.
        let mut bytes = row.to_le_bytes();
        bytes[0] = 0;
        assert_eq!(
            CacheLedgerRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
        // A control byte inside the declared label is refused on decode too,
        // not only at construction: the bytes may arrive from another process.
        let mut bytes = row.to_le_bytes();
        bytes[96] = 0x1b;
        assert_eq!(
            CacheLedgerRecord::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn cache_ledger_list_request_round_trips_and_rejects_reserved() {
        use super::CacheLedgerListRequest;
        let req = CacheLedgerListRequest {
            offset: 4,
            limit: 32,
            flags: 0,
        };
        assert_eq!(
            CacheLedgerListRequest::from_bytes(&req.to_le_bytes()),
            Ok(req)
        );
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(
            CacheLedgerListRequest::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            CacheLedgerListRequest::from_bytes(&[0u8; CacheLedgerListRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn cache_report_request_bounds_what_one_process_may_submit() {
        use super::{CacheReportRequest, MAX_CACHE_REPORT_ENTRIES};
        let req = CacheReportRequest {
            count: 3,
            flags: 0,
            reserved: 0,
        };
        assert_eq!(CacheReportRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        // Withdrawing every row is a legal report, not a malformed one.
        let empty = CacheReportRequest::default();
        assert_eq!(
            CacheReportRequest::from_bytes(&empty.to_le_bytes()),
            Ok(empty)
        );
        // More rows than the bound admits is refused before a byte is read.
        let over = CacheReportRequest {
            count: u16::try_from(MAX_CACHE_REPORT_ENTRIES).unwrap() + 1,
            flags: 0,
            reserved: 0,
        };
        assert_eq!(
            CacheReportRequest::from_bytes(&over.to_le_bytes()),
            Err(Errno::LengthOutOfRange)
        );
        let mut bytes = req.to_le_bytes();
        bytes[2] = 1;
        assert_eq!(CacheReportRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        let mut bytes = req.to_le_bytes();
        bytes[4] = 1;
        assert_eq!(CacheReportRequest::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn folding_cache_rows_reproduces_the_class_totals() {
        use super::{
            fold_cache_ledgers, CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind,
            RECLAIM_CLASS_COUNT,
        };
        let mut kernel =
            CacheLedgerRecord::new(b"kernel.block", CacheOwnerKind::KernelSubsystem, 0, 5)
                .expect("label accepted");
        kernel.origin = CacheLedgerOrigin::Kernel;
        kernel.payload_bytes = 8192;
        kernel.metadata_bytes = 512;
        kernel.entries = 2;
        kernel.hits = 10;

        let mut reported =
            CacheLedgerRecord::new(b"wm.cursor", CacheOwnerKind::DesktopSession, 1, 0)
                .expect("label accepted");
        reported.origin = CacheLedgerOrigin::SelfReported;
        reported.payload_bytes = 4096;
        reported.metadata_bytes = 256;
        reported.entries = 3;
        reported.misses = 4;

        let totals = fold_cache_ledgers(&[kernel, reported]);
        assert_eq!(totals.len(), RECLAIM_CLASS_COUNT);
        // Every class is present, in class order, even with no cache in it.
        for (index, total) in totals.iter().enumerate() {
            assert_eq!(usize::from(total.class), index);
        }
        // A kernel-measured class total carries no self-reported share.
        assert_eq!(totals[5].payload_bytes, 8192);
        assert_eq!(totals[5].metadata_bytes, 512);
        assert_eq!(totals[5].entries, 2);
        assert_eq!(totals[5].hits, 10);
        assert_eq!(totals[5].self_reported_bytes, 0);
        // A self-reported row lands in the class total *and* is separately
        // attributed, so a reader can see what it is taking on trust.
        assert_eq!(totals[0].payload_bytes, 4096);
        assert_eq!(totals[0].metadata_bytes, 256);
        assert_eq!(totals[0].misses, 4);
        assert_eq!(totals[0].self_reported_bytes, 4096 + 256);
        // An untouched class is a truthful row of zeros, never absent.
        assert_eq!(
            totals[3],
            super::ReclaimClassRecord {
                class: 3,
                ..super::ReclaimClassRecord::default()
            }
        );
    }

    #[test]
    fn folding_saturates_rather_than_wrapping() {
        use super::{fold_cache_ledgers, CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind};
        let mut row = CacheLedgerRecord::new(b"huge", CacheOwnerKind::KernelSubsystem, 0, 1)
            .expect("label accepted");
        row.origin = CacheLedgerOrigin::SelfReported;
        row.payload_bytes = u64::MAX;
        row.metadata_bytes = u64::MAX;
        let totals = fold_cache_ledgers(&[row, row]);
        assert_eq!(totals[1].payload_bytes, u64::MAX);
        assert_eq!(totals[1].self_reported_bytes, u64::MAX);
    }

    #[test]
    fn ramzip_stats_round_trips_and_fails_closed() {
        use super::RamzipStats;
        let stats = RamzipStats {
            entries: 7,
            logical_bytes: 7 * 4096,
            compressed_bytes: 9000,
            stored_bytes: 9500,
            metadata_bytes: 700,
            min_cap_bytes: 8 << 20,
            soft_cap_bytes: 64 << 20,
            hard_cap_bytes: 128 << 20,
            attempts: 30,
            accepted: 7,
            rejected_policy: 3,
            rejected_ineligible: 4,
            rejected_incompressible: 5,
            rejected_cap: 2,
            rejected_reserve: 1,
            rejected_task_share: 6,
            rejected_thrash: 2,
            fault_ins: 4,
            auth_failures: 0,
            decode_failures: 0,
            warm_attempts: 3,
            warm_restored: 2,
            warm_stopped: 1,
            cluster_restored: 2,
            thrash_detected: 1,
            pinned_bytes: 3 << 20,
        };
        let decoded = RamzipStats::from_bytes(&stats.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, stats);
        // The pinned aggregate is a live figure carried at the former
        // reserved slot; it round-trips like every other counter.
        assert_eq!(decoded.pinned_bytes, 3 << 20);

        // An idle tier is a valid, truthful all-zero record.
        let idle = RamzipStats::default();
        assert_eq!(RamzipStats::from_bytes(&idle.to_le_bytes()), Ok(idle));

        assert_eq!(
            RamzipStats::from_bytes(&[0u8; RamzipStats::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn net_interface_rates_request_round_trips_and_rejects_reserved() {
        use super::NetInterfaceRatesRequest;
        let req = NetInterfaceRatesRequest {
            offset: 3,
            limit: 8,
            flags: 0,
            window: Duration64::from_secs(1),
        };
        assert_eq!(
            NetInterfaceRatesRequest::from_bytes(&req.to_le_bytes()),
            Ok(req)
        );
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(
            NetInterfaceRatesRequest::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            NetInterfaceRatesRequest::from_bytes(&[0u8; NetInterfaceRatesRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn cpu_load_request_round_trips_and_rejects_reserved() {
        use super::CpuLoadRequest;
        let req = CpuLoadRequest {
            offset: 1,
            limit: 8,
            flags: 0,
        };
        assert_eq!(CpuLoadRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(CpuLoadRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            CpuLoadRequest::from_bytes(&[0u8; CpuLoadRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn cpu_load_record_round_trips_and_rejects_reserved() {
        use super::CpuLoadRecord;
        let record = CpuLoadRecord {
            cpu: 3,
            reserved: 0,
            queue_depth: 5,
            switches: 12_345,
            preemptions: 678,
        };
        let decoded = CpuLoadRecord::from_bytes(&record.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, record);

        assert_eq!(
            CpuLoadRecord::from_bytes(&[0u8; CpuLoadRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bytes = record.to_le_bytes();
        bytes[4] = 1;
        assert_eq!(CpuLoadRecord::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn cpu_info_list_request_round_trips_and_rejects_reserved() {
        let req = CpuInfoListRequest {
            offset: 1,
            limit: 8,
            flags: 0,
        };
        assert_eq!(CpuInfoListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(CpuInfoListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            CpuInfoListRequest::from_bytes(&[0u8; CpuInfoListRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn cpu_core_class_round_trips_and_fails_closed() {
        for c in [CpuCoreClass::Performance, CpuCoreClass::Efficiency] {
            assert_eq!(CpuCoreClass::from_u8(c.as_u8()), Ok(c));
        }
        assert_eq!(CpuCoreClass::from_u8(2), Err(Errno::OutOfRange));
        assert_eq!(CpuCoreClass::from_u8(255), Err(Errno::OutOfRange));
    }

    #[test]
    fn cpu_info_record_round_trips_and_fails_closed() {
        let record = CpuInfoRecord::new(
            2,
            CpuCoreClass::Efficiency,
            CPU_INFO_FLAG_FREQ_MEASURED,
            0x0000_0000_00A5_00A5,
            0x410F_D083,
            1_512_000_000,
            54_000_000,
            b"ARM Cortex-A72",
        )
        .expect("model fits");
        let decoded = CpuInfoRecord::from_bytes(&record.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, record);
        assert_eq!(decoded.model_bytes(), b"ARM Cortex-A72");
        assert!(decoded.freq_measured());
        assert_eq!(decoded.current_freq_hz, 1_512_000_000);
        assert_eq!(decoded.reference_hz, 54_000_000);

        // An unmeasured frequency: flag clear, zero rate — the honest unknown.
        let unmeasured =
            CpuInfoRecord::new(0, CpuCoreClass::Performance, 0, 0, 0, 0, 0, b"").expect("empty ok");
        assert!(!unmeasured.freq_measured());
        assert_eq!(unmeasured.model_bytes(), b"");
        assert_eq!(
            CpuInfoRecord::from_bytes(&unmeasured.to_le_bytes()),
            Ok(unmeasured)
        );

        // A model name exactly at the cap round-trips; one over is rejected.
        let max_name = [b'x'; CPU_MODEL_NAME_MAX];
        assert!(CpuInfoRecord::new(0, CpuCoreClass::Performance, 0, 0, 0, 0, 0, &max_name).is_ok());
        let over = [b'x'; CPU_MODEL_NAME_MAX + 1];
        assert_eq!(
            CpuInfoRecord::new(0, CpuCoreClass::Performance, 0, 0, 0, 0, 0, &over),
            Err(Errno::OutOfRange)
        );

        // Fail-closed decode paths: short buffer, reserved byte, unknown flag
        // bit, unknown class, overlong model length.
        assert_eq!(
            CpuInfoRecord::from_bytes(&[0u8; CpuInfoRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bytes = record.to_le_bytes();
        bytes[7] = 1;
        assert_eq!(CpuInfoRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        let mut bytes = record.to_le_bytes();
        bytes[5] = 0x80;
        assert_eq!(CpuInfoRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        let mut bytes = record.to_le_bytes();
        bytes[4] = 9;
        assert_eq!(CpuInfoRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        let mut bytes = record.to_le_bytes();
        bytes[6] = u8::try_from(CPU_MODEL_NAME_MAX + 1).expect("fits u8");
        assert_eq!(CpuInfoRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn irq_list_request_round_trips_and_rejects_reserved() {
        let req = IrqListRequest {
            offset: 2,
            limit: 16,
            flags: 0,
        };
        assert_eq!(IrqListRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(IrqListRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            IrqListRequest::from_bytes(&[0u8; IrqListRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn irq_record_round_trips_and_reports_quarantine() {
        let record = IrqRecord {
            line: 111,
            flags: IRQ_FLAG_QUARANTINED,
            owner: 13,
            count: 1_000_000,
        };
        let decoded = IrqRecord::from_bytes(&record.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, record);
        assert!(decoded.is_quarantined());

        // A healthy, high-count line: no flag bits set.
        let healthy = IrqRecord {
            line: 27,
            flags: 0,
            owner: 5,
            count: 9_999,
        };
        assert_eq!(IrqRecord::from_bytes(&healthy.to_le_bytes()), Ok(healthy));
        assert!(!healthy.is_quarantined());

        assert_eq!(
            IrqRecord::from_bytes(&[0u8; IrqRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // An undefined flag bit fails closed on an unknown record shape.
        let mut bytes = record.to_le_bytes();
        bytes[4] = 0x02;
        assert_eq!(IrqRecord::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn volume_io_health_request_round_trips_and_rejects_reserved() {
        let req = VolumeIoHealthRequest {
            offset: 2,
            limit: 8,
            flags: 0,
        };
        assert_eq!(
            VolumeIoHealthRequest::from_bytes(&req.to_le_bytes()),
            Ok(req)
        );
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(
            VolumeIoHealthRequest::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            VolumeIoHealthRequest::from_bytes(&[0u8; VolumeIoHealthRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn volume_io_health_record_round_trips_and_preserves_health_and_counters() {
        let counters = BlkHealthCounters {
            completions: 4096,
            reissues: 12,
            ok: 4000,
            degraded: 8,
            transient: 40,
            timeouts: 3,
            resets: 30,
            medium_errors: 5,
            offline: 7,
            faults: 3,
        };
        let volume_id = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let record = VolumeIoHealthRecord::new(
            volume_id,
            0x5953_2001,
            MountAvailability::Recovering,
            counters,
        );
        let decoded = VolumeIoHealthRecord::from_bytes(&record.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, record);
        assert_eq!(decoded.volume_id(), volume_id);
        assert_eq!(decoded.dev(), 0x5953_2001);
        assert_eq!(decoded.availability(), MountAvailability::Recovering);
        assert_eq!(decoded.counters(), counters);
        // The record leads with the volume id and carries the counters block
        // as its tail.
        let bytes = record.to_le_bytes();
        assert_eq!(&bytes[0..16], &volume_id);
        assert_eq!(&bytes[32..], &counters.to_le_bytes());
        assert_eq!(VolumeIoHealthRecord::WIRE_LEN, 32 + BLK_HEALTH_COUNTERS_LEN);
    }

    #[test]
    fn volume_io_health_record_fails_closed_on_a_corrupt_wire() {
        let record = VolumeIoHealthRecord::new(
            [0u8; 16],
            7,
            MountAvailability::Available,
            BlkHealthCounters::default(),
        );
        // Short buffer.
        assert_eq!(
            VolumeIoHealthRecord::from_bytes(&[0u8; VolumeIoHealthRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // An unknown availability discriminant is rejected, not guessed.
        let mut bytes = record.to_le_bytes();
        bytes[24] = 0xFF;
        assert_eq!(
            VolumeIoHealthRecord::from_bytes(&bytes),
            Err(Errno::OutOfRange)
        );
        // A non-zero reserved padding byte fails closed on an unknown shape.
        let mut bytes = record.to_le_bytes();
        bytes[28] = 1;
        assert_eq!(
            VolumeIoHealthRecord::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn crash_fault_class_and_bucket_round_trip_and_fail_closed() {
        for c in [
            CrashFaultClass::Stack,
            CrashFaultClass::StackLimit,
            CrashFaultClass::FileRegion,
            CrashFaultClass::Anon,
            CrashFaultClass::Wild,
        ] {
            assert_eq!(CrashFaultClass::from_u8(c.as_u8()), Ok(c));
        }
        assert_eq!(CrashFaultClass::from_u8(5), Err(Errno::OutOfRange));
        for b in [
            CrashFaultBucket::NullPage,
            CrashFaultBucket::BelowStackGuard,
            CrashFaultBucket::PastRegion,
            CrashFaultBucket::Wild,
            CrashFaultBucket::InRegion,
        ] {
            assert_eq!(CrashFaultBucket::from_u8(b.as_u8()), Ok(b));
        }
        assert_eq!(CrashFaultBucket::from_u8(5), Err(Errno::OutOfRange));
    }

    #[test]
    fn crash_named_reg_round_trips_and_recovers_name_length() {
        let reg = CrashNamedReg::new(b"x29", 0xDEAD_BEEF).expect("fits");
        let decoded = CrashNamedReg::from_bytes(&reg.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, reg);
        assert_eq!(decoded.name_bytes(), b"x29");
        assert_eq!(decoded.value, 0xDEAD_BEEF);
        // A full-width eight-byte name has no NUL pad; the length is the
        // whole field.
        let full = CrashNamedReg::new(b"abcdefgh", 1).expect("fits");
        assert_eq!(full.name_bytes(), b"abcdefgh");
        assert_eq!(
            CrashNamedReg::from_bytes(&full.to_le_bytes())
                .unwrap()
                .name_bytes(),
            b"abcdefgh"
        );
        // Over-long names are refused, never truncated.
        assert_eq!(
            CrashNamedReg::new(&[b'r'; CRASH_REG_NAME_LEN + 1], 0),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            CrashNamedReg::from_bytes(&[0u8; CrashNamedReg::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn crash_record_request_round_trips_and_rejects_reserved() {
        let req = CrashRecordRequest {
            offset: 3,
            limit: 4,
            flags: 0,
        };
        assert_eq!(CrashRecordRequest::from_bytes(&req.to_le_bytes()), Ok(req));
        let mut bytes = req.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(CrashRecordRequest::from_bytes(&bytes), Err(Errno::BadMagic));
        assert_eq!(
            CrashRecordRequest::from_bytes(&[0u8; CrashRecordRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn crash_record_round_trips_with_frames_and_registers() {
        let mut rec = CrashRecord::new(
            ProcId::from_raw([
                0xF0, 0xDE, 0xBC, 0x9A, 0x78, 0x56, 0x34, 0x12, 0, 0, 0, 0, 0, 0, 0, 0,
            ]),
            42,
            1000,
            1000,
            true,
            CrashFaultClass::Wild,
            CrashFaultBucket::NullPage,
            0x18,
            b"crasher",
        )
        .expect("fits");
        rec.set_registers(0x40, 0x7FFF_0000, 0x7FFF_0100, true, true);
        assert!(rec.push_frame(0x40));
        assert!(rec.push_frame(0x120));
        assert!(rec.push_reg(CrashNamedReg::new(b"x0", 0xAA).unwrap()));
        assert!(rec.push_reg(CrashNamedReg::new(b"x1", 0xBB).unwrap()));

        let decoded = CrashRecord::from_bytes(&rec.to_le_bytes()).expect("round trip");
        assert_eq!(decoded, rec);
        assert_eq!(decoded.name_bytes(), b"crasher");
        assert_eq!(decoded.pid, 42);
        assert!(decoded.is_write());
        assert!(decoded.load_base_known());
        assert!(decoded.fp_valid());
        assert_eq!(decoded.fault_class, CrashFaultClass::Wild);
        assert_eq!(decoded.fault_bucket, CrashFaultBucket::NullPage);
        assert_eq!(decoded.fault_offset, 0x18);
        assert_eq!(decoded.pc, 0x40);
        assert_eq!(decoded.frames(), &[0x40, 0x120]);
        assert_eq!(decoded.regs().len(), 2);
        assert_eq!(decoded.regs()[1].name_bytes(), b"x1");
        assert_eq!(decoded.regs()[1].value, 0xBB);
    }

    #[test]
    fn crash_record_fills_are_bounded_and_flags_default_clear() {
        let mut rec = CrashRecord::new(
            ProcId::KERNEL,
            1,
            0,
            0,
            false,
            CrashFaultClass::Anon,
            CrashFaultBucket::Wild,
            0,
            b"",
        )
        .expect("empty name fits");
        // A read fault leaves the write flag clear, and no registers were
        // recorded, so load-base/fp flags stay clear too.
        assert!(!rec.is_write());
        assert!(!rec.load_base_known());
        assert!(!rec.fp_valid());
        // Fill both arrays to capacity; the next push is dropped, never a
        // panic or an overflow.
        for i in 0..CRASH_MAX_FRAMES as u64 {
            assert!(rec.push_frame(i));
        }
        assert!(!rec.push_frame(999));
        assert_eq!(rec.frames().len(), CRASH_MAX_FRAMES);
        for _ in 0..CRASH_MAX_REGS {
            assert!(rec.push_reg(CrashNamedReg::new(b"r", 0).unwrap()));
        }
        assert!(!rec.push_reg(CrashNamedReg::new(b"r", 0).unwrap()));
        assert_eq!(rec.regs().len(), CRASH_MAX_REGS);
        // Still round-trips at capacity.
        assert_eq!(CrashRecord::from_bytes(&rec.to_le_bytes()), Ok(rec));
    }

    #[test]
    fn crash_record_fails_closed_on_corrupt_wire() {
        let rec = CrashRecord::new(
            ProcId::from_raw([7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            9,
            0,
            0,
            false,
            CrashFaultClass::Wild,
            CrashFaultBucket::Wild,
            0,
            b"x",
        )
        .expect("fits");
        assert_eq!(
            CrashRecord::from_bytes(&[0u8; CrashRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Undefined flag bit.
        let mut bytes = rec.to_le_bytes();
        bytes[32] = 0xF0;
        assert_eq!(CrashRecord::from_bytes(&bytes), Err(Errno::BadMagic));
        // Unknown fault class / bucket.
        let mut bytes = rec.to_le_bytes();
        bytes[33] = 9;
        assert_eq!(CrashRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        let mut bytes = rec.to_le_bytes();
        bytes[34] = 9;
        assert_eq!(CrashRecord::from_bytes(&bytes), Err(Errno::OutOfRange));
        // A frame count beyond the array bound is refused.
        let mut bytes = rec.to_le_bytes();
        super::put_u16(&mut bytes, 36, u16::try_from(CRASH_MAX_FRAMES + 1).unwrap());
        assert_eq!(
            CrashRecord::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
    }

    /// A sound epoch: a screenful of frames, each damaging part of it.
    fn frame_totals() -> DesktopFrameTotals {
        DesktopFrameTotals {
            screen_px: 1024 * 768,
            frames: 4,
            damaged_px: 40_000,
            blended_px: 120_000,
            opaque_px: 12_000,
            blur_px: 9_000,
            encoded_px: 40_000,
            dirty_rects: 9,
            present_calls: 4,
            chrome_hits: 31,
            chrome_misses: 2,
            peak_damaged_px: 20_000,
            peak_blended_px: 60_000,
        }
    }

    #[test]
    fn desktop_frame_totals_round_trip() {
        let totals = frame_totals();
        assert_eq!(DesktopFrameTotals::WIRE_LEN, 104);
        assert_eq!(
            DesktopFrameTotals::from_bytes(&totals.to_le_bytes()),
            Ok(totals)
        );
        // An epoch that has counted nothing is a legal report: it is what a
        // session that has composed no frame yet would say.
        assert_eq!(
            DesktopFrameTotals::from_bytes(&DesktopFrameTotals::ZERO.to_le_bytes()),
            Ok(DesktopFrameTotals::ZERO)
        );
        assert_eq!(
            DesktopFrameTotals::from_bytes(&totals.to_le_bytes()[..103]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn desktop_frame_totals_fail_closed_on_impossible_counts() {
        // Each mutation is a count no composite pass could have produced,
        // and each is refused where the untrusted submission is decoded.
        let reject = |mutate: fn(&mut DesktopFrameTotals)| {
            let mut totals = frame_totals();
            mutate(&mut totals);
            assert_eq!(
                DesktopFrameTotals::from_bytes(&totals.to_le_bytes()),
                Err(Errno::OutOfRange),
                "{totals:?}"
            );
        };

        // Work with no frame to do it in.
        reject(|t| t.frames = 0);
        // Damage without a rectangle, and a rectangle without damage.
        reject(|t| t.dirty_rects = 0);
        reject(|t| t.damaged_px = 0);
        // Copies and encodes resolve damaged pixels, never others.
        reject(|t| t.opaque_px = t.damaged_px + 1);
        reject(|t| t.encoded_px = t.damaged_px + 1);
        // More damage than the counted frames could clip to the screen.
        reject(|t| t.damaged_px = t.screen_px * t.frames + 1);
        // More driver calls than one per rectangle plus one per frame.
        reject(|t| t.present_calls = t.dirty_rects + t.frames + 1);
        // A maximum above its own sum, above one screen, or zero beside a
        // non-zero sum.
        reject(|t| t.peak_damaged_px = t.damaged_px + 1);
        reject(|t| {
            t.screen_px = 8;
            t.damaged_px = 32;
            t.peak_damaged_px = 9;
        });
        reject(|t| t.peak_damaged_px = 0);
        reject(|t| t.peak_blended_px = t.blended_px + 1);
        reject(|t| t.peak_blended_px = 0);
    }

    #[test]
    fn desktop_frame_totals_admit_the_unbounded_counters() {
        // Blends count layer contributions, a recomputed frost is blurred
        // over the window rectangle that caused it, and a furniture lookup
        // is not a pixel: none of the three is bounded by the damage.
        let mut totals = frame_totals();
        totals.blended_px = totals.screen_px * totals.frames * 13;
        totals.peak_blended_px = totals.blended_px;
        totals.blur_px = totals.screen_px * 3;
        totals.chrome_hits = u64::MAX;
        assert_eq!(
            DesktopFrameTotals::from_bytes(&totals.to_le_bytes()),
            Ok(totals)
        );
    }

    #[test]
    fn desktop_frame_record_round_trips_and_demands_a_publisher() {
        let record = DesktopFrameRecord {
            reporter_pid: 42,
            totals: frame_totals(),
        };
        assert_eq!(DesktopFrameRecord::WIRE_LEN, 112);
        assert_eq!(
            DesktopFrameRecord::from_bytes(&record.to_le_bytes()),
            Ok(record)
        );
        // A served row is always attributed; an anonymous one is corruption.
        let mut anonymous = record;
        anonymous.reporter_pid = 0;
        assert_eq!(
            DesktopFrameRecord::from_bytes(&anonymous.to_le_bytes()),
            Err(Errno::BadMagic)
        );
        // The body is validated through the same gate as a bare submission.
        let mut impossible = record;
        impossible.totals.opaque_px = impossible.totals.damaged_px + 1;
        assert_eq!(
            DesktopFrameRecord::from_bytes(&impossible.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            DesktopFrameRecord::from_bytes(&record.to_le_bytes()[..111]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn desktop_frame_stats_request_round_trips_and_refuses_reserved_flags() {
        let request = DesktopFrameStatsRequest {
            offset: 3,
            limit: 8,
            flags: 0,
        };
        assert_eq!(DesktopFrameStatsRequest::WIRE_LEN, 8);
        assert_eq!(
            DesktopFrameStatsRequest::from_bytes(&request.to_le_bytes()),
            Ok(request)
        );
        let mut bytes = request.to_le_bytes();
        bytes[6] = 1;
        assert_eq!(
            DesktopFrameStatsRequest::from_bytes(&bytes),
            Err(Errno::BadMagic)
        );
        assert_eq!(
            DesktopFrameStatsRequest::from_bytes(&bytes[..7]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn desktop_frame_query_gates_are_the_submission_read_pair() {
        // The submission describes only its own caller, so it is ungated and
        // unaudited exactly as the cache report is; the read names another
        // principal's work, so it carries the cross-principal gate and is
        // audited.
        let report = spec_for(SysinfoQueryId::DESKTOP_FRAME_REPORT).unwrap();
        assert_eq!(report.required_capability, None);
        assert!(!report.audit);
        let read = spec_for(SysinfoQueryId::DESKTOP_FRAME_STATS).unwrap();
        assert_eq!(read.required_capability, Some(CapabilityId::SYSINFO_GLOBAL));
        assert!(read.audit);
    }
}
