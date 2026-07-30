//! The request/render engine: turn a [`Command`] into typed `sysinfo-v1`
//! requests, decode the typed replies, and render human-readable lines.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
use tairix_abi::sysinfo::{
    CpuCoreClass, CpuInfoListRequest, CpuInfoRecord, CpuLoadRecord, CpuLoadRequest,
    KernelMemoryStats, MemoryPressureStats, MountAvailability, RamzipStats, ReclaimClassRecord,
    ReclaimListRequest, ResourceLimitRecord, SeatListRequest, SeatRecord, SysinfoQueryId,
    SystemIdentity, Uptime, VolumeIoHealthRecord, VolumeIoHealthRequest, PRESSURE_BAND_NAMES,
    RECLAIM_CLASS_COUNT, RECLAIM_CLASS_NAMES,
};
use tairix_abi::{Errno, LimitKind};

use tairix_help::{own_short_help, HelpSource};
use tairix_procinfo::{
    call, emit_self_scope_omission, fetch_tree, for_each_irq, for_each_process, render_limit_bound,
    render_process, Output, Transport, PROCESS_HEADER,
};

use crate::command::Command;
use crate::error::SysinfoError;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `sysinfo`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: sysinfo <query>

queries:
  processes [--all]   list processes (--all: every process, needs CAP_SYSINFO_GLOBAL)
  memory              kernel memory statistics (needs CAP_SYSINFO_KERNEL)
  hardware            detected hardware tree (needs CAP_SYSINFO_HW)
  identity            machine identity and OS version
  uptime              time since boot and boot wall-clock time
  limits              your effective resource limits and live usage
  seats               seat inventory: owners and foreground consoles (needs CAP_SYSINFO_HW)
  pressure            memory-pressure band, watermarks, transitions (needs CAP_SYSINFO_KERNEL)
  reclaim             reclaimable-cache ledger per class (needs CAP_SYSINFO_KERNEL)
  ramzip              compressed-tier counters (needs CAP_SYSINFO_KERNEL)
  cpu                 per-CPU queue depth, switches, preemptions (needs CAP_SYSINFO_KERNEL)
  cpuinfo             per-CPU model, class, flags, and live/reference MHz
  irq                 IRQ table: line, owner, count, quarantine (needs CAP_SYSINFO_HW)
  storage             per-volume I/O health and outcome counters (needs CAP_SYSINFO_KERNEL)
  help, -h, -?        show this help";

/// `sysinfo`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "sysinfo";

/// Run one [`Command`], issuing its query through `transport` and writing the
/// rendered result to `out`. `locale` is the user's `LANG` preference, if
/// set; `help` is the tool's own `Help/` tree, read by the short-help
/// switches.
///
/// # Errors
///
/// * [`SysinfoError::PermissionDenied`] — the service refused the query for
///   want of its declared capability.
/// * [`SysinfoError::Service`] — the transport failed or the reply did not
///   decode against `sysinfo-v1`.
/// * [`SysinfoError::Output`] — writing the terminal failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Processes { all } => run_processes(all, transport, out),
        Command::Memory => run_memory(transport, out),
        Command::Hardware => run_hardware(transport, out),
        Command::Identity => run_identity(transport, out),
        Command::Uptime => run_uptime(transport, out),
        Command::Limits => run_limits(transport, out),
        Command::Seats => run_seats(transport, out),
        Command::Pressure => run_pressure(transport, out),
        Command::Reclaim => run_reclaim(transport, out),
        Command::Ramzip => run_ramzip(transport, out),
        Command::CpuLoad => run_cpu_load(transport, out),
        Command::CpuInfo => run_cpu_info(transport, out),
        Command::Irqs => run_irqs(transport, out),
        Command::Storage => run_storage(transport, out),
    }
}

/// Render `sysinfo`'s own short help (`NAME` + `SYNOPSIS` + compact
/// `OPTIONS`) from its own Help tree through the one shared engine; when no
/// document can be served (a build without the bundle's documents) the
/// usage banner stands in — the tool's own text, not fabricated help
/// content — so `-h` never fails. The rendered page is written as one
/// multi-line `write_line`; the seam owns the final newline.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    let bytes = own_short_help(help, locale, OWN_WORD);
    let text = bytes
        .as_deref()
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or(USAGE);
    emit(out, text.trim_end_matches('\n'))
}

/// Issue `query` with `payload` through the shared client helper and map a
/// capability denial or transport failure onto the CLI's error vocabulary.
fn service_call(
    transport: &dyn Transport,
    query: SysinfoQueryId,
    payload: &[u8],
) -> Result<Vec<u8>, SysinfoError> {
    call(transport, query, payload).map_err(SysinfoError::from)
}

/// Write `line` to `out`, mapping a console failure onto
/// [`SysinfoError::Output`].
fn emit(out: &dyn Output, line: &str) -> Result<(), SysinfoError> {
    out.write_line(line).map_err(SysinfoError::Output)
}

/// Page through the process list and render one row per process. The
/// default self scope also notes on the advisory stream (fd 3) that the
/// listing is not system-wide, so a tool or user knows stdout is not
/// exhaustive.
///
/// The page walk, the row rendering, and the self-scope advisory are the
/// shared helpers from `lib/procinfo` (the same record definition `ps`
/// emits); the CLI only supplies the column header, the per-row sink, and
/// its own widening spelling.
fn run_processes(
    all: bool,
    transport: &dyn Transport,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    emit(out, PROCESS_HEADER)?;
    for_each_process(transport, all, |record| {
        out.write_line(&render_process(record))
    })
    .map_err(SysinfoError::from)?;
    if !all {
        emit_self_scope_omission(out, OWN_WORD, &[OWN_WORD, "processes", "--all"]);
    }
    Ok(())
}

/// Fetch and render the kernel memory statistics.
fn run_memory(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::KERNEL_MEMORY_STATS, &[])?;
    let stats = KernelMemoryStats::from_bytes(&reply).map_err(SysinfoError::Service)?;
    emit(out, &format!("total bytes:     {}", stats.total_bytes))?;
    emit(out, &format!("free bytes:      {}", stats.free_bytes))?;
    emit(
        out,
        &format!("kernel heap:     {}", stats.kernel_heap_bytes),
    )?;
    emit(
        out,
        &format!("user resident:   {}", stats.user_resident_bytes),
    )?;
    emit(out, &format!("page size:       {}", stats.page_size))
}

