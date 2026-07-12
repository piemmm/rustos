//! The userspace `info:`/`stats:` resource resolver (`plans/ALIAS.md` §6.2,
//! §6.3, §14).
//!
//! `info:` and `stats:` references are **not** kernel-owned resources: they
//! name facts and measurements that must be served by the System Information
//! API, never by a virtual file, by text scraping, or by a kernel
//! `resource_open` backing (which would bypass the `sysinfod` broker's
//! per-principal scoping). This resolver is therefore userspace: it maps a
//! parsed [`ResourceRef`] onto a [`SysinfoQueryId`], issues it through the
//! shared [`Transport`] seam (the same path `ps`/`sysinfo` use), and turns
//! the typed reply into a [`ResourceResponse`] (`crate::resinfo`).
//!
//! It fails closed: an unknown selector, a decoration (`@guard`, `::facet`,
//! `?param`) on a resource that takes none, a capability the caller does not
//! hold, or a reply that does not decode all yield a typed
//! [`ResolveInfoError`] and never a fabricated value (`AGENTS.md` §5.4, §2.9).
//! The served set grows in place here as sibling queries land; today it covers
//! exactly the ungated/self-scoped and kernel-memory `sysinfo-v1` queries that
//! already exist (`info:system/{hostname,kernel,machine-id,boot-time}`,
//! `info:process/{pid,uid,gid,proc-id,trust-domain,caps}`,
//! `info:mem/{physical,page-size}`,
//! `info:limits/<kind>/{soft,hard}`, `stats:uptime`, `stats:mem/*`, and
//! `stats:limits/<kind>`).

use alloc::string::{String, ToString};

use alloc::vec::Vec;
use rustos_abi::origin::{Origin, TrustDomain};

use rustos_abi::sysinfo::{
    reclaim_class_from_name, CpuLoadRecord, CpuLoadRequest, KernelMemoryStats, MemoryPressureStats,
    RamzipStats, ReclaimClassRecord, ReclaimListRequest, ResourceLimitRecord, SysinfoQueryId,
    SystemIdentity, Uptime, PRESSURE_BAND_NAMES, RECLAIM_CLASS_COUNT, RESOURCE_LIMITS_REPORT_LEN,
};
use rustos_abi::time::Time64;
use rustos_abi::{CapabilityId, Errno, LimitKind, ResourceLimit};
use rustos_resref::{KnownNamespace, ResourceRef};

use crate::cputime::for_each_cpu_time;
use crate::list::{field_lossy, ListError};
use crate::request::{call, CallError};
use crate::resinfo::{
    render_limit_bound, Authorization, InfoValue, Metric, MetricKind, Producer, ResetBehavior,
    ResourceResponse, ResponsePayload, Sensitivity, Unit,
};
use crate::transport::Transport;

/// Why resolving an `info:`/`stats:` reference did not produce a value.
///
/// A resolver-level error, distinct from the parser's syntax errors: the
/// reference parsed but names nothing this resolver serves, requests a shape
/// it does not offer, or could not be answered by the service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveInfoError {
    /// The reference is not in the `info:` or `stats:` namespace; this
    /// resolver does not own it (the caller routed it to the wrong resolver).
    NotInfoOrStats,
    /// The namespace is served but the selector names no resource in it.
    UnknownSelector,
    /// The selector is understood but the request is not serviceable: a
    /// guard, facet, or query parameter on a resource that takes none.
    UnsupportedRequest,
    /// The System Information API refused the query for want of the
    /// capability it declares.
    CapabilityDenied,
    /// The System Information API call failed for another reason.
    Service(Errno),
    /// The service's reply did not decode as the expected record.
    Malformed,
}

/// Resolve an `info:`/`stats:` `reference` to a [`ResourceResponse`], reading
/// the value from the System Information API through `transport` and stamping
/// the envelope with `now`.
///
/// # Errors
///
/// A [`ResolveInfoError`] naming the first refusal; no value is produced on
/// any error path (fail closed).
pub fn resolve(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    match reference.namespace().known() {
        Some(KnownNamespace::Info) => resolve_info(reference, now, transport),
        Some(KnownNamespace::Stats) => resolve_stats(reference, now, transport),
        _ => Err(ResolveInfoError::NotInfoOrStats),
    }
}

/// Resolve an `info:` reference (a stable fact) to a single [`InfoValue`].
fn resolve_info(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    reject_decoration(reference)?;
    let (value, authorization) = resolve_info_value(&selector(reference), transport)?;
    envelope(reference, now, authorization, ResponsePayload::Info(value))
}

