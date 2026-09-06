//! The request/render engine: turn a [`Command`] into typed `sysinfo-v1`
//! requests, decode the typed replies, and render human-readable lines.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
use tairix_abi::raid::{ArrayHealth, RaidLevel};
use tairix_abi::raid_admin::{
    RaidArrayRecord, RaidMemberDisposition, RaidMemberRecord, RAID_SLOT_NONE,
};
use tairix_abi::sysinfo::{
    CpuCoreClass, CpuInfoListRequest, CpuInfoRecord, CpuLoadRecord, CpuLoadRequest,
    KernelMemoryStats, MemoryPressureStats, MountAvailability, RamzipStats, ReclaimClassRecord,
    ReclaimListRequest, ResourceLimitRecord, SeatListRequest, SeatRecord, SysinfoQueryId,
    SystemIdentity, Uptime, VolumeIoHealthRecord, VolumeIoQueueRecord, VolumeIoRequest,
    VolumeIoStatsRecord, PRESSURE_BAND_NAMES, RECLAIM_CLASS_COUNT, RECLAIM_CLASS_NAMES,
};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{Errno, LimitKind};

use tairix_help::{own_short_help, HelpSource};
use tairix_procinfo::{
    call, emit_self_scope_omission, fetch_tree, for_each_desktop_frame_report, for_each_irq,
    for_each_process, for_each_raid_array, for_each_raid_member, format_count, format_tenths,
    render_limit_bound, render_process, resolve, Authorization, InfoValue, Metric, MetricKind,
    Output, Producer, ResetBehavior, ResourceResponse, ResponsePayload, Sensitivity, Transport,
    Unit, ValueKind, WalkStep, PROCESS_HEADER,
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
  frames              what each desktop session's frames cost (needs CAP_SYSINFO_GLOBAL)
  storage             per-volume I/O health and outcome counters (needs CAP_SYSINFO_KERNEL)
  raid                composed arrays and the devices they are made of (needs CAP_SYSINFO_HW)
  show <ref>          read one info:/state:/stats: resource reference
  describe <ref>      report a reference's producer, authorization, and metric metadata
  help, -h, -?        show this help";

/// `sysinfo`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "sysinfo";

/// Run one [`Command`], issuing its query through `transport` and writing the
/// rendered result to `out`. `locale` is the user's `LANG` preference, if
/// set; `help` is the tool's own `Help/` tree, read by the short-help
/// switches; `now` is the wall-clock instant the caller read, which stamps a
/// `show`/`describe` response envelope (the library reads no clock of its
/// own, exactly as it opens no transport of its own).
///
/// # Errors
///
/// * [`SysinfoError::PermissionDenied`] — the service refused the query for
///   want of its declared capability.
/// * [`SysinfoError::Service`] — the transport failed or the reply did not
///   decode against `sysinfo-v1`.
/// * [`SysinfoError::Output`] — writing the terminal failed.
/// * [`SysinfoError::BadReference`] / [`SysinfoError::Unresolvable`] — a
///   `show`/`describe` operand that is not a well-formed reference, or names
///   nothing this resolver serves.
pub fn run(
    command: Command<'_>,
    locale: Option<&str>,
    now: Time64,
    transport: &dyn Transport,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Show { reference } => run_show(reference, now, transport, out),
        Command::Describe { reference } => run_describe(reference, now, transport, out),
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
        Command::Frames => run_frames(transport, out),
        Command::Storage => run_storage(transport, out),
        Command::Raid => run_raid(transport, out),
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

/// Resolve one resource reference to its typed response.
///
/// Both steps are the shared ones: the spelling goes through the single
/// resource-reference parser (`lib/resref`) and the resolution through the
/// single userspace `info:`/`state:`/`stats:` resolver (`lib/procinfo`), which
/// carries it to `sysinfod` over the same [`Transport`] every other
/// subcommand uses. This tool therefore adds no second reference grammar, no
/// second resolver, and no path around the broker's per-principal scoping.
fn resolve_reference(
    reference: &str,
    now: Time64,
    transport: &dyn Transport,
) -> Result<ResourceResponse, SysinfoError> {
    let parsed = tairix_resref::parse(reference).map_err(|_| SysinfoError::BadReference)?;
    resolve(&parsed, now, transport).map_err(SysinfoError::from)
}

/// `show <resource-ref>`: print the value, and nothing else.
///
/// One bare value on one line, so a shell can capture it
/// (`host=$(sysinfo show info:system/hostname)`) without stripping a label or
/// a unit. What the figure *means* — its unit, kind, and sampling window — is
/// [`run_describe`]'s job, one command away, rather than decoration this
/// command's callers would have to parse back off.
///
/// The rendering itself is the shared
/// [`ResourceResponse::display_value`](tairix_procinfo::ResourceResponse::display_value),
/// not a match in this tool: the shell's input redirection
/// (`cat < info:mem/physical`) reads the same values through the same
/// renderer, so the two spellings of one read can never disagree.
fn run_show(
    reference: &str,
    now: Time64,
    transport: &dyn Transport,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    let response = resolve_reference(reference, now, transport)?;
    emit(out, &response.display_value())
}

/// `describe <resource-ref>`: print the response envelope rather than the
/// value (`plans/ALIAS.md` §14.5).
///
/// Every field comes from the typed [`ResourceResponse`] the resolver already
/// builds — the producer, the authorization the value was served under, and
/// the payload's own metadata — so this renders a record rather than
/// re-deriving anything about the resource.
fn run_describe(
    reference: &str,
    now: Time64,
    transport: &dyn Transport,
    out: &dyn Output,
) -> Result<(), SysinfoError> {
    let response = resolve_reference(reference, now, transport)?;
    emit(out, &format!("reference     : {}", response.query()))?;
    emit(out, &format!("envelope      : v{}", response.version))?;
    emit(
        out,
        &format!("producer      : {}", producer_name(response.producer)),
    )?;
    emit(
        out,
        &format!(
            "authorization : {}",
            authorization_text(response.authorization)
        ),
    )?;
    match &response.payload {
        ResponsePayload::Info(info) => describe_value(out, "info", info),
        ResponsePayload::State(info) => describe_value(out, "state", info),
        ResponsePayload::Metric(metric) => describe_metric(out, metric),
    }
}

/// The `describe` lines an `info:` fact or `state:` reading adds: its scalar
/// type and how sensitive it is (`plans/ALIAS.md` §14.2).
fn describe_value(out: &dyn Output, payload: &str, info: &InfoValue) -> Result<(), SysinfoError> {
    emit(out, &format!("payload       : {payload}"))?;
    let kind = match info.kind {
        ValueKind::Str => "string",
    };
    emit(out, &format!("value kind    : {kind}"))?;
    let sensitivity = match info.sensitivity {
        Sensitivity::Public => "public",
        Sensitivity::Sensitive => "sensitive",
    };
    emit(out, &format!("sensitivity   : {sensitivity}"))
}

/// The `describe` lines a `stats:` metric adds (`plans/ALIAS.md` §14.3,
/// §14.5): its name, kind, unit, reset behaviour, and — for a rate, which is
/// undefined without one — the window the service actually measured over.
fn describe_metric(out: &dyn Output, metric: &Metric) -> Result<(), SysinfoError> {
    emit(out, "payload       : metric")?;
    emit(out, &format!("metric        : {}", metric.name()))?;
    let kind = match metric.kind {
        MetricKind::Gauge => "gauge",
        MetricKind::Counter => "counter",
        MetricKind::Rate => "rate",
    };
    emit(out, &format!("kind          : {kind}"))?;
    emit(out, &format!("unit          : {}", unit_name(metric.unit)))?;
    let reset = match metric.reset_behavior {
        ResetBehavior::Never => "never",
        ResetBehavior::Boot => "boot",
    };
    emit(out, &format!("reset         : {reset}"))?;
    // A gauge and a counter have no window; saying "none" is the honest
    // answer, not a missing line the reader has to notice.
    let window = match metric.window {
        Some(window) => format_window(window),
        None => String::from("none"),
    };
    emit(out, &format!("window        : {window}"))
}

/// The service that produced a response.
fn producer_name(producer: Producer) -> &'static str {
    match producer {
        Producer::Sysinfod => "sysinfod",
    }
}

/// The authorization a response was served under: the capability that was
/// spent, or that none was needed.
fn authorization_text(authorization: Authorization) -> String {
    match authorization {
        Authorization::Unprivileged => String::from("unprivileged"),
        // A capability the frozen registry does not name cannot be spelled;
        // its number is reported rather than a guess at a name.
        Authorization::Capability(cap) => cap
            .name()
            .map_or_else(|| format!("capability {}", cap.as_u16()), String::from),
    }
}

/// The display spelling of a metric unit.
fn unit_name(unit: Unit) -> &'static str {
    match unit {
        Unit::Bytes => "bytes",
        Unit::Seconds => "seconds",
        Unit::Count => "count",
        Unit::Percent => "percent",
        Unit::PacketsPerSecond => "packets/s",
        Unit::BitsPerSecond => "bits/s",
    }
}