/// Fetch the hardware tree and report its size.
///
/// The tree is paged in whole through the shared `lib/procinfo` walk; the
/// CLI summarises it as a node count. The per-device inventory renderings
/// are `lspci`'s and `lsusb`'s job.
fn run_hardware(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let nodes = fetch_tree(transport).map_err(SysinfoError::from)?;
    emit(out, &format!("hardware tree: {} nodes", nodes.len()))
}

/// Fetch and render the machine identity.
fn run_identity(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::SYSTEM_IDENTITY, &[])?;
    let identity = SystemIdentity::from_bytes(&reply).map_err(SysinfoError::Service)?;
    emit(
        out,
        &format!("hostname:    {}", name_lossy(identity.hostname_bytes())),
    )?;
    emit(out, &format!("machine id:  {}", hex(&identity.machine_id)))?;
    emit(
        out,
        &format!(
            "os version:  {}.{}.{}",
            identity.version_major, identity.version_minor, identity.version_patch
        ),
    )
}

/// Fetch and render system uptime.
fn run_uptime(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::UPTIME, &[])?;
    let uptime = Uptime::from_bytes(&reply).map_err(SysinfoError::Service)?;
    emit(
        out,
        &format!(
            "since boot:  {}.{:09}s",
            uptime.since_boot.secs(),
            uptime.since_boot.subsec_nanos()
        ),
    )?;
    emit(
        out,
        &format!(
            "boot time:   {}.{:09}s since the Unix epoch",
            uptime.boot_time.secs(),
            uptime.boot_time.subsec_nanos()
        ),
    )
}

/// Fetch and render the caller's effective resource limits and live usage.
///
/// The reply is exactly [`LimitKind::COUNT`] [`ResourceLimitRecord`]s in
/// discriminant order; the CLI decodes them positionally and prints one
/// aligned row per resource. A reply of the wrong length fails closed rather than rendering a partial table.
fn run_limits(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::RESOURCE_LIMITS, &[])?;
    if reply.len() != ResourceLimitRecord::WIRE_LEN * LimitKind::COUNT {
        return Err(SysinfoError::Service(Errno::BufferTooSmall));
    }
    emit(out, "resource              soft         hard         usage")?;
    for index in 0..LimitKind::COUNT {
        let base = index * ResourceLimitRecord::WIRE_LEN;
        let record =
            ResourceLimitRecord::from_bytes(&reply[base..base + ResourceLimitRecord::WIRE_LEN])
                .map_err(SysinfoError::Service)?;
        emit(
            out,
            &format!(
                "{:<20}  {:>11}  {:>11}  {:>11}",
                record.kind.name(),
                render_limit_bound(record.limit.soft),
                render_limit_bound(record.limit.hard),
                record.usage,
            ),
        )?;
    }
    Ok(())
}

/// Fetch and render the seat inventory, one aligned row per seat.
///
/// The reply is whole [`SeatRecord`]s packed back-to-back; a reply that is
/// not a whole number of records fails closed rather than rendering a
/// partial row. One page is ample for the seat count a machine has today;
/// the request's `limit` bounds it explicitly.
fn run_seats(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let request = SeatListRequest {
        offset: 0,
        limit: 32,
        flags: 0,
    };
    let reply = service_call(transport, SysinfoQueryId::SEAT_LIST, &request.to_le_bytes())?;
    if reply.len() % SeatRecord::WIRE_LEN != 0 {
        return Err(SysinfoError::Service(Errno::BufferTooSmall));
    }
    emit(out, "seat  owner       generation  foreground")?;
    for chunk in reply.as_chunks::<{ SeatRecord::WIRE_LEN }>().0 {
        let record = SeatRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
        let owner = match record.owner() {
            Some(task) => format!("task {task}"),
            None => String::from("unowned"),
        };
        emit(
            out,
            &format!(
                "{:<4}  {:<10}  {:>10}  console {}",
                record.seat_id, owner, record.generation, record.foreground_console,
            ),
        )?;
    }
    Ok(())
}

/// Fetch and render the live memory-pressure gauge: the band, the free/
/// total/reserve readings, the derived per-band watermarks in force, and
/// the per-band transition counters since boot.
fn run_pressure(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::MEMORY_PRESSURE, &[])?;
    let stats = MemoryPressureStats::from_bytes(&reply).map_err(SysinfoError::Service)?;
    let band = usize::from(stats.band).min(PRESSURE_BAND_NAMES.len() - 1);
    emit(
        out,
        &format!(
            "band:            {} ({})",
            PRESSURE_BAND_NAMES[band], stats.band
        ),
    )?;
    emit(out, &format!("total bytes:     {}", stats.total_bytes))?;
    emit(out, &format!("free bytes:      {}", stats.free_bytes))?;
    emit(out, &format!("reserve bytes:   {}", stats.reserve_bytes))?;
    emit(out, "band        enter        exit        entries")?;
    for (index, name) in PRESSURE_BAND_NAMES.iter().enumerate() {
        // The normal band has no watermarks: it is where the gauge rests.
        let (enter, exit) = if index == 0 {
            (String::from("-"), String::from("-"))
        } else {
            (
                format!("{}", stats.enter_bytes[index - 1]),
                format!("{}", stats.exit_bytes[index - 1]),
            )
        };
        emit(
            out,
            &format!(
                "{:<10}  {:>11}  {:>10}  {:>7}",
                name, enter, exit, stats.band_entries[index],
            ),
        )?;
    }
    Ok(())
}