/// Map an `info:` `selector` onto its typed value, issuing only the System
/// Information query the matched selector actually needs.
fn resolve_info_value(
    selector: &[&str],
    transport: &dyn Transport,
) -> Result<(InfoValue, Authorization), ResolveInfoError> {
    let (value, authorization) = match selector {
        ["system", "hostname"] => (
            InfoValue::new_str(
                Sensitivity::Public,
                &field_lossy(query_identity(transport)?.hostname_bytes()),
            ),
            Authorization::Unprivileged,
        ),
        ["system", "kernel"] => (
            InfoValue::new_str(
                Sensitivity::Public,
                &version_string(&query_identity(transport)?),
            ),
            Authorization::Unprivileged,
        ),
        // Machine identity is identifying, not public (`plans/ALIAS.md` §6.2).
        ["system", "machine-id"] => (
            InfoValue::new_str(
                Sensitivity::Sensitive,
                &hex_lower(&query_identity(transport)?.machine_id),
            ),
            Authorization::Unprivileged,
        ),
        // The wall-clock instant of boot is fixed for the life of the boot, so
        // it is a stable fact rather than a measurement; it is not sensitive.
        // It rides the same ungated `UPTIME` query that `stats:uptime` uses.
        ["system", "boot-time"] => (
            InfoValue::new_str(
                Sensitivity::Public,
                &time_string(query_uptime(transport)?.boot_time),
            ),
            Authorization::Unprivileged,
        ),
        // The caller's own kernel-attested identity. The self-scoped
        // `PROCESS_IDENTITY` query needs no capability and answers only for the
        // asking principal, so these are public facts about the caller itself,
        // not a cross-principal disclosure. The `trust-domain` and `caps`
        // leaves ride the same reply (no extra query): the kernel fills the
        // capability summary as a non-secret bitset, so it is `Public`.
        ["process", leaf @ ("pid" | "uid" | "gid" | "proc-id" | "trust-domain" | "caps")] => {
            let origin = query_process_identity(transport)?;
            // The or-pattern fixes `leaf` to one of these six, so the final
            // arm is `caps` and there is no unhandled case.
            let value = match *leaf {
                "pid" => InfoValue::new_str(Sensitivity::Public, &origin.pid().to_string()),
                "uid" => InfoValue::new_str(Sensitivity::Public, &origin.uid().to_string()),
                "gid" => InfoValue::new_str(Sensitivity::Public, &origin.gid().to_string()),
                "proc-id" => {
                    InfoValue::new_str(Sensitivity::Public, &hex_lower(origin.proc_id().as_bytes()))
                }
                "trust-domain" => InfoValue::new_str(
                    Sensitivity::Public,
                    trust_domain_name(origin.trust_domain()),
                ),
                _ => InfoValue::new_str(
                    Sensitivity::Public,
                    &hex_lower(origin.capabilities().as_bytes()),
                ),
            };
            (value, Authorization::Unprivileged)
        }
        // Stable hardware facts (total physical memory, the reporting
        // architecture's page size), not measurements, so they are `info:`
        // values. Both are carried only by the kernel-memory query, which the
        // broker gates on `CAP_SYSINFO_KERNEL`; the sizes themselves are not
        // secret (hence `Public`), but the sole query that reports them is
        // privileged, so the answer costs that capability and a denial
        // surfaces below.
        ["mem", leaf @ ("physical" | "page-size")] => {
            let stats = query_kernel_memory(transport)?;
            // The or-pattern fixes `leaf` to one of these two, so the final
            // arm is `page-size` and there is no unhandled case.
            let value = match *leaf {
                "physical" => stats.total_bytes,
                _ => u64::from(stats.page_size),
            };
            (
                InfoValue::new_str(Sensitivity::Public, &value.to_string()),
                Authorization::Capability(CapabilityId::SYSINFO_KERNEL),
            )
        }
        // The number of online CPUs, counted from the gated per-CPU load
        // query: a kernel-tier hardware fact, so — like `info:mem/physical`
        // — the answer costs `CAP_SYSINFO_KERNEL` and a denial surfaces
        // below.
        ["cpu", "count"] => {
            let cpus = query_cpu_loads(transport)?;
            (
                InfoValue::new_str(Sensitivity::Public, &cpus.len().to_string()),
                Authorization::Capability(CapabilityId::SYSINFO_KERNEL),
            )
        }
        // A configured soft/hard bound on one of the caller's own resources.
        // The self-scoped `RESOURCE_LIMITS` query needs no capability and
        // answers only for the asking principal, so its own limits are public
        // facts about itself, not a cross-principal disclosure. An unlimited
        // bound renders as `unlimited`, sharing the one spelling the `limits`
        // CLI uses.
        ["limits", kind_name, bound @ ("soft" | "hard")] => {
            let kind = LimitKind::from_name(kind_name).ok_or(ResolveInfoError::UnknownSelector)?;
            let limit = limit_for(kind, transport)?;
            // The or-pattern fixes `bound` to `soft` or `hard`, so the final
            // arm is `hard` and there is no unhandled case.
            let rendered = match *bound {
                "soft" => render_limit_bound(limit.soft),
                _ => render_limit_bound(limit.hard),
            };
            (
                InfoValue::new_str(Sensitivity::Public, &rendered),
                Authorization::Unprivileged,
            )
        }
        _ => return Err(ResolveInfoError::UnknownSelector),
    };
    Ok((
        value.map_err(|_| ResolveInfoError::Malformed)?,
        authorization,
    ))
}

/// Resolve a `stats:` reference (a measurement) to a single [`Metric`].
fn resolve_stats(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    reject_decoration(reference)?;
    match selector(reference).as_slice() {
        ["uptime"] => {
            let uptime = query_uptime(transport)?;
            // A monotonic span since boot never precedes boot; clamp the
            // signed span to a non-negative count of seconds.
            let secs = u64::try_from(uptime.since_boot.secs().max(0)).unwrap_or(0);
            let metric = Metric::new(
                "uptime",
                MetricKind::Counter,
                Unit::Seconds,
                secs,
                now,
                None,
                ResetBehavior::Boot,
            )
            .map_err(|_| ResolveInfoError::Malformed)?;
            envelope(
                reference,
                now,
                Authorization::Unprivileged,
                ResponsePayload::Metric(metric),
            )
        }
        ["mem", leaf @ ("used" | "available" | "total" | "kernel-heap" | "user-resident")] => {
            let stats = query_kernel_memory(transport)?;
            // The or-pattern above fixes `leaf` to one of these five, so the
            // final arm is `user-resident` and there is no unhandled case.
            let value = match *leaf {
                "used" => stats.total_bytes.saturating_sub(stats.free_bytes),
                "available" => stats.free_bytes,
                "total" => stats.total_bytes,
                "kernel-heap" => stats.kernel_heap_bytes,
                _ => stats.user_resident_bytes,
            };
            let mut name = String::from("mem/");
            name.push_str(leaf);
            let metric = Metric::new(
                &name,
                MetricKind::Gauge,
                Unit::Bytes,
                value,
                now,
                None,
                ResetBehavior::Never,
            )
            .map_err(|_| ResolveInfoError::Malformed)?;
            envelope(
                reference,
                now,
                // The kernel-memory query is gated on `CAP_SYSINFO_KERNEL`;
                // the broker enforces it, and a denial surfaces below.
                Authorization::Capability(CapabilityId::SYSINFO_KERNEL),
                ResponsePayload::Metric(metric),
            )
        }
        // The all-CPU (or one CPU's) cumulative busy share since boot, from
        // the ungated busy/idle accounting. A one-shot resolution has no
        // caller-controlled sampling window, so the honest figure is the
        // share of uptime spent busy; a windowed view is a monitor's job
        // (two timed reads over its own refresh interval).
        ["cpu", "load"] => {
            let (busy, total) = busy_share_input(transport, None)?;
            cpu_load_metric(reference, now, "cpu/load", busy, total)
        }
        ["cpu", index, "load"] => {
            let cpu: u32 = index
                .parse()
                .map_err(|_| ResolveInfoError::UnknownSelector)?;
            let (busy, total) = busy_share_input(transport, Some(cpu))?;
            let mut name = String::from("cpu/");
            name.push_str(index);
            name.push_str("/load");
            cpu_load_metric(reference, now, &name, busy, total)
        }
        // Cumulative context switches across every CPU, from the gated
        // per-CPU load query.
        ["cpu", "switches"] => cpu_switches_metric(reference, now, transport),
        // The live pressure band as a small integer gauge (its depth); the
        // band's name rides in the metric name so a reader never has to
        // decode the depth itself.
        ["mem", "pressure"] => pressure_band_metric(reference, now, transport),
        // Band transitions since boot, summed across every band.
        ["mem", "pressure", "transitions"] => {
            pressure_transitions_metric(reference, now, transport)
        }
        // Reclaimable bytes held — the whole ledger, or one class by its
        // stable name. Unknown class names fail closed.
        ["mem", "reclaim", leaf] => reclaim_bytes_metric(reference, now, transport, leaf),
        // The compressed tier's stored/logical byte gauges and the bytes
        // it is saving (their difference).
        ["mem", "ramzip", leaf @ ("stored" | "logical" | "saved")] => {
            ramzip_bytes_metric(reference, now, transport, leaf)
        }
        // The caller's own live usage of one of its limited resources: a
        // measurement, so a gauge (it rises and falls and never resets over
        // the life of the process). Byte-denominated resources report
        // [`Unit::Bytes`]; the rest are a dimensionless [`Unit::Count`]. The
        // query is ungated and self-scoped, so the usage is unprivileged.
        ["limits", kind_name] => {
            let kind = LimitKind::from_name(kind_name).ok_or(ResolveInfoError::UnknownSelector)?;
            let usage = usage_for(kind, transport)?;
            let mut name = String::from("limits/");
            name.push_str(kind_name);
            let metric = Metric::new(
                &name,
                MetricKind::Gauge,
                unit_for_limit(kind),
                usage,
                now,
                None,
                ResetBehavior::Never,
            )
            .map_err(|_| ResolveInfoError::Malformed)?;
            envelope(
                reference,
                now,
                Authorization::Unprivileged,
                ResponsePayload::Metric(metric),
            )
        }
        _ => Err(ResolveInfoError::UnknownSelector),
    }
}

