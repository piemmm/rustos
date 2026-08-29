//! The shared kernel-statistics fetches (`plans/STRESSTEST.md` ST1/ST4).
//!
//! The kernel-wide observability queries — the memory-pressure gauge, the
//! reclaim ledger and its per-cache breakdown, the `ramzip` tier counters,
//! the per-CPU scheduler load, the bound-interrupt table, and the network
//! stack's interface and bond tables — are consumed by both the
//! `info:`/`stats:` resolver ([`mod@crate::resolve`]) and its terminal
//! clients (the `sysmon` monitor, the shell's resource-selector
//! enumeration), so the fetch + fail-closed decode lives here once. The
//! paged walks are the generic [`walk_pages`](crate::list) the process and
//! mount lists use; the scalar queries share its convention that a
//! structurally invalid reply is [`Errno::BadMagic`], never a partial
//! decode.
//!
//! A walk here is the *only* paging of its query in this crate: the
//! resolver's own per-name lookups are built on these walks (stopping at the
//! record they want) rather than re-deriving the loop, so a consumer that
//! needs the whole table — completion listing every interface — adds no
//! second walk of the same query.
//!
//! Most queries here are gated on `CAP_SYSINFO_KERNEL` by `sysinfod`; a
//! denial surfaces as [`CallError::PermissionDenied`] so a consumer can
//! render the refusal and continue (the queries are observability, never
//! load-bearing for a session). The exceptions are the two coarse
//! self-regulation reads every process may make —
//! [`memory_pressure_band`] and [`memory_total_bytes`] — which each
//! document why they need no capability.

use tairix_abi::net_ipc::{NetBondMemberRecord, NetInterfaceFactsRecord, NetStackDefenceCounters};
use tairix_abi::sysinfo::{
    CacheLedgerListRequest, CacheLedgerRecord, CpuLoadRecord, CpuLoadRequest, DesktopFrameRecord,
    DesktopFrameStatsRequest, IrqListRequest, IrqRecord, MemoryPressureBand, MemoryPressureStats,
    MemoryTotal, NetInterfaceListRequest, RamzipStats, ReclaimClassRecord, ReclaimListRequest,
    SysinfoQueryId, RECLAIM_CLASS_COUNT,
};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError, WalkStep};
use crate::request::{call, CallError};
use crate::transport::Transport;

/// Number of [`CpuLoadRecord`]s requested per CPU-load page.
///
/// A page bounds the reply size without bounding how many CPUs the machine
/// may have; [`for_each_cpu_load`] walks pages until a short page ends the
/// list.
pub const CPU_LOAD_PAGE: u16 = 64;

/// Number of [`ReclaimClassRecord`]s requested per reclaim page.
///
/// The class set is small and closed, so one page always carries the whole
/// ledger today; the walk still pages so a future wider class set needs no
/// client change.
pub const RECLAIM_PAGE: u16 = {
    assert!(RECLAIM_CLASS_COUNT <= u16::MAX as usize);
    #[allow(clippy::cast_possible_truncation)] // Guarded by the assert above.
    {
        RECLAIM_CLASS_COUNT as u16
    }
};

/// Number of [`CacheLedgerRecord`]s requested per cache-ledger page.
///
/// Unlike the closed reclaim-class set, the number of registered caches is
/// open-ended (every reclaimable cache in the system, kernel and
/// self-reported alike), so [`for_each_cache_ledger`] genuinely walks
/// multiple pages on a busy desktop.
pub const CACHE_LEDGER_PAGE: u16 = 64;

/// Query the live memory-pressure snapshot
/// ([`SysinfoQueryId::MEMORY_PRESSURE`]).
///
/// # Errors
///
/// * [`CallError::PermissionDenied`] — the caller lacks
///   `CAP_SYSINFO_KERNEL`.
/// * [`CallError::Service`] — the transport failed or the reply did not
///   decode against `sysinfo-v1` (reported as [`Errno::BadMagic`], the
///   structurally-invalid-reply convention of the paged walks).
pub fn memory_pressure(transport: &dyn Transport) -> Result<MemoryPressureStats, CallError> {
    let reply = call(transport, SysinfoQueryId::MEMORY_PRESSURE, &[])?;
    MemoryPressureStats::from_bytes(&reply).map_err(|_| CallError::Service(Errno::BadMagic))
}

