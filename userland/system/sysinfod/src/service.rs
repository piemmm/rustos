//! The request dispatcher: the one place a `sysinfo` request is decoded,
//! capability-checked, audited, and answered.

use alloc::vec::Vec;

use tairix_abi::hwtree::{HwNode, HwTreeHeader};
use tairix_abi::net_ipc::{
    NetBondMemberRecord, NetInterfaceCountersRecord, NetInterfaceFactsRecord,
    NetInterfaceRatesRecord, NetInterfaceStateRecord, NetResolverServer, NetSocketRecord,
};
use tairix_abi::raid_admin::{RaidArrayRecord, RaidMemberRecord};
use tairix_abi::sysinfo::{
    fold_cache_ledgers, spec_for, CacheLedgerListRequest, CacheLedgerRecord, CacheReportRequest,
    CpuInfoListRequest, CpuInfoRecord, CpuLoadRecord, CpuLoadRequest, CpuTimeListRequest,
    CpuTimeRecord, CrashRecord, CrashRecordRequest, HardwareTreeRequest, IrqListRequest, IrqRecord,
    MountListRequest, MountRecord, NetInterfaceListRequest, NetInterfaceRatesRequest,
    ProcessListRequest, ProcessRecord, RaidListRequest, ReclaimClassRecord, ReclaimListRequest,
    ResourceLimitRecord, SeatListRequest, SeatRecord, SysinfoQueryId, SysinfoRequestHeader,
    UserDirectoryRecord, UserDirectoryRequest, VolumeIoHealthRecord, VolumeIoHealthRequest,
};
use tairix_abi::{Errno, LimitKind};
use tairix_log::{log, Event, EventId, Field, Level, Sink};

use crate::events;
use crate::reporters::CacheLedgerRegistry;
use crate::source::{Caller, ProcessScope, SysinfoSource};

/// Serve one System Information request.
///
/// Decodes the [`SysinfoRequestHeader`] (and any typed payload) from
/// `request`, enforces the query's declared capability against `caller`,
/// emits an audit record through `audit` where the query demands it, and
/// writes the encoded typed response into `response`, returning the number
/// of bytes written. `registry` is `sysinfod`'s own reported-cache-ledger
/// state, threaded through rather than owned by this function so the
/// service's serve loop can keep it alive across calls.
///
/// The pipeline **fails closed**: the capability check
/// happens before any data is touched, and every early return leaves
/// `response` untouched. There is no path that answers a privileged query
/// without first passing its capability gate — `sysinfod` is the only
/// server of the API and the kernel exposes no bypass.
///
/// # Response encoding
///
/// * Process-list queries return zero or more [`ProcessRecord`]s packed
///   back-to-back; the caller pages with the request's `offset`/`limit` and
///   detects the end of the list when it receives fewer than `limit`
///   records.
/// * The scalar queries return the little-endian wire image of their
///   response struct.
/// * The hardware-tree query returns the source's encoded bytes verbatim.
/// * [`SysinfoQueryId::RECLAIM_STATS`] folds the kernel's own
///   [`CacheLedgerRecord`] rows with `registry`'s self-reported rows
///   (after dropping any reporter whose process has since exited) into the
///   nine per-class [`ReclaimClassRecord`]s, packed back-to-back and paged
///   like any other list.
/// * [`SysinfoQueryId::CACHE_LEDGERS`] pages that same combined row list
///   — kernel rows first, then reported rows — as raw [`CacheLedgerRecord`]s.
/// * [`SysinfoQueryId::CACHE_REPORT`] carries no reply payload; a caller
///   whose submission is accepted gets back zero bytes.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `request` is shorter than its declared
///   payload, or `response` cannot hold the encoded answer.
/// * [`Errno::BadMagic`] / [`Errno::AbiVersionUnsupported`] /
///   [`Errno::OutOfRange`] / [`Errno::LengthOutOfRange`] — the header or
///   payload failed to decode against `sysinfo-v1`.
/// * [`Errno::PermissionDenied`] — the caller lacks the query's required
///   capability.
/// * [`Errno::NotImplemented`] — the request named a reserved-but-unassigned
///   query identifier.
/// * Any error returned by the backing [`SysinfoSource`].
pub fn serve(
    source: &dyn SysinfoSource,
    caller: &Caller,
    registry: &mut CacheLedgerRegistry,
    audit: &dyn Sink,
    request: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let header = match SysinfoRequestHeader::from_bytes(request) {
        Ok(header) => header,
        Err(err) => {
            emit(
                audit,
                Level::Warn,
                events::REQUEST_MALFORMED,
                "sysinfo request rejected: header decode failed",
                &[],
            );
            return Err(err);
        }
    };

    let Some(spec) = spec_for(header.query) else {
        emit(
            audit,
            Level::Warn,
            events::QUERY_UNAVAILABLE,
            "sysinfo query rejected: unassigned query identifier",
            &[],
        );
        return Err(Errno::NotImplemented);
    };

    let payload_len = header.payload_len as usize;
    let payload_end = SysinfoRequestHeader::WIRE_LEN
        .checked_add(payload_len)
        .ok_or(Errno::LengthOutOfRange)?;
    if request.len() < payload_end {
        emit(
            audit,
            Level::Warn,
            events::REQUEST_MALFORMED,
            "sysinfo request rejected: declared payload is truncated",
            &[query_field(spec.name)],
        );
        return Err(Errno::BufferTooSmall);
    }
    let payload = &request[SysinfoRequestHeader::WIRE_LEN..payload_end];

    if let Some(required) = spec.required_capability {
        if !caller.capabilities().holds(required) {
            emit(
                audit,
                Level::Warn,
                events::QUERY_DENIED,
                "sysinfo query denied: caller lacks required capability",
                &[query_field(spec.name)],
            );
            return Err(Errno::PermissionDenied);
        }
    }

    // Record the invocation before dispatch so every privileged call is
    // accounted for even if the backing source later errors. Recorded at
    // `Debug`: a monitor polling privileged queries emits this allow
    // record continuously, and at `Info` it floods the default console
    // filter; lowering the filter recovers it for forensics. Denials stay
    // at `Warn` and always surface.
    if spec.audit {
        emit(
            audit,
            Level::Debug,
            events::QUERY_SERVED,
            "sysinfo query served",
            &[query_field(spec.name)],
        );
    }

    dispatch(source, caller, registry, header.query, payload, response)
}