/// Refuse a guard, facet, or query parameter: none of the resources this
/// resolver serves takes one, so a decorated reference is not serviceable.
fn reject_decoration(reference: &ResourceRef) -> Result<(), ResolveInfoError> {
    if reference.guard().is_some() || reference.facet().is_some() || !reference.params().is_empty()
    {
        return Err(ResolveInfoError::UnsupportedRequest);
    }
    Ok(())
}

/// Borrow the reference's selector segments as string slices for matching.
fn selector(reference: &ResourceRef) -> alloc::vec::Vec<&str> {
    reference.selector().iter().map(String::as_str).collect()
}

/// The gated `stats:cpu/switches` counter: cumulative context switches
/// summed across every CPU.
fn cpu_switches_metric(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    let switches = query_cpu_loads(transport)?
        .iter()
        .fold(0u64, |acc, record| acc.saturating_add(record.switches));
    gated_metric(
        reference,
        now,
        "cpu/switches",
        MetricKind::Counter,
        Unit::Count,
        switches,
        ResetBehavior::Boot,
    )
}

/// The gated `stats:mem/pressure` gauge: the band depth, with the band's
/// stable name carried in the metric name.
fn pressure_band_metric(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    let stats = query_memory_pressure(transport)?;
    let band = usize::from(stats.band).min(PRESSURE_BAND_NAMES.len() - 1);
    let mut name = String::from("mem/pressure/");
    name.push_str(PRESSURE_BAND_NAMES[band]);
    gated_metric(
        reference,
        now,
        &name,
        MetricKind::Gauge,
        Unit::Count,
        u64::from(stats.band),
        ResetBehavior::Never,
    )
}

/// The gated `stats:mem/pressure/transitions` counter: band entries since
/// boot, summed across every band.
fn pressure_transitions_metric(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, ResolveInfoError> {
    let stats = query_memory_pressure(transport)?;
    let transitions = stats
        .band_entries
        .iter()
        .fold(0u64, |acc, entries| acc.saturating_add(*entries));
    gated_metric(
        reference,
        now,
        "mem/pressure/transitions",
        MetricKind::Counter,
        Unit::Count,
        transitions,
        ResetBehavior::Boot,
    )
}

/// The gated `stats:mem/reclaim/*` byte gauges: the whole ledger
/// (`total`) or one class by its stable name; an unknown class name fails
/// closed.
fn reclaim_bytes_metric(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
    leaf: &str,
) -> Result<ResourceResponse, ResolveInfoError> {
    let records = query_reclaim_records(transport)?;
    let value = if leaf == "total" {
        records.iter().fold(0u64, |acc, record| {
            acc.saturating_add(record.payload_bytes)
                .saturating_add(record.metadata_bytes)
        })
    } else {
        let class = reclaim_class_from_name(leaf).ok_or(ResolveInfoError::UnknownSelector)?;
        let record = records
            .iter()
            .find(|record| record.class == class)
            .ok_or(ResolveInfoError::Malformed)?;
        record.payload_bytes.saturating_add(record.metadata_bytes)
    };
    let mut name = String::from("mem/reclaim/");
    name.push_str(leaf);
    gated_metric(
        reference,
        now,
        &name,
        MetricKind::Gauge,
        Unit::Bytes,
        value,
        ResetBehavior::Never,
    )
}

/// The gated `stats:mem/ramzip/*` byte gauges: stored, logical, or the
/// saved difference. The caller's or-pattern fixes `leaf` to the closed
/// three-name set.
fn ramzip_bytes_metric(
    reference: &ResourceRef,
    now: Time64,
    transport: &dyn Transport,
    leaf: &str,
) -> Result<ResourceResponse, ResolveInfoError> {
    let stats = query_ramzip(transport)?;
    let value = match leaf {
        "stored" => stats.stored_bytes,
        "logical" => stats.logical_bytes,
        _ => stats.logical_bytes.saturating_sub(stats.stored_bytes),
    };
    let mut name = String::from("mem/ramzip/");
    name.push_str(leaf);
    gated_metric(
        reference,
        now,
        &name,
        MetricKind::Gauge,
        Unit::Bytes,
        value,
        ResetBehavior::Never,
    )
}

/// Build one `CAP_SYSINFO_KERNEL`-gated metric response — the envelope
/// every kernel-statistics selector shares.
fn gated_metric(
    reference: &ResourceRef,
    now: Time64,
    name: &str,
    kind: MetricKind,
    unit: Unit,
    value: u64,
    reset_behavior: ResetBehavior,
) -> Result<ResourceResponse, ResolveInfoError> {
    let metric = Metric::new(name, kind, unit, value, now, None, reset_behavior)
        .map_err(|_| ResolveInfoError::Malformed)?;
    envelope(
        reference,
        now,
        Authorization::Capability(CapabilityId::SYSINFO_KERNEL),
        ResponsePayload::Metric(metric),
    )
}

/// Sum the cumulative busy nanoseconds and total (busy + idle)
/// nanoseconds since boot — across every CPU, or for the one CPU named by
/// `cpu`. A named CPU that does not exist fails closed as an unknown
/// selector.
fn busy_share_input(
    transport: &dyn Transport,
    cpu: Option<u32>,
) -> Result<(u64, u64), ResolveInfoError> {
    let mut busy = 0u64;
    let mut total = 0u64;
    let mut found = false;
    for_each_cpu_time(transport, |record| {
        if cpu.is_none() || cpu == Some(record.cpu) {
            found = true;
            busy = busy.saturating_add(record.busy_ns);
            total = total
                .saturating_add(record.busy_ns)
                .saturating_add(record.idle_ns);
        }
        Ok(())
    })
    .map_err(map_list_error)?;
    if !found {
        return Err(ResolveInfoError::UnknownSelector);
    }
    Ok((busy, total))
}

/// Build the busy-share percentage metric for [`busy_share_input`]'s sums.
/// A zero total (no time has passed) truthfully reports zero.
fn cpu_load_metric(
    reference: &ResourceRef,
    now: Time64,
    name: &str,
    busy: u64,
    total: u64,
) -> Result<ResourceResponse, ResolveInfoError> {
    let percent = busy.saturating_mul(100).checked_div(total).unwrap_or(0);
    let metric = Metric::new(
        name,
        MetricKind::Gauge,
        Unit::Percent,
        percent,
        now,
        None,
        ResetBehavior::Never,
    )
    .map_err(|_| ResolveInfoError::Malformed)?;
    envelope(
        reference,
        now,
        // The busy/idle accounting is the ungated utilisation split.
        Authorization::Unprivileged,
        ResponsePayload::Metric(metric),
    )
}

/// Query the live memory-pressure snapshot (gated on
/// `CAP_SYSINFO_KERNEL` by the broker).
fn query_memory_pressure(
    transport: &dyn Transport,
) -> Result<MemoryPressureStats, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::MEMORY_PRESSURE, &[]).map_err(map_call_error)?;
    MemoryPressureStats::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Query the `ramzip` tier counters (gated on `CAP_SYSINFO_KERNEL`).