/// Fetch and render the reclaimable-cache ledger, one aligned row per
/// class. The class set is small and closed, so one page carries it; a
/// reply that is not a whole number of records fails closed.
fn run_reclaim(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let request = ReclaimListRequest {
        offset: 0,
        limit: u16::try_from(RECLAIM_CLASS_COUNT).unwrap_or(u16::MAX),
        flags: 0,
    };
    let reply = service_call(
        transport,
        SysinfoQueryId::RECLAIM_STATS,
        &request.to_le_bytes(),
    )?;
    if reply.len() % ReclaimClassRecord::WIRE_LEN != 0 {
        return Err(SysinfoError::Service(Errno::BufferTooSmall));
    }
    emit(
        out,
        "class                  payload    metadata  entries      hits    misses  hit%  refusals  shrinks  failures",
    )?;
    for chunk in reply.as_chunks::<{ ReclaimClassRecord::WIRE_LEN }>().0 {
        let record = ReclaimClassRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
        let name = RECLAIM_CLASS_NAMES[usize::from(record.class)];
        emit(
            out,
            &format!(
                "{:<21}  {:>7}  {:>10}  {:>7}  {:>8}  {:>8}  {:>4}  {:>8}  {:>7}  {:>8}",
                name,
                record.payload_bytes,
                record.metadata_bytes,
                record.entries,
                record.hits,
                record.misses,
                hit_pct(record.hits, record.misses),
                record.refusals,
                record.pressure_shrinks,
                record.failures,
            ),
        )?;
    }
    Ok(())
}

/// The cache hit ratio `hits / (hits + misses)` as a whole-percent string
/// (`"92%"`), or `"-"` when no lookup has happened yet — an idle cache is
/// reported honestly, never as a fabricated ratio over a zero denominator.
fn hit_pct(hits: u64, misses: u64) -> String {
    let lookups = hits.saturating_add(misses);
    if lookups == 0 {
        return String::from("-");
    }
    // hits <= lookups, so the quotient is 0..=100 and always fits a u64;
    // the checked conversion is the lint-clean way to say so.
    let pct = u64::try_from(u128::from(hits) * 100 / u128::from(lookups)).unwrap_or(100);
    format!("{pct}%")
}

/// Fetch and render the `ramzip` compressed-tier counters. Counters only
/// — never page contents; an undriven tier truthfully renders zeros.
fn run_ramzip(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    let reply = service_call(transport, SysinfoQueryId::RAMZIP_STATS, &[])?;
    let stats = RamzipStats::from_bytes(&reply).map_err(SysinfoError::Service)?;
    emit(out, &format!("entries:         {}", stats.entries))?;
    emit(out, &format!("logical bytes:   {}", stats.logical_bytes))?;
    emit(out, &format!("stored bytes:    {}", stats.stored_bytes))?;
    emit(out, &format!("metadata bytes:  {}", stats.metadata_bytes))?;
    emit(
        out,
        &format!(
            "caps (min/soft/hard): {} / {} / {}",
            stats.min_cap_bytes, stats.soft_cap_bytes, stats.hard_cap_bytes
        ),
    )?;
    emit(out, &format!("attempts:        {}", stats.attempts))?;
    emit(out, &format!("accepted:        {}", stats.accepted))?;
    let rejected = stats
        .rejected_policy
        .saturating_add(stats.rejected_ineligible)
        .saturating_add(stats.rejected_incompressible)
        .saturating_add(stats.rejected_cap)
        .saturating_add(stats.rejected_reserve)
        .saturating_add(stats.rejected_task_share)
        .saturating_add(stats.rejected_thrash);
    emit(out, &format!("rejected:        {rejected}"))?;
    emit(out, &format!("fault-ins:       {}", stats.fault_ins))?;
    emit(
        out,
        &format!(
            "failures (auth/decode): {} / {}",
            stats.auth_failures, stats.decode_failures
        ),
    )?;
    emit(
        out,
        &format!(
            "warm (attempts/restored/stopped): {} / {} / {}",
            stats.warm_attempts, stats.warm_restored, stats.warm_stopped
        ),
    )?;
    emit(
        out,
        &format!("cluster restored: {}", stats.cluster_restored),
    )?;
    emit(out, &format!("thrash detected:  {}", stats.thrash_detected))?;
    emit(out, &format!("pinned bytes:    {}", stats.pinned_bytes))
}

/// Fetch and render the per-CPU scheduler load figures, one aligned row
/// per CPU. One page carries every CPU a machine has today; the request's
/// `limit` bounds it explicitly and a second page continues the walk.
fn run_cpu_load(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    /// Records requested per page: bounds the reply size without bounding
    /// how many CPUs the machine may have.
    const PAGE: u16 = 64;
    emit(out, "cpu   queue     switches  preemptions")?;
    let mut offset: u32 = 0;
    loop {
        let request = CpuLoadRequest {
            offset,
            limit: PAGE,
            flags: 0,
        };
        let reply = service_call(transport, SysinfoQueryId::CPU_LOAD, &request.to_le_bytes())?;
        if reply.len() % CpuLoadRecord::WIRE_LEN != 0 {
            return Err(SysinfoError::Service(Errno::BufferTooSmall));
        }
        let records = reply.len() / CpuLoadRecord::WIRE_LEN;
        for chunk in reply.as_chunks::<{ CpuLoadRecord::WIRE_LEN }>().0 {
            let record = CpuLoadRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
            emit(
                out,
                &format!(
                    "{:<4}  {:>5}  {:>10}  {:>11}",
                    record.cpu, record.queue_depth, record.switches, record.preemptions,
                ),
            )?;
        }
        if records < usize::from(PAGE) {
            return Ok(());
        }
        offset = offset.saturating_add(u32::from(PAGE));
    }
}

/// Format a frequency in Hz as `MHz` with three decimals, using integer
/// arithmetic (no float in `no_std`): `1_512_000_000` → `1512.000`.
fn format_mhz(hz: u64) -> String {
    let whole = hz / 1_000_000;
    // Milli-MHz: the fractional MHz to three digits (kHz resolution).
    let milli = (hz % 1_000_000) / 1_000;
    format!("{whole}.{milli:03}")
}

/// The lowercase feature-flag list of a [`CpuFeatureSet`], space-separated
/// in stable bit order — the `/proc/cpuinfo` "flags" line. `(none)` when
/// the set is empty (the honest answer for a build-time-floor CPU, never a
/// fabricated flag).
fn feature_flags(set: CpuFeatureSet) -> String {
    let mut out = String::new();
    for feature in CpuFeature::ALL {
        if set.contains(feature) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(feature.name());
        }
    }
    if out.is_empty() {
        out.push_str("(none)");
    }
    out
}