/// Query the published memory-pressure band alone
/// ([`SysinfoQueryId::MEMORY_PRESSURE_BAND`]).
///
/// Ungated: this is how a process learns it must give its own caches
/// back. It is the drain for the edge-triggered
/// [`WaitSourceKind::MemoryPressure`](tairix_abi::WaitSourceKind::MemoryPressure)
/// wait source — park until the band moves, then read it here — never a
/// polling surface.
///
/// # Errors
///
/// * [`CallError::Service`] — the transport failed, or the reply did not
///   decode against `sysinfo-v1` (reported as [`Errno::BadMagic`]). A
///   band the caller does not recognise is a decode failure, not a
///   guess: the caller keeps the band it already had rather than
///   assuming the machine is comfortable.
pub fn memory_pressure_band(transport: &dyn Transport) -> Result<MemoryPressureBand, CallError> {
    let reply = call(transport, SysinfoQueryId::MEMORY_PRESSURE_BAND, &[])?;
    MemoryPressureBand::from_bytes(&reply).map_err(|_| CallError::Service(Errno::BadMagic))
}

/// Query the machine's total usable physical RAM in bytes
/// ([`SysinfoQueryId::MEMORY_TOTAL`]).
///
/// Ungated: installed RAM is a static hardware fact — the figure on the
/// machine's spec sheet — so it discloses strictly less than the
/// already-ungated load average. This is how a process sizes a cache
/// budget against the real machine instead of a hand-picked constant;
/// the detailed [`memory_pressure`] view stays behind
/// `CAP_SYSINFO_KERNEL`.
///
/// **Zero means unknown and admits nothing.** An unprovisioned machine
/// (or a kernel that cannot report the census) answers zero, and a
/// budget derived from it must come out as "size nothing" — never as
/// "unbounded". Scale the budget from this figure so that zero bytes of
/// RAM yields zero bytes of cache; a caller that needs a floor states
/// that floor itself rather than reading one into a zero answer.
///
/// # Errors
///
/// * [`CallError::Service`] — the transport failed, or the reply did not
///   decode against `sysinfo-v1` (reported as [`Errno::BadMagic`]). A
///   malformed reply is refused rather than read as a size, so a caller
///   never budgets against a figure the service did not send.
pub fn memory_total_bytes(transport: &dyn Transport) -> Result<u64, CallError> {
    let reply = call(transport, SysinfoQueryId::MEMORY_TOTAL, &[])?;
    MemoryTotal::from_bytes(&reply)
        .map(|total| total.total_bytes)
        .map_err(|_| CallError::Service(Errno::BadMagic))
}

/// Query the `ramzip` compressed-tier counters
/// ([`SysinfoQueryId::RAMZIP_STATS`]).
///
/// Counters only — never page contents, never key material. A build whose
/// tier is not yet driven truthfully answers an idle tier (all zeros).
///
/// # Errors
///
/// As [`memory_pressure`].
pub fn ramzip_stats(transport: &dyn Transport) -> Result<RamzipStats, CallError> {
    let reply = call(transport, SysinfoQueryId::RAMZIP_STATS, &[])?;
    RamzipStats::from_bytes(&reply).map_err(|_| CallError::Service(Errno::BadMagic))
}

/// Query the network stack's stack-wide TCP connection-defence counters
/// ([`SysinfoQueryId::NET_STACK_DEFENCE`]).
///
/// One record, not a page: the counters belong to the stack's socket table
/// as a whole and name no interface. Each is monotonic over the boot, so a
/// flood stays visible after the listening socket it targeted has closed.
///
/// # Errors
///
/// As [`memory_pressure`].
pub fn net_stack_defence(transport: &dyn Transport) -> Result<NetStackDefenceCounters, CallError> {
    let reply = call(transport, SysinfoQueryId::NET_STACK_DEFENCE, &[])?;
    NetStackDefenceCounters::from_bytes(&reply).map_err(|_| CallError::Service(Errno::BadMagic))
}

