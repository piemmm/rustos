//! The shared kernel-statistics fetches (`plans/STRESSTEST.md` ST1/ST4).
//!
//! The four kernel-wide observability queries — the memory-pressure gauge,
//! the reclaim ledger, the `ramzip` tier counters, and the per-CPU scheduler
//! load — are consumed by both the `info:`/`stats:` resolver
//! ([`mod@crate::resolve`]) and the `sysmon` monitor, so the fetch + fail-closed
//! decode lives here once. The paged walks are the generic
//! [`walk_pages`](crate::list) the process and mount lists use; the scalar
//! queries share its convention that a structurally invalid reply is
//! [`Errno::BadMagic`], never a partial decode.
//!
//! Every query here is gated on `CAP_SYSINFO_KERNEL` by `sysinfod`; a
//! denial surfaces as [`CallError::PermissionDenied`] so a consumer can
//! render the refusal and continue (the queries are observability, never
//! load-bearing for a session).

use tairix_abi::sysinfo::{
    CpuLoadRecord, CpuLoadRequest, IrqListRequest, IrqRecord, MemoryPressureBand,
    MemoryPressureStats, RamzipStats, ReclaimClassRecord, ReclaimListRequest, SysinfoQueryId,
    RECLAIM_CLASS_COUNT,
};
use tairix_abi::Errno;

use crate::list::{walk_pages, ListError};
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

/// Page through the reclaim ledger ([`SysinfoQueryId::RECLAIM_STATS`]) and
/// hand each decoded [`ReclaimClassRecord`] to `sink`, in class order.
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
    mut sink: impl FnMut(&ReclaimClassRecord) -> Result<(), Errno>,
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
    mut sink: impl FnMut(&CpuLoadRecord) -> Result<(), Errno>,
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
    mut sink: impl FnMut(&IrqRecord) -> Result<(), Errno>,
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

#[cfg(test)]
mod tests {
    use super::{
        for_each_cpu_load, for_each_irq, for_each_reclaim_class, memory_pressure, ramzip_stats,
        CPU_LOAD_PAGE, IRQ_PAGE, RECLAIM_PAGE,
    };
    use crate::list::ListError;
    use crate::request::CallError;
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::sysinfo::{
        CpuLoadRecord, CpuLoadRequest, IrqListRequest, IrqRecord, MemoryPressureStats, RamzipStats,
        ReclaimClassRecord, ReclaimListRequest, SysinfoQueryId, SysinfoRequestHeader,
        RECLAIM_CLASS_COUNT,
    };
    use tairix_abi::Errno;

    /// An in-memory `sysinfod` stand-in answering the four kernel-stats
    /// queries from fixed data, decoding each request exactly as the real
    /// service does.
    struct Fixture {
        pressure: MemoryPressureStats,
        ramzip: RamzipStats,
        reclaim: Vec<ReclaimClassRecord>,
        loads: Vec<CpuLoadRecord>,
        irqs: Vec<IrqRecord>,
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
                _ => Err(Errno::NotFound),
            }
        }
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
    }

    #[test]
    fn reclaim_walk_yields_every_class_in_order() {
        let fixture = Fixture::new();
        let seen = RefCell::new(Vec::new());
        for_each_reclaim_class(&fixture, |record| {
            seen.borrow_mut().push(*record);
            Ok(())
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
            Ok(())
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
            for_each_reclaim_class(&fixture, |_| Ok(())),
            Err(ListError::Call(CallError::PermissionDenied))
        );
        fixture.deny = None;
        fixture.malformed = Some(SysinfoQueryId::CPU_LOAD);
        assert_eq!(
            for_each_cpu_load(&fixture, |_| Ok(())),
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
            Ok(())
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
            Ok(())
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
            for_each_irq(&fixture, |_| Ok(())),
            Err(ListError::Call(CallError::PermissionDenied))
        );
    }
}