fn query_ramzip(transport: &dyn Transport) -> Result<RamzipStats, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::RAMZIP_STATS, &[]).map_err(map_call_error)?;
    RamzipStats::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Query the whole reclaim ledger (gated on `CAP_SYSINFO_KERNEL`). The
/// class set is small and closed, so one page always carries it; a reply
/// that is not a whole number of records fails closed.
fn query_reclaim_records(
    transport: &dyn Transport,
) -> Result<Vec<ReclaimClassRecord>, ResolveInfoError> {
    let request = ReclaimListRequest {
        offset: 0,
        limit: u16::try_from(RECLAIM_CLASS_COUNT).unwrap_or(u16::MAX),
        flags: 0,
    };
    let reply = call(
        transport,
        SysinfoQueryId::RECLAIM_STATS,
        &request.to_le_bytes(),
    )
    .map_err(map_call_error)?;
    if reply.len() % ReclaimClassRecord::WIRE_LEN != 0 {
        return Err(ResolveInfoError::Malformed);
    }
    let mut records = Vec::new();
    for chunk in reply.chunks_exact(ReclaimClassRecord::WIRE_LEN) {
        records
            .push(ReclaimClassRecord::from_bytes(chunk).map_err(|_| ResolveInfoError::Malformed)?);
    }
    Ok(records)
}

/// Page through the per-CPU load records (gated on `CAP_SYSINFO_KERNEL`).
fn query_cpu_loads(transport: &dyn Transport) -> Result<Vec<CpuLoadRecord>, ResolveInfoError> {
    /// Records requested per page: bounds the reply size without bounding
    /// how many CPUs the machine may have.
    const CPU_LOAD_PAGE: u16 = 64;
    let mut records = Vec::new();
    crate::list::walk_pages(
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
            records.push(record);
            Ok(())
        },
    )
    .map_err(map_list_error)?;
    Ok(records)
}

/// Map a paged-walk failure onto the resolver's error vocabulary.
fn map_list_error(err: ListError) -> ResolveInfoError {
    match err {
        ListError::Call(call) => map_call_error(call),
        ListError::Sink(errno) => ResolveInfoError::Service(errno),
    }
}

/// Wrap `payload` in the shared response envelope.
fn envelope(
    reference: &ResourceRef,
    now: Time64,
    authorization: Authorization,
    payload: ResponsePayload,
) -> Result<ResourceResponse, ResolveInfoError> {
    ResourceResponse::new(
        Producer::Sysinfod,
        authorization,
        now,
        &reference.to_string(),
        payload,
    )
    .map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::SYSTEM_IDENTITY`] and decode the reply.
fn query_identity(transport: &dyn Transport) -> Result<SystemIdentity, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::SYSTEM_IDENTITY, &[]).map_err(map_call_error)?;
    SystemIdentity::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::UPTIME`] and decode the reply.
fn query_uptime(transport: &dyn Transport) -> Result<Uptime, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::UPTIME, &[]).map_err(map_call_error)?;
    Uptime::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::KERNEL_MEMORY_STATS`] and decode the reply.
fn query_kernel_memory(transport: &dyn Transport) -> Result<KernelMemoryStats, ResolveInfoError> {
    let reply =
        call(transport, SysinfoQueryId::KERNEL_MEMORY_STATS, &[]).map_err(map_call_error)?;
    KernelMemoryStats::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::PROCESS_IDENTITY`] and decode the caller's own
/// kernel-attested [`Origin`].
fn query_process_identity(transport: &dyn Transport) -> Result<Origin, ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::PROCESS_IDENTITY, &[]).map_err(map_call_error)?;
    Origin::from_bytes(&reply).map_err(|_| ResolveInfoError::Malformed)
}

/// Issue [`SysinfoQueryId::RESOURCE_LIMITS`] and decode the caller's own
/// per-resource limits, indexed by [`LimitKind`] discriminant.
///
/// The reply is exactly [`LimitKind::COUNT`] [`ResourceLimitRecord`]s packed
/// in discriminant order. A reply of any other length, a record that does not
/// decode, or a record whose self-describing `kind` disagrees with its
/// position is corrupt and fails closed as [`ResolveInfoError::Malformed`] —
/// never a partial or mis-attributed answer.
fn query_resource_limits(
    transport: &dyn Transport,
) -> Result<[ResourceLimitRecord; LimitKind::COUNT], ResolveInfoError> {
    let reply = call(transport, SysinfoQueryId::RESOURCE_LIMITS, &[]).map_err(map_call_error)?;
    if reply.len() != RESOURCE_LIMITS_REPORT_LEN {
        return Err(ResolveInfoError::Malformed);
    }
    let mut records =
        [ResourceLimitRecord::new(LimitKind::AddressSpaceBytes, ResourceLimit::UNLIMITED, 0);
            LimitKind::COUNT];
    for (index, kind) in LimitKind::ALL.iter().enumerate() {
        let base = index * ResourceLimitRecord::WIRE_LEN;
        let record =
            ResourceLimitRecord::from_bytes(&reply[base..base + ResourceLimitRecord::WIRE_LEN])
                .map_err(|_| ResolveInfoError::Malformed)?;
        // Records are positional in discriminant order; the self-describing
        // kind field must agree with the slot it occupies, or the reply is
        // corrupt.
        if record.kind != *kind {
            return Err(ResolveInfoError::Malformed);
        }
        records[index] = record;
    }
    Ok(records)
}

/// The caller's effective soft/hard bound for `kind`.
fn limit_for(
    kind: LimitKind,
    transport: &dyn Transport,
) -> Result<ResourceLimit, ResolveInfoError> {
    let records = query_resource_limits(transport)?;
    Ok(records[kind.as_u32() as usize].limit)
}

/// The caller's current live usage of `kind`, in its natural unit.
fn usage_for(kind: LimitKind, transport: &dyn Transport) -> Result<u64, ResolveInfoError> {
    let records = query_resource_limits(transport)?;
    Ok(records[kind.as_u32() as usize].usage)
}

/// The unit a resource's live usage is measured in: bytes for the
/// byte-denominated resources, a dimensionless count for the rest.
fn unit_for_limit(kind: LimitKind) -> Unit {
    match kind {
        LimitKind::AddressSpaceBytes | LimitKind::StackBytes => Unit::Bytes,
        LimitKind::OpenStreams | LimitKind::Processes => Unit::Count,
    }
}

/// Map a transport [`CallError`] onto the resolver's error vocabulary.
fn map_call_error(err: CallError) -> ResolveInfoError {
    match err {
        CallError::PermissionDenied => ResolveInfoError::CapabilityDenied,
        CallError::Service(errno) => ResolveInfoError::Service(errno),
    }
}

/// The stable name of a [`TrustDomain`], the spelling `info:process/trust-domain`
/// reports.
fn trust_domain_name(domain: TrustDomain) -> &'static str {
    match domain {
        TrustDomain::Kernel => "kernel",
        TrustDomain::User => "user",
    }
}