/// Page through the reclaim ledger ([`SysinfoQueryId::RECLAIM_STATS`]) and
/// hand each decoded [`ReclaimClassRecord`] to `sink`, in class order.
///
/// `sink` answers [`WalkStep::Continue`] to be given the next record or
/// [`WalkStep::Stop`] to end the walk there, which is how a caller bounds
/// how much of a long or hostile list it will accept. Stopping is an
/// ordinary success, so it stays distinguishable from a failure.
///
/// The walk **fails closed**: a reply that is not a whole number of
/// records, or a record that does not decode, is rejected rather than
/// partially delivered.
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error for some record; the
///   walk stops at that record.
pub fn for_each_reclaim_class(
    transport: &dyn Transport,
    mut sink: impl FnMut(&ReclaimClassRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::RECLAIM_STATS,
        ReclaimClassRecord::WIRE_LEN,
        RECLAIM_PAGE,
        |offset, limit| {
            ReclaimListRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = ReclaimClassRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Page through the per-cache ledger breakdown behind the reclaim classes
/// ([`SysinfoQueryId::CACHE_LEDGERS`]) and hand each decoded
/// [`CacheLedgerRecord`] to `sink`, kernel rows first then reported rows, in
/// the service's stable order.
///
/// Summing the rows this walk yields for one class reproduces that class's
/// [`ReclaimClassRecord`] exactly (`fold_cache_ledgers`); this is the
/// per-cache detail behind that per-class total, so a caller can see which
/// specific cache holds a class's bytes.
///
/// # Errors
///
/// As [`for_each_reclaim_class`].
pub fn for_each_cache_ledger(
    transport: &dyn Transport,
    mut sink: impl FnMut(&CacheLedgerRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::CACHE_LEDGERS,
        CacheLedgerRecord::WIRE_LEN,
        CACHE_LEDGER_PAGE,
        |offset, limit| {
            CacheLedgerListRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = CacheLedgerRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Page through the per-CPU scheduler load figures
/// ([`SysinfoQueryId::CPU_LOAD`]) and hand each decoded [`CpuLoadRecord`]
/// to `sink`, in ascending CPU order.
///
/// The cumulative busy/idle split lives in the ungated `CPU_TIME_STATS`
/// walk ([`crate::for_each_cpu_time`]); this record carries only the
/// remainder (queue depth, switches, preemptions), so the same figure is
/// never served twice.
///
/// # Errors
///
/// As [`for_each_reclaim_class`].
pub fn for_each_cpu_load(
    transport: &dyn Transport,
    mut sink: impl FnMut(&CpuLoadRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::CPU_LOAD,
        CpuLoadRecord::WIRE_LEN,
        CPU_LOAD_PAGE,
        |offset, limit| {
            CpuLoadRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = CpuLoadRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Number of [`IrqRecord`]s requested per IRQ-table page.
///
/// A page bounds the reply size without bounding how many interrupt lines
/// the machine may bind; [`for_each_irq`] walks pages until a short page
/// ends the list.
pub const IRQ_PAGE: u16 = 64;

/// Records per page the network interface-table walks request
/// ([`for_each_net_interface`], [`for_each_net_bond_member`], and the
/// resolver's sibling per-interface lookups).
///
/// Small enough that one page of the widest interface record fits a single
/// framed reply, large enough that one page covers every realistic interface
/// table — so the common case is one round trip and a larger machine simply
/// pages.
pub const NET_INTERFACE_PAGE: u16 = 16;

/// Page through the kernel IRQ table ([`SysinfoQueryId::IRQ_LIST`]) and hand
/// each decoded [`IrqRecord`] to `sink`, in ascending line order — one
/// record per bound line (line id, owning driver task, monotonic fire count
/// since boot, quarantine flag).
///
/// Gated on `CAP_SYSINFO_HW` by `sysinfod` (the table names which task owns
/// each interrupt line — cross-principal surface topology, like the seat
/// inventory), so a denial surfaces as
/// [`CallError::PermissionDenied`]. The
/// walk **fails closed**: a reply that is not a whole number of records, or
/// a record that does not decode, is rejected rather than partially
/// delivered.
///
/// # Errors
///
/// As [`for_each_cpu_load`].
pub fn for_each_irq(
    transport: &dyn Transport,
    mut sink: impl FnMut(&IrqRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::IRQ_LIST,
        IrqRecord::WIRE_LEN,
        IRQ_PAGE,
        |offset, limit| {
            IrqListRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = IrqRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// [`DesktopFrameRecord`]s requested per desktop frame-accounting page.
///
/// One publisher per compositing session, so a machine has a handful at
/// most and one page is the whole list; [`for_each_desktop_frame_report`]
/// pages regardless, because the count is a function of how many seats and
/// switched-away sessions exist rather than a fixed number.
pub const DESKTOP_FRAME_PAGE: u16 = 16;

/// Page through the desktop frame accounting every live session has
/// published ([`SysinfoQueryId::DESKTOP_FRAME_STATS`]) and hand each decoded
/// [`DesktopFrameRecord`] to `sink`, in publisher order.
///
/// The figures are **self-reported**: only the process that owns a
/// compositor can count pixels, so it submits them and `sysinfod` retains
/// them against its kernel-attested identity. They are diagnostics — no
/// kernel decision reads them — and a reader renders them as the desktop's
/// own statement about itself.
///
/// Gated on `CAP_SYSINFO_GLOBAL` by `sysinfod` (a record names another
/// principal and its work), so a denial surfaces as
/// [`CallError::PermissionDenied`]. The walk **fails closed**: a reply that
/// is not a whole number of records, or a record that does not decode, is
/// rejected rather than partially delivered.
///
/// # Errors
///
/// As [`for_each_cpu_load`].
pub fn for_each_desktop_frame_report(
    transport: &dyn Transport,
    mut sink: impl FnMut(&DesktopFrameRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::DESKTOP_FRAME_STATS,
        DesktopFrameRecord::WIRE_LEN,
        DESKTOP_FRAME_PAGE,
        |offset, limit| {
            DesktopFrameStatsRequest {
                offset,
                limit,
                flags: 0,
            }
            .to_le_bytes()
            .to_vec()
        },
        |chunk| {
            let record = DesktopFrameRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Page through the network stack's interface table
/// ([`SysinfoQueryId::NET_INTERFACE_FACTS`]) and hand each decoded
/// [`NetInterfaceFactsRecord`] to `sink`, in the stack's own interface order.
///
/// This is the one paging of the interface table in this crate. The
/// resolver's per-name lookup (`info:net/<iface>/…`) is this walk stopped at
/// the matching record, and a consumer that wants every *name* — the shell
/// expanding the `<iface>` placeholder in a resource reference — is the same
/// walk run to the end, so neither re-derives the loop.
///
/// `sink` answers [`WalkStep::Continue`] to be given the next record or
/// [`WalkStep::Stop`] to end the walk there; stopping is an ordinary success,
/// so a caller that found what it wanted stays distinguishable from a
/// failure. The walk **fails closed**: a reply that is not a whole number of
/// records, or a record that does not decode, is rejected rather than
/// partially delivered.
///
/// `sysinfod` gates this query on `CAP_SYSINFO_HW` (the interface inventory
/// is hardware topology), so a caller without it sees
/// [`CallError::PermissionDenied`].
///
/// # Errors
///
/// * [`ListError::Call`] — the transport failed, the service denied the
///   query, or the reply was structurally invalid.
/// * [`ListError::Sink`] — `sink` returned an error; the walk stops there.
pub fn for_each_net_interface(
    transport: &dyn Transport,
    mut sink: impl FnMut(&NetInterfaceFactsRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::NET_INTERFACE_FACTS,
        NetInterfaceFactsRecord::WIRE_LEN,
        NET_INTERFACE_PAGE,
        net_list_request,
        |chunk| {
            let record = NetInterfaceFactsRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Page through the network stack's bond-membership table
/// ([`SysinfoQueryId::NET_BOND_MEMBERS`]) and hand each decoded
/// [`NetBondMemberRecord`] to `sink`, in the stack's configured member order.
///
/// One record per *member*, each naming the bond that owns it, so a caller
/// after one bond's members filters on
/// [`NetBondMemberRecord::bond`](tairix_abi::net_ipc::NetBondMemberRecord::bond)
/// and a caller after the set of bond *names* collects that field instead.
/// Both are this one walk; see [`for_each_net_interface`] for why.
///
/// `sysinfod` gates this query on `CAP_SYSINFO_GLOBAL` (interface aliases are
/// surface topology), so a caller without it sees
/// [`CallError::PermissionDenied`].
///
/// # Errors
///
/// As [`for_each_net_interface`].
pub fn for_each_net_bond_member(
    transport: &dyn Transport,
    mut sink: impl FnMut(&NetBondMemberRecord) -> Result<WalkStep, Errno>,
) -> Result<(), ListError> {
    walk_pages(
        transport,
        SysinfoQueryId::NET_BOND_MEMBERS,
        NetBondMemberRecord::WIRE_LEN,
        NET_INTERFACE_PAGE,
        net_list_request,
        |chunk| {
            let record = NetBondMemberRecord::from_bytes(chunk)
                .map_err(|errno| ListError::Call(CallError::Service(errno)))?;
            sink(&record).map_err(ListError::Sink)
        },
    )
}

/// Encode one page request of the interface-table query family, whose
/// `offset`/`limit` envelope is shared by every `NET_*` list query.
fn net_list_request(offset: u32, limit: u16) -> alloc::vec::Vec<u8> {
    NetInterfaceListRequest {
        offset,
        limit,
        flags: 0,
    }
    .to_le_bytes()
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::{
        for_each_cache_ledger, for_each_cpu_load, for_each_desktop_frame_report, for_each_irq,
        for_each_reclaim_class, memory_pressure, memory_total_bytes, ramzip_stats, WalkStep,
        CACHE_LEDGER_PAGE, CPU_LOAD_PAGE, IRQ_PAGE, RECLAIM_PAGE,
    };
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::sysinfo::{
        CacheLedgerListRequest, CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind,
        CpuLoadRecord, CpuLoadRequest, DesktopFrameRecord, DesktopFrameStatsRequest,
        DesktopFrameTotals, IrqListRequest, IrqRecord, MemoryPressureStats, MemoryTotal,
        RamzipStats, ReclaimClassRecord, ReclaimListRequest, SysinfoQueryId, SysinfoRequestHeader,
        RECLAIM_CLASS_COUNT,
    };
    use tairix_abi::Errno;

    /// An in-memory `sysinfod` stand-in answering the kernel-stats queries
    /// from fixed data, decoding each request exactly as the real service
    /// does.
    struct Fixture {
        pressure: MemoryPressureStats,
        ramzip: RamzipStats,
        reclaim: Vec<ReclaimClassRecord>,
        caches: Vec<CacheLedgerRecord>,
        loads: Vec<CpuLoadRecord>,
        irqs: Vec<IrqRecord>,
        frames: Vec<DesktopFrameRecord>,
        deny: Option<SysinfoQueryId>,
        malformed: Option<SysinfoQueryId>,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new() -> Self {
            let mut reclaim = Vec::new();
            for class in 0..RECLAIM_CLASS_COUNT {
                reclaim.push(ReclaimClassRecord {
                    class: u8::try_from(class).unwrap_or(0),
                    payload_bytes: 1024 * (class as u64 + 1),
                    ..ReclaimClassRecord::default()
                });
            }
            Self {
                pressure: MemoryPressureStats {
                    band: 2,
                    total_bytes: 1 << 30,
                    free_bytes: 1 << 28,
                    ..MemoryPressureStats::default()
                },
                ramzip: RamzipStats {
                    stored_bytes: 4096,
                    logical_bytes: 16384,
                    pinned_bytes: 8192,
                    ..RamzipStats::default()
                },
                reclaim,
                caches: alloc::vec![
                    {
                        let mut row = CacheLedgerRecord::new(
                            b"fontd-glyphs",
                            CacheOwnerKind::KernelSubsystem,
                            0,
                            0,
                        )
                        .unwrap();
                        row.origin = CacheLedgerOrigin::Kernel;
                        row.payload_bytes = 4096;
                        row.entries = 4;
                        row.hits = 40;
                        row.misses = 4;
                        row
                    },
                    {
                        let mut row = CacheLedgerRecord::new(
                            b"taskbar-icons",
                            CacheOwnerKind::UserlandProcess,
                            0,
                            0,
                        )
                        .unwrap();
                        row.origin = CacheLedgerOrigin::SelfReported;
                        row.reporter_pid = 77;
                        row.payload_bytes = 2048;
                        row.entries = 2;
                        row.hits = 10;
                        row.misses = 1;
                        row
                    },
                ],
                irqs: alloc::vec![
                    IrqRecord {
                        line: 27,
                        flags: 0,
                        owner: 14,
                        count: 1234,
                    },
                    IrqRecord {
                        line: 111,
                        flags: 0,
                        owner: 13,
                        count: 200_000,
                    },
                ],
                loads: alloc::vec![
                    CpuLoadRecord {
                        cpu: 0,
                        reserved: 0,
                        queue_depth: 3,
                        switches: 100,
                        preemptions: 7,
                    },
                    CpuLoadRecord {
                        cpu: 1,
                        reserved: 0,
                        queue_depth: 0,
                        switches: 50,
                        preemptions: 2,
                    },
                ],
                frames: fixture_frames(),
                deny: None,
                malformed: None,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.deny == Some(header.query) {
                return Err(Errno::PermissionDenied);
            }
            if self.malformed == Some(header.query) {
                return Ok(alloc::vec![0u8; 3]);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            match header.query {
                SysinfoQueryId::MEMORY_PRESSURE => Ok(self.pressure.to_le_bytes().to_vec()),
                // The gauge's own total: the fixture models one machine, so
                // the ungated size and the gated view cannot drift apart
                // here any more than they can on a live kernel.
                SysinfoQueryId::MEMORY_TOTAL => Ok(MemoryTotal {
                    total_bytes: self.pressure.total_bytes,
                }
                .to_le_bytes()
                .to_vec()),
                SysinfoQueryId::RAMZIP_STATS => Ok(self.ramzip.to_le_bytes().to_vec()),
                SysinfoQueryId::RECLAIM_STATS => {
                    let req = ReclaimListRequest::from_bytes(payload)?;
                    Ok(page(&self.reclaim, req.offset, req.limit, |r| {
                        r.to_le_bytes().to_vec()
                    }))
                }
                SysinfoQueryId::CPU_LOAD => {
                    let req = CpuLoadRequest::from_bytes(payload)?;
                    Ok(page(&self.loads, req.offset, req.limit, |r| {
                        r.to_le_bytes().to_vec()
                    }))
                }
                SysinfoQueryId::IRQ_LIST => {
                    let req = IrqListRequest::from_bytes(payload)?;
                    Ok(page(&self.irqs, req.offset, req.limit, |r| {
                        r.to_le_bytes().to_vec()
                    }))
                }
                SysinfoQueryId::CACHE_LEDGERS => {
                    let req = CacheLedgerListRequest::from_bytes(payload)?;
                    Ok(page(&self.caches, req.offset, req.limit, |r| {
                        r.to_le_bytes().to_vec()
                    }))
                }
                SysinfoQueryId::DESKTOP_FRAME_STATS => {
                    let req = DesktopFrameStatsRequest::from_bytes(payload)?;
                    Ok(page(&self.frames, req.offset, req.limit, |r| {
                        r.to_le_bytes().to_vec()
                    }))
                }
                _ => Err(Errno::NotFound),
            }
        }
    }

    /// Two desktop sessions' published accounting: a busy one on a large
    /// screen, and a quiet one on a smaller screen.
    fn fixture_frames() -> Vec<DesktopFrameRecord> {
        alloc::vec![
            DesktopFrameRecord {
                reporter_pid: 91,
                totals: DesktopFrameTotals {
                    screen_px: 1920 * 1080,
                    frames: 40,
                    damaged_px: 800_000,
                    blended_px: 1_600_000,
                    opaque_px: 200_000,
                    blur_px: 60_000,
                    encoded_px: 800_000,
                    dirty_rects: 96,
                    present_calls: 40,
                    chrome_hits: 300,
                    chrome_misses: 6,
                    peak_damaged_px: 120_000,
                    peak_blended_px: 250_000,
                },
            },
            DesktopFrameRecord {
                reporter_pid: 92,
                totals: DesktopFrameTotals {
                    screen_px: 1024 * 768,
                    frames: 3,
                    damaged_px: 9_000,
                    blended_px: 9_000,
                    opaque_px: 0,
                    blur_px: 0,
                    encoded_px: 9_000,
                    dirty_rects: 6,
                    present_calls: 3,
                    chrome_hits: 4,
                    chrome_misses: 1,
                    peak_damaged_px: 4_000,
                    peak_blended_px: 4_000,
                },
            },
        ]
    }

    fn page<T>(records: &[T], offset: u32, limit: u16, encode: impl Fn(&T) -> Vec<u8>) -> Vec<u8> {
        let offset = offset as usize;
        if offset >= records.len() {
            return Vec::new();
        }
        let take = core::cmp::min(records.len() - offset, limit as usize);
        let mut out = Vec::new();
        for record in &records[offset..offset + take] {
            out.extend_from_slice(&encode(record));
        }
        out
    }

    #[test]
    fn desktop_frame_reports_walk_in_publisher_order_and_fail_closed() {
        let fixture = Fixture::new();
        let mut seen = Vec::new();
        for_each_desktop_frame_report(&fixture, |record| {
            seen.push((record.reporter_pid, record.totals.peak_damaged_px));
            Ok(WalkStep::Continue)
        })
        .expect("the walk is served");
        assert_eq!(seen, [(91, 120_000), (92, 4_000)]);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::DESKTOP_FRAME_STATS],
            "a page short of the limit ends the walk in one round trip"
        );

        // A denial is the caller's to render, not a panic: the query names
        // another principal's work and is gated.
        let mut denied = Fixture::new();
        denied.deny = Some(SysinfoQueryId::DESKTOP_FRAME_STATS);
        assert!(matches!(
            for_each_desktop_frame_report(&denied, |_| Ok(WalkStep::Continue)),
            Err(ListError::Call(CallError::PermissionDenied))
        ));

        // A reply that is not a whole number of records is rejected rather
        // than partially delivered.
        let mut malformed = Fixture::new();
        malformed.malformed = Some(SysinfoQueryId::DESKTOP_FRAME_STATS);
        let mut delivered = 0;
        assert!(for_each_desktop_frame_report(&malformed, |_| {
            delivered += 1;
            Ok(WalkStep::Continue)
        })
        .is_err());
        assert_eq!(delivered, 0);
    }

    #[test]
    fn pressure_and_ramzip_decode() {
        let fixture = Fixture::new();
        let pressure = memory_pressure(&fixture).expect("pressure");
        assert_eq!(pressure.band, 2);
        assert_eq!(pressure.free_bytes, 1 << 28);
        let ramzip = ramzip_stats(&fixture).expect("ramzip");
        assert_eq!(ramzip.pinned_bytes, 8192);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[
                SysinfoQueryId::MEMORY_PRESSURE,
                SysinfoQueryId::RAMZIP_STATS
            ]
        );
    }

    /// The ungated total is the machine's own RAM size, in bytes, and is
    /// the same figure the gauge reports as its total: one machine, one
    /// size.
    #[test]
    fn memory_total_decodes_to_the_machines_ram_size() {
        let fixture = Fixture::new();
        assert_eq!(memory_total_bytes(&fixture), Ok(1 << 30));
        assert_eq!(
            memory_total_bytes(&fixture),
            Ok(fixture.pressure.total_bytes)
        );
    }

    /// Zero is passed through as the honest "unknown" answer, never
    /// rewritten into a default the caller would mistake for a real
    /// machine: a budget scaled from it must come out as "size nothing".
    #[test]
    fn an_unknown_total_reads_as_zero_and_admits_nothing() {
        let mut fixture = Fixture::new();
        fixture.pressure.total_bytes = 0;
        let total = memory_total_bytes(&fixture).expect("read");
        assert_eq!(total, 0);

        // A budget expressed as a fraction of the machine's RAM sizes to
        // nothing when the machine's size is unknown.
        let eighth_of_ram = total / 8;
        assert_eq!(eighth_of_ram, 0);
    }

    #[test]
    fn denial_maps_to_permission_denied() {
        let mut fixture = Fixture::new();
        fixture.deny = Some(SysinfoQueryId::MEMORY_PRESSURE);
        assert_eq!(memory_pressure(&fixture), Err(CallError::PermissionDenied));
        fixture.deny = Some(SysinfoQueryId::RAMZIP_STATS);
        assert_eq!(ramzip_stats(&fixture), Err(CallError::PermissionDenied));
    }

    #[test]
    fn malformed_scalar_replies_fail_closed() {
        let mut fixture = Fixture::new();
        fixture.malformed = Some(SysinfoQueryId::MEMORY_PRESSURE);
        assert_eq!(
            memory_pressure(&fixture),
            Err(CallError::Service(Errno::BadMagic))
        );
        fixture.malformed = Some(SysinfoQueryId::RAMZIP_STATS);
        assert_eq!(
            ramzip_stats(&fixture),
            Err(CallError::Service(Errno::BadMagic))
        );
        // A refused decode, not a zero-extended size: a caller must never
        // budget against a figure the service did not send.
        fixture.malformed = Some(SysinfoQueryId::MEMORY_TOTAL);
        assert_eq!(
            memory_total_bytes(&fixture),
            Err(CallError::Service(Errno::BadMagic))
        );
    }

    #[test]
    fn reclaim_walk_yields_every_class_in_order() {
        let fixture = Fixture::new();
        let seen = RefCell::new(Vec::new());
        for_each_reclaim_class(&fixture, |record| {
            seen.borrow_mut().push(*record);
            Ok(WalkStep::Continue)
        })
        .expect("walk");
        let got = seen.into_inner();
        assert_eq!(got.len(), RECLAIM_CLASS_COUNT);
        for (index, record) in got.iter().enumerate() {
            assert_eq!(usize::from(record.class), index);
        }
    }

    #[test]
    fn cpu_load_walk_yields_records_and_pages_until_short() {
        let mut fixture = Fixture::new();
        fixture.loads.clear();
        for cpu in 0..=u32::from(CPU_LOAD_PAGE) {
            fixture.loads.push(CpuLoadRecord {
                cpu,
                reserved: 0,
                queue_depth: 1,
                switches: 1,
                preemptions: 0,
            });
        }
        let count = RefCell::new(0usize);
        for_each_cpu_load(&fixture, |_| {
            *count.borrow_mut() += 1;
            Ok(WalkStep::Continue)
        })
        .expect("walk");
        assert_eq!(*count.borrow(), usize::from(CPU_LOAD_PAGE) + 1);
        // Two pages: the full page plus the short trailer.
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn walk_denial_and_malformed_fail_closed() {
        let mut fixture = Fixture::new();
        fixture.deny = Some(SysinfoQueryId::RECLAIM_STATS);
        assert_eq!(
            for_each_reclaim_class(&fixture, |_| Ok(WalkStep::Continue)),
            Err(ListError::Call(CallError::PermissionDenied))
        );
        fixture.deny = None;
        fixture.malformed = Some(SysinfoQueryId::CPU_LOAD);
        assert_eq!(
            for_each_cpu_load(&fixture, |_| Ok(WalkStep::Continue)),
            Err(ListError::Call(CallError::Service(Errno::BadMagic)))
        );
    }

    #[test]
    fn sink_error_stops_the_walk() {
        let fixture = Fixture::new();
        let count = RefCell::new(0usize);
        let result = for_each_cpu_load(&fixture, |_| {
            *count.borrow_mut() += 1;
            Err(Errno::NotFound)
        });
        assert_eq!(result, Err(ListError::Sink(Errno::NotFound)));
        assert_eq!(*count.borrow(), 1);
    }

    #[test]
    fn reclaim_page_covers_the_class_set() {
        assert!(usize::from(RECLAIM_PAGE) >= RECLAIM_CLASS_COUNT);
    }

    #[test]
    fn irq_walk_yields_records_in_line_order_with_counts_and_flags() {
        let fixture = Fixture::new();
        let seen = RefCell::new(Vec::new());
        for_each_irq(&fixture, |record| {
            seen.borrow_mut().push(*record);
            Ok(WalkStep::Continue)
        })
        .expect("walk");
        let got = seen.into_inner();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].line, 27);
        assert_eq!(got[0].owner, 14);
        assert_eq!(got[0].count, 1234);
        assert_eq!(got[1].line, 111);
        assert_eq!(got[1].count, 200_000);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::IRQ_LIST]
        );
    }

    #[test]
    fn irq_walk_pages_until_short() {
        let mut fixture = Fixture::new();
        fixture.irqs.clear();
        for line in 0..=u32::from(IRQ_PAGE) {
            fixture.irqs.push(IrqRecord {
                line,
                flags: 0,
                owner: 1,
                count: u64::from(line),
            });
        }
        let count = RefCell::new(0usize);
        for_each_irq(&fixture, |_| {
            *count.borrow_mut() += 1;
            Ok(WalkStep::Continue)
        })
        .expect("walk");
        assert_eq!(*count.borrow(), usize::from(IRQ_PAGE) + 1);
        // Two pages: the full page plus the short trailer.
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn irq_walk_denial_fails_closed() {
        let mut fixture = Fixture::new();
        fixture.deny = Some(SysinfoQueryId::IRQ_LIST);
        assert_eq!(
            for_each_irq(&fixture, |_| Ok(WalkStep::Continue)),
            Err(ListError::Call(CallError::PermissionDenied))
        );
    }

    #[test]
    fn cache_ledger_walk_yields_kernel_and_reported_rows() {
        let fixture = Fixture::new();
        let seen = RefCell::new(Vec::new());
        for_each_cache_ledger(&fixture, |record| {
            seen.borrow_mut().push(*record);
            Ok(WalkStep::Continue)
        })
        .expect("walk");
        let got = seen.into_inner();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].label(), "fontd-glyphs");
        assert_eq!(got[0].origin, CacheLedgerOrigin::Kernel);
        assert_eq!(got[0].reporter_pid, 0);
        assert_eq!(got[1].label(), "taskbar-icons");
        assert_eq!(got[1].origin, CacheLedgerOrigin::SelfReported);
        assert_eq!(got[1].reporter_pid, 77);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::CACHE_LEDGERS]
        );
    }

    #[test]
    fn cache_ledger_walk_pages_until_short() {
        let mut fixture = Fixture::new();
        fixture.caches.clear();
        for i in 0..=u32::from(CACHE_LEDGER_PAGE) {
            let mut label = alloc::format!("cache-{i}");
            label.truncate(32);
            let mut row =
                CacheLedgerRecord::new(label.as_bytes(), CacheOwnerKind::KernelSubsystem, 0, 0)
                    .unwrap();
            row.origin = CacheLedgerOrigin::Kernel;
            fixture.caches.push(row);
        }
        let count = RefCell::new(0usize);
        for_each_cache_ledger(&fixture, |_| {
            *count.borrow_mut() += 1;
            Ok(WalkStep::Continue)
        })
        .expect("walk");
        assert_eq!(*count.borrow(), usize::from(CACHE_LEDGER_PAGE) + 1);
        // Two pages: the full page plus the short trailer.
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn cache_ledger_walk_denial_fails_closed() {
        let mut fixture = Fixture::new();
        fixture.deny = Some(SysinfoQueryId::CACHE_LEDGERS);
        assert_eq!(
            for_each_cache_ledger(&fixture, |_| Ok(WalkStep::Continue)),
            Err(ListError::Call(CallError::PermissionDenied))
        );
    }
}