/// Render a sampling window in the same `<n>ms`/`<n>s` spelling a `?window=`
/// parameter is written in, so a described window can be typed straight back
/// into a reference.
fn format_window(window: Duration64) -> String {
    let secs = window.secs();
    let millis = u64::from(window.subsec_nanos()) / 1_000_000;
    if millis == 0 {
        return format!("{secs}s");
    }
    // A sub-second remainder is reported in milliseconds throughout, rather
    // than as a mixed `1s500ms` no parameter spelling would accept.
    let total_millis = (secs.unsigned_abs() * 1000).saturating_add(millis);
    format!("{total_millis}ms")
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
            .map(|()| WalkStep::Continue)
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
        "class                  payload    metadata  entries      hits    misses  hit%  refusals  shrinks  failures      self",
    )?;
    for chunk in reply.as_chunks::<{ ReclaimClassRecord::WIRE_LEN }>().0 {
        let record = ReclaimClassRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
        let name = RECLAIM_CLASS_NAMES[usize::from(record.class)];
        emit(
            out,
            &format!(
                "{:<21}  {:>7}  {:>10}  {:>7}  {:>8}  {:>8}  {:>4}  {:>8}  {:>7}  {:>8}  {:>8}",
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
                record.self_reported_bytes,
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
        .map(|()| WalkStep::Continue)
    })
    .map_err(SysinfoError::from)
}

/// Fetch and render what each desktop session's frames have cost, one
/// aligned row per publishing session.
///
/// The reading that matters is the first three figures together: the pixels
/// the desktop recomposed, what resolving them blended, and the worst single
/// frame as a share of the screen. A desktop that changes a few thousand
/// pixels per frame but blends millions is paying for depth nobody can see,
/// and one whose worst frame is the whole screen is repainting everything to
/// move a cursor.
///
/// The figures are the desktop's own statement about itself — only the
/// process holding a compositor can count pixels — so a row is labelled by
/// the publisher `sysinfod` attested it to. A session that has published
/// nothing prints no row: absent is not zero.
///
/// The service gates the query on `CAP_SYSINFO_GLOBAL`; a denial surfaces as
/// the shared refusal rather than an empty table.
fn run_frames(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    emit(
        out,
        "pid     frames    damaged  per frame  overdraw  copied  frosted  scanned    worst frame  presents  furniture",
    )?;
    for_each_desktop_frame_report(transport, |record| {
        let t = &record.totals;
        out.write_line(&format!(
            "{:<6}  {:>6}  {:>9}  {:>9}  {:>7}x  {:>5}%  {:>7}  {:>7}  {:>7} {:>3}%  {:>8}  {}/{}",
            record.reporter_pid,
            t.frames,
            format_count(t.damaged_px),
            per_frame(t.damaged_px, t.frames),
            format_tenths(ratio_tenths(t.blended_px, t.damaged_px)),
            percent(t.opaque_px, t.damaged_px),
            format_count(t.blur_px),
            format_count(t.encoded_px),
            format_count(t.peak_damaged_px),
            percent(t.peak_damaged_px, t.screen_px),
            t.present_calls,
            t.chrome_hits,
            t.chrome_misses,
        ))
        .map(|()| WalkStep::Continue)
    })
    .map_err(SysinfoError::from)
}