/// The OS version as `major.minor.patch`.
fn version_string(identity: &SystemIdentity) -> String {
    let mut s = String::new();
    push_u16(&mut s, identity.version_major);
    s.push('.');
    push_u16(&mut s, identity.version_minor);
    s.push('.');
    push_u16(&mut s, identity.version_patch);
    s
}

/// Append the decimal spelling of `value` to `out`.
fn push_u16(out: &mut String, value: u16) {
    // A `u16` is at most five decimal digits; format without `alloc::fmt`
    // machinery so the helper stays trivially bounded.
    let mut buf = [0u8; 5];
    let mut n = value;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &b in &buf[i..] {
        out.push(b as char);
    }
}

/// The decimal, epoch-relative spelling of an instant, losslessly: whole
/// seconds, and a nine-digit zero-padded fraction only when the sub-second
/// field is non-zero (e.g. `1719936000` or `1719936000.000000040`).
fn time_string(instant: Time64) -> String {
    let mut s = instant.secs().to_string();
    let nanos = instant.subsec_nanos();
    if nanos != 0 {
        s.push('.');
        let digits = nanos.to_string();
        // A canonical sub-second field is `< NANOS_PER_SEC`, so `digits` is at
        // most nine characters; left-pad the shorter cases to keep the place
        // value of each digit.
        for _ in 0..(9 - digits.len()) {
            s.push('0');
        }
        s.push_str(&digits);
    }
    s
}