/// Route a capability-cleared request to its [`SysinfoSource`] method and
/// encode the answer.
fn dispatch(
    source: &dyn SysinfoSource,
    caller: &Caller,
    registry: &mut CacheLedgerRegistry,
    query: SysinfoQueryId,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    if query == SysinfoQueryId::SELF_PROCESS_LIST {
        process_list(source, caller, ProcessScope::Caller, payload, response)
    } else if query == SysinfoQueryId::GLOBAL_PROCESS_LIST {
        process_list(source, caller, ProcessScope::Global, payload, response)
    } else if query == SysinfoQueryId::KERNEL_MEMORY_STATS {
        write_bytes(&source.kernel_memory_stats(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::HARDWARE_TREE {
        hardware_tree(source, caller, payload, response)
    } else if query == SysinfoQueryId::SYSTEM_IDENTITY {
        write_bytes(&source.system_identity(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::UPTIME {
        write_bytes(&source.uptime(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::LOAD_AVERAGE {
        write_bytes(&source.load_average(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::MOUNT_LIST {
        mount_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::CPU_TIME_STATS {
        cpu_time_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::SEAT_LIST {
        seat_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::RESOURCE_LIMITS {
        resource_limits(source, caller, response)
    } else if query == SysinfoQueryId::USER_DIRECTORY {
        user_directory(source, caller, payload, response)
    } else if query == SysinfoQueryId::MEMORY_PRESSURE {
        write_bytes(&source.memory_pressure(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::MEMORY_PRESSURE_BAND {
        write_bytes(
            &source.memory_pressure_band(caller)?.to_le_bytes(),
            response,
        )
    } else if query == SysinfoQueryId::MEMORY_TOTAL {
        write_bytes(&source.memory_total(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::RECLAIM_STATS {
        reclaim_list(source, caller, registry, payload, response)
    } else if query == SysinfoQueryId::CACHE_LEDGERS {
        cache_ledgers_list(source, caller, registry, payload, response)
    } else if query == SysinfoQueryId::CACHE_REPORT {
        cache_report(source, caller, registry, payload, response)
    } else if query == SysinfoQueryId::RAMZIP_STATS {
        write_bytes(&source.ramzip_stats(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::CPU_LOAD {
        cpu_load_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::CPU_INFO {
        cpu_info_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::NET_INTERFACE_FACTS {
        net_facts_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::NET_INTERFACE_STATE {
        net_state_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::NET_INTERFACE_COUNTERS {
        net_counters_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::NET_INTERFACE_RATES {
        net_rates_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::NET_SOCKETS {
        net_sockets_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::NET_BOND_MEMBERS {
        net_bond_members_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::NET_RESOLVER_SERVERS {
        net_resolver_servers_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::IRQ_LIST {
        irq_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::CRASH_RECORD {
        crash_record_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::VOLUME_IO_HEALTH {
        volume_io_health_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::RAID_ARRAYS {
        raid_array_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::RAID_MEMBERS {
        raid_member_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::PROCESS_IDENTITY {
        // The answer is the caller's own kernel-attested origin, which the
        // dispatcher already holds: it is the attested principal, not state a
        // `SysinfoSource` would supply, so it is encoded here directly rather
        // than echoed through the source seam.
        write_bytes(&caller.origin().to_le_bytes(), response)
    } else {
        Err(Errno::NotImplemented)
    }
}

/// Encode the caller's per-[`LimitKind`] effective-limit + live-usage report
/// into `response`.
///
/// The response is the fixed `LimitKind::COUNT` records packed back-to-back
/// in discriminant order (no paging — the set is small and closed). Fails
/// closed with [`Errno::BufferTooSmall`] if `response` cannot hold them.
fn resource_limits(
    source: &dyn SysinfoSource,
    caller: &Caller,
    response: &mut [u8],
) -> Result<usize, Errno> {
    let records = source.resource_limits(caller)?;
    let needed = ResourceLimitRecord::WIRE_LEN * LimitKind::COUNT;
    if response.len() < needed {
        return Err(Errno::BufferTooSmall);
    }
    let mut written = 0;
    for record in &records {
        response[written..written + ResourceLimitRecord::WIRE_LEN]
            .copy_from_slice(&record.to_le_bytes());
        written += ResourceLimitRecord::WIRE_LEN;
    }
    Ok(written)
}

/// Decode the [`HardwareTreeRequest`], validate the snapshot the source
/// returned, and pack the snapshot's [`HwTreeHeader`] plus the selected
/// window of whole [`HwNode`] records into `response`.
///
/// Every page repeats the header, so a paging client always sees the
/// snapshot's total node count and the generation the page was served
/// from, and can detect a tree that changed under its walk. The source's
/// blob is validated before a byte is served: a snapshot whose header and
/// body disagree fails closed with [`Errno::BadMagic`] rather than paging
/// bytes a client would mis-frame.
fn hardware_tree(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = HardwareTreeRequest::from_bytes(payload)?;
    let blob = source.hardware_tree(caller)?;
    let header = HwTreeHeader::from_bytes(&blob)?;
    let count = usize::try_from(header.node_count()).map_err(|_| Errno::LengthOutOfRange)?;
    let body_len = count
        .checked_mul(HwNode::WIRE_LEN)
        .ok_or(Errno::LengthOutOfRange)?;
    let body = &blob[HwTreeHeader::WIRE_LEN..];
    if body.len() != body_len {
        return Err(Errno::BadMagic);
    }
    if response.len() < HwTreeHeader::WIRE_LEN {
        return Err(Errno::BufferTooSmall);
    }
    response[..HwTreeHeader::WIRE_LEN].copy_from_slice(&header.to_le_bytes());
    let written = page_records(
        &mut response[HwTreeHeader::WIRE_LEN..],
        request.offset as usize,
        request.limit as usize,
        count,
        HwNode::WIRE_LEN,
        |index, slot| {
            slot.copy_from_slice(&body[index * HwNode::WIRE_LEN..(index + 1) * HwNode::WIRE_LEN]);
        },
    )?;
    Ok(HwTreeHeader::WIRE_LEN + written)
}

/// Decode the [`ProcessListRequest`], apply paging, and pack the selected
/// [`ProcessRecord`]s into `response`.
fn process_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    scope: ProcessScope,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = ProcessListRequest::from_bytes(payload)?;
    let records = source.process_records(caller, scope)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        ProcessRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`MountListRequest`], apply paging, and pack the selected
/// [`MountRecord`]s into `response`.
fn mount_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = MountListRequest::from_bytes(payload)?;
    let records = source.mount_records(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        MountRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`NetInterfaceListRequest`] and page the interface-facts
/// records into `response`.
fn net_facts_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = NetInterfaceListRequest::from_bytes(payload)?;
    let records = source.net_interface_facts(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        NetInterfaceFactsRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`NetInterfaceListRequest`] and page the interface-state
/// records into `response`.
fn net_state_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = NetInterfaceListRequest::from_bytes(payload)?;
    let records = source.net_interface_state(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        NetInterfaceStateRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`NetInterfaceListRequest`] and page the interface-counter
/// records into `response`.
fn net_counters_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = NetInterfaceListRequest::from_bytes(payload)?;
    let records = source.net_interface_counters(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        NetInterfaceCountersRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`NetInterfaceRatesRequest`] (carrying the averaging window)
/// and page the interface-rates records into `response`.
fn net_rates_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = NetInterfaceRatesRequest::from_bytes(payload)?;
    let records = source.net_interface_rates(caller, request.window)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        NetInterfaceRatesRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`NetInterfaceListRequest`] and page the socket-listing
/// records into `response`.
fn net_sockets_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = NetInterfaceListRequest::from_bytes(payload)?;
    let records = source.net_sockets(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        NetSocketRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`NetInterfaceListRequest`] and page the bond-member
/// records into `response`.
fn net_bond_members_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = NetInterfaceListRequest::from_bytes(payload)?;
    let records = source.net_bond_members(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        NetBondMemberRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`NetInterfaceListRequest`] and page the resolver-server
/// records into `response`. The active set is small and closed, so a
/// single page always suffices, but it shares the one paging codec
/// rather than inventing a bespoke reply shape.
fn net_resolver_servers_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = NetInterfaceListRequest::from_bytes(payload)?;
    let records = source.net_resolver_servers(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        NetResolverServer::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Read the kernel's own per-cache rows, drop any reporter registry entry
/// whose process instance has since exited, and return the combined row
/// list: kernel rows first, then `registry`'s self-reported rows.
///
/// Shared by [`reclaim_list`] and [`cache_ledgers_list`] so the two query
/// answers can never disagree about which rows exist.
fn combined_cache_rows(
    source: &dyn SysinfoSource,
    caller: &Caller,
    registry: &mut CacheLedgerRegistry,
) -> Result<Vec<CacheLedgerRecord>, Errno> {
    let mut rows = source.cache_ledger_records(caller)?;
    registry.retain_live(&source.live_process_instances()?);
    rows.extend(registry.rows());
    Ok(rows)
}

/// Decode the [`ReclaimListRequest`], fold the combined kernel and
/// self-reported cache rows into the nine per-class totals, apply paging,
/// and pack the selected [`ReclaimClassRecord`]s into `response`.
fn reclaim_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    registry: &mut CacheLedgerRegistry,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = ReclaimListRequest::from_bytes(payload)?;
    let rows = combined_cache_rows(source, caller, registry)?;
    let totals = fold_cache_ledgers(&rows);
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        totals.len(),
        ReclaimClassRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&totals[index].to_le_bytes()),
    )
}

/// Decode the [`CacheLedgerListRequest`], apply paging, and pack the
/// combined kernel and self-reported [`CacheLedgerRecord`]s into
/// `response`.
fn cache_ledgers_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    registry: &mut CacheLedgerRegistry,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = CacheLedgerListRequest::from_bytes(payload)?;
    let rows = combined_cache_rows(source, caller, registry)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        rows.len(),
        CacheLedgerRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&rows[index].to_le_bytes()),
    )
}

/// Decode a [`CacheReportRequest`] header followed by exactly `count`
/// [`CacheLedgerRecord`]s and replace the caller's entry in `registry`.
///
/// The reply carries no payload, so a successful call always returns
/// `Ok(0)`. Every row is decoded before `registry` is touched, so a
/// malformed submission — a short body, trailing bytes, an over-long
/// declared count, or a row with an unrenderable label — fails closed with
/// the failing decoder's own [`Errno`] and leaves `registry` exactly as it
/// was.
fn cache_report(
    source: &dyn SysinfoSource,
    caller: &Caller,
    registry: &mut CacheLedgerRegistry,
    payload: &[u8],
    _response: &mut [u8],
) -> Result<usize, Errno> {
    let header = CacheReportRequest::from_bytes(payload)?;
    let count = usize::from(header.count);
    let body = payload
        .get(CacheReportRequest::WIRE_LEN..)
        .ok_or(Errno::BufferTooSmall)?;
    let (records, trailing) = body.as_chunks::<{ CacheLedgerRecord::WIRE_LEN }>();
    if records.len() != count || !trailing.is_empty() {
        return Err(Errno::BadMagic);
    }
    let mut rows = Vec::with_capacity(count);
    for record in records {
        rows.push(CacheLedgerRecord::from_bytes(record)?);
    }

    // A full registry only refuses a genuinely *new* reporter: drop any
    // entry whose process has already exited before deciding whether
    // there is room for this one.
    registry.retain_live(&source.live_process_instances()?);
    registry.report(caller, rows)?;
    Ok(0)
}

/// Decode the [`CpuLoadRequest`], apply paging, and pack the selected
/// [`CpuLoadRecord`]s into `response`.
fn cpu_load_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = CpuLoadRequest::from_bytes(payload)?;
    let records = source.cpu_load(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        CpuLoadRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`CpuInfoListRequest`], apply paging, and pack the selected
/// [`CpuInfoRecord`]s into `response`.
fn cpu_info_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = CpuInfoListRequest::from_bytes(payload)?;
    let records = source.cpu_info(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        CpuInfoRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`CpuTimeListRequest`], apply paging, and pack the selected
/// [`CpuTimeRecord`]s into `response`.
fn cpu_time_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = CpuTimeListRequest::from_bytes(payload)?;
    let records = source.cpu_times(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        CpuTimeRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`VolumeIoHealthRequest`], apply paging, and pack the selected
/// [`VolumeIoHealthRecord`]s into `response`.
fn volume_io_health_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = VolumeIoHealthRequest::from_bytes(payload)?;
    let records = source.volume_io_health(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        VolumeIoHealthRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`RaidListRequest`], apply paging, and pack the selected
/// [`RaidArrayRecord`]s into `response`.
fn raid_array_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = RaidListRequest::from_bytes(payload)?;
    let records = source.raid_arrays(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        RaidArrayRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`RaidListRequest`], apply paging, and pack the selected
/// [`RaidMemberRecord`]s into `response`.
fn raid_member_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = RaidListRequest::from_bytes(payload)?;
    let records = source.raid_members(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        RaidMemberRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`IrqListRequest`], apply paging, and pack the selected
/// [`IrqRecord`]s into `response`.
fn irq_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = IrqListRequest::from_bytes(payload)?;
    let records = source.irqs(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        IrqRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`CrashRecordRequest`], apply paging, and pack the selected
/// [`CrashRecord`]s into `response`.
fn crash_record_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = CrashRecordRequest::from_bytes(payload)?;
    let records = source.crashes(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        CrashRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`SeatListRequest`], apply paging, and pack the selected
/// [`SeatRecord`]s into `response`.
fn seat_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = SeatListRequest::from_bytes(payload)?;
    let records = source.seats(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        SeatRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`UserDirectoryRequest`], apply paging, and pack the selected
/// [`UserDirectoryRecord`]s into `response`.
fn user_directory(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = UserDirectoryRequest::from_bytes(payload)?;
    let records = source.user_directory(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        UserDirectoryRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Pack a paged window of fixed-`wire_len` records into `response`.
///
/// Shared by every list query so the paging arithmetic — offset bounds, the
/// `limit` window, the buffer-capacity check, and the fail-closed
/// `BufferTooSmall` — lives in exactly one place. `encode`
/// writes record `index` into the supplied `wire_len`-byte slot.
fn page_records(
    response: &mut [u8],
    offset: usize,
    limit: usize,
    count: usize,
    wire_len: usize,
    mut encode: impl FnMut(usize, &mut [u8]),
) -> Result<usize, Errno> {
    if offset >= count {
        return Ok(0);
    }
    let take = core::cmp::min(count - offset, limit);
    let needed = take.checked_mul(wire_len).ok_or(Errno::LengthOutOfRange)?;
    if response.len() < needed {
        return Err(Errno::BufferTooSmall);
    }
    let mut written = 0;
    for index in offset..offset + take {
        encode(index, &mut response[written..written + wire_len]);
        written += wire_len;
    }
    Ok(written)
}

/// Copy `src` into `dst`, failing closed if `dst` is too small.
fn write_bytes(src: &[u8], dst: &mut [u8]) -> Result<usize, Errno> {
    if dst.len() < src.len() {
        return Err(Errno::BufferTooSmall);
    }
    dst[..src.len()].copy_from_slice(src);
    Ok(src.len())
}

/// Submit one audit record to `audit`.
fn emit(audit: &dyn Sink, level: Level, id: EventId, message: &str, fields: &[Field<'_>]) {
    log(
        audit,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}

/// Build the `query=<name>` field carried by audit records.
fn query_field(name: &'static str) -> Field<'static> {
    Field {
        key: "query",
        value: tairix_log::FieldValue::Str(name),
    }
}

#[cfg(test)]
mod tests {
    use super::{serve, CacheLedgerRegistry};
    use crate::events;
    use crate::reporters::{MIN_REPORTERS, RAM_BYTES_PER_REPORTER};
    use crate::source::{Caller, ProcessScope, SysinfoSource};
    use crate::testing::{kernel_caller, user_caller};
    use core::cell::RefCell;
    use tairix_abi::blkio::{BlkDeviceClass, BlkHealthCounters};
    use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
    use tairix_abi::hwtree::{HwDeviceClass, HwNode, HwTreeHeader, HW_NODE_ROOT};
    use tairix_abi::net_ipc::{
        NetBondMemberRecord, NetInterfaceCountersRecord, NetInterfaceFactsRecord,
        NetInterfaceRatesRecord, NetInterfaceStateRecord, NetResolverServer, NetSockProto,
        NetSockState, NetSocketRecord,
    };
    use tairix_abi::raid::{ArrayHealth, RaidLevel};
    use tairix_abi::raid_admin::{
        RaidArrayRecord, RaidMemberDisposition, RaidMemberRecord, RAID_ARRAY_FLAG_RESYNCING,
        RAID_SLOT_NONE,
    };
    use tairix_abi::sysinfo::{
        CacheLedgerListRequest, CacheLedgerOrigin, CacheLedgerRecord, CacheOwnerKind,
        CacheReportRequest, CpuInfoRecord, CpuLoadRecord, CpuLoadRequest, CpuTimeListRequest,
        CpuTimeRecord, CrashFaultBucket, CrashFaultClass, CrashRecord, CrashRecordRequest,
        HardwareTreeRequest, IrqListRequest, IrqRecord, KernelMemoryStats, LoadAverage,
        MemoryPressureBand, MemoryPressureStats, MemoryTotal, MountAvailability, MountListRequest,
        MountRecord, MountVolumeState, ProcessListRequest, ProcessRecord, ProcessState,
        RaidListRequest, RamzipStats, ReclaimClassRecord, ReclaimListRequest, ResourceLimitRecord,
        SeatListRequest, SeatRecord, SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime,
        UserDirectoryRecord, UserDirectoryRequest, VolumeIoHealthRecord, VolumeIoHealthRequest,
        IRQ_FLAG_QUARANTINED, LOAD_FIXED_SHIFT, MACHINE_ID_LEN, MAX_CACHE_REPORT_ENTRIES,
        RECLAIM_CLASS_COUNT, RESOURCE_LIMITS_REPORT_LEN, SEAT_FLAG_OWNED, SYSINFO_MAX_REPLY,
        SYSINFO_REPLY_STATUS_LEN, SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT,
    };
    use tairix_abi::sysinfo::{NetInterfaceListRequest, NetInterfaceRatesRequest};
    use tairix_abi::time::{Duration64, Time64};
    use tairix_abi::{
        CapabilityId, Errno, LimitKind, Origin, ProcId, ResourceLimit, SchedPriority, TrustDomain,
        ORIGIN_WIRE_LEN,
    };
    use tairix_log::{Event, Level, Sink};

    /// The capabilities a test caller's attested origin should summarise.
    struct Caps(&'static [CapabilityId]);

    /// Records every event it receives so tests can assert on audit output.
    struct RecordingSink {
        events: RefCell<heapless_vec::Vec>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(heapless_vec::Vec::new()),
            }
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push((event.level, event.id));
        }
    }

    /// Tiny fixed-capacity vector so the test sink needs no allocator.
    mod heapless_vec {
        use tairix_log::{EventId, Level};

        pub struct Vec {
            buf: [(Level, EventId); 8],
            len: usize,
        }
        impl Default for Vec {
            fn default() -> Self {
                Self {
                    buf: [(Level::Trace, EventId(0)); 8],
                    len: 0,
                }
            }
        }
        impl Vec {
            pub fn new() -> Self {
                Self::default()
            }
            pub fn push(&mut self, item: (Level, EventId)) {
                if self.len < self.buf.len() {
                    self.buf[self.len] = item;
                    self.len += 1;
                }
            }
            pub fn as_slice(&self) -> &[(Level, EventId)] {
                &self.buf[..self.len]
            }
        }
    }

    /// Encode a hardware-tree snapshot exactly as `hw_tree_read` returns
    /// it: the [`HwTreeHeader`] followed by every node's wire image.
    fn tree_blob(generation: u64, nodes: &[HwNode]) -> alloc::vec::Vec<u8> {
        let mut blob = alloc::vec::Vec::new();
        blob.extend_from_slice(&HwTreeHeader::new(generation, nodes.len() as u64).to_le_bytes());
        for node in nodes {
            blob.extend_from_slice(&node.to_le_bytes());
        }
        blob
    }

    /// A three-node discovered tree: a root and two devices under it.
    fn tree_nodes() -> alloc::vec::Vec<HwNode> {
        alloc::vec![
            HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(1, 0, HwDeviceClass::Serial),
            HwNode::new(2, 0, HwDeviceClass::Storage),
        ]
    }

    /// The pressure snapshot the fixture serves.
    fn fixture_pressure() -> MemoryPressureStats {
        MemoryPressureStats {
            band: 1,
            reserved: [0u8; 7],
            total_bytes: 1 << 30,
            free_bytes: 180 << 20,
            reserve_bytes: 16 << 20,
            enter_bytes: [204 << 20, 102 << 20, 64 << 20, 32 << 20],
            exit_bytes: [256 << 20, 143 << 20, 81 << 20, 51 << 20],
            band_entries: [0, 2, 1, 0, 0],
        }
    }

    /// One kernel-measured cache-ledger row per reclaim class, with figures
    /// derived from the class id so paging and folding tests can assert
    /// exact windows and values. Standing in for the retired
    /// `fixture_reclaim`, which built the folded totals directly; folding
    /// these through [`tairix_abi::sysinfo::fold_cache_ledgers`] now
    /// reproduces the exact same per-class totals.
    fn fixture_cache_ledgers() -> alloc::vec::Vec<CacheLedgerRecord> {
        (0..RECLAIM_CLASS_COUNT)
            .map(|i| {
                let mut record = CacheLedgerRecord::new(
                    b"kernel-cache",
                    CacheOwnerKind::KernelSubsystem,
                    0,
                    u8::try_from(i).unwrap(),
                )
                .unwrap();
                record.origin = CacheLedgerOrigin::Kernel;
                record.payload_bytes = (i as u64) * 1000;
                record.metadata_bytes = (i as u64) * 10;
                record.entries = i as u64;
                record.pressure_shrinks = 1;
                record.hits = (i as u64) * 100;
                record.misses = i as u64;
                record
            })
            .collect()
    }

    /// The `ramzip` snapshot the fixture serves.
    fn fixture_ramzip() -> RamzipStats {
        RamzipStats {
            entries: 4,
            logical_bytes: 4 * 4096,
            stored_bytes: 6000,
            attempts: 9,
            accepted: 4,
            fault_ins: 2,
            ..RamzipStats::default()
        }
    }

    /// Two CPUs' load records.
    fn fixture_cpu_load() -> alloc::vec::Vec<CpuLoadRecord> {
        alloc::vec![
            CpuLoadRecord {
                cpu: 0,
                reserved: 0,
                queue_depth: 1,
                switches: 42,
                preemptions: 5,
            },
            CpuLoadRecord {
                cpu: 1,
                reserved: 0,
                queue_depth: 0,
                switches: 17,
                preemptions: 2,
            },
        ]
    }

    /// A crash record with a distinct pid and name, a load-relative pc, and
    /// two backtrace frames, so paging and decode are checkable.
    fn fixture_crash(pid: u64, name: &[u8]) -> CrashRecord {
        let mut rec = CrashRecord::new(
            ProcId::KERNEL,
            pid,
            1000,
            1000,
            true,
            CrashFaultClass::Wild,
            CrashFaultBucket::NullPage,
            0x18,
            name,
        )
        .unwrap();
        rec.set_registers(0x40, 0x7fff_0000, 0x7fff_0100, true, true);
        rec.push_frame(0x40);
        rec.push_frame(0x120);
        rec
    }

    /// Two arrays the composer would report: an optimal mirror and a
    /// rebuilding parity array, so level, health, and the in-flight flags are
    /// all exercised by a decode.
    fn fixture_raid_arrays() -> alloc::vec::Vec<RaidArrayRecord> {
        alloc::vec![
            RaidArrayRecord::new(
                [0x11; 16],
                RaidLevel::Mirror,
                ArrayHealth::Optimal,
                0,
                2,
                2,
                512,
                0,
                1_000_000,
                0x5241_2001,
                40,
                1_000_000,
                1_000_000,
                7,
            ),
            RaidArrayRecord::new(
                [0x22; 16],
                RaidLevel::Parity,
                ArrayHealth::Recovering,
                RAID_ARRAY_FLAG_RESYNCING,
                3,
                2,
                4096,
                128,
                2_000_000,
                0x5241_2002,
                41,
                2_000_000,
                640_000,
                9,
            ),
        ]
    }

    /// Three devices the composer holds: an in-sync member, one rebuilding
    /// into the same array, and an unaffiliated candidate in no slot.
    fn fixture_raid_members() -> alloc::vec::Vec<RaidMemberRecord> {
        alloc::vec![
            RaidMemberRecord::new(
                [0x22; 16],
                RaidMemberDisposition::InSync,
                0,
                50,
                0x5241_3001,
                1_000_000,
                4096,
                9,
            ),
            RaidMemberRecord::new(
                [0x22; 16],
                RaidMemberDisposition::Resyncing,
                1,
                51,
                0x5241_3002,
                1_000_000,
                4096,
                9,
            ),
            RaidMemberRecord::new(
                [0u8; 16],
                RaidMemberDisposition::Candidate,
                RAID_SLOT_NONE,
                52,
                0x5241_3003,
                4_000_000,
                512,
                0,
            ),
        ]
    }

    /// In-memory fixture standing in for the kernel's live state.
    struct FixtureSource {
        own: [ProcessRecord; 2],
        global: [ProcessRecord; 3],
        hwtree: alloc::vec::Vec<u8>,
        mounts: [MountRecord; 2],
        cache_ledgers: alloc::vec::Vec<CacheLedgerRecord>,
        /// Which process instances `live_process_instances` reports as
        /// live, set by tests through [`FixtureSource::set_live`]; empty by
        /// default, since most tests never report a cache and so never
        /// need a reporter kept alive across a query.
        live: RefCell<alloc::vec::Vec<ProcId>>,
    }
    impl FixtureSource {
        /// Declare which process instances `live_process_instances` should
        /// report as live, for tests that exercise reporter expiry.
        fn set_live(&self, live: alloc::vec::Vec<ProcId>) {
            *self.live.borrow_mut() = live;
        }

        fn new() -> Self {
            let mk = |pid, uid, name: &[u8], io_bytes_read, io_bytes_written| {
                ProcessRecord::new(
                    pid,
                    1,
                    ProcId::KERNEL,
                    ProcId::KERNEL,
                    uid,
                    uid,
                    ProcessState::Running,
                    0,
                    SchedPriority::Normal,
                    0,
                    0,
                    io_bytes_read,
                    io_bytes_written,
                    name,
                )
                .unwrap()
            };
            Self {
                own: [
                    mk(10, 1000, b"shell", 4096, 512),
                    mk(11, 1000, b"editor", 8192, 1024),
                ],
                global: [
                    mk(1, 0, b"init", u64::MAX, u64::MAX),
                    mk(10, 1000, b"shell", 4096, 512),
                    mk(11, 1000, b"editor", 8192, 1024),
                ],
                hwtree: tree_blob(7, &tree_nodes()),
                mounts: [
                    MountRecord::new(
                        b"rootfs",
                        b"/",
                        b"arxfs",
                        MountFlags::READ_ONLY,
                        MountVolumeState {
                            usage: VolumeStats::default(),
                            availability: MountAvailability::Available,
                            medium: Some(BlkDeviceClass::SolidState),
                        },
                        [0u8; 16],
                    )
                    .unwrap(),
                    MountRecord::new(
                        b"data",
                        b"/Storage/data",
                        b"arxfs",
                        MountFlags::NOSUID.union(MountFlags::NODEV),
                        MountVolumeState {
                            usage: VolumeStats::default(),
                            availability: MountAvailability::Available,
                            medium: None,
                        },
                        [0u8; 16],
                    )
                    .unwrap(),
                ],
                cache_ledgers: fixture_cache_ledgers(),
                live: RefCell::new(alloc::vec::Vec::new()),
            }
        }
    }
    impl SysinfoSource for FixtureSource {
        fn process_records(
            &self,
            _caller: &Caller,
            scope: ProcessScope,
        ) -> Result<alloc::vec::Vec<ProcessRecord>, Errno> {
            Ok(match scope {
                ProcessScope::Caller => self.own.to_vec(),
                ProcessScope::Global => self.global.to_vec(),
            })
        }
        fn kernel_memory_stats(&self, _caller: &Caller) -> Result<KernelMemoryStats, Errno> {
            Ok(KernelMemoryStats {
                total_bytes: 1 << 30,
                free_bytes: 1 << 29,
                kernel_heap_bytes: 4096,
                user_resident_bytes: 1 << 20,
                page_size: 4096,
                reserved: 0,
            })
        }
        fn hardware_tree(&self, _caller: &Caller) -> Result<alloc::vec::Vec<u8>, Errno> {
            Ok(self.hwtree.clone())
        }
        fn user_directory(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<UserDirectoryRecord>, Errno> {
            Ok(alloc::vec![
                UserDirectoryRecord::new(0, b"root").unwrap(),
                UserDirectoryRecord::new(1000, b"alice").unwrap(),
                UserDirectoryRecord::new(1001, b"bob").unwrap(),
            ])
        }
        fn system_identity(&self, _caller: &Caller) -> Result<SystemIdentity, Errno> {
            SystemIdentity::new([9u8; MACHINE_ID_LEN], 1, 0, 0, b"tairix-box")
        }
        fn uptime(&self, _caller: &Caller) -> Result<Uptime, Errno> {
            Ok(Uptime {
                since_boot: Duration64::from_nanos(1_000),
                boot_time: Time64::from_secs(1_700_000_000),
            })
        }
        fn load_average(&self, _caller: &Caller) -> Result<LoadAverage, Errno> {
            Ok(LoadAverage {
                load1: 3 << LOAD_FIXED_SHIFT,
                load5: 2 << LOAD_FIXED_SHIFT,
                load15: 1 << LOAD_FIXED_SHIFT,
                runnable: 3,
                total_tasks: 11,
                users: 2,
            })
        }
        fn mount_records(&self, _caller: &Caller) -> Result<alloc::vec::Vec<MountRecord>, Errno> {
            Ok(self.mounts.to_vec())
        }
        fn cpu_times(&self, _caller: &Caller) -> Result<alloc::vec::Vec<CpuTimeRecord>, Errno> {
            Ok(alloc::vec![
                CpuTimeRecord {
                    cpu: 0,
                    reserved: 0,
                    busy_ns: 750,
                    idle_ns: 250,
                },
                CpuTimeRecord {
                    cpu: 1,
                    reserved: 0,
                    busy_ns: 100,
                    idle_ns: 900,
                },
            ])
        }
        fn memory_pressure(&self, _caller: &Caller) -> Result<MemoryPressureStats, Errno> {
            Ok(fixture_pressure())
        }
        fn memory_pressure_band(&self, _caller: &Caller) -> Result<MemoryPressureBand, Errno> {
            Ok(MemoryPressureBand {
                band: fixture_pressure().band,
                reserved: [0; 7],
            })
        }
        fn memory_total(&self, caller: &Caller) -> Result<MemoryTotal, Errno> {
            // Projected from the gated view's own figure, as a live source
            // must: one machine, one size, whatever the caller holds.
            Ok(MemoryTotal {
                total_bytes: self.kernel_memory_stats(caller)?.total_bytes,
            })
        }
        fn cache_ledger_records(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<CacheLedgerRecord>, Errno> {
            Ok(self.cache_ledgers.clone())
        }
        fn live_process_instances(&self) -> Result<alloc::vec::Vec<ProcId>, Errno> {
            Ok(self.live.borrow().clone())
        }
        fn ramzip_stats(&self, _caller: &Caller) -> Result<RamzipStats, Errno> {
            Ok(fixture_ramzip())
        }
        fn cpu_load(&self, _caller: &Caller) -> Result<alloc::vec::Vec<CpuLoadRecord>, Errno> {
            Ok(fixture_cpu_load())
        }
        fn cpu_info(&self, _caller: &Caller) -> Result<alloc::vec::Vec<CpuInfoRecord>, Errno> {
            Ok(alloc::vec![CpuInfoRecord::new(
                0,
                tairix_abi::sysinfo::CpuCoreClass::Performance,
                tairix_abi::sysinfo::CPU_INFO_FLAG_FREQ_MEASURED,
                0xA5,
                0x410F_D083,
                1_512_000_000,
                54_000_000,
                b"ARM Cortex-A72",
            )
            .unwrap()])
        }
        fn seats(&self, _caller: &Caller) -> Result<alloc::vec::Vec<SeatRecord>, Errno> {
            Ok(alloc::vec![SeatRecord {
                seat_id: 0,
                owner_task: 7,
                generation: 3,
                foreground_console: 1,
                flags: SEAT_FLAG_OWNED,
            }])
        }

        fn net_interface_facts(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<NetInterfaceFactsRecord>, Errno> {
            Ok(alloc::vec![fixture_net_facts()])
        }

        fn net_interface_state(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<NetInterfaceStateRecord>, Errno> {
            Ok(alloc::vec![fixture_net_state()])
        }
        fn net_interface_counters(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<NetInterfaceCountersRecord>, Errno> {
            Ok(alloc::vec![fixture_net_counters()])
        }
        fn net_interface_rates(
            &self,
            _caller: &Caller,
            window: tairix_abi::time::Duration64,
        ) -> Result<alloc::vec::Vec<NetInterfaceRatesRecord>, Errno> {
            Ok(alloc::vec![fixture_net_rates(window)])
        }
        fn net_sockets(&self, _caller: &Caller) -> Result<alloc::vec::Vec<NetSocketRecord>, Errno> {
            Ok(alloc::vec![fixture_net_socket()])
        }
        fn net_bond_members(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<NetBondMemberRecord>, Errno> {
            Ok(alloc::vec![fixture_net_bond_member()])
        }
        fn net_resolver_servers(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<NetResolverServer>, Errno> {
            Ok(fixture_resolver_servers())
        }
        fn irqs(&self, _caller: &Caller) -> Result<alloc::vec::Vec<IrqRecord>, Errno> {
            Ok(alloc::vec![
                IrqRecord {
                    line: 27,
                    flags: 0,
                    owner: 14,
                    count: 1234,
                },
                IrqRecord {
                    line: 111,
                    flags: IRQ_FLAG_QUARANTINED,
                    owner: 13,
                    count: 200_000,
                },
            ])
        }
        fn crashes(&self, _caller: &Caller) -> Result<alloc::vec::Vec<CrashRecord>, Errno> {
            Ok(alloc::vec![
                fixture_crash(2, b"crasher"),
                fixture_crash(3, b"other"),
            ])
        }
        fn volume_io_health(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<VolumeIoHealthRecord>, Errno> {
            Ok(alloc::vec![
                VolumeIoHealthRecord::new(
                    [0xAA; 16],
                    0x5953_2001,
                    MountAvailability::Available,
                    BlkHealthCounters {
                        completions: 2048,
                        ok: 2048,
                        ..BlkHealthCounters::default()
                    },
                ),
                VolumeIoHealthRecord::new(
                    [0xBB; 16],
                    0x5953_2002,
                    MountAvailability::Recovering,
                    BlkHealthCounters {
                        completions: 130,
                        ok: 100,
                        resets: 30,
                        reissues: 12,
                        ..BlkHealthCounters::default()
                    },
                ),
            ])
        }
        fn raid_arrays(&self, _caller: &Caller) -> Result<alloc::vec::Vec<RaidArrayRecord>, Errno> {
            Ok(fixture_raid_arrays())
        }
        fn raid_members(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<RaidMemberRecord>, Errno> {
            Ok(fixture_raid_members())
        }
        fn resource_limits(
            &self,
            _caller: &Caller,
        ) -> Result<[ResourceLimitRecord; LimitKind::COUNT], Errno> {
            // A distinct usage per kind so the positional decode is checkable.
            Ok([
                ResourceLimitRecord::new(
                    LimitKind::AddressSpaceBytes,
                    ResourceLimit::new(1 << 20, 1 << 21).unwrap(),
                    4096,
                ),
                ResourceLimitRecord::new(
                    LimitKind::OpenStreams,
                    ResourceLimit::new(4, 8).unwrap(),
                    3,
                ),
                ResourceLimitRecord::new(LimitKind::Processes, ResourceLimit::UNLIMITED, 2),
                ResourceLimitRecord::new(
                    LimitKind::StackBytes,
                    ResourceLimit::new(64 * 1024, 64 * 1024).unwrap(),
                    0,
                ),
                ResourceLimitRecord::new(
                    LimitKind::PinnedMemoryBytes,
                    ResourceLimit::new(1 << 20, 1 << 20).unwrap(),
                    0,
                ),
                ResourceLimitRecord::new(
                    LimitKind::Threads,
                    ResourceLimit::new(16, 64).unwrap(),
                    1,
                ),
            ])
        }
    }

    fn request_bytes(query: SysinfoQueryId, payload: &[u8]) -> [u8; 64] {
        let header = SysinfoRequestHeader {
            magic: SYSINFO_REQUEST_MAGIC,
            version: SYSINFO_VERSION_CURRENT,
            flags: 0,
            query,
            reserved: 0,
            payload_len: u32::try_from(payload.len()).unwrap(),
            request_id: 7,
        };
        let mut buf = [0u8; 64];
        let head = header.to_le_bytes();
        buf[..head.len()].copy_from_slice(&head);
        buf[head.len()..head.len() + payload.len()].copy_from_slice(payload);
        buf
    }

    /// The representative attested caller most tests use: the dispatcher
    /// gates on its capability summary and scopes by its uid, both read
    /// from the attested origin the shared fixture mints.
    fn caller(caps: &Caps) -> Caller {
        user_caller(caps.0, 0x10, 10)
    }

    /// Call [`serve`] against a fresh, generously-sized registry.
    ///
    /// Every pre-existing single-request test uses this: it never reports
    /// a cache or looks at reporter state, so a registry that lives no
    /// longer than the one call is equivalent to a persistent one. The
    /// reporter-specific tests below thread their own registry explicitly
    /// across several calls instead of using this helper.
    fn serve_once(
        source: &dyn SysinfoSource,
        caller: &Caller,
        sink: &dyn Sink,
        request: &[u8],
        response: &mut [u8],
    ) -> Result<usize, Errno> {
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        serve(source, caller, &mut registry, sink, request, response)
    }

    /// Like [`request_bytes`], but returned as an owned, exactly-sized
    /// buffer rather than padded into a fixed 64-byte array — needed for a
    /// [`SysinfoQueryId::CACHE_REPORT`] payload, which can carry up to
    /// [`MAX_CACHE_REPORT_ENTRIES`] whole [`CacheLedgerRecord`]s and so can
    /// exceed 64 bytes many times over.
    fn request_bytes_vec(query: SysinfoQueryId, payload: &[u8]) -> alloc::vec::Vec<u8> {
        let header = SysinfoRequestHeader {
            magic: SYSINFO_REQUEST_MAGIC,
            version: SYSINFO_VERSION_CURRENT,
            flags: 0,
            query,
            reserved: 0,
            payload_len: u32::try_from(payload.len()).unwrap(),
            request_id: 7,
        };
        let mut buf = alloc::vec::Vec::new();
        buf.extend_from_slice(&header.to_le_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    /// A [`CacheLedgerRecord`] with `origin`/`reporter_pid` left at the
    /// caller-submittable defaults and `payload_bytes` set to
    /// `resident_bytes`, ready to encode into a
    /// [`SysinfoQueryId::CACHE_REPORT`] payload.
    fn report_row(label: &str, class: u8, resident_bytes: u64) -> CacheLedgerRecord {
        let mut row =
            CacheLedgerRecord::new(label.as_bytes(), CacheOwnerKind::UserlandProcess, 0, class)
                .expect("valid label");
        row.payload_bytes = resident_bytes;
        row
    }

    /// Encode a [`CacheReportRequest`] header followed by `rows`.
    fn cache_report_payload(rows: &[CacheLedgerRecord]) -> alloc::vec::Vec<u8> {
        let header = CacheReportRequest {
            count: u16::try_from(rows.len()).unwrap(),
            flags: 0,
            reserved: 0,
        };
        let mut payload = alloc::vec::Vec::new();
        payload.extend_from_slice(&header.to_le_bytes());
        for row in rows {
            payload.extend_from_slice(&row.to_le_bytes());
        }
        payload
    }

    #[test]
    fn process_identity_returns_the_callers_attested_origin() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let who = caller(&caps);
        let expected = *who.origin();
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::PROCESS_IDENTITY, &[]);
        let mut resp = [0u8; ORIGIN_WIRE_LEN];
        let n = serve_once(&source, &who, &sink, &req, &mut resp).expect("served");
        assert_eq!(n, ORIGIN_WIRE_LEN);
        let decoded = Origin::from_bytes(&resp[..n]).expect("valid origin");
        assert_eq!(decoded, expected);
        assert_eq!(decoded.uid(), 1000);
        assert_eq!(decoded.trust_domain(), TrustDomain::User);
        assert!(decoded
            .capabilities()
            .holds_cap(CapabilityId::SYSINFO_GLOBAL));
    }

    #[test]
    fn self_process_list_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let plr = ProcessListRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::SELF_PROCESS_LIST, &plr.to_le_bytes());
        let mut resp = [0u8; 256];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * ProcessRecord::WIRE_LEN);
        let first = ProcessRecord::from_bytes(&resp[..ProcessRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.name_bytes(), b"shell");
        assert_eq!(first.io_bytes_read, 4096);
        assert_eq!(first.io_bytes_written, 512);
        // Self-scoped queries are not audited.
        assert!(sink.events.borrow().as_slice().is_empty());
    }

    /// The per-process I/O counters survive the service's re-serialisation
    /// in the system-wide view too, saturated value included, and the
    /// capability gate on that view is unchanged.
    #[test]
    fn global_process_list_carries_the_io_counters() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let plr = ProcessListRequest {
            offset: 0,
            limit: 3,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::GLOBAL_PROCESS_LIST, &plr.to_le_bytes());
        let mut resp = [0u8; 512];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 3 * ProcessRecord::WIRE_LEN);
        let init = ProcessRecord::from_bytes(&resp[..ProcessRecord::WIRE_LEN]).unwrap();
        assert_eq!(init.name_bytes(), b"init");
        assert_eq!(init.io_bytes_read, u64::MAX);
        assert_eq!(init.io_bytes_written, u64::MAX);
        let editor = ProcessRecord::from_bytes(&resp[2 * ProcessRecord::WIRE_LEN..]).unwrap();
        assert_eq!(editor.name_bytes(), b"editor");
        assert_eq!(editor.io_bytes_read, 8192);
        assert_eq!(editor.io_bytes_written, 1024);
    }

    #[test]
    fn process_list_paging_offset_and_limit() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let plr = ProcessListRequest {
            offset: 1,
            limit: 1,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::GLOBAL_PROCESS_LIST, &plr.to_le_bytes());
        let mut resp = [0u8; 256];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, ProcessRecord::WIRE_LEN);
        let rec = ProcessRecord::from_bytes(&resp[..ProcessRecord::WIRE_LEN]).unwrap();
        assert_eq!(rec.name_bytes(), b"shell");
        // Offset past the end returns an empty page.
        let plr_end = ProcessListRequest {
            offset: 99,
            limit: 10,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::GLOBAL_PROCESS_LIST, &plr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn global_process_list_denied_without_capability() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let plr = ProcessListRequest::default();
        let req = request_bytes(SysinfoQueryId::GLOBAL_PROCESS_LIST, &plr.to_le_bytes());
        let mut resp = [0u8; 256];
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        let events = sink.events.borrow();
        assert_eq!(events.as_slice(), &[(Level::Warn, events::QUERY_DENIED)]);
    }

    #[test]
    fn audited_query_emits_served_record() {
        // The served record is `Debug` (below the default `Info` filter),
        // so widen the global filter to observe it.
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::KERNEL_MEMORY_STATS, &[]);
        let mut resp = [0u8; 64];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, KernelMemoryStats::WIRE_LEN);
        let stats = KernelMemoryStats::from_bytes(&resp).unwrap();
        assert_eq!(stats.page_size, 4096);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );
    }

    /// Decode one hardware-tree reply page: the repeated snapshot header
    /// and the whole node records that follow it.
    fn decode_tree_page(reply: &[u8]) -> (HwTreeHeader, alloc::vec::Vec<HwNode>) {
        let header = HwTreeHeader::from_bytes(reply).unwrap();
        let body = &reply[HwTreeHeader::WIRE_LEN..];
        assert_eq!(body.len() % HwNode::WIRE_LEN, 0);
        let nodes = body
            .as_chunks::<{ HwNode::WIRE_LEN }>()
            .0
            .iter()
            .map(|chunk| HwNode::from_bytes(chunk).unwrap())
            .collect();
        (header, nodes)
    }

    #[test]
    fn hardware_tree_is_gated_and_pages() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let page = |offset: u32, limit: u16| {
            request_bytes(
                SysinfoQueryId::HARDWARE_TREE,
                &HardwareTreeRequest {
                    offset,
                    limit,
                    flags: 0,
                }
                .to_le_bytes(),
            )
        };
        let mut resp = [0u8; HwTreeHeader::WIRE_LEN + 2 * HwNode::WIRE_LEN];

        let denied = Caps(&[]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &page(0, 2), &mut resp),
            Err(Errno::PermissionDenied)
        );

        // Page 1: the snapshot header plus the first two of three nodes.
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let n = serve_once(&source, &caller(&granted), &sink, &page(0, 2), &mut resp).unwrap();
        let (header, nodes) = decode_tree_page(&resp[..n]);
        assert_eq!(header.generation(), 7);
        assert_eq!(header.node_count(), 3);
        assert_eq!(nodes, tree_nodes()[..2]);

        // Page 2: the same header, the final node.
        let n = serve_once(&source, &caller(&granted), &sink, &page(2, 2), &mut resp).unwrap();
        let (header, nodes) = decode_tree_page(&resp[..n]);
        assert_eq!(header.node_count(), 3);
        assert_eq!(nodes, tree_nodes()[2..]);

        // Past the end: the header alone, no records.
        let n = serve_once(&source, &caller(&granted), &sink, &page(3, 2), &mut resp).unwrap();
        let (_, nodes) = decode_tree_page(&resp[..n]);
        assert!(nodes.is_empty());
    }

    #[test]
    fn hardware_tree_larger_than_one_reply_serves_across_pages() {
        // A tree far larger than one framed reply window: the regression
        // that used to fail closed with `BufferTooSmall` instead of paging.
        let nodes: alloc::vec::Vec<HwNode> = (0..40)
            .map(|id| {
                HwNode::new(
                    id,
                    if id == 0 { HW_NODE_ROOT } else { 0 },
                    HwDeviceClass::Other,
                )
            })
            .collect();
        let mut source = FixtureSource::new();
        source.hwtree = tree_blob(9, &nodes);
        assert!(source.hwtree.len() > SYSINFO_MAX_REPLY);

        let sink = RecordingSink::new();
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let limit = u16::try_from(
            (SYSINFO_MAX_REPLY - SYSINFO_REPLY_STATUS_LEN - HwTreeHeader::WIRE_LEN)
                / HwNode::WIRE_LEN,
        )
        .unwrap();
        let mut resp = [0u8; SYSINFO_MAX_REPLY - SYSINFO_REPLY_STATUS_LEN];
        let mut walked: alloc::vec::Vec<HwNode> = alloc::vec::Vec::new();
        loop {
            let req = request_bytes(
                SysinfoQueryId::HARDWARE_TREE,
                &HardwareTreeRequest {
                    offset: u32::try_from(walked.len()).unwrap(),
                    limit,
                    flags: 0,
                }
                .to_le_bytes(),
            );
            let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
            let (header, page) = decode_tree_page(&resp[..n]);
            assert_eq!(header.generation(), 9);
            assert_eq!(header.node_count(), nodes.len() as u64);
            if page.is_empty() {
                break;
            }
            walked.extend_from_slice(&page);
        }
        assert_eq!(walked, nodes);
    }

    #[test]
    fn hardware_tree_malformed_snapshot_fails_closed() {
        // A snapshot whose header claims more nodes than its body carries
        // is refused whole, never paged.
        let mut source = FixtureSource::new();
        source.hwtree = tree_blob(1, &tree_nodes());
        source.hwtree.pop();
        let sink = RecordingSink::new();
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let req = request_bytes(
            SysinfoQueryId::HARDWARE_TREE,
            &HardwareTreeRequest {
                offset: 0,
                limit: 4,
                flags: 0,
            }
            .to_le_bytes(),
        );
        let mut resp = [0u8; 4096];
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn ungated_scalar_queries_round_trip() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();

        let req = request_bytes(SysinfoQueryId::UPTIME, &[]);
        let mut resp = [0u8; 64];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        let up = Uptime::from_bytes(&resp[..n]).unwrap();
        assert_eq!(up.since_boot, Duration64::from_nanos(1_000));
        assert_eq!(up.boot_time, Time64::from_secs(1_700_000_000));

        let req = request_bytes(SysinfoQueryId::SYSTEM_IDENTITY, &[]);
        let mut resp = [0u8; 128];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        let id = SystemIdentity::from_bytes(&resp[..n]).unwrap();
        assert_eq!(id.hostname_bytes(), b"tairix-box");
        // Neither query is audited.
        assert!(sink.events.borrow().as_slice().is_empty());
    }

    #[test]
    fn load_average_needs_no_capability_and_round_trips() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::LOAD_AVERAGE, &[]);
        let mut resp = [0u8; 64];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        let load = LoadAverage::from_bytes(&resp[..n]).unwrap();
        assert_eq!(LoadAverage::whole(load.load1), 3);
        assert_eq!(load.runnable, 3);
        assert_eq!(load.total_tasks, 11);
        assert_eq!(load.users, 2);
        // System-wide and secret-free, so unaudited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Fails closed when the response buffer cannot hold the record.
        let mut tiny = [0u8; LoadAverage::WIRE_LEN - 1];
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req, &mut tiny),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn resource_limits_needs_no_capability_and_round_trips() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::RESOURCE_LIMITS, &[]);
        let mut resp = [0u8; 256];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, RESOURCE_LIMITS_REPORT_LEN);
        // The records decode positionally, one per LimitKind in order.
        for (index, kind) in LimitKind::ALL.iter().enumerate() {
            let base = index * ResourceLimitRecord::WIRE_LEN;
            let rec =
                ResourceLimitRecord::from_bytes(&resp[base..base + ResourceLimitRecord::WIRE_LEN])
                    .unwrap();
            assert_eq!(rec.kind, *kind);
        }
        let first =
            ResourceLimitRecord::from_bytes(&resp[..ResourceLimitRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.kind, LimitKind::AddressSpaceBytes);
        assert_eq!(first.limit, ResourceLimit::new(1 << 20, 1 << 21).unwrap());
        assert_eq!(first.usage, 4096);
        // Self-scoped, so unaudited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Fails closed when the response buffer cannot hold the report.
        let mut tiny = [0u8; RESOURCE_LIMITS_REPORT_LEN - 1];
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req, &mut tiny),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn user_directory_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let udr = UserDirectoryRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::USER_DIRECTORY, &udr.to_le_bytes());
        let mut resp = [0u8; 1024];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 3 * UserDirectoryRecord::WIRE_LEN);
        let first =
            UserDirectoryRecord::from_bytes(&resp[..UserDirectoryRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.uid, 0);
        assert_eq!(first.name_bytes(), b"root");
        // Secret-free and self-evidently public, so unaudited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Paging: a window starting past the first record returns the tail.
        let udr_tail = UserDirectoryRequest {
            offset: 2,
            limit: 10,
            flags: 0,
        };
        let req_tail = request_bytes(SysinfoQueryId::USER_DIRECTORY, &udr_tail.to_le_bytes());
        let n = serve_once(&source, &caller(&caps), &sink, &req_tail, &mut resp).unwrap();
        assert_eq!(n, UserDirectoryRecord::WIRE_LEN);
        let tail = UserDirectoryRecord::from_bytes(&resp[..UserDirectoryRecord::WIRE_LEN]).unwrap();
        assert_eq!(tail.name_bytes(), b"bob");

        // Paging past the end returns an empty page (the terminator).
        let udr_end = UserDirectoryRequest {
            offset: 9,
            limit: 10,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::USER_DIRECTORY, &udr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn mount_list_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let mlr = MountListRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::MOUNT_LIST, &mlr.to_le_bytes());
        let mut resp = [0u8; 1024];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * MountRecord::WIRE_LEN);
        let first = MountRecord::from_bytes(&resp[..MountRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.target_bytes(), b"/");
        assert!(first.flags().contains(MountFlags::READ_ONLY));
        // The medium the source reported is relayed unchanged, and a volume
        // whose backing medium is unknown stays unknown rather than guessed.
        assert_eq!(first.medium(), Some(BlkDeviceClass::SolidState));
        let second =
            MountRecord::from_bytes(&resp[MountRecord::WIRE_LEN..2 * MountRecord::WIRE_LEN])
                .unwrap();
        assert_eq!(second.medium(), None);
        // The mount table is not audited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Paging past the end returns an empty page.
        let mlr_end = MountListRequest {
            offset: 9,
            limit: 4,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::MOUNT_LIST, &mlr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn cpu_time_stats_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let ctr = CpuTimeListRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::CPU_TIME_STATS, &ctr.to_le_bytes());
        let mut resp = [0u8; 256];
        let n = serve_once(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * CpuTimeRecord::WIRE_LEN);
        let first = CpuTimeRecord::from_bytes(&resp[..CpuTimeRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.cpu, 0);
        assert_eq!(first.busy_ns, 750);
        assert_eq!(first.idle_ns, 250);
        // The utilisation figures are not audited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Paging past the end returns an empty page.
        let ctr_end = CpuTimeListRequest {
            offset: 5,
            limit: 4,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::CPU_TIME_STATS, &ctr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    /// The interface-facts record the fixture serves.
    fn fixture_net_facts() -> NetInterfaceFactsRecord {
        let mut name = [0u8; tairix_abi::net_ipc::IF_NAME_LEN];
        name[..3].copy_from_slice(b"wan");
        NetInterfaceFactsRecord {
            name,
            kind: tairix_abi::net_ipc::NetIfKind::Ethernet,
            mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
            mtu: 1500,
            offloads: 0,
            rx_queues: 1,
        }
    }

    /// The interface-state record the fixture serves.
    fn fixture_net_state() -> NetInterfaceStateRecord {
        let mut name = [0u8; tairix_abi::net_ipc::IF_NAME_LEN];
        name[..3].copy_from_slice(b"wan");
        let mut addrs =
            [NetInterfaceStateRecord::EMPTY_ADDR; tairix_abi::net_ipc::NET_IF_MAX_ADDRS];
        addrs[0] = tairix_abi::net_ipc::NetIfAddr {
            family: tairix_abi::net_ipc::NetAddrFamily::V4,
            prefix: 24,
            state: tairix_abi::net_ipc::NetAddrState::Preferred,
            addr: {
                let mut a = [0u8; 16];
                a[..4].copy_from_slice(&[10, 0, 2, 15]);
                a
            },
        };
        NetInterfaceStateRecord {
            name,
            link_up: true,
            addr_count: 1,
            addrs,
        }
    }

    /// The interface-counters record the fixture serves.
    fn fixture_net_counters() -> NetInterfaceCountersRecord {
        let mut name = [0u8; tairix_abi::net_ipc::IF_NAME_LEN];
        name[..3].copy_from_slice(b"wan");
        NetInterfaceCountersRecord {
            name,
            counters: tairix_abi::net_ipc::NetCounters {
                rx_frames: 128,
                rx_bytes: 190_000,
                rx_dropped: 2,
                tx_frames: 96,
                tx_bytes: 140_000,
                icmp_errors_sent: 1,
                icmp_errors_suppressed: 0,
                reassembly_expired: 0,
                pending_dropped: 0,
            },
        }
    }

    /// The interface-rates record the fixture serves; it echoes the
    /// requested window so a test can confirm the window threaded through.
    fn fixture_net_rates(window: Duration64) -> NetInterfaceRatesRecord {
        let mut name = [0u8; tairix_abi::net_ipc::IF_NAME_LEN];
        name[..3].copy_from_slice(b"wan");
        NetInterfaceRatesRecord {
            name,
            window,
            rx_pps: 1000,
            rx_bps: 12_000_000,
            tx_pps: 800,
            tx_bps: 9_600_000,
        }
    }

    /// The socket-listing record the fixture serves.
    fn fixture_net_socket() -> NetSocketRecord {
        let mut local = [0u8; 16];
        local[..4].copy_from_slice(&[10, 0, 2, 15]);
        let mut peer = [0u8; 16];
        peer[..4].copy_from_slice(&[10, 0, 2, 2]);
        NetSocketRecord {
            proto: NetSockProto::Tcp,
            state: NetSockState::Established,
            family: tairix_abi::net_ipc::NetAddrFamily::V4,
            local_addr: local,
            local_port: 4321,
            peer_addr: peer,
            peer_port: 80,
            owner: 42,
            recv_q: 0,
            send_q: 128,
        }
    }

    /// The active resolver-server set the fixture serves: one V4 and one
    /// V6 recursive server.
    fn fixture_resolver_servers() -> alloc::vec::Vec<NetResolverServer> {
        let mut v4 = [0u8; 16];
        v4[..4].copy_from_slice(&[10, 0, 2, 3]);
        alloc::vec![
            NetResolverServer {
                family: tairix_abi::net_ipc::NetAddrFamily::V4,
                addr: v4,
            },
            NetResolverServer {
                family: tairix_abi::net_ipc::NetAddrFamily::V6,
                addr: [0x26; 16],
            },
        ]
    }

    /// The bond-member record the fixture serves.
    fn fixture_net_bond_member() -> NetBondMemberRecord {
        let mut bond = [0u8; tairix_abi::net_ipc::IF_NAME_LEN];
        bond[..5].copy_from_slice(b"bond0");
        let mut member = [0u8; tairix_abi::net_ipc::IF_NAME_LEN];
        member[..4].copy_from_slice(b"eth1");
        NetBondMemberRecord {
            bond,
            member,
            active: true,
            link_up: true,
            eligible: true,
        }
    }

    #[test]
    fn net_bond_members_is_gated_audited_and_round_trips() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let nlr = NetInterfaceListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::NET_BOND_MEMBERS, &nlr.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_GLOBAL`; the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_GLOBAL` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, NetBondMemberRecord::WIRE_LEN);
        let record = NetBondMemberRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record, fixture_net_bond_member());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );
    }

    #[test]
    fn net_sockets_is_gated_audited_and_round_trips() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let nlr = NetInterfaceListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::NET_SOCKETS, &nlr.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_GLOBAL`; the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_GLOBAL` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, NetSocketRecord::WIRE_LEN);
        let record = NetSocketRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record, fixture_net_socket());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );
    }

    #[test]
    fn net_resolver_servers_is_ungated_and_round_trips() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let nlr = NetInterfaceListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::NET_RESOLVER_SERVERS, &nlr.to_le_bytes());
        let mut resp = [0u8; 512];

        // Served with *no* capability: the recursive resolver set is public
        // host configuration (the resolv.conf analogue), and — being
        // ungated — emits no audit record.
        let none = Caps(&[]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&none), &sink, &req, &mut resp).unwrap();
        let expected = fixture_resolver_servers();
        assert_eq!(n, expected.len() * NetResolverServer::WIRE_LEN);
        for (index, want) in expected.iter().enumerate() {
            let base = index * NetResolverServer::WIRE_LEN;
            let got = NetResolverServer::from_bytes(&resp[base..]).unwrap();
            assert_eq!(&got, want);
        }
        assert!(
            sink.events.borrow().as_slice().is_empty(),
            "an ungated read emits no audit record"
        );
    }

    #[test]
    fn net_interface_rates_is_gated_audited_and_threads_the_window() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let window = Duration64::from_secs(1);
        let nrr = NetInterfaceRatesRequest {
            offset: 0,
            limit: 8,
            flags: 0,
            window,
        };
        let req = request_bytes(SysinfoQueryId::NET_INTERFACE_RATES, &nrr.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_GLOBAL`; the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_GLOBAL` holder; the
        // requested window threads through to the record.
        let granted = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, NetInterfaceRatesRecord::WIRE_LEN);
        let record = NetInterfaceRatesRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record, fixture_net_rates(window));
        assert_eq!(record.window, window);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );
    }

    #[test]
    fn net_interface_counters_is_gated_audited_and_round_trips() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let nlr = NetInterfaceListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::NET_INTERFACE_COUNTERS, &nlr.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_GLOBAL`; the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_HW]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_GLOBAL` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, NetInterfaceCountersRecord::WIRE_LEN);
        let record = NetInterfaceCountersRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record, fixture_net_counters());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );
    }

    #[test]
    fn net_interface_facts_is_gated_audited_and_pages() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let nlr = NetInterfaceListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::NET_INTERFACE_FACTS, &nlr.to_le_bytes());
        let mut resp = [0u8; 256];

        // Denied without `CAP_SYSINFO_HW`; the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_HW` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, NetInterfaceFactsRecord::WIRE_LEN);
        let record = NetInterfaceFactsRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record, fixture_net_facts());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );

        // Paging past the single interface returns the empty terminator.
        let nlr_end = NetInterfaceListRequest {
            offset: 1,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::NET_INTERFACE_FACTS, &nlr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn net_interface_state_is_gated_audited_and_round_trips() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let nlr = NetInterfaceListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::NET_INTERFACE_STATE, &nlr.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_GLOBAL`; the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_HW]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_GLOBAL` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, NetInterfaceStateRecord::WIRE_LEN);
        let record = NetInterfaceStateRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record, fixture_net_state());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );
    }

    #[test]
    fn seat_list_is_gated_audited_and_pages() {
        // The served record is `Debug` (below the default `Info` filter),
        // so widen the global filter to observe it.
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let slr = SeatListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::SEAT_LIST, &slr.to_le_bytes());
        let mut resp = [0u8; 256];

        // Denied without `CAP_SYSINFO_HW`; the refusal is logged.
        let denied = Caps(&[]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_HW` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, SeatRecord::WIRE_LEN);
        let record = SeatRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record.seat_id, 0);
        assert_eq!(record.owner(), Some(7));
        assert_eq!(record.generation, 3);
        assert_eq!(record.foreground_console, 1);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );

        // Paging past the single seat returns the empty terminator.
        let slr_end = SeatListRequest {
            offset: 1,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::SEAT_LIST, &slr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn irq_list_is_gated_audited_and_pages() {
        // The served record is `Debug` (below the default `Info` filter),
        // so widen the global filter to observe it.
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let request = IrqListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::IRQ_LIST, &request.to_le_bytes());
        let mut resp = [0u8; 256];

        // Denied without `CAP_SYSINFO_HW`; the refusal is logged.
        let denied = Caps(&[]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_HW` holder: both lines,
        // in order, with counts, owners, and the quarantine flag intact.
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * IrqRecord::WIRE_LEN);
        let first = IrqRecord::from_bytes(&resp[..IrqRecord::WIRE_LEN]).unwrap();
        let second = IrqRecord::from_bytes(&resp[IrqRecord::WIRE_LEN..n]).unwrap();
        assert_eq!(first.line, 27);
        assert_eq!(first.owner, 14);
        assert_eq!(first.count, 1234);
        assert!(!first.is_quarantined());
        assert_eq!(second.line, 111);
        assert_eq!(second.owner, 13);
        assert!(second.is_quarantined());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );

        // Paging past the last line returns the empty terminator.
        let end = IrqListRequest {
            offset: 2,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::IRQ_LIST, &end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn volume_io_health_is_gated_on_kernel_audited_and_pages() {
        // The served record is `Debug` (below the default `Info` filter),
        // so widen the global filter to observe it.
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let request = VolumeIoHealthRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::VOLUME_IO_HEALTH, &request.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_KERNEL` (the per-device tallies are
        // kernel operational state); the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_HW]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_KERNEL` holder: both
        // volumes, in order, with their availability and folded counters.
        let granted = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * VolumeIoHealthRecord::WIRE_LEN);
        let first = VolumeIoHealthRecord::from_bytes(&resp[..VolumeIoHealthRecord::WIRE_LEN])
            .expect("decode first");
        let second = VolumeIoHealthRecord::from_bytes(&resp[VolumeIoHealthRecord::WIRE_LEN..n])
            .expect("decode second");
        assert_eq!(first.dev(), 0x5953_2001);
        assert_eq!(first.availability(), MountAvailability::Available);
        assert_eq!(first.counters().completions, 2048);
        assert_eq!(second.dev(), 0x5953_2002);
        assert_eq!(second.availability(), MountAvailability::Recovering);
        assert_eq!(second.counters().resets, 30);
        assert_eq!(second.counters().reissues, 12);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );

        // Paging past the last volume returns the empty terminator.
        let end = VolumeIoHealthRequest {
            offset: 2,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::VOLUME_IO_HEALTH, &end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn raid_arrays_is_gated_on_hw_audited_and_pages() {
        // The served record is `Debug` (below the default `Info` filter),
        // so widen the global filter to observe it.
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let request = RaidListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::RAID_ARRAYS, &request.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_HW` — the composition of the machine's
        // storage is read under the same authority as the hardware tree — and
        // the refusal is logged. Nothing is served, so no call ever goes out
        // to the composer.
        let denied = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_HW` holder: both arrays,
        // in order, with level, health, and the rebuild flag intact.
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * RaidArrayRecord::WIRE_LEN);
        let first =
            RaidArrayRecord::from_bytes(&resp[..RaidArrayRecord::WIRE_LEN]).expect("decode first");
        let second = RaidArrayRecord::from_bytes(&resp[RaidArrayRecord::WIRE_LEN..n])
            .expect("decode second");
        assert_eq!(first.array(), [0x11; 16]);
        assert_eq!(first.level(), RaidLevel::Mirror);
        assert_eq!(first.health(), ArrayHealth::Optimal);
        assert!(!first.resyncing());
        assert_eq!(first.active_members(), 2);
        assert_eq!(second.array(), [0x22; 16]);
        assert_eq!(second.level(), RaidLevel::Parity);
        assert_eq!(second.health(), ArrayHealth::Recovering);
        assert!(second.resyncing());
        assert_eq!(second.resync_cursor(), 640_000);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );

        // A window inside the list is served from its offset.
        let middle = RaidListRequest {
            offset: 1,
            limit: 8,
            flags: 0,
        };
        let req_middle = request_bytes(SysinfoQueryId::RAID_ARRAYS, &middle.to_le_bytes());
        let n = serve_once(&source, &caller(&granted), &sink, &req_middle, &mut resp).unwrap();
        assert_eq!(n, RaidArrayRecord::WIRE_LEN);
        assert_eq!(
            RaidArrayRecord::from_bytes(&resp[..n])
                .expect("decode window")
                .array(),
            [0x22; 16]
        );

        // Paging past the last array returns the empty terminator.
        let end = RaidListRequest {
            offset: 2,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::RAID_ARRAYS, &end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn raid_members_is_gated_on_hw_audited_and_pages() {
        // The served record is `Debug` (below the default `Info` filter),
        // so widen the global filter to observe it.
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let request = RaidListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::RAID_MEMBERS, &request.to_le_bytes());
        let mut resp = [0u8; 512];

        // Denied without `CAP_SYSINFO_HW`; the refusal is logged and no call
        // reaches the composer.
        let denied = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_HW` holder: every device
        // the composer holds, with its disposition, slot, and affiliation.
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 3 * RaidMemberRecord::WIRE_LEN);
        let decoded: alloc::vec::Vec<RaidMemberRecord> = resp[..n]
            .chunks(RaidMemberRecord::WIRE_LEN)
            .map(|chunk| RaidMemberRecord::from_bytes(chunk).expect("decode member"))
            .collect();
        assert_eq!(decoded[0].disposition(), RaidMemberDisposition::InSync);
        assert_eq!(decoded[0].slot(), 0);
        assert_eq!(decoded[0].node(), 50);
        assert!(!decoded[0].is_unaffiliated());
        assert_eq!(decoded[1].disposition(), RaidMemberDisposition::Resyncing);
        assert_eq!(decoded[1].slot(), 1);
        assert_eq!(decoded[2].disposition(), RaidMemberDisposition::Candidate);
        assert_eq!(decoded[2].slot(), RAID_SLOT_NONE);
        assert!(decoded[2].is_unaffiliated());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );

        // A bounded window is served from its offset: the second page of one.
        let middle = RaidListRequest {
            offset: 1,
            limit: 1,
            flags: 0,
        };
        let req_middle = request_bytes(SysinfoQueryId::RAID_MEMBERS, &middle.to_le_bytes());
        let n = serve_once(&source, &caller(&granted), &sink, &req_middle, &mut resp).unwrap();
        assert_eq!(n, RaidMemberRecord::WIRE_LEN);
        assert_eq!(
            RaidMemberRecord::from_bytes(&resp[..n])
                .expect("decode window")
                .slot(),
            1
        );

        // Paging past the last device returns the empty terminator.
        let end = RaidListRequest {
            offset: 3,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::RAID_MEMBERS, &end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn crash_record_is_gated_on_kernel_audited_and_pages() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let request = CrashRecordRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::CRASH_RECORD, &request.to_le_bytes());
        let mut resp = [0u8; 4096];

        // Denied without `CAP_SYSINFO_KERNEL` (the record carries absolute
        // register values); the refusal is logged.
        let denied = Caps(&[CapabilityId::SYSINFO_HW]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_KERNEL` holder: both
        // records, newest first, with their identity and load-relative pc.
        let granted = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * CrashRecord::WIRE_LEN);
        let first = CrashRecord::from_bytes(&resp[..CrashRecord::WIRE_LEN]).unwrap();
        let second = CrashRecord::from_bytes(&resp[CrashRecord::WIRE_LEN..n]).unwrap();
        assert_eq!(first.pid, 2);
        assert_eq!(first.name_bytes(), b"crasher");
        assert!(first.is_write());
        assert!(first.load_base_known());
        assert_eq!(first.pc, 0x40);
        assert_eq!(first.frames(), &[0x40, 0x120]);
        assert_eq!(second.pid, 3);
        assert_eq!(second.name_bytes(), b"other");
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );

        // Paging past the last record returns the empty terminator.
        let end = CrashRecordRequest {
            offset: 2,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::CRASH_RECORD, &end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn memory_pressure_is_gated_audited_and_round_trips() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let req = request_bytes(SysinfoQueryId::MEMORY_PRESSURE, &[]);
        let mut resp = [0u8; 256];

        // Denied without `CAP_SYSINFO_KERNEL`; the refusal is logged.
        let sink = RecordingSink::new();
        let denied = Caps(&[]);
        assert_eq!(
            serve_once(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_KERNEL` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let sink = RecordingSink::new();
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, MemoryPressureStats::WIRE_LEN);
        let decoded = MemoryPressureStats::from_bytes(&resp[..n]).unwrap();
        assert_eq!(decoded, fixture_pressure());
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Debug, events::QUERY_SERVED)]
        );
    }

    #[test]
    fn the_memory_pressure_band_is_ungated_unaudited_and_matches_the_gated_view() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let req = request_bytes(SysinfoQueryId::MEMORY_PRESSURE_BAND, &[]);
        let mut resp = [0u8; 256];

        // No capability at all: a process must be able to learn that the
        // machine is short of memory in order to give its caches back.
        let sink = RecordingSink::new();
        let none = Caps(&[]);
        let n = serve_once(&source, &caller(&none), &sink, &req, &mut resp).expect("ungated");
        assert_eq!(n, MemoryPressureBand::WIRE_LEN);
        let decoded = MemoryPressureBand::from_bytes(&resp[..n]).expect("round trip");

        // The same band the gated detailed view reports: one gauge, two
        // views, never two notions of pressure.
        assert_eq!(decoded.band, fixture_pressure().band);

        // Unaudited: an ungated query a process may issue on every band
        // change must not be able to drive the security log.
        assert!(sink.events.borrow().as_slice().is_empty());
    }

    #[test]
    fn the_memory_total_is_ungated_unaudited_and_matches_the_gated_view() {
        tairix_log::set_max_level(Level::Trace);
        let source = FixtureSource::new();
        let req = request_bytes(SysinfoQueryId::MEMORY_TOTAL, &[]);
        let mut resp = [0u8; 256];

        // No capability at all: installed RAM is a static hardware fact,
        // strictly coarser than the already-ungated load average, and a
        // process needs it to size its caches against the real machine.
        let sink = RecordingSink::new();
        let none = Caps(&[]);
        let n = serve_once(&source, &caller(&none), &sink, &req, &mut resp).expect("ungated");
        assert_eq!(n, MemoryTotal::WIRE_LEN);
        let decoded = MemoryTotal::from_bytes(&resp[..n]).expect("round trip");

        // The same size the gated kernel-memory view reports: one machine,
        // two views, never two notions of how much RAM is installed.
        let gated = source
            .kernel_memory_stats(&caller(&Caps(&[CapabilityId::SYSINFO_KERNEL])))
            .expect("gated view");
        assert_eq!(decoded.total_bytes, gated.total_bytes);

        // Usable as a cache budget: a real machine reports a non-zero size,
        // and zero is reserved for "unknown" (which admits nothing).
        assert_ne!(decoded.total_bytes, 0);

        // Unaudited: an ungated query must not be able to drive the
        // security log.
        assert!(sink.events.borrow().as_slice().is_empty());
    }

    #[test]
    fn reclaim_stats_is_gated_and_pages_per_class() {
        let source = FixtureSource::new();
        let mut resp = [0u8; 1024];
        let granted = Caps(&[CapabilityId::SYSINFO_KERNEL]);

        // Denied without the gate.
        let sink = RecordingSink::new();
        let rlr = ReclaimListRequest {
            offset: 0,
            limit: 16,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::RECLAIM_STATS, &rlr.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&Caps(&[])), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );

        // The whole ledger: one record per class, class-id order.
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, ReclaimClassRecord::WIRE_LEN * RECLAIM_CLASS_COUNT);
        for i in 0..RECLAIM_CLASS_COUNT {
            let window =
                &resp[i * ReclaimClassRecord::WIRE_LEN..(i + 1) * ReclaimClassRecord::WIRE_LEN];
            let record = ReclaimClassRecord::from_bytes(window).unwrap();
            assert_eq!(usize::from(record.class), i);
            assert_eq!(record.payload_bytes, (i as u64) * 1000);
        }

        // A paged window returns whole records from the offset…
        let rlr_page = ReclaimListRequest {
            offset: 7,
            limit: 16,
            flags: 0,
        };
        let req_page = request_bytes(SysinfoQueryId::RECLAIM_STATS, &rlr_page.to_le_bytes());
        let n = serve_once(&source, &caller(&granted), &sink, &req_page, &mut resp).unwrap();
        assert_eq!(n, ReclaimClassRecord::WIRE_LEN * 2);
        let record = ReclaimClassRecord::from_bytes(&resp[..ReclaimClassRecord::WIRE_LEN]).unwrap();
        assert_eq!(usize::from(record.class), 7);

        // …and paging past the end is the empty terminator.
        let rlr_end = ReclaimListRequest {
            offset: 9,
            limit: 16,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::RECLAIM_STATS, &rlr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn ramzip_stats_is_gated_and_round_trips() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::RAMZIP_STATS, &[]);
        let mut resp = [0u8; 256];

        assert_eq!(
            serve_once(&source, &caller(&Caps(&[])), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );

        let granted = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, RamzipStats::WIRE_LEN);
        let decoded = RamzipStats::from_bytes(&resp[..n]).unwrap();
        assert_eq!(decoded, fixture_ramzip());
    }

    #[test]
    fn cpu_load_is_gated_and_pages() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut resp = [0u8; 256];
        let granted = Caps(&[CapabilityId::SYSINFO_KERNEL]);

        let clr = CpuLoadRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::CPU_LOAD, &clr.to_le_bytes());

        // Denied without the gate: the queue depths and preemption
        // counters are kernel scheduler internals, unlike the ungated
        // busy/idle split.
        assert_eq!(
            serve_once(&source, &caller(&Caps(&[])), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );

        let n = serve_once(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, CpuLoadRecord::WIRE_LEN * 2);
        let first = CpuLoadRecord::from_bytes(&resp[..CpuLoadRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.cpu, 0);
        assert_eq!(first.switches, 42);

        // Offset paging serves the second CPU alone, then the terminator.
        let clr_next = CpuLoadRequest {
            offset: 1,
            limit: 8,
            flags: 0,
        };
        let req_next = request_bytes(SysinfoQueryId::CPU_LOAD, &clr_next.to_le_bytes());
        let n = serve_once(&source, &caller(&granted), &sink, &req_next, &mut resp).unwrap();
        assert_eq!(n, CpuLoadRecord::WIRE_LEN);
        let second = CpuLoadRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(second.cpu, 1);
        let clr_end = CpuLoadRequest {
            offset: 2,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::CPU_LOAD, &clr_end.to_le_bytes());
        assert_eq!(
            serve_once(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn malformed_header_is_rejected_and_logged() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let mut resp = [0u8; 64];
        // Too short to hold a header.
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &[0u8; 4], &mut resp),
            Err(Errno::BufferTooSmall)
        );
        // Corrupt magic.
        let mut req = request_bytes(SysinfoQueryId::UPTIME, &[]);
        req[0] ^= 0xFF;
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );
        let events = sink.events.borrow();
        assert_eq!(events.as_slice().len(), 2);
        assert!(events
            .as_slice()
            .iter()
            .all(|&(level, id)| level == Level::Warn && id == events::REQUEST_MALFORMED));
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        // Claim an 8-byte payload but supply only the header.
        let header = SysinfoRequestHeader {
            magic: SYSINFO_REQUEST_MAGIC,
            version: SYSINFO_VERSION_CURRENT,
            flags: 0,
            query: SysinfoQueryId::SELF_PROCESS_LIST,
            reserved: 0,
            payload_len: u32::try_from(ProcessListRequest::WIRE_LEN).unwrap(),
            request_id: 1,
        };
        let mut resp = [0u8; 64];
        assert_eq!(
            serve_once(
                &source,
                &caller(&caps),
                &sink,
                &header.to_le_bytes(),
                &mut resp
            ),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn unassigned_query_is_rejected() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let unassigned = SysinfoQueryId::from_raw(900).unwrap();
        let req = request_bytes(unassigned, &[]);
        let mut resp = [0u8; 64];
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_UNAVAILABLE)]
        );
    }

    #[test]
    fn response_buffer_too_small_fails_closed() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::KERNEL_MEMORY_STATS, &[]);
        let mut resp = [0u8; 8]; // smaller than KernelMemoryStats::WIRE_LEN
        assert_eq!(
            serve_once(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn cache_report_row_appears_in_cache_ledgers_and_reclaim_fold() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 4096];
        let kernel_reader = caller(&Caps(&[CapabilityId::SYSINFO_KERNEL]));

        // Report one row for class 3, and keep the reporter alive across
        // the queries below.
        let reporter = user_caller(&[], 0xA1, 555);
        source.set_live(alloc::vec![reporter.origin().proc_id()]);
        let payload = cache_report_payload(&[report_row("glyphs", 3, 4096)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        let n = serve(&source, &reporter, &mut registry, &sink, &req, &mut resp)
            .expect("report accepted");
        assert_eq!(n, 0);

        // The row shows up in CACHE_LEDGERS, after the kernel rows.
        let cll = CacheLedgerListRequest {
            offset: 0,
            limit: 32,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::CACHE_LEDGERS, &cll.to_le_bytes());
        let n = serve(
            &source,
            &kernel_reader,
            &mut registry,
            &sink,
            &req,
            &mut resp,
        )
        .expect("cache_ledgers served");
        assert_eq!(n, CacheLedgerRecord::WIRE_LEN * (RECLAIM_CLASS_COUNT + 1));
        let last = CacheLedgerRecord::from_bytes(&resp[n - CacheLedgerRecord::WIRE_LEN..n])
            .expect("valid record");
        assert_eq!(last.label(), "glyphs");
        assert_eq!(last.origin, CacheLedgerOrigin::SelfReported);
        assert_eq!(last.reporter_pid, 555);

        // …and its bytes are folded into class 3's `self_reported_bytes`,
        // alongside (not instead of) the kernel row's own bytes; a kernel
        // row's bytes never count towards it.
        let rlr = ReclaimListRequest {
            offset: 0,
            limit: u16::try_from(RECLAIM_CLASS_COUNT).expect("the class count fits a paging limit"),
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::RECLAIM_STATS, &rlr.to_le_bytes());
        let n = serve(
            &source,
            &kernel_reader,
            &mut registry,
            &sink,
            &req,
            &mut resp,
        )
        .expect("reclaim_stats served");
        assert_eq!(n, ReclaimClassRecord::WIRE_LEN * RECLAIM_CLASS_COUNT);
        let class3 = ReclaimClassRecord::from_bytes(
            &resp[3 * ReclaimClassRecord::WIRE_LEN..4 * ReclaimClassRecord::WIRE_LEN],
        )
        .unwrap();
        assert_eq!(class3.payload_bytes, 3_000 + 4096);
        assert_eq!(class3.self_reported_bytes, 4096);
        let class0 = ReclaimClassRecord::from_bytes(&resp[..ReclaimClassRecord::WIRE_LEN]).unwrap();
        assert_eq!(class0.self_reported_bytes, 0);
    }

    #[test]
    fn cache_report_refuses_row_with_preset_origin_or_reporter_pid() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 256];
        let reporter = user_caller(&[], 0xB2, 7);

        let mut preset_origin = report_row("glyphs", 0, 10);
        preset_origin.origin = CacheLedgerOrigin::Kernel;
        let payload = cache_report_payload(&[preset_origin]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        assert_eq!(
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );

        let mut preset_pid = report_row("glyphs", 0, 10);
        preset_pid.reporter_pid = 999;
        let payload = cache_report_payload(&[preset_pid]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        assert_eq!(
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );

        assert!(registry.rows().is_empty());
    }

    #[test]
    fn cache_report_refuses_a_row_claiming_a_kernel_subsystem_owner() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 256];
        let reporter = user_caller(&[], 0xB3, 11);

        let mut pretender = report_row("glyphs", 0, 4096);
        pretender.owner_kind = CacheOwnerKind::KernelSubsystem;
        // Every kernel row carries this owner kind, so the wire format
        // accepts it and only the reported-row policy can refuse it: a
        // process describing its own caches is not a kernel subsystem.
        assert!(CacheLedgerRecord::from_bytes(&pretender.to_le_bytes()).is_ok());

        let payload = cache_report_payload(&[pretender]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        assert_eq!(
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn cache_report_second_call_replaces_rather_than_appends() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 256];
        let reporter = user_caller(&[], 0xC3, 8);

        let first =
            cache_report_payload(&[report_row("glyphs", 0, 10), report_row("artwork", 1, 20)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &first);
        serve(&source, &reporter, &mut registry, &sink, &req, &mut resp).expect("first report");

        let second = cache_report_payload(&[report_row("cursors", 2, 5)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &second);
        serve(&source, &reporter, &mut registry, &sink, &req, &mut resp).expect("second report");

        let rows = registry.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "cursors");
    }

    #[test]
    fn cache_report_empty_withdraws_the_reporter() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 256];
        let reporter = user_caller(&[], 0xD4, 9);

        let payload = cache_report_payload(&[report_row("glyphs", 0, 10)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        serve(&source, &reporter, &mut registry, &sink, &req, &mut resp).expect("reported");

        let empty = cache_report_payload(&[]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &empty);
        serve(&source, &reporter, &mut registry, &sink, &req, &mut resp).expect("withdrawn");

        assert!(registry.rows().is_empty());
    }

    #[test]
    fn dead_reporter_is_dropped_and_recycled_pid_does_not_inherit_its_rows() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 4096];
        let kernel_reader = caller(&Caps(&[CapabilityId::SYSINFO_KERNEL]));
        let cll = CacheLedgerListRequest {
            offset: 0,
            limit: 32,
            flags: 0,
        };

        let original = user_caller(&[], 0xE5, 111);
        source.set_live(alloc::vec![original.origin().proc_id()]);
        let payload = cache_report_payload(&[report_row("glyphs", 0, 10)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        serve(&source, &original, &mut registry, &sink, &req, &mut resp).expect("reported");

        // The process has exited: nobody is live any more.
        source.set_live(alloc::vec::Vec::new());
        let req = request_bytes(SysinfoQueryId::CACHE_LEDGERS, &cll.to_le_bytes());
        let n = serve(
            &source,
            &kernel_reader,
            &mut registry,
            &sink,
            &req,
            &mut resp,
        )
        .unwrap();
        assert_eq!(n, CacheLedgerRecord::WIRE_LEN * RECLAIM_CLASS_COUNT);

        // A different process instance, recycled to the same numeric pid,
        // reports.
        let recycled = user_caller(&[], 0xF6, 111);
        source.set_live(alloc::vec![recycled.origin().proc_id()]);
        let payload = cache_report_payload(&[report_row("cursors", 0, 20)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        serve(&source, &recycled, &mut registry, &sink, &req, &mut resp)
            .expect("recycled report admitted");

        let req = request_bytes(SysinfoQueryId::CACHE_LEDGERS, &cll.to_le_bytes());
        let n = serve(
            &source,
            &kernel_reader,
            &mut registry,
            &sink,
            &req,
            &mut resp,
        )
        .unwrap();
        assert_eq!(n, CacheLedgerRecord::WIRE_LEN * (RECLAIM_CLASS_COUNT + 1));
        let last = CacheLedgerRecord::from_bytes(&resp[n - CacheLedgerRecord::WIRE_LEN..n])
            .expect("valid record");
        assert_eq!(last.label(), "cursors");
        assert_eq!(last.reporter_pid, 111);
    }

    #[test]
    fn full_registry_refuses_new_reporter_without_evicting_a_live_one() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        // Exactly the registry's own floor, read from the derivation policy
        // rather than copied, so this test cannot drift from it.
        let floor = u8::try_from(MIN_REPORTERS).expect("the reporter floor fits a fixture tag");
        let mut registry = CacheLedgerRegistry::new(RAM_BYTES_PER_REPORTER * u64::from(floor));
        let mut resp = [0u8; 256];
        let mut live = alloc::vec::Vec::new();

        for tag in 0..floor {
            let reporter = user_caller(&[], tag, u64::from(tag));
            live.push(reporter.origin().proc_id());
            source.set_live(live.clone());
            let payload = cache_report_payload(&[report_row("glyphs", 0, 1)]);
            let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp)
                .expect("admitted within capacity");
        }

        let overflow = user_caller(&[], 200, 200);
        live.push(overflow.origin().proc_id());
        source.set_live(live);
        let payload = cache_report_payload(&[report_row("glyphs", 0, 1)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        assert_eq!(
            serve(&source, &overflow, &mut registry, &sink, &req, &mut resp),
            Err(Errno::NoSpace)
        );
        assert_eq!(registry.rows().len(), MIN_REPORTERS);
    }

    #[test]
    fn cache_ledgers_requires_capability_while_cache_report_does_not() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 256];

        let cll = CacheLedgerListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::CACHE_LEDGERS, &cll.to_le_bytes());
        assert_eq!(
            serve(
                &source,
                &caller(&Caps(&[])),
                &mut registry,
                &sink,
                &req,
                &mut resp
            ),
            Err(Errno::PermissionDenied)
        );

        let reporter = user_caller(&[], 0x88, 1);
        source.set_live(alloc::vec![reporter.origin().proc_id()]);
        let payload = cache_report_payload(&[report_row("glyphs", 0, 1)]);
        let report_req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        assert_eq!(
            serve(
                &source,
                &reporter,
                &mut registry,
                &sink,
                &report_req,
                &mut resp
            ),
            Ok(0)
        );
    }

    #[test]
    fn cache_report_from_a_kernel_domain_caller_leaves_no_rows() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let mut resp = [0u8; 4096];

        // `CACHE_REPORT` demands no capability, so the attested instance id
        // is the only thing standing between a kernel-domain principal and
        // a row that would read as self-reported. Nothing else — not the
        // gate, not the decoder — refuses this request.
        let payload = cache_report_payload(&[report_row("glyphs", 3, 4096)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        assert_eq!(
            serve(
                &source,
                &kernel_caller(),
                &mut registry,
                &sink,
                &req,
                &mut resp
            ),
            Err(Errno::PermissionDenied)
        );
        assert!(registry.rows().is_empty());

        // Claiming to be live does not help either: expiry reads the
        // kernel's live-instance list, and the reserved id is refused
        // before the registry is consulted at all.
        source.set_live(alloc::vec![ProcId::KERNEL]);
        assert_eq!(
            serve(
                &source,
                &kernel_caller(),
                &mut registry,
                &sink,
                &req,
                &mut resp
            ),
            Err(Errno::PermissionDenied)
        );

        // The combined view a reader gets is the kernel's own rows alone.
        let cll = CacheLedgerListRequest {
            offset: 0,
            limit: 32,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::CACHE_LEDGERS, &cll.to_le_bytes());
        let n = serve(
            &source,
            &caller(&Caps(&[CapabilityId::SYSINFO_KERNEL])),
            &mut registry,
            &sink,
            &req,
            &mut resp,
        )
        .expect("cache_ledgers served");
        assert_eq!(n, CacheLedgerRecord::WIRE_LEN * RECLAIM_CLASS_COUNT);
    }

    #[test]
    fn cache_ledgers_paging_never_skips_or_repeats_a_row() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let kernel_reader = caller(&Caps(&[CapabilityId::SYSINFO_KERNEL]));
        let mut resp = [0u8; 4096];

        let reporter = user_caller(&[], 0x99, 2);
        source.set_live(alloc::vec![reporter.origin().proc_id()]);
        let payload = cache_report_payload(&[report_row("glyphs", 0, 1)]);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &payload);
        serve(&source, &reporter, &mut registry, &sink, &req, &mut resp).expect("reported");

        let total = RECLAIM_CLASS_COUNT + 1;
        let mut seen: alloc::vec::Vec<CacheLedgerRecord> = alloc::vec::Vec::new();
        let mut offset: u32 = 0;
        loop {
            let cll = CacheLedgerListRequest {
                offset,
                limit: 3,
                flags: 0,
            };
            let req = request_bytes(SysinfoQueryId::CACHE_LEDGERS, &cll.to_le_bytes());
            let n = serve(
                &source,
                &kernel_reader,
                &mut registry,
                &sink,
                &req,
                &mut resp,
            )
            .unwrap();
            if n == 0 {
                break;
            }
            let (records, trailing) = resp[..n].as_chunks::<{ CacheLedgerRecord::WIRE_LEN }>();
            assert!(trailing.is_empty(), "a page is whole records");
            for record in records {
                seen.push(CacheLedgerRecord::from_bytes(record).unwrap());
            }
            offset += u32::try_from(records.len()).expect("a page length fits a u32");
        }
        assert_eq!(seen.len(), total);
        for i in 0..seen.len() {
            for j in (i + 1)..seen.len() {
                assert_ne!(seen[i], seen[j], "duplicate row at {i} and {j}");
            }
        }
    }

    #[test]
    fn malformed_cache_report_fails_closed_and_leaves_registry_untouched() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let mut registry = CacheLedgerRegistry::new(1 << 30);
        let reporter = user_caller(&[], 0x77, 42);
        let mut resp = [0u8; 512];

        // Short body: declares one row but supplies none.
        let short_header = CacheReportRequest {
            count: 1,
            flags: 0,
            reserved: 0,
        };
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &short_header.to_le_bytes());
        assert_eq!(
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );
        assert!(registry.rows().is_empty());

        // Trailing bytes: declares zero rows but supplies one anyway.
        let zero_header = CacheReportRequest {
            count: 0,
            flags: 0,
            reserved: 0,
        };
        let mut trailing = zero_header.to_le_bytes().to_vec();
        trailing.extend_from_slice(&report_row("glyphs", 0, 1).to_le_bytes());
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &trailing);
        assert_eq!(
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );
        assert!(registry.rows().is_empty());

        // Over-long declared count.
        let over_header = CacheReportRequest {
            count: u16::try_from(MAX_CACHE_REPORT_ENTRIES + 1).unwrap(),
            flags: 0,
            reserved: 0,
        };
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &over_header.to_le_bytes());
        assert_eq!(
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp),
            Err(Errno::LengthOutOfRange)
        );
        assert!(registry.rows().is_empty());

        // Unrenderable label: corrupt the wire image directly, since the
        // safe constructor itself refuses to build such a row.
        let mut bytes = report_row("glyphs", 0, 1).to_le_bytes();
        bytes[96] = 0x01; // first label byte becomes a control character
        let one_header = CacheReportRequest {
            count: 1,
            flags: 0,
            reserved: 0,
        };
        let mut req_payload = one_header.to_le_bytes().to_vec();
        req_payload.extend_from_slice(&bytes);
        let req = request_bytes_vec(SysinfoQueryId::CACHE_REPORT, &req_payload);
        assert_eq!(
            serve(&source, &reporter, &mut registry, &sink, &req, &mut resp),
            Err(Errno::OutOfRange)
        );
        assert!(registry.rows().is_empty());
    }
}