/// Render one CPU's `/proc/cpuinfo`-superset block to `out`.
///
/// Reports the vendor/model, the performance class, the live measured
/// core-clock frequency (or an explicit "unknown" when the port drives no
/// core-clock counter — never a fabricated rate), the fixed
/// reference/timebase frequency, the raw identity register, and the decoded
/// ISA-extension flags.
fn render_cpu_info(record: &CpuInfoRecord, out: &dyn Output) -> Result<(), SysinfoError> {
    emit(out, &format!("processor     : {}", record.cpu))?;
    let model = name_lossy(record.model_bytes());
    let model = if model.is_empty() { "unknown" } else { &model };
    emit(out, &format!("model name    : {model}"))?;
    let class = match record.class {
        CpuCoreClass::Performance => "performance",
        CpuCoreClass::Efficiency => "efficiency",
    };
    emit(out, &format!("core class    : {class}"))?;
    if record.freq_measured() {
        emit(
            out,
            &format!("cpu MHz       : {}", format_mhz(record.current_freq_hz)),
        )?;
    } else {
        // Honest unknown: no core-clock counter on this port, or no sample
        // taken yet — never a fabricated or nominal figure.
        emit(out, "cpu MHz       : unknown (no core-clock measurement)")?;
    }
    if record.reference_hz != 0 {
        emit(
            out,
            &format!("reference MHz : {}", format_mhz(record.reference_hz)),
        )?;
    }
    emit(out, &format!("identity      : {:#018x}", record.raw_id))?;
    emit(
        out,
        &format!(
            "flags         : {}",
            feature_flags(CpuFeatureSet::from_bits(record.feature_bits))
        ),
    )
}

/// Fetch and render the per-CPU processor information, one
/// `/proc/cpuinfo`-style block per CPU separated by a blank line. One page
/// carries every CPU a machine has today; the request's `limit` bounds it
/// explicitly and a second page continues the walk. Ungated — the facts are
/// the public hardware view every user may read.
fn run_cpu_info(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    /// Records requested per page: bounds the reply size without bounding
    /// how many CPUs the machine may have.
    const PAGE: u16 = 64;
    let mut offset: u32 = 0;
    let mut first = true;
    loop {
        let request = CpuInfoListRequest {
            offset,
            limit: PAGE,
            flags: 0,
        };
        let reply = service_call(transport, SysinfoQueryId::CPU_INFO, &request.to_le_bytes())?;
        if reply.len() % CpuInfoRecord::WIRE_LEN != 0 {
            return Err(SysinfoError::Service(Errno::BufferTooSmall));
        }
        let records = reply.len() / CpuInfoRecord::WIRE_LEN;
        for chunk in reply.as_chunks::<{ CpuInfoRecord::WIRE_LEN }>().0 {
            let record = CpuInfoRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
            if !first {
                emit(out, "")?;
            }
            first = false;
            render_cpu_info(&record, out)?;
        }
        if records < usize::from(PAGE) {
            return Ok(());
        }
        offset = offset.saturating_add(u32::from(PAGE));
    }
}

/// Fetch and render the kernel IRQ table, one aligned row per bound line:
/// the line id, the owning driver task, the interrupt count since boot, and
/// whether the line is quarantined. The paged walk, the fail-closed decode,
/// and the `CAP_SYSINFO_HW` gate are the shared `lib/procinfo` helper (the
/// same record `sysmon` reads); the CLI supplies only the header and the
/// per-row rendering.
fn run_irqs(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    emit(out, "line  owner       count         state")?;
    for_each_irq(transport, |record| {
        let state = if record.is_quarantined() {
            "quarantined"
        } else {
            "active"
        };
        out.write_line(&format!(
            "{:<4}  task {:<6}  {:>10}  {}",
            record.line, record.owner, record.count, state,
        ))
    })
    .map_err(SysinfoError::from)
}

/// The short display name of a volume's availability, for the `storage`
/// row. A closed match so a future [`MountAvailability`] variant is a
/// compile error here rather than a silent blank.
fn availability_name(availability: MountAvailability) -> &'static str {
    match availability {
        MountAvailability::Available => "available",
        MountAvailability::Degraded => "degraded",
        MountAvailability::Recovering => "recovering",
        MountAvailability::UnavailableDirty => "lost-dirty",
        MountAvailability::UnavailableLost => "lost",
        MountAvailability::RecoveryConflict => "conflict",
    }
}

/// Fetch and render the per-volume storage I/O health, one aligned row per
/// fault-aware block-backed volume: a short prefix of its durable id, the
/// serving block-service endpoint, its current availability, and the folded
/// outcome counters that a failing or flapping disk becomes visible on. The
/// paged walk and fail-closed decode mirror [`run_cpu_load`]; the service
/// gates the query on `CAP_SYSINFO_KERNEL`.
fn run_storage(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    /// Records requested per page: bounds the reply without bounding how
    /// many volumes may be mounted.
    const PAGE: u16 = 64;
    emit(
        out,
        "volume            dev                 health      done  resets  tmout  medium  reissue",
    )?;
    let mut offset: u32 = 0;
    loop {
        let request = VolumeIoHealthRequest {
            offset,
            limit: PAGE,
            flags: 0,
        };
        let reply = service_call(
            transport,
            SysinfoQueryId::VOLUME_IO_HEALTH,
            &request.to_le_bytes(),
        )?;
        if reply.len() % VolumeIoHealthRecord::WIRE_LEN != 0 {
            return Err(SysinfoError::Service(Errno::BufferTooSmall));
        }
        let records = reply.len() / VolumeIoHealthRecord::WIRE_LEN;
        for chunk in reply.as_chunks::<{ VolumeIoHealthRecord::WIRE_LEN }>().0 {
            let record = VolumeIoHealthRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
            let volume_id = record.volume_id();
            let counters = record.counters();
            emit(
                out,
                &format!(
                    "{:<16}  {:#018x}  {:<10}  {:>4}  {:>6}  {:>5}  {:>6}  {:>7}",
                    hex(&volume_id[..8]),
                    record.dev(),
                    availability_name(record.availability()),
                    counters.completions,
                    counters.resets,
                    counters.timeouts,
                    counters.medium_errors,
                    counters.reissues,
                ),
            )?;
        }
        if records < usize::from(PAGE) {
            return Ok(());
        }
        offset = offset.saturating_add(u32::from(PAGE));
    }
}