/// Lowercase-hex encoding of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{resolve, ResolveInfoError};
    use crate::resinfo::{
        Authorization, MetricKind, Producer, ResetBehavior, ResponsePayload, Sensitivity, Unit,
    };
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::origin::{CapabilitySummary, Origin, ProcId, TrustDomain};
    use rustos_abi::sysinfo::{
        CpuLoadRecord, CpuLoadRequest, KernelMemoryStats, MemoryPressureStats, RamzipStats,
        ReclaimClassRecord, ReclaimListRequest, ResourceLimitRecord, SysinfoQueryId,
        SysinfoRequestHeader, SystemIdentity, Uptime, RECLAIM_CLASS_COUNT,
    };
    use rustos_abi::time::{Duration64, Time64};
    use rustos_abi::{CapabilityId, Errno, LimitKind, ResourceLimit};
    use rustos_resref::parse;

    /// An in-memory `sysinfod` stand-in that answers the singleton queries
    /// this resolver uses, decoding the request exactly as the real service
    /// and optionally denying a chosen query.
    struct Fixture {
        identity: SystemIdentity,
        uptime: Uptime,
        memory: KernelMemoryStats,
        origin: Origin,
        limits: [ResourceLimitRecord; LimitKind::COUNT],
        pressure: MemoryPressureStats,
        reclaim: Vec<ReclaimClassRecord>,
        ramzip: RamzipStats,
        cpu_times: Vec<rustos_abi::sysinfo::CpuTimeRecord>,
        cpu_loads: Vec<CpuLoadRecord>,
        deny: Option<SysinfoQueryId>,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    /// The pressure snapshot the fixture serves.
    fn fixture_pressure() -> MemoryPressureStats {
        MemoryPressureStats {
            band: 2,
            reserved: [0u8; 7],
            total_bytes: 1 << 30,
            free_bytes: 96 << 20,
            reserve_bytes: 16 << 20,
            enter_bytes: [204 << 20, 102 << 20, 64 << 20, 32 << 20],
            exit_bytes: [256 << 20, 143 << 20, 81 << 20, 51 << 20],
            band_entries: [0, 3, 2, 0, 0],
        }
    }

    /// One reclaim record per class, figures derived from the class id.
    fn fixture_reclaim() -> Vec<ReclaimClassRecord> {
        (0..RECLAIM_CLASS_COUNT)
            .map(|i| ReclaimClassRecord {
                class: u8::try_from(i).unwrap(),
                reserved: [0u8; 7],
                payload_bytes: (i as u64) * 1000,
                metadata_bytes: (i as u64) * 10,
                entries: i as u64,
                refusals: 0,
                pressure_shrinks: 0,
                teardowns: 0,
                failures: 0,
            })
            .collect()
    }

    /// The `ramzip` snapshot the fixture serves.
    fn fixture_ramzip() -> RamzipStats {
        RamzipStats {
            entries: 4,
            logical_bytes: 16384,
            stored_bytes: 6000,
            ..RamzipStats::default()
        }
    }

    /// Two CPUs' cumulative busy/idle accounting (50% busy overall).
    fn fixture_cpu_times() -> Vec<rustos_abi::sysinfo::CpuTimeRecord> {
        alloc::vec![
            rustos_abi::sysinfo::CpuTimeRecord {
                cpu: 0,
                reserved: 0,
                busy_ns: 750,
                idle_ns: 250,
            },
            rustos_abi::sysinfo::CpuTimeRecord {
                cpu: 1,
                reserved: 0,
                busy_ns: 250,
                idle_ns: 750,
            },
        ]
    }

    /// Two CPUs' load records (42 switches in total).
    fn fixture_cpu_loads() -> Vec<CpuLoadRecord> {
        alloc::vec![
            CpuLoadRecord {
                cpu: 0,
                reserved: 0,
                queue_depth: 1,
                switches: 40,
                preemptions: 5,
            },
            CpuLoadRecord {
                cpu: 1,
                reserved: 0,
                queue_depth: 0,
                switches: 2,
                preemptions: 1,
            },
        ]
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                identity: SystemIdentity::new([0xAB; 16], 1, 2, 3, b"rustbox").expect("identity"),
                uptime: Uptime {
                    since_boot: Duration64::from_secs(4200),
                    boot_time: Time64::from_secs(1000),
                },
                memory: KernelMemoryStats {
                    total_bytes: 8192,
                    free_bytes: 2048,
                    kernel_heap_bytes: 512,
                    user_resident_bytes: 4096,
                    page_size: 4096,
                    reserved: 0,
                },
                origin: Origin::new(
                    TrustDomain::User,
                    1000,
                    50,
                    42,
                    ProcId::from_raw([0xCD; 16]),
                    CapabilitySummary::EMPTY,
                    rustos_abi::ORIGIN_CONSOLE_NONE,
                ),
                // One record per `LimitKind`, in discriminant order. `Processes`
                // is left unlimited so the `unlimited` rendering is exercised.
                limits: [
                    ResourceLimitRecord::new(
                        LimitKind::AddressSpaceBytes,
                        ResourceLimit::new(1_048_576, 2_097_152).expect("well-formed"),
                        4096,
                    ),
                    ResourceLimitRecord::new(
                        LimitKind::OpenStreams,
                        ResourceLimit::new(16, 32).expect("well-formed"),
                        5,
                    ),
                    ResourceLimitRecord::new(LimitKind::Processes, ResourceLimit::UNLIMITED, 3),
                    ResourceLimitRecord::new(
                        LimitKind::StackBytes,
                        ResourceLimit::new(8192, 8192).expect("well-formed"),
                        2048,
                    ),
                ],
                pressure: fixture_pressure(),
                reclaim: fixture_reclaim(),
                ramzip: fixture_ramzip(),
                cpu_times: fixture_cpu_times(),
                cpu_loads: fixture_cpu_loads(),
                deny: None,
                seen: RefCell::new(Vec::new()),
            }
        }

        /// Frame the window of `records` a paged request selects, exactly
        /// as the real service pages whole records.
        fn page_reply<const N: usize>(
            records: &[impl Fn() -> [u8; N]],
            offset: u32,
            limit: u16,
        ) -> Vec<u8> {
            let start = (offset as usize).min(records.len());
            let end = start.saturating_add(limit as usize).min(records.len());
            let mut out = Vec::new();
            for encode in &records[start..end] {
                out.extend_from_slice(&encode());
            }
            out
        }

        /// The `RESOURCE_LIMITS` reply: the four records packed in
        /// discriminant order, exactly as the real service frames it.
        fn limits_report(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for record in &self.limits {
                out.extend_from_slice(&record.to_le_bytes());
            }
            out
        }
    }

    impl crate::transport::Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.deny == Some(header.query) {
                return Err(Errno::PermissionDenied);
            }
            let payload = &request[rustos_abi::sysinfo::SysinfoRequestHeader::WIRE_LEN..];
            match header.query {
                SysinfoQueryId::SYSTEM_IDENTITY => Ok(self.identity.to_le_bytes().to_vec()),
                SysinfoQueryId::UPTIME => Ok(self.uptime.to_le_bytes().to_vec()),
                SysinfoQueryId::KERNEL_MEMORY_STATS => Ok(self.memory.to_le_bytes().to_vec()),
                SysinfoQueryId::PROCESS_IDENTITY => Ok(self.origin.to_le_bytes().to_vec()),
                SysinfoQueryId::RESOURCE_LIMITS => Ok(self.limits_report()),
                SysinfoQueryId::MEMORY_PRESSURE => Ok(self.pressure.to_le_bytes().to_vec()),
                SysinfoQueryId::RAMZIP_STATS => Ok(self.ramzip.to_le_bytes().to_vec()),
                SysinfoQueryId::RECLAIM_STATS => {
                    let req = ReclaimListRequest::from_bytes(payload)?;
                    let encoders: Vec<_> = self
                        .reclaim
                        .iter()
                        .map(|record| move || record.to_le_bytes())
                        .collect();
                    Ok(Self::page_reply(&encoders, req.offset, req.limit))
                }
                SysinfoQueryId::CPU_LOAD => {
                    let req = CpuLoadRequest::from_bytes(payload)?;
                    let encoders: Vec<_> = self
                        .cpu_loads
                        .iter()
                        .map(|record| move || record.to_le_bytes())
                        .collect();
                    Ok(Self::page_reply(&encoders, req.offset, req.limit))
                }
                SysinfoQueryId::CPU_TIME_STATS => {
                    let req = rustos_abi::sysinfo::CpuTimeListRequest::from_bytes(payload)?;
                    let encoders: Vec<_> = self
                        .cpu_times
                        .iter()
                        .map(|record| move || record.to_le_bytes())
                        .collect();
                    Ok(Self::page_reply(&encoders, req.offset, req.limit))
                }
                _ => Err(Errno::NotFound),
            }
        }
    }

    fn now() -> Time64 {
        Time64::from_secs(5200)
    }

    fn resolve_str(
        s: &str,
        fixture: &Fixture,
    ) -> Result<super::ResourceResponse, ResolveInfoError> {
        let reference = parse(s).expect("parse");
        resolve(&reference, now(), fixture)
    }

    #[test]
    fn info_hostname_is_public_text() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/hostname", &fixture).expect("ok");
        assert_eq!(r.producer, Producer::Sysinfod);
        assert_eq!(r.authorization, Authorization::Unprivileged);
        assert_eq!(r.query(), "info:system/hostname");
        match r.payload {
            ResponsePayload::Info(v) => {
                assert_eq!(v.value(), "rustbox");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_kernel_version_is_dotted() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/kernel", &fixture).expect("ok");
        match r.payload {
            ResponsePayload::Info(v) => assert_eq!(v.value(), "1.2.3"),
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_machine_id_is_sensitive_hex() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/machine-id", &fixture).expect("ok");
        match r.payload {
            ResponsePayload::Info(v) => {
                assert_eq!(v.value(), "abababababababababababababababab");
                assert_eq!(v.sensitivity, Sensitivity::Sensitive);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_boot_time_is_public_epoch_seconds() {
        let fixture = Fixture::new();
        let r = resolve_str("info:system/boot-time", &fixture).expect("ok");
        assert_eq!(r.authorization, Authorization::Unprivileged);
        assert_eq!(r.query(), "info:system/boot-time");
        match r.payload {
            ResponsePayload::Info(v) => {
                // The fixture's boot instant is 1000 s with a zero sub-second
                // field, so the fraction is omitted.
                assert_eq!(v.value(), "1000");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_process_identity_fields_are_public_and_self_scoped() {
        let fixture = Fixture::new();
        for (selector, expected) in [
            ("info:process/pid", "42"),
            ("info:process/uid", "1000"),
            ("info:process/gid", "50"),
            ("info:process/proc-id", "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"),
            // The fixture's origin is in the user trust domain and holds no
            // capabilities, so the summary is 32 zero bytes (64 hex zeros).
            ("info:process/trust-domain", "user"),
            (
                "info:process/caps",
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
        ] {
            let r = resolve_str(selector, &fixture).expect("ok");
            assert_eq!(r.authorization, Authorization::Unprivileged);
            assert_eq!(r.query(), selector);
            match r.payload {
                ResponsePayload::Info(v) => {
                    assert_eq!(v.value(), expected);
                    assert_eq!(v.sensitivity, Sensitivity::Public);
                }
                ResponsePayload::Metric(_) => panic!("expected info value"),
            }
        }
        // Every field rode the one self-scoped, ungated identity query.
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .all(|q| *q == SysinfoQueryId::PROCESS_IDENTITY));
    }

    #[test]
    fn info_process_caps_reflects_held_capabilities() {
        let mut fixture = Fixture::new();
        let mut caps = CapabilitySummary::EMPTY;
        caps.insert(CapabilityId::SYSINFO_KERNEL);
        fixture.origin = Origin::new(
            TrustDomain::User,
            1000,
            50,
            42,
            ProcId::from_raw([0xCD; 16]),
            caps,
            rustos_abi::ORIGIN_CONSOLE_NONE,
        );
        let r = resolve_str("info:process/caps", &fixture).expect("ok");
        match r.payload {
            ResponsePayload::Info(v) => {
                // The full 32-byte summary renders as 64 lowercase hex chars,
                // and a held capability makes it something other than all-zero.
                assert_eq!(v.value().len(), 64);
                assert_ne!(v.value(), "0".repeat(64));
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_process_unknown_leaf_fails_closed() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("info:process/parent", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn info_process_malformed_reply_fails_closed() {
        struct Short;
        impl crate::transport::Transport for Short {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; 3])
            }
        }
        let reference = parse("info:process/pid").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Short),
            Err(ResolveInfoError::Malformed)
        );
    }

    #[test]
    fn time_string_pads_and_omits_the_sub_second_fraction() {
        // A zero sub-second field prints no fraction.
        assert_eq!(super::time_string(Time64::from_secs(1000)), "1000");
        // A non-zero sub-second field is nine-digit zero-padded, losslessly.
        assert_eq!(
            super::time_string(Time64::new(1_719_936_000, 40).expect("instant")),
            "1719936000.000000040"
        );
        assert_eq!(
            super::time_string(Time64::new(0, 999_999_999).expect("instant")),
            "0.999999999"
        );
        // Instants before the epoch keep their sign.
        assert_eq!(super::time_string(Time64::from_secs(-5)), "-5");
    }

    #[test]
    fn stats_uptime_is_a_boot_counter_in_seconds() {
        let fixture = Fixture::new();
        let r = resolve_str("stats:uptime", &fixture).expect("ok");
        assert_eq!(r.authorization, Authorization::Unprivileged);
        match r.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "uptime");
                assert_eq!(m.value, 4200);
                assert_eq!(m.kind, MetricKind::Counter);
                assert_eq!(m.unit, Unit::Seconds);
                assert_eq!(m.reset_behavior, ResetBehavior::Boot);
                assert_eq!(m.window, None);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn stats_mem_used_and_available_are_gated_gauges() {
        let fixture = Fixture::new();
        let used = resolve_str("stats:mem/used", &fixture).expect("ok");
        assert_eq!(
            used.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        match used.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "mem/used");
                assert_eq!(m.value, 8192 - 2048);
                assert_eq!(m.kind, MetricKind::Gauge);
                assert_eq!(m.unit, Unit::Bytes);
                assert_eq!(m.reset_behavior, ResetBehavior::Never);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        let avail = resolve_str("stats:mem/available", &fixture).expect("ok");
        match avail.payload {
            ResponsePayload::Metric(m) => assert_eq!(m.value, 2048),
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        let total = resolve_str("stats:mem/total", &fixture).expect("ok");
        match total.payload {
            ResponsePayload::Metric(m) => assert_eq!(m.value, 8192),
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn stats_mem_kernel_heap_and_user_resident_are_gated_gauges() {
        let fixture = Fixture::new();
        let heap = resolve_str("stats:mem/kernel-heap", &fixture).expect("ok");
        assert_eq!(
            heap.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        match heap.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "mem/kernel-heap");
                assert_eq!(m.value, 512);
                assert_eq!(m.kind, MetricKind::Gauge);
                assert_eq!(m.unit, Unit::Bytes);
                assert_eq!(m.reset_behavior, ResetBehavior::Never);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        let resident = resolve_str("stats:mem/user-resident", &fixture).expect("ok");
        match resident.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "mem/user-resident");
                assert_eq!(m.value, 4096);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn info_mem_physical_is_a_gated_public_fact() {
        let fixture = Fixture::new();
        let r = resolve_str("info:mem/physical", &fixture).expect("ok");
        // Total physical memory is carried only by the kernel-memory query, so
        // the answer costs `CAP_SYSINFO_KERNEL` even though the size is public.
        assert_eq!(
            r.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        assert_eq!(r.query(), "info:mem/physical");
        match r.payload {
            ResponsePayload::Info(v) => {
                // The fixture reports 8192 bytes of total memory.
                assert_eq!(v.value(), "8192");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_mem_physical_denial_maps_to_capability_denied() {
        let mut fixture = Fixture::new();
        fixture.deny = Some(SysinfoQueryId::KERNEL_MEMORY_STATS);
        assert_eq!(
            resolve_str("info:mem/physical", &fixture),
            Err(ResolveInfoError::CapabilityDenied)
        );
    }

    #[test]
    fn info_mem_page_size_is_a_gated_public_fact() {
        let fixture = Fixture::new();
        let r = resolve_str("info:mem/page-size", &fixture).expect("ok");
        // The page size rides the same kernel-memory query as `physical`, so
        // the answer costs `CAP_SYSINFO_KERNEL` even though the value is public.
        assert_eq!(
            r.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        assert_eq!(r.query(), "info:mem/page-size");
        match r.payload {
            ResponsePayload::Info(v) => {
                // The fixture reports a 4096-byte page.
                assert_eq!(v.value(), "4096");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn info_mem_unknown_leaf_fails_closed() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("info:mem/used", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn stats_limits_usage_are_unprivileged_gauges() {
        let fixture = Fixture::new();
        // A byte-denominated resource reports its usage in bytes.
        let addr = resolve_str("stats:limits/address-space-bytes", &fixture).expect("ok");
        assert_eq!(addr.authorization, Authorization::Unprivileged);
        match addr.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "limits/address-space-bytes");
                assert_eq!(m.value, 4096);
                assert_eq!(m.kind, MetricKind::Gauge);
                assert_eq!(m.unit, Unit::Bytes);
                assert_eq!(m.reset_behavior, ResetBehavior::Never);
                assert_eq!(m.window, None);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
        // A countable resource reports a dimensionless count.
        let procs = resolve_str("stats:limits/processes", &fixture).expect("ok");
        match procs.payload {
            ResponsePayload::Metric(m) => {
                assert_eq!(m.name(), "limits/processes");
                assert_eq!(m.value, 3);
                assert_eq!(m.unit, Unit::Count);
            }
            ResponsePayload::Info(_) => panic!("expected metric"),
        }
    }

    #[test]
    fn info_limits_bounds_are_public_facts() {
        let fixture = Fixture::new();
        let soft = resolve_str("info:limits/open-streams/soft", &fixture).expect("ok");
        assert_eq!(soft.authorization, Authorization::Unprivileged);
        assert_eq!(soft.query(), "info:limits/open-streams/soft");
        match soft.payload {
            ResponsePayload::Info(v) => {
                assert_eq!(v.value(), "16");
                assert_eq!(v.sensitivity, Sensitivity::Public);
            }
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
        let hard = resolve_str("info:limits/open-streams/hard", &fixture).expect("ok");
        match hard.payload {
            ResponsePayload::Info(v) => assert_eq!(v.value(), "32"),
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
        // An unlimited bound renders as `unlimited`, not as a raw sentinel.
        let unlimited = resolve_str("info:limits/processes/soft", &fixture).expect("ok");
        match unlimited.payload {
            ResponsePayload::Info(v) => assert_eq!(v.value(), "unlimited"),
            ResponsePayload::Metric(_) => panic!("expected info value"),
        }
    }

    #[test]
    fn limits_unknown_kind_or_bound_fails_closed() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("stats:limits/nope", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("info:limits/nope/soft", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        // A known kind but an unknown bound word matches no arm.
        assert_eq!(
            resolve_str("info:limits/processes/median", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn limits_reply_wrong_length_fails_closed() {
        struct Short;
        impl crate::transport::Transport for Short {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; 3])
            }
        }
        let reference = parse("stats:limits/processes").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Short),
            Err(ResolveInfoError::Malformed)
        );
    }

    #[test]
    fn limits_reply_kind_out_of_order_fails_closed() {
        // A full-length report whose records are packed in the wrong order:
        // the self-describing `kind` no longer matches its slot, so the reply
        // is rejected rather than mis-attributed.
        struct Scrambled;
        impl crate::transport::Transport for Scrambled {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                let mut out = Vec::new();
                // `OpenStreams` sits where `AddressSpaceBytes` (slot 0) belongs.
                for kind in [
                    LimitKind::OpenStreams,
                    LimitKind::AddressSpaceBytes,
                    LimitKind::Processes,
                    LimitKind::StackBytes,
                ] {
                    out.extend_from_slice(
                        &ResourceLimitRecord::new(kind, ResourceLimit::UNLIMITED, 0).to_le_bytes(),
                    );
                }
                Ok(out)
            }
        }
        let reference = parse("info:limits/open-streams/soft").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Scrambled),
            Err(ResolveInfoError::Malformed)
        );
    }

    #[test]
    fn denied_kernel_memory_maps_to_capability_denied() {
        let mut fixture = Fixture::new();
        fixture.deny = Some(SysinfoQueryId::KERNEL_MEMORY_STATS);
        assert_eq!(
            resolve_str("stats:mem/used", &fixture),
            Err(ResolveInfoError::CapabilityDenied)
        );
    }

    #[test]
    fn unknown_selectors_fail_closed() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("info:system/nope", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("stats:mem/pagefaults", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("stats:cpu/nope", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("stats:mem/ramzip/ratio", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("stats:mem/reclaim/page-cache", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn stats_cpu_load_is_an_unprivileged_busy_share() {
        let fixture = Fixture::new();
        let response = resolve_str("stats:cpu/load", &fixture).expect("resolves");
        assert_eq!(response.authorization, Authorization::Unprivileged);
        let ResponsePayload::Metric(metric) = &response.payload else {
            panic!("expected a metric");
        };
        // 1000 busy of 2000 total nanoseconds across both CPUs.
        assert_eq!(metric.value, 50);
        assert_eq!(metric.unit, Unit::Percent);
        assert_eq!(metric.kind, MetricKind::Gauge);
    }

    #[test]
    fn stats_cpu_indexed_load_resolves_and_unknown_cpu_fails_closed() {
        let fixture = Fixture::new();
        let response = resolve_str("stats:cpu/1/load", &fixture).expect("resolves");
        let ResponsePayload::Metric(metric) = &response.payload else {
            panic!("expected a metric");
        };
        assert_eq!(metric.value, 25);
        assert_eq!(metric.name(), "cpu/1/load");
        assert_eq!(
            resolve_str("stats:cpu/9/load", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
        assert_eq!(
            resolve_str("stats:cpu/one/load", &fixture),
            Err(ResolveInfoError::UnknownSelector)
        );
    }

    #[test]
    fn stats_cpu_switches_is_a_gated_counter() {
        let fixture = Fixture::new();
        let response = resolve_str("stats:cpu/switches", &fixture).expect("resolves");
        assert_eq!(
            response.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        let ResponsePayload::Metric(metric) = &response.payload else {
            panic!("expected a metric");
        };
        assert_eq!(metric.value, 42);
        assert_eq!(metric.kind, MetricKind::Counter);

        // A broker denial maps to the capability error, never a guess.
        let mut denied = Fixture::new();
        denied.deny = Some(SysinfoQueryId::CPU_LOAD);
        assert_eq!(
            resolve_str("stats:cpu/switches", &denied),
            Err(ResolveInfoError::CapabilityDenied)
        );
    }

    #[test]
    fn info_cpu_count_is_a_gated_fact() {
        let fixture = Fixture::new();
        let response = resolve_str("info:cpu/count", &fixture).expect("resolves");
        assert_eq!(
            response.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        let ResponsePayload::Info(value) = &response.payload else {
            panic!("expected an info value");
        };
        assert_eq!(value.value(), "2");

        let mut denied = Fixture::new();
        denied.deny = Some(SysinfoQueryId::CPU_LOAD);
        assert_eq!(
            resolve_str("info:cpu/count", &denied),
            Err(ResolveInfoError::CapabilityDenied)
        );
    }

    #[test]
    fn stats_mem_pressure_is_a_named_band_gauge_with_transitions() {
        let fixture = Fixture::new();
        let response = resolve_str("stats:mem/pressure", &fixture).expect("resolves");
        assert_eq!(
            response.authorization,
            Authorization::Capability(CapabilityId::SYSINFO_KERNEL)
        );
        let ResponsePayload::Metric(metric) = &response.payload else {
            panic!("expected a metric");
        };
        assert_eq!(metric.value, 2);
        assert_eq!(metric.name(), "mem/pressure/moderate");

        let response = resolve_str("stats:mem/pressure/transitions", &fixture).expect("resolves");
        let ResponsePayload::Metric(metric) = &response.payload else {
            panic!("expected a metric");
        };
        assert_eq!(metric.value, 5);
        assert_eq!(metric.kind, MetricKind::Counter);

        let mut denied = Fixture::new();
        denied.deny = Some(SysinfoQueryId::MEMORY_PRESSURE);
        assert_eq!(
            resolve_str("stats:mem/pressure", &denied),
            Err(ResolveInfoError::CapabilityDenied)
        );
    }

    #[test]
    fn stats_mem_reclaim_total_and_class_are_byte_gauges() {
        let fixture = Fixture::new();
        let response = resolve_str("stats:mem/reclaim/total", &fixture).expect("resolves");
        let ResponsePayload::Metric(metric) = &response.payload else {
            panic!("expected a metric");
        };
        // Sum of (i * 1000 + i * 10) for i in 0..9.
        assert_eq!(metric.value, 36 * 1010);
        assert_eq!(metric.unit, Unit::Bytes);

        let response =
            resolve_str("stats:mem/reclaim/clean-file-data", &fixture).expect("resolves");
        let ResponsePayload::Metric(metric) = &response.payload else {
            panic!("expected a metric");
        };
        // Class id 5: 5 * 1000 + 5 * 10.
        assert_eq!(metric.value, 5050);
        assert_eq!(metric.name(), "mem/reclaim/clean-file-data");
    }

    #[test]
    fn stats_mem_ramzip_gauges_report_stored_logical_and_saved() {
        let fixture = Fixture::new();
        for (leaf, expected) in [("stored", 6000), ("logical", 16384), ("saved", 10384)] {
            let mut reference = String::from("stats:mem/ramzip/");
            reference.push_str(leaf);
            let response = resolve_str(&reference, &fixture).expect("resolves");
            let ResponsePayload::Metric(metric) = &response.payload else {
                panic!("expected a metric");
            };
            assert_eq!(metric.value, expected, "{leaf}");
            assert_eq!(metric.unit, Unit::Bytes);
        }
    }

    #[test]
    fn decorations_are_unserviceable() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("info:system/hostname::record", &fixture),
            Err(ResolveInfoError::UnsupportedRequest)
        );
        assert_eq!(
            resolve_str("stats:uptime?window=1s", &fixture),
            Err(ResolveInfoError::UnsupportedRequest)
        );
    }

    #[test]
    fn wrong_namespace_is_not_ours() {
        let fixture = Fixture::new();
        assert_eq!(
            resolve_str("sys:random", &fixture),
            Err(ResolveInfoError::NotInfoOrStats)
        );
    }

    #[test]
    fn malformed_reply_fails_closed() {
        struct Short;
        impl crate::transport::Transport for Short {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Ok(alloc::vec![0u8; 3])
            }
        }
        let reference = parse("info:system/hostname").expect("parse");
        assert_eq!(
            resolve(&reference, now(), &Short),
            Err(ResolveInfoError::Malformed)
        );
    }
}