/// What a figure with no denominator to divide by prints as: absent, never
/// a fabricated zero.
const UNMEASURABLE: &str = "-";

/// `total` spread over `frames`, or [`UNMEASURABLE`] when there is no frame.
///
/// A served record's frame count is a publisher's own figure, so a reader
/// never divides by it without asking: an unmeasurable average prints as
/// absent rather than as a fabricated zero.
fn per_frame(total: u64, frames: u64) -> String {
    match total.checked_div(frames) {
        Some(mean) => format_count(mean),
        None => String::from(UNMEASURABLE),
    }
}

/// `part` as a percentage of `whole`, or [`UNMEASURABLE`] when `whole` is
/// zero.
fn percent(part: u64, whole: u64) -> String {
    match part.saturating_mul(100).checked_div(whole) {
        Some(pct) => format!("{pct}"),
        None => String::from(UNMEASURABLE),
    }
}

/// `part` over `whole` in tenths, for a `1.0x`-style multiplier. Zero when
/// `whole` is zero: no damage is no overdraw, not an unknown one.
fn ratio_tenths(part: u64, whole: u64) -> u32 {
    let tenths = part
        .saturating_mul(10)
        .checked_div(whole)
        .unwrap_or_default();
    u32::try_from(tenths).unwrap_or(u32::MAX)
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

/// Records requested per page of a per-volume list: bounds the reply without
/// bounding how many volumes may be mounted.
const VOLUME_PAGE: u16 = 64;

/// Fetch and render the per-volume storage report: the cumulative service
/// counters every user may read, then the queue occupancy and the folded
/// outcome counters the kernel scope gates.
///
/// The ungated table is rendered first, so a caller without
/// `CAP_SYSINFO_KERNEL` still gets the throughput and utilisation figures
/// before the gated reads report their refusal.
fn run_storage(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    run_volume_service(transport, out)?;
    run_volume_queue(transport, out)?;
    run_volume_health(transport, out)
}

/// Fetch and render the cumulative per-volume service counters, one aligned
/// row per fault-aware block-backed volume: a short prefix of its durable id,
/// the serving block-service endpoint, and the raw tallies a reader deltas
/// into throughput, IOPS, utilisation and await. Nothing is pre-derived here,
/// so two readers never inherit one averaging window. Ungated.
fn run_volume_service(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    emit(
        out,
        "volume            dev                    read-B     write-B  read-ops  write-ops     busy-ms",
    )?;
    let mut offset: u32 = 0;
    loop {
        let request = VolumeIoRequest {
            offset,
            limit: VOLUME_PAGE,
            flags: 0,
        };
        let reply = service_call(
            transport,
            SysinfoQueryId::VOLUME_IO_STATS,
            &request.to_le_bytes(),
        )?;
        if reply.len() % VolumeIoStatsRecord::WIRE_LEN != 0 {
            return Err(SysinfoError::Service(Errno::BufferTooSmall));
        }
        let records = reply.len() / VolumeIoStatsRecord::WIRE_LEN;
        for chunk in reply.as_chunks::<{ VolumeIoStatsRecord::WIRE_LEN }>().0 {
            let record = VolumeIoStatsRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
            let volume_id = record.volume_id();
            let counters = record.counters();
            emit(
                out,
                &format!(
                    "{:<16}  {:#018x}  {:>10}  {:>10}  {:>8}  {:>9}  {:>10}",
                    hex(&volume_id[..8]),
                    record.dev(),
                    counters.read_bytes,
                    counters.write_bytes,
                    counters.read_ops,
                    counters.write_ops,
                    counters.busy_ns / 1_000_000,
                ),
            )?;
        }
        if records < usize::from(VOLUME_PAGE) {
            return Ok(());
        }
        offset = offset.saturating_add(u32::from(VOLUME_PAGE));
    }
}

/// Fetch and render the per-volume queue occupancy against the budget in
/// force: what is outstanding now, the accumulators a mean depth deltas out
/// of, and the device class's own depth and deadline. The service gates the
/// query on `CAP_SYSINFO_KERNEL`.
fn run_volume_queue(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    emit(
        out,
        "volume            dev                 in-flight   depth-sum    arrivals  max-depth  deadline-ms",
    )?;
    let mut offset: u32 = 0;
    loop {
        let request = VolumeIoRequest {
            offset,
            limit: VOLUME_PAGE,
            flags: 0,
        };
        let reply = service_call(
            transport,
            SysinfoQueryId::VOLUME_IO_QUEUE,
            &request.to_le_bytes(),
        )?;
        if reply.len() % VolumeIoQueueRecord::WIRE_LEN != 0 {
            return Err(SysinfoError::Service(Errno::BufferTooSmall));
        }
        let records = reply.len() / VolumeIoQueueRecord::WIRE_LEN;
        for chunk in reply.as_chunks::<{ VolumeIoQueueRecord::WIRE_LEN }>().0 {
            let record = VolumeIoQueueRecord::from_bytes(chunk).map_err(SysinfoError::Service)?;
            let volume_id = record.volume_id();
            let queue = record.queue();
            emit(
                out,
                &format!(
                    "{:<16}  {:#018x}  {:>9}  {:>10}  {:>10}  {:>9}  {:>11}",
                    hex(&volume_id[..8]),
                    record.dev(),
                    queue.in_flight,
                    queue.queue_depth_sum,
                    queue.queue_samples,
                    record.budget_depth(),
                    record.budget_deadline_ns() / 1_000_000,
                ),
            )?;
        }
        if records < usize::from(VOLUME_PAGE) {
            return Ok(());
        }
        offset = offset.saturating_add(u32::from(VOLUME_PAGE));
    }
}

/// Fetch and render the per-volume storage I/O health, one aligned row per
/// fault-aware block-backed volume: a short prefix of its durable id, the
/// serving block-service endpoint, its current availability, and the folded
/// outcome counters that a failing or flapping disk becomes visible on. The
/// paged walk and fail-closed decode mirror [`run_cpu_load`]; the service
/// gates the query on `CAP_SYSINFO_KERNEL`.
fn run_volume_health(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    /// Records requested per page: bounds the reply without bounding how
    /// many volumes may be mounted.
    const PAGE: u16 = VOLUME_PAGE;
    emit(
        out,
        "volume            dev                 health      done  resets  tmout  medium  reissue",
    )?;
    let mut offset: u32 = 0;
    loop {
        let request = VolumeIoRequest {
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

/// The short display name of a RAID level, for the `raid` array row. A
/// closed match so a future [`RaidLevel`] variant is a compile error here
/// rather than a silent blank.
fn level_name(level: RaidLevel) -> &'static str {
    match level {
        RaidLevel::Mirror => "mirror",
        RaidLevel::Stripe => "stripe",
        RaidLevel::Parity => "parity",
        RaidLevel::DualParity => "dual-parity",
        RaidLevel::TripleParity => "triple-parity",
        RaidLevel::Raid10 => "raid10",
    }
}

/// The short display name of an array's health, for the `raid` array row. A
/// closed match, as [`level_name`] is.
fn health_name(health: ArrayHealth) -> &'static str {
    match health {
        ArrayHealth::Optimal => "optimal",
        ArrayHealth::Degraded => "degraded",
        ArrayHealth::Recovering => "recovering",
        ArrayHealth::Failed => "failed",
    }
}

/// The short display name of a held device's disposition, for the `raid`
/// device row. A closed match, as [`level_name`] is.
fn disposition_name(disposition: RaidMemberDisposition) -> &'static str {
    match disposition {
        RaidMemberDisposition::Candidate => "candidate",
        RaidMemberDisposition::Held => "held",
        RaidMemberDisposition::InSync => "in-sync",
        RaidMemberDisposition::Resyncing => "resyncing",
        RaidMemberDisposition::Faulted => "faulted",
    }
}

/// What an array is doing right now, as a column: how far a running rebuild
/// or verification pass has reached, as a percentage of the array's blocks.
///
/// A cursor at the array's block count means no pass is running, which reads
/// as an idle array rather than a completed one; a zero-length array cannot
/// be expressed as a fraction, so it reads idle too rather than dividing by
/// zero.
fn array_progress(record: &RaidArrayRecord) -> String {
    let total = record.block_count();
    let (label, cursor) = if record.resyncing() {
        ("resync", record.resync_cursor())
    } else if record.scrubbing() {
        ("scrub", record.scrub_cursor())
    } else {
        return String::from("idle");
    };
    if total == 0 {
        return String::from("idle");
    }
    format!("{label} {}%", cursor.min(total) * 100 / total)
}

/// Fetch and render the composed arrays and the devices the composer holds:
/// one aligned row per array, then a blank line, then one aligned row per
/// device. The paged walks, fail-closed decode, and `CAP_SYSINFO_HW` gate are
/// the shared `lib/procinfo` helpers (the same records the `mdadm`
/// administrator reads); the CLI supplies only the headers and the per-row
/// rendering.
///
/// A machine with no running composer surfaces the transport's refusal, so an
/// empty table always means "the composer holds nothing", never "nothing
/// answered".
fn run_raid(transport: &dyn Transport, out: &dyn Output) -> Result<(), SysinfoError> {
    emit(
        out,
        "array             level          health      members  chunk  blocks       progress",
    )?;
    for_each_raid_array(transport, |record| {
        out.write_line(&format!(
            "{:<16}  {:<13}  {:<10}  {:>3}/{:<3}  {:>5}  {:>11}  {}",
            hex(&record.array()[..8]),
            level_name(record.level()),
            health_name(record.health()),
            record.active_members(),
            record.member_count(),
            record.chunk_blocks(),
            record.block_count(),
            array_progress(record),
        ))
        .map(|()| WalkStep::Continue)
    })
    .map_err(SysinfoError::from)?;

    emit(out, "")?;
    emit(
        out,
        "device  array             slot  disposition  blocks       bsize  generation",
    )?;
    for_each_raid_member(transport, |record| {
        out.write_line(&format!(
            "{:<6}  {:<16}  {:<4}  {:<11}  {:>11}  {:>5}  {:>10}",
            record.node(),
            member_affiliation(record),
            member_slot(record),
            disposition_name(record.disposition()),
            record.block_count(),
            record.block_size(),
            record.generation(),
        ))
        .map(|()| WalkStep::Continue)
    })
    .map_err(SysinfoError::from)
}

/// The array a held device belongs to, as a column: a short prefix of the
/// array's identity, or a dash for an unaffiliated candidate that belongs to
/// none.
fn member_affiliation(record: &RaidMemberRecord) -> String {
    if record.is_unaffiliated() {
        String::from("-")
    } else {
        hex(&record.array()[..8])
    }
}

/// The array slot a held device occupies, as a column: a dash when it
/// occupies none.
fn member_slot(record: &RaidMemberRecord) -> String {
    if record.slot() == RAID_SLOT_NONE {
        String::from("-")
    } else {
        format!("{}", record.slot())
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
    use tairix_abi::blkio::{BlkDeviceClass, BlkHealthCounters, BlkIoCounters, BlkQueueCounters};
    use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};
    use tairix_abi::hwtree::{HwDeviceClass, HwNode, HwTreeHeader, HW_NODE_ROOT};
    use tairix_abi::raid::{ArrayHealth, RaidLevel};
    use tairix_abi::raid_admin::{
        RaidArrayRecord, RaidMemberDisposition, RaidMemberRecord, RAID_ARRAY_FLAG_RESYNCING,
        RAID_SLOT_NONE,
    };
    use tairix_abi::sysinfo::{
        CpuCoreClass, CpuInfoListRequest, CpuInfoRecord, CpuLoadRecord, CpuLoadRequest,
        DesktopFrameRecord, DesktopFrameStatsRequest, DesktopFrameTotals, KernelMemoryStats,
        MemoryPressureStats, MountAvailability, ProcessListRequest, ProcessRecord, ProcessState,
        RaidListRequest, RamzipStats, ReclaimClassRecord, ReclaimListRequest, ResourceLimitRecord,
        SeatListRequest, SeatRecord, SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime,
        VolumeIoHealthRecord, VolumeIoQueueRecord, VolumeIoStatsRecord,
        CPU_INFO_FLAG_FREQ_MEASURED, SEAT_FLAG_OWNED,
    };
    use tairix_abi::time::{Duration64, Time64};
    use tairix_abi::{Errno, LimitKind, ProcId, ResourceLimit, SchedPriority, RLIMIT_INFINITY};
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

    /// A fixed wall-clock instant for the response envelopes a
    /// `show`/`describe` stamps, so a rendered envelope is deterministic.
    const NOW: Time64 = Time64::from_secs(1_718_452_800);

    /// The engine under the fixtures' default seams: no locale preference
    /// and an empty Help tree, so every existing scenario exercises the
    /// query paths unchanged.
    fn run(
        command: Command<'_>,
        transport: &dyn Transport,
        out: &dyn Output,
    ) -> Result<(), SysinfoError> {
        engine_run(command, None, NOW, transport, &NoHelp, out)
    }

    /// Two arrays the composer would report: an idle mirror and a parity
    /// array part-way through a rebuild, so both the healthy and the
    /// in-progress rendering are covered.
    fn fixture_arrays() -> Vec<RaidArrayRecord> {
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
                500_000,
                9,
            ),
        ]
    }

    /// Two devices the composer holds: a slotted in-sync member and an
    /// unaffiliated candidate, so the affiliation and slot dashes are
    /// covered.
    fn fixture_members() -> Vec<RaidMemberRecord> {
        alloc::vec![
            RaidMemberRecord::new(
                [0x22; 16],
                RaidMemberDisposition::InSync,
                1,
                51,
                0x5241_3001,
                1_000_000,
                4096,
                9,
            ),
            RaidMemberRecord::new(
                [0u8; 16],
                RaidMemberDisposition::Candidate,
                RAID_SLOT_NONE,
                52,
                0x5241_3002,
                4_000_000,
                512,
                0,
            ),
        ]
    }

    /// Serve the window a paged request selects out of `records`, exactly as
    /// the real service's paging does.
    fn page<T>(
        payload: &[u8],
        records: &[T],
        encode: impl Fn(&T) -> Vec<u8>,
    ) -> Result<Vec<u8>, Errno> {
        // Every paged list query shares one paging-header layout, so one
        // decode serves them all.
        let request = RaidListRequest::from_bytes(payload)?;
        let offset = request.offset as usize;
        if offset >= records.len() {
            return Ok(Vec::new());
        }
        let take = core::cmp::min(records.len() - offset, request.limit as usize);
        let mut out = Vec::new();
        for record in &records[offset..offset + take] {
            out.extend_from_slice(&encode(record));
        }
        Ok(out)
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
        frames: Vec<DesktopFrameRecord>,
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
                    self_reported_bytes: 1024,
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
                frames: alloc::vec![DesktopFrameRecord {
                    reporter_pid: 91,
                    totals: DesktopFrameTotals {
                        screen_px: 1000 * 1000,
                        frames: 10,
                        damaged_px: 400_000,
                        blended_px: 1_200_000,
                        opaque_px: 100_000,
                        blur_px: 30_000,
                        encoded_px: 400_000,
                        dirty_rects: 25,
                        present_calls: 10,
                        chrome_hits: 96,
                        chrome_misses: 4,
                        peak_damaged_px: 250_000,
                        peak_blended_px: 500_000,
                    },
                }],
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
            } else if header.query == SysinfoQueryId::DESKTOP_FRAME_STATS {
                let req = DesktopFrameStatsRequest::from_bytes(payload)?;
                let offset = req.offset as usize;
                if offset >= self.frames.len() {
                    return Ok(Vec::new());
                }
                let take = core::cmp::min(self.frames.len() - offset, req.limit as usize);
                let mut out = Vec::with_capacity(take * DesktopFrameRecord::WIRE_LEN);
                for record in &self.frames[offset..offset + take] {
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
                page(
                    payload,
                    &[VolumeIoHealthRecord::new(
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
                    )],
                    |record| record.to_le_bytes().to_vec(),
                )
            } else if header.query == SysinfoQueryId::VOLUME_IO_STATS {
                page(
                    payload,
                    &[VolumeIoStatsRecord::new(
                        [0xAB; 16],
                        0x5953_2001,
                        BlkIoCounters {
                            read_bytes: 4 << 20,
                            write_bytes: 1 << 20,
                            read_ops: 1024,
                            write_ops: 256,
                            busy_ns: 500_000_000,
                            read_wait_ns: 128_000_000,
                            write_wait_ns: 64_000_000,
                        },
                    )],
                    |record| record.to_le_bytes().to_vec(),
                )
            } else if header.query == SysinfoQueryId::VOLUME_IO_QUEUE {
                page(
                    payload,
                    &[VolumeIoQueueRecord::new(
                        [0xAB; 16],
                        0x5953_2001,
                        BlkQueueCounters {
                            in_flight: 3,
                            queue_depth_sum: 2560,
                            queue_samples: 1280,
                        },
                        BlkDeviceClass::SolidState.budget(),
                    )],
                    |record| record.to_le_bytes().to_vec(),
                )
            } else if header.query == SysinfoQueryId::RAID_ARRAYS {
                page(payload, &fixture_arrays(), |record| {
                    record.to_le_bytes().to_vec()
                })
            } else if header.query == SysinfoQueryId::RAID_MEMBERS {
                page(payload, &fixture_members(), |record| {
                    record.to_le_bytes().to_vec()
                })
            } else if header.query == SysinfoQueryId::NET_INTERFACE_RATES {
                // One interface's throughput rates, echoing the window the
                // caller asked for so a described window can be checked
                // against the request that produced it.
                let req = tairix_abi::sysinfo::NetInterfaceRatesRequest::from_bytes(payload)?;
                let mut name = [0u8; tairix_abi::net_ipc::IF_NAME_LEN];
                name[..3].copy_from_slice(b"wan");
                Ok(tairix_abi::net_ipc::NetInterfaceRatesRecord {
                    name,
                    window: req.window,
                    rx_pps: 120,
                    rx_bps: 960_000,
                    tx_pps: 60,
                    tx_bps: 480_000,
                }
                .to_le_bytes()
                .to_vec())
            } else if header.query == SysinfoQueryId::NET_RESOLVER_SERVERS {
                // A host that has learned no recursive servers: a valid
                // answer (`none`), not a failure, so the `state:` payload
                // renders without a network fixture.
                Ok(Vec::new())
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
            SchedPriority::Normal,
            0,
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

    /// `show` prints the value and nothing else, for each payload the
    /// resolver can produce: an `info:` fact, a `stats:` metric, and a
    /// `state:` reading.
    #[test]
    fn show_prints_the_bare_value_of_every_payload() {
        // `info:` — a fact, through SYSTEM_IDENTITY.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Show {
                    reference: "info:system/hostname"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        assert_eq!(out.lines(), alloc::vec!["rustbox".to_string()]);

        // `stats:` — a metric, printed as the bare figure: its unit is
        // `describe`'s business, so a shell can capture this verbatim.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Show {
                    reference: "stats:mem/used"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        assert_eq!(out.lines(), alloc::vec!["3072".to_string()]);

        // `state:` — a mutable reading, rendered like a fact.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Show {
                    reference: "state:net/resolver/servers"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        assert_eq!(out.lines(), alloc::vec!["none".to_string()]);
    }

    /// `describe` reports the envelope rather than the value: the producer,
    /// the authorization the read was served under, and the payload's own
    /// metadata — a metric's kind/unit/reset/window, a fact's type and
    /// sensitivity.
    #[test]
    fn describe_reports_the_envelope_for_every_payload() {
        // A counter: kind, unit, and the reset behaviour a counter must
        // declare; no window, stated as `none` rather than omitted.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Describe {
                    reference: "stats:uptime"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        let lines = out.lines();
        assert!(lines.contains(&"reference     : stats:uptime".to_string()));
        assert!(lines.contains(&"producer      : sysinfod".to_string()));
        assert!(lines.contains(&"authorization : unprivileged".to_string()));
        assert!(lines.contains(&"payload       : metric".to_string()));
        assert!(lines.contains(&"kind          : counter".to_string()));
        assert!(lines.contains(&"unit          : seconds".to_string()));
        assert!(lines.contains(&"reset         : boot".to_string()));
        assert!(lines.contains(&"window        : none".to_string()));

        // A gated fact names the capability it was served under, so the
        // provenance of a privileged read is visible.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Describe {
                    reference: "info:mem/physical"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        let lines = out.lines();
        assert!(lines.contains(&"authorization : CAP_SYSINFO_KERNEL".to_string()));
        assert!(lines.contains(&"payload       : info".to_string()));
        assert!(lines.contains(&"value kind    : string".to_string()));
        assert!(lines.contains(&"sensitivity   : public".to_string()));

        // An identifying fact is marked sensitive, so a consumer can treat
        // it with care.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Describe {
                    reference: "info:system/machine-id"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        assert!(out
            .lines()
            .contains(&"sensitivity   : sensitive".to_string()));

        // A `state:` reading is labelled as such: it is not a stable fact.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Describe {
                    reference: "state:net/resolver/servers"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        assert!(out.lines().contains(&"payload       : state".to_string()));
    }

    /// A rate carries the window it was averaged over, spelled the way a
    /// `?window=` parameter is written — so a described window can be typed
    /// straight back into a reference.
    #[test]
    fn describe_reports_a_rate_window_in_parameter_spelling() {
        for (window, spelling) in [("1s", "1s"), ("10s", "10s"), ("500ms", "500ms")] {
            let fixture = Fixture::new(Vec::new());
            let out = Recorder::new();
            let reference = alloc::format!("stats:net/wan/rx.pps?window={window}");
            assert_eq!(
                run(
                    Command::Describe {
                        reference: &reference
                    },
                    &fixture,
                    &out
                ),
                Ok(())
            );
            let lines = out.lines();
            assert!(lines.contains(&"kind          : rate".to_string()));
            assert!(lines.contains(&"unit          : packets/s".to_string()));
            assert!(
                lines.contains(&alloc::format!("window        : {spelling}")),
                "window {window} rendered as {lines:?}"
            );
        }

        // And `show` of the same rate is the bare figure.
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Show {
                    reference: "stats:net/wan/rx.pps?window=1s"
                },
                &fixture,
                &out
            ),
            Ok(())
        );
        assert_eq!(out.lines(), alloc::vec!["120".to_string()]);
    }

    /// A capability denial names the capability the resource needs, so the
    /// user learns which grant to ask for rather than that "something" was
    /// refused. The name comes from the frozen query registry, never a table
    /// in this tool.
    #[test]
    fn a_denied_read_names_the_capability_it_needs() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.deny = Some(SysinfoQueryId::KERNEL_MEMORY_STATS);
        let out = Recorder::new();
        let err = run(
            Command::Show {
                reference: "info:mem/physical",
            },
            &fixture,
            &out,
        )
        .expect_err("denied");
        assert_eq!(
            err,
            SysinfoError::Unresolvable(tairix_procinfo::ResolveInfoError::CapabilityDenied(
                SysinfoQueryId::KERNEL_MEMORY_STATS
            ))
        );
        assert_eq!(
            alloc::format!("{err}"),
            "permission denied: this resource requires CAP_SYSINFO_KERNEL"
        );
        // Nothing was printed: a refused read writes no value.
        assert!(out.lines().is_empty());
    }

    /// A rate is undefined without a sampling window, so a reference missing
    /// its mandatory `?window=` fails closed — before any query is issued,
    /// so a malformed request never reaches the service.
    #[test]
    fn a_rate_without_a_window_fails_closed() {
        for command in [
            Command::Show {
                reference: "stats:net/wan/rx.pps",
            },
            Command::Describe {
                reference: "stats:net/wan/rx.pps",
            },
        ] {
            let fixture = Fixture::new(Vec::new());
            let out = Recorder::new();
            assert_eq!(
                run(command, &fixture, &out),
                Err(SysinfoError::Unresolvable(
                    tairix_procinfo::ResolveInfoError::UnsupportedRequest
                ))
            );
            assert!(out.lines().is_empty());
            assert!(
                fixture.seen.borrow().is_empty(),
                "an undefined rate never reaches the service"
            );
        }
    }

    /// The other two ways a reference can fail: a spelling the shared parser
    /// refuses, and a namespace whose values are not read here at all.
    #[test]
    fn a_malformed_or_unserved_reference_fails_closed() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        // Not a reference at all (a filesystem path has no namespace).
        assert_eq!(
            run(
                Command::Show {
                    reference: "/System/Kernel"
                },
                &fixture,
                &out
            ),
            Err(SysinfoError::BadReference)
        );
        // A well-formed reference in a namespace this resolver does not
        // serve: `sys:` is a kernel byte stream, not a value.
        assert_eq!(
            run(
                Command::Show {
                    reference: "sys:null"
                },
                &fixture,
                &out
            ),
            Err(SysinfoError::Unresolvable(
                tairix_procinfo::ResolveInfoError::NamespaceNotServed
            ))
        );
        assert!(out.lines().is_empty());
        assert!(fixture.seen.borrow().is_empty());
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(
            engine_run(Command::Help, None, NOW, &fixture, &OneDoc, &out),
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
        // The self-reported share (a trust-boundary diagnostic) has its own
        // column, distinct from the attested `payload`/`metadata` figures.
        assert!(lines[0].contains("self"), "header: {}", lines[0]);
        assert!(lines[1].contains("1024"), "row: {}", lines[1]);
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
    fn storage_renders_service_queue_and_health_rows() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::Storage, &fixture, &out).expect("storage renders");
        let lines = out.lines();
        // Three tables, one volume each: the ungated service counters, then
        // the two the kernel scope gates.
        assert_eq!(
            lines.len(),
            6,
            "three headers plus one volume each: {lines:?}"
        );
        for (header, row) in [(0, 1), (2, 3), (4, 5)] {
            assert!(lines[header].starts_with("volume"), "{}", lines[header]);
            assert!(
                lines[row].starts_with("abababababababab"),
                "row: {}",
                lines[row]
            );
        }
        // The service row carries the raw tallies, never a derived rate: a
        // reader deltas them over its own interval.
        assert!(lines[1].contains("4194304"), "read bytes: {}", lines[1]);
        assert!(lines[1].contains("1024"), "read ops: {}", lines[1]);
        assert!(lines[1].contains("500"), "busy ms: {}", lines[1]);
        // The queue row reads its occupancy against the class's own ceiling.
        assert!(lines[3].contains("2560"), "depth sum: {}", lines[3]);
        assert!(
            lines[3].contains(&BlkDeviceClass::SolidState.budget().queue_depth.to_string()),
            "budget depth: {}",
            lines[3]
        );
        // The health row is unchanged.
        assert!(lines[5].contains("recovering"), "row: {}", lines[5]);
        assert!(lines[5].contains("4096"), "completions: {}", lines[5]);
        for query in [
            SysinfoQueryId::VOLUME_IO_STATS,
            SysinfoQueryId::VOLUME_IO_QUEUE,
            SysinfoQueryId::VOLUME_IO_HEALTH,
        ] {
            assert!(fixture.seen.borrow().contains(&query), "{query:?}");
        }
    }

    #[test]
    fn storage_prints_the_ungated_service_table_before_a_gated_denial() {
        // The service counters are ungated, so a caller without the kernel
        // scope still gets them; the gated read then fails closed with the
        // CLI's permission error rather than the whole report vanishing.
        let mut denied = Fixture::new(Vec::new());
        denied.deny = Some(SysinfoQueryId::VOLUME_IO_QUEUE);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Storage, &denied, &out),
            Err(SysinfoError::PermissionDenied)
        );
        let lines = out.lines();
        // The service table and its row, then the heading of the table that
        // was refused — which names *which* table the reported refusal is
        // about — and nothing fabricated under it.
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[1].starts_with("abababababababab"),
            "row: {}",
            lines[1]
        );
        assert!(lines[2].contains("in-flight"), "heading: {}", lines[2]);
    }

    #[test]
    fn raid_renders_the_array_and_device_tables() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        run(Command::Raid, &fixture, &out).expect("raid renders");
        let lines = out.lines();
        // An array header, its two arrays, a blank separator, a device
        // header, and its two devices.
        assert_eq!(lines.len(), 7, "lines: {lines:?}");
        assert!(lines[0].starts_with("array"), "header: {}", lines[0]);
        assert!(lines[0].contains("health"), "header: {}", lines[0]);
        assert!(lines[0].contains("progress"), "header: {}", lines[0]);

        // The idle mirror: level, health, full member tally, and no pass
        // running.
        assert!(
            lines[1].starts_with("1111111111111111"),
            "row: {}",
            lines[1]
        );
        assert!(lines[1].contains("mirror"), "row: {}", lines[1]);
        assert!(lines[1].contains("optimal"), "row: {}", lines[1]);
        assert!(lines[1].contains("2/2"), "row: {}", lines[1]);
        assert!(lines[1].ends_with("idle"), "row: {}", lines[1]);

        // The rebuilding parity array: the degraded tally and the rebuild's
        // progress as a percentage of its blocks.
        assert!(
            lines[2].starts_with("2222222222222222"),
            "row: {}",
            lines[2]
        );
        assert!(lines[2].contains("parity"), "row: {}", lines[2]);
        assert!(lines[2].contains("recovering"), "row: {}", lines[2]);
        assert!(lines[2].contains("2/3"), "row: {}", lines[2]);
        assert!(lines[2].ends_with("resync 25%"), "row: {}", lines[2]);

        assert!(lines[3].is_empty(), "separator: {}", lines[3]);
        assert!(lines[4].starts_with("device"), "header: {}", lines[4]);
        assert!(lines[4].contains("disposition"), "header: {}", lines[4]);

        // The slotted member names its array and slot.
        assert!(lines[5].starts_with("51"), "row: {}", lines[5]);
        assert!(lines[5].contains("2222222222222222"), "row: {}", lines[5]);
        assert!(lines[5].contains("in-sync"), "row: {}", lines[5]);

        // The unaffiliated candidate belongs to no array and holds no slot,
        // which reads as a dash rather than a fabricated zero.
        assert!(lines[6].starts_with("52"), "row: {}", lines[6]);
        assert!(lines[6].contains("candidate"), "row: {}", lines[6]);
        assert!(!lines[6].contains("2222222222222222"), "row: {}", lines[6]);

        // Both queries routed, arrays before devices.
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::RAID_ARRAYS, SysinfoQueryId::RAID_MEMBERS]
        );
    }

    #[test]
    fn raid_fails_closed_on_a_denial_of_either_query() {
        // A refused array read ends the command; nothing is rendered beyond
        // the header, and no device query follows a refusal.
        let mut denied = Fixture::new(Vec::new());
        denied.deny = Some(SysinfoQueryId::RAID_ARRAYS);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Raid, &denied, &out),
            Err(SysinfoError::PermissionDenied)
        );
        assert_eq!(
            denied.seen.borrow().as_slice(),
            &[SysinfoQueryId::RAID_ARRAYS]
        );

        // A refused device read likewise ends it, rather than reporting the
        // arrays and silently dropping the devices.
        let mut denied = Fixture::new(Vec::new());
        denied.deny = Some(SysinfoQueryId::RAID_MEMBERS);
        assert_eq!(
            run(Command::Raid, &denied, &Recorder::new()),
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
    #[test]
    fn frames_render_the_desktop_reading_and_a_denial() {
        let fixture = Fixture::new(Vec::new());
        let out = Recorder::new();
        assert_eq!(run(Command::Frames, &fixture, &out), Ok(()));
        let lines = out.lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("damaged"));
        assert!(lines[0].contains("worst frame"));
        // The publisher, the frame count, the per-frame mean (400 000 over
        // ten frames), the overdraw multiple (1.2 M over 400 k), the copied
        // share (a quarter), and the worst frame as a quarter of the screen.
        assert!(lines[1].starts_with("91"), "{}", lines[1]);
        assert!(lines[1].contains("40000"), "{}", lines[1]);
        assert!(lines[1].contains("3.0x"), "{}", lines[1]);
        assert!(lines[1].contains("25%"), "{}", lines[1]);
        assert!(lines[1].contains("96/4"), "{}", lines[1]);
        assert_eq!(
            fixture.seen.borrow().as_slice(),
            &[SysinfoQueryId::DESKTOP_FRAME_STATS]
        );

        // A session that has published nothing prints the header alone:
        // absent is not a row of zeros.
        let mut quiet = Fixture::new(Vec::new());
        quiet.frames = Vec::new();
        let out = Recorder::new();
        assert_eq!(run(Command::Frames, &quiet, &out), Ok(()));
        assert_eq!(out.lines().len(), 1);

        // The service's capability refusal surfaces as the CLI's
        // permission-denied error, never a fabricated table.
        let mut denied = Fixture::new(Vec::new());
        denied.deny = Some(SysinfoQueryId::DESKTOP_FRAME_STATS);
        let out = Recorder::new();
        assert_eq!(
            run(Command::Frames, &denied, &out),
            Err(SysinfoError::PermissionDenied)
        );
    }

    #[test]
    fn a_frame_figure_with_no_denominator_prints_absent() {
        // A publisher's own frame count is its figure, not the reader's, so
        // a record that claims a screen but no frame must not make the
        // renderer divide by zero — it prints absent instead.
        let mut fixture = Fixture::new(Vec::new());
        fixture.frames = alloc::vec![DesktopFrameRecord {
            reporter_pid: 7,
            totals: DesktopFrameTotals {
                screen_px: 0,
                frames: 0,
                ..DesktopFrameTotals::ZERO
            },
        }];
        let out = Recorder::new();
        assert_eq!(run(Command::Frames, &fixture, &out), Ok(()));
        let row = &out.lines()[1];
        assert!(row.contains('-'), "{row}");
        assert!(row.contains("0.0x"), "no damage is no overdraw: {row}");
    }
}