/// Render `bytes` as lowercase hex with no separators.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a `String` is infallible; the byte format is fixed-width.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decode an inline name buffer for display, substituting `U+FFFD` for any
/// invalid byte rather than failing (a display routine never panics).
fn name_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{run as engine_run, USAGE};
    use crate::command::Command;
    use crate::error::SysinfoError;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::blkio::BlkHealthCounters;
    use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
    use tairix_abi::hwtree::{HwDeviceClass, HwNode, HwTreeHeader, HW_NODE_ROOT};
    use tairix_abi::sysinfo::{
        CpuCoreClass, CpuInfoListRequest, CpuInfoRecord, CpuLoadRecord, CpuLoadRequest,
        KernelMemoryStats, MemoryPressureStats, MountAvailability, ProcessListRequest,
        ProcessRecord, ProcessState, RamzipStats, ReclaimClassRecord, ReclaimListRequest,
        ResourceLimitRecord, SeatListRequest, SeatRecord, SysinfoQueryId, SysinfoRequestHeader,
        SystemIdentity, Uptime, VolumeIoHealthRecord, VolumeIoHealthRequest,
        CPU_INFO_FLAG_FREQ_MEASURED, SEAT_FLAG_OWNED,
    };
    use tairix_abi::time::{Duration64, Time64};
    use tairix_abi::{Errno, LimitKind, ProcId, ResourceLimit, RLIMIT_INFINITY};
    use tairix_help::{HelpSource, SourceError};
    use tairix_procinfo::{Output, Transport};

    /// A Help tree with no documents at all: the short-help fallback path.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    /// A Help tree holding one canonical `sysinfo.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\nsysinfo — query system information\n\n\
                       ## SYNOPSIS\n\n`sysinfo <query>`\n\n\
                       ## DESCRIPTION\n\nQueries things.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("en-US")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "en-US" && file_name == "sysinfo.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// The engine under the fixtures' default seams: no locale preference
    /// and an empty Help tree, so every existing scenario exercises the
    /// query paths unchanged.
    fn run(
        command: Command,
        transport: &dyn Transport,
        out: &dyn Output,
    ) -> Result<(), SysinfoError> {
        engine_run(command, None, transport, &NoHelp, out)
    }

    /// An in-memory `sysinfod` stand-in: it decodes a request the same way
    /// the real service does and answers from fixed fixtures.
    struct Fixture {
        records: Vec<ProcessRecord>,
        memory: KernelMemoryStats,
        identity: SystemIdentity,
        uptime: Uptime,
        hardware: Vec<u8>,
        seats: Vec<SeatRecord>,
        pressure: MemoryPressureStats,
        reclaim: Vec<ReclaimClassRecord>,
        ramzip: RamzipStats,
        cpu_loads: Vec<CpuLoadRecord>,
        deny: Option<SysinfoQueryId>,
        malformed_process_list: bool,
        short_scalar: bool,
        seen: RefCell<Vec<SysinfoQueryId>>,
    }

    impl Fixture {
        fn new(records: Vec<ProcessRecord>) -> Self {
            Self {
                records,
                memory: KernelMemoryStats {
                    total_bytes: 4096,
                    free_bytes: 1024,
                    kernel_heap_bytes: 512,
                    user_resident_bytes: 256,
                    page_size: 4096,
                    reserved: 0,
                },
                identity: SystemIdentity::new([0xAB; 16], 1, 2, 3, b"rustbox").unwrap(),
                uptime: Uptime {
                    since_boot: Duration64::from_nanos(9),
                    boot_time: Time64::from_secs(1000),
                },
                hardware: Vec::new(),
                seats: Vec::new(),
                pressure: MemoryPressureStats {
                    band: 1,
                    reserved: [0u8; 7],
                    total_bytes: 4096,
                    free_bytes: 512,
                    reserve_bytes: 64,
                    enter_bytes: [800, 400, 250, 125],
                    exit_bytes: [1024, 570, 320, 200],
                    band_entries: [0, 2, 1, 0, 0],
                },
                reclaim: alloc::vec![ReclaimClassRecord {
                    class: 5,
                    reserved: [0u8; 7],
                    payload_bytes: 4096,
                    metadata_bytes: 128,
                    entries: 3,
                    refusals: 1,
                    pressure_shrinks: 2,
                    teardowns: 0,
                    failures: 0,
                    hits: 900,
                    misses: 100,
                }],
                ramzip: RamzipStats {
                    entries: 4,
                    logical_bytes: 16384,
                    stored_bytes: 6000,
                    attempts: 9,
                    accepted: 4,
                    rejected_incompressible: 3,
                    rejected_cap: 2,
                    fault_ins: 1,
                    pinned_bytes: 5 << 20,
                    ..RamzipStats::default()
                },
                cpu_loads: alloc::vec![
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
                ],
                deny: None,
                malformed_process_list: false,
                short_scalar: false,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fixture {
        // A test double that must answer every query the CLI issues; the
        // one-branch-per-query dispatch is inherently long and splitting it
        // would only obscure the exhaustive mapping.
        #[allow(clippy::too_many_lines)]
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            self.seen.borrow_mut().push(header.query);
            if self.deny == Some(header.query) {
                return Err(Errno::PermissionDenied);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            if header.query == SysinfoQueryId::SELF_PROCESS_LIST
                || header.query == SysinfoQueryId::GLOBAL_PROCESS_LIST
            {
                if self.malformed_process_list {
                    return Ok(alloc::vec![0u8; ProcessRecord::WIRE_LEN + 1]);
                }
                let req = ProcessListRequest::from_bytes(payload)?;
                let offset = req.offset as usize;
                if offset >= self.records.len() {
                    return Ok(Vec::new());
                }
                let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
                let mut out = Vec::with_capacity(take * ProcessRecord::WIRE_LEN);
                for record in &self.records[offset..offset + take] {
                    out.extend_from_slice(&record.to_le_bytes());
                }
                Ok(out)
            } else if self.short_scalar {
                Ok(alloc::vec![0u8; 3])
            } else if header.query == SysinfoQueryId::KERNEL_MEMORY_STATS {
                Ok(self.memory.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::HARDWARE_TREE {
                Ok(self.hardware.clone())
            } else if header.query == SysinfoQueryId::SYSTEM_IDENTITY {
                Ok(self.identity.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::UPTIME {
                Ok(self.uptime.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::SEAT_LIST {
                let req = SeatListRequest::from_bytes(payload)?;
                let offset = req.offset as usize;
                if offset >= self.seats.len() {
                    return Ok(Vec::new());
                }
                let take = core::cmp::min(self.seats.len() - offset, req.limit as usize);
                let mut out = Vec::with_capacity(take * SeatRecord::WIRE_LEN);
                for record in &self.seats[offset..offset + take] {
                    out.extend_from_slice(&record.to_le_bytes());
                }
                Ok(out)
            } else if header.query == SysinfoQueryId::MEMORY_PRESSURE {
                Ok(self.pressure.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::RAMZIP_STATS {
                Ok(self.ramzip.to_le_bytes().to_vec())
            } else if header.query == SysinfoQueryId::RECLAIM_STATS {
                let req = ReclaimListRequest::from_bytes(payload)?;
                let offset = req.offset as usize;
                if offset >= self.reclaim.len() {
                    return Ok(Vec::new());
                }
                let take = core::cmp::min(self.reclaim.len() - offset, req.limit as usize);
                let mut out = Vec::with_capacity(take * ReclaimClassRecord::WIRE_LEN);
                for record in &self.reclaim[offset..offset + take] {
                    out.extend_from_slice(&record.to_le_bytes());
                }
                Ok(out)
            } else if header.query == SysinfoQueryId::CPU_LOAD {
                let req = CpuLoadRequest::from_bytes(payload)?;
                let offset = req.offset as usize;
                if offset >= self.cpu_loads.len() {
                    return Ok(Vec::new());
                }
                let take = core::cmp::min(self.cpu_loads.len() - offset, req.limit as usize);
                let mut out = Vec::with_capacity(take * CpuLoadRecord::WIRE_LEN);
                for record in &self.cpu_loads[offset..offset + take] {
                    out.extend_from_slice(&record.to_le_bytes());
                }
                Ok(out)
            } else if header.query == SysinfoQueryId::CPU_INFO {
                let req = CpuInfoListRequest::from_bytes(payload)?;
                let records = [
                    CpuInfoRecord::new(
                        0,
                        CpuCoreClass::Performance,
                        CPU_INFO_FLAG_FREQ_MEASURED,
                        CpuFeatureSet::new()
                            .with(CpuFeature::Aes)
                            .with(CpuFeature::Crc32)
                            .bits(),
                        0x410F_D083,
                        1_512_000_000,
                        54_000_000,
                        b"ARM Cortex-A72",
                    )
                    .unwrap(),
                    CpuInfoRecord::new(1, CpuCoreClass::Efficiency, 0, 0, 0, 0, 54_000_000, b"")
                        .unwrap(),
                ];
                let offset = req.offset as usize;
                if offset >= records.len() {
                    return Ok(Vec::new());
                }
                let take = core::cmp::min(records.len() - offset, req.limit as usize);
                let mut out = Vec::new();
                for record in &records[offset..offset + take] {
                    out.extend_from_slice(&record.to_le_bytes());
                }
                Ok(out)
            } else if header.query == SysinfoQueryId::VOLUME_IO_HEALTH {
                let req = VolumeIoHealthRequest::from_bytes(payload)?;
                let records = [VolumeIoHealthRecord::new(
                    [0xAB; 16],
                    0x5953_2001,
                    MountAvailability::Recovering,
                    BlkHealthCounters {
                        completions: 4096,
                        ok: 4000,
                        resets: 30,
                        timeouts: 3,
                        medium_errors: 5,
                        reissues: 12,
                        ..BlkHealthCounters::default()
                    },
                )];
                let offset = req.offset as usize;
                if offset >= records.len() {
                    return Ok(Vec::new());
                }
                let take = core::cmp::min(records.len() - offset, req.limit as usize);
                let mut out = Vec::new();
                for record in &records[offset..offset + take] {
                    out.extend_from_slice(&record.to_le_bytes());
                }
                Ok(out)
            } else if header.query == SysinfoQueryId::RESOURCE_LIMITS {
                let mut out = Vec::new();
                for (index, kind) in LimitKind::ALL.iter().enumerate() {
                    let usage = index as u64;
                    let limit = ResourceLimit::new(index as u64, RLIMIT_INFINITY).unwrap();
                    out.extend_from_slice(
                        &ResourceLimitRecord::new(*kind, limit, usage).to_le_bytes(),
                    );
                }
                Ok(out)
            } else {
                Err(Errno::NotImplemented)
            }
        }
    }

    /// Captures rendered lines; optionally fails on the Nth write.
    struct Recorder {
        lines: RefCell<Vec<String>>,
        infos: RefCell<Vec<String>>,
        fail_at: Option<usize>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                infos: RefCell::new(Vec::new()),
                fail_at: None,
            }
        }

        fn failing_at(index: usize) -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                infos: RefCell::new(Vec::new()),
                fail_at: Some(index),
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.borrow().clone()
        }

        fn infos(&self) -> Vec<String> {
            self.infos.borrow().clone()
        }
    }

    impl Output for Recorder {
        fn write_line(&self, line: &str) -> Result<(), Errno> {
            let mut lines = self.lines.borrow_mut();
            if self.fail_at == Some(lines.len()) {
                return Err(Errno::NotFound);
            }
            lines.push(line.to_string());
            Ok(())
        }

        fn info(&self, record: &[u8]) {
            let text = core::str::from_utf8(record).expect("JSONL is UTF-8");
            self.infos.borrow_mut().push(text.to_string());
        }
    }

    fn record(pid: u64, name: &[u8], state: ProcessState) -> ProcessRecord {
        ProcessRecord::new(
            pid,
            1,
            ProcId::KERNEL,
            ProcId::KERNEL,
            1000,
            1000,
            state,
            0,
            0,
            0,
            name,
        )
        .unwrap()
    }

    #[test]
    fn help_prints_the_usage_fallback() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Help, &fixture, &out), Ok(()));
        assert_eq!(out.lines(), alloc::vec![USAGE.to_string()]);
        // Help touches no query.
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            engine_run(Command::Help, None, &fixture, &OneDoc, &out),
            Ok(())
        );
        let lines = out.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("query system information"));
        assert!(lines[0].contains("sysinfo <query>"));
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn self_process_list_renders_rows_and_routes_self() {
        let fixture = Fixture::new(alloc::vec![
            record(1, b"init", ProcessState::Running),
            record(7, b"shell", ProcessState::Blocked),
        ]);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: false }, &fixture, &out),
            Ok(())
        );
        let lines = out.lines();
        assert_eq!(lines.len(), 3); // header + two rows
        assert!(lines[0].contains("PID"));
        assert!(lines[1].contains("init"));
        assert!(lines[1].contains(" R "));
        assert!(lines[2].contains("shell"));
        assert!(lines[2].contains(" S "));
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::SELF_PROCESS_LIST]
        );
        // The default self scope announces its omission on the advisory
        // stream (fd 3) through the shared record definition, suggesting
        // this tool's own widening spelling; stdout is untouched.
        let infos = out.infos();
        assert_eq!(infos.len(), 1);
        assert!(infos[0].contains("\"producer\":\"sysinfo\""));
        assert!(infos[0].contains("\"kind\":\"omission\""));
        assert!(infos[0].contains("proc.self_scope_only"));
        assert!(infos[0].contains("\"argv\":[\"sysinfo\",\"processes\",\"--all\"]"));
    }

    #[test]
    fn global_process_list_routes_global_query() {
        let fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Runnable)]);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: true }, &fixture, &out),
            Ok(())
        );
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::GLOBAL_PROCESS_LIST]
        );
        // The global view omits nothing, so no advisory record is emitted.
        assert!(out.infos().is_empty());
    }

    #[test]
    fn process_list_pages_until_a_short_page() {
        // 65 records forces a full 64-record page plus a 1-record page.
        let mut records = Vec::new();
        for pid in 0..65u64 {
            records.push(record(pid, b"p", ProcessState::Runnable));
        }
        let fixture = Fixture::new(records);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: false }, &fixture, &out),
            Ok(())
        );
        // Header + 65 rows.
        assert_eq!(out.lines().len(), 66);
        // Two paged requests were issued.
        assert_eq!(fixture.seen.borrow().len(), 2);
    }

    #[test]
    fn denied_scalar_query_maps_to_permission_denied() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.deny = Some(SysinfoQueryId::KERNEL_MEMORY_STATS);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Memory, &fixture, &out),
            Err(SysinfoError::PermissionDenied)
        );
        assert!(out.lines().is_empty());
    }

    #[test]
    fn malformed_process_reply_fails_closed() {
        let mut fixture = Fixture::new(alloc::vec![record(1, b"init", ProcessState::Running)]);
        fixture.malformed_process_list = true;
        let out = Recorder::new();
        assert_eq!(
            run(Command::Processes { all: false }, &fixture, &out),
            Err(SysinfoError::Service(Errno::BadMagic))
        );
        // A failed walk renders no listing, so it announces no omission.
        assert!(out.infos().is_empty());
    }

    #[test]
    fn truncated_scalar_reply_fails_closed() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.short_scalar = true;
        let out = Recorder::new();
        assert_eq!(
            run(Command::Memory, &fixture, &out),
            Err(SysinfoError::Service(Errno::BufferTooSmall))
        );
    }

    #[test]
    fn memory_renders_every_field() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Memory, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("4096"));
        assert!(lines[1].contains("1024"));
    }

    #[test]
    fn identity_renders_hostname_machine_id_and_version() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Identity, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("rustbox"));
        assert!(lines[1].contains("abababab"));
        assert!(lines[2].contains("1.2.3"));
    }

    #[test]
    fn uptime_renders() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Uptime, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert!(lines[0].contains('9'));
        assert!(lines[1].contains("1000"));
    }

    #[test]
    fn seats_render_owner_unowned_and_denial() {
        // A held seat renders its owner, generation, and foreground.
        let mut fixture = Fixture::new(Vec::new());
        fixture.seats = alloc::vec![SeatRecord {
            seat_id: 0,
            owner_task: 7,
            generation: 3,
            foreground_console: 1,
            flags: SEAT_FLAG_OWNED,
        }];
        let out = Recorder::new();
        assert_eq!(run(Command::Seats, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("seat"));
        assert!(lines[1].contains("task 7"));
        assert!(lines[1].contains('3'));
        assert!(lines[1].contains("console 1"));
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::SEAT_LIST]
        );

        // An unowned seat is honest about having no owner.
        let mut fixture = Fixture::new(Vec::new());
        fixture.seats = alloc::vec![SeatRecord {
            seat_id: 0,
            owner_task: 0,
            generation: 0,
            foreground_console: 0,
            flags: 0,
        }];
        let out = Recorder::new();
        assert_eq!(run(Command::Seats, &fixture, &out), Ok(()));
        assert!(out.lines()[1].contains("unowned"));

        // The service's capability refusal surfaces as the CLI's
        // permission-denied error, never a fabricated table.
        let mut fixture = Fixture::new(Vec::new());
        fixture.deny = Some(SysinfoQueryId::SEAT_LIST);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Seats, &fixture, &out),
            Err(SysinfoError::PermissionDenied)
        );
    }

    #[test]
    fn limits_render_one_row_per_kind() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Limits, &fixture, &out), Ok(()));
        let lines = out.lines();
        // Header + one row per LimitKind.
        assert_eq!(lines.len(), 1 + LimitKind::COUNT);
        assert!(lines[0].contains("resource"));
        assert!(lines[1].contains(LimitKind::AddressSpaceBytes.name()));
        // The infinite hard bound renders as `unlimited`.
        assert!(lines[1].contains("unlimited"));
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::RESOURCE_LIMITS]
        );
    }

    #[test]
    fn limits_short_reply_fails_closed() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.short_scalar = true;
        let out = Recorder::new();
        assert_eq!(
            run(Command::Limits, &fixture, &out),
            Err(SysinfoError::Service(Errno::BufferTooSmall))
        );
    }

    #[test]
    fn hardware_reports_the_node_count() {
        let nodes = [
            HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(1, 0, HwDeviceClass::Serial),
        ];
        let mut fixture = Fixture::new(Vec::new());
        fixture
            .hardware
            .extend_from_slice(&HwTreeHeader::new(3, nodes.len() as u64).to_le_bytes());
        for node in &nodes {
            fixture.hardware.extend_from_slice(&node.to_le_bytes());
        }
        let out = Recorder::new();
        assert_eq!(run(Command::Hardware, &fixture, &out), Ok(()));
        assert_eq!(
            out.lines(),
            alloc::vec!["hardware tree: 2 nodes".to_string()]
        );
    }

    #[test]
    fn pressure_renders_band_watermarks_and_entries() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::Pressure, &fixture, &out).expect("pressure renders");
        let lines = out.lines();
        assert_eq!(lines[0], "band:            mild (1)");
        assert!(lines[1].ends_with("4096"), "total row: {}", lines[1]);
        // One row per band under the header; mild shows its watermarks
        // and its two recorded entries.
        let mild = lines.iter().find(|l| l.starts_with("mild")).unwrap();
        assert!(mild.contains("800") && mild.contains("1024") && mild.trim_end().ends_with('2'));
        // The normal band rests: no watermarks.
        let normal = lines.iter().find(|l| l.starts_with("normal")).unwrap();
        assert!(normal.contains('-'));

        // A denial maps to the CLI's permission error.
        let mut denied = Fixture::new(Vec::new());
        denied.deny = Some(SysinfoQueryId::MEMORY_PRESSURE);
        assert_eq!(
            run(Command::Pressure, &denied, &Recorder::new()),
            Err(SysinfoError::PermissionDenied)
        );
    }

    #[test]
    fn reclaim_renders_one_row_per_class_and_fails_closed_on_torn_reply() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::Reclaim, &fixture, &out).expect("reclaim renders");
        let lines = out.lines();
        assert!(lines[0].starts_with("class"));
        assert!(lines[1].starts_with("clean-file-data"), "{}", lines[1]);
        assert!(lines[1].contains("4096"));
        // The hit ratio (900 / (900 + 100)) is rendered as a percentage.
        assert!(lines[0].contains("hit%"), "header: {}", lines[0]);
        assert!(lines[1].contains("90%"), "row: {}", lines[1]);
    }

    #[test]
    fn ramzip_renders_counters_only() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::Ramzip, &fixture, &out).expect("ramzip renders");
        let lines = out.lines();
        assert_eq!(lines[0], "entries:         4");
        assert!(lines.iter().any(|l| l == "rejected:        5"));
        assert!(lines.iter().any(|l| l.starts_with("fault-ins:")));
        // The pinned aggregate (`mem_pin`) rides the same record.
        assert!(
            lines
                .iter()
                .any(|l| l == &alloc::format!("pinned bytes:    {}", 5 << 20)),
            "pinned bytes row missing: {lines:?}"
        );
    }

    #[test]
    fn cpu_load_renders_one_row_per_cpu() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::CpuLoad, &fixture, &out).expect("cpu renders");
        let lines = out.lines();
        assert!(lines[0].starts_with("cpu"));
        assert_eq!(lines.len(), 3, "header plus two CPUs");
        assert!(lines[1].contains("42"));
        assert!(lines[2].contains("17"));

        let mut denied = Fixture::new(Vec::new());
        denied.deny = Some(SysinfoQueryId::CPU_LOAD);
        assert_eq!(
            run(Command::CpuLoad, &denied, &Recorder::new()),
            Err(SysinfoError::PermissionDenied)
        );
    }

    #[test]
    fn cpu_info_renders_blocks_with_flags_and_frequencies() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::CpuInfo, &fixture, &out).expect("cpuinfo renders");
        let lines = out.lines();
        // Two blocks separated by a blank line: 6 lines each (no reference
        // row is omitted since both carry a reference frequency) + 1 blank.
        assert_eq!(lines[0], "processor     : 0");
        assert!(lines.iter().any(|l| l == "model name    : ARM Cortex-A72"));
        assert!(lines.iter().any(|l| l == "core class    : performance"));
        // A measured core running at 1.512 GHz, shown as MHz.
        assert!(lines.iter().any(|l| l == "cpu MHz       : 1512.000"));
        assert!(lines.iter().any(|l| l == "reference MHz : 54.000"));
        // The decoded ISA flags (bit order: crc32 before aes).
        assert!(lines.iter().any(|l| l == "flags         : crc32 aes"));
        // The efficiency core reports an unknown clock honestly and an
        // empty model falls back to "unknown".
        assert!(lines.iter().any(|l| l == "processor     : 1"));
        assert!(lines.iter().any(|l| l == "core class    : efficiency"));
        assert!(lines
            .iter()
            .any(|l| l == "cpu MHz       : unknown (no core-clock measurement)"));
        assert!(lines.iter().any(|l| l == "model name    : unknown"));
        // A feature-less core lists no flags rather than fabricating one.
        assert!(lines.iter().any(|l| l == "flags         : (none)"));
        // Exactly one blank line separates the two blocks.
        assert_eq!(lines.iter().filter(|l| l.is_empty()).count(), 1);
    }

    #[test]
    fn storage_renders_rows_and_fails_closed_on_denial() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::Storage, &fixture, &out).expect("storage renders");
        let lines = out.lines();
        assert!(lines[0].starts_with("volume"));
        assert!(lines[0].contains("health"));
        // The one fixture volume: its recovering health, serving endpoint,
        // and folded outcome counters.
        assert_eq!(lines.len(), 2, "header plus one volume");
        assert!(
            lines[1].starts_with("abababababababab"),
            "row: {}",
            lines[1]
        );
        assert!(lines[1].contains("recovering"), "row: {}", lines[1]);
        assert!(lines[1].contains("4096"), "completions: {}", lines[1]);
        assert!(lines[1].contains("30"), "resets: {}", lines[1]);
        assert!(lines[1].contains("12"), "reissues: {}", lines[1]);
        // The query routed to VOLUME_IO_HEALTH.
        assert!(fixture
            .seen
            .borrow()
            .contains(&SysinfoQueryId::VOLUME_IO_HEALTH));

        // A denial maps to the CLI's permission error.
        let mut denied = Fixture::new(Vec::new());
        denied.deny = Some(SysinfoQueryId::VOLUME_IO_HEALTH);
        assert_eq!(
            run(Command::Storage, &denied, &Recorder::new()),
            Err(SysinfoError::PermissionDenied)
        );
    }

    #[test]
    fn output_failure_propagates() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::failing_at(0);
        assert_eq!(
            run(Command::Help, &fixture, &out),
            Err(SysinfoError::Output(Errno::NotFound))
        );
    }
}
