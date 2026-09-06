//! Unit tests for the [`Sampler`] and [`probe_scopes`].

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::blkio::BlkHealthCounters;
use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
use tairix_abi::hwtree::{HwDeviceClass, HwNode, HwTreeHeader, HW_NODE_ROOT};
use tairix_abi::net_ipc::{
    NetAddrFamily, NetCounters, NetIfKind, NetInterfaceCountersRecord, NetInterfaceFactsRecord,
    NetInterfaceRatesRecord, NetInterfaceStateRecord, NetServerAddr, NetSockProto, NetSockState,
    NetSocketRecord, NetStackDefenceCounters, IF_NAME_LEN, NET_IF_MAX_ADDRS,
};
use tairix_abi::rlimit::{LimitKind, ResourceLimit};
use tairix_abi::sysinfo::{
    CacheLedgerRecord, CacheOwnerKind, CpuCoreClass, CpuInfoRecord, CpuLoadRecord,
    CpuTimeListRequest, CpuTimeRecord, CrashFaultBucket, CrashFaultClass, CrashRecord,
    KernelMemoryStats, LoadAverage, MemoryPressureBand, MemoryPressureStats, MemoryTotal,
    MountAvailability, MountListRequest, MountRecord, MountVolumeState, ProcessListRequest,
    ProcessRecord, ProcessState, RamzipStats, ReclaimClassRecord, ResourceLimitRecord, SeatRecord,
    SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime, VolumeIoHealthRecord,
    CPU_INFO_FLAG_FREQ_MEASURED, MACHINE_ID_LEN, MOUNT_VOLUME_ID_LEN,
};
use tairix_abi::{Duration64, Errno, ProcId, SchedPriority, Time64};
use tairix_procinfo::Transport;

use super::{probe_scopes, Absence, DegradedField, Sampler, ScopeVerdicts};

/// How the fixture answers one query family.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
enum Answer {
    /// Serve the configured records.
    #[default]
    Serve,
    /// Refuse with the capability denial (for the process family this
    /// denies only the global-scope query; the self-scope one still
    /// serves, exactly as the real gate behaves).
    Deny,
    /// Fail with a transient, non-capability error.
    Fail,
}

/// An in-memory `sysinfod` stand-in answering every query [`Sampler`]
/// issues, decoding each request exactly as the real service.
///
/// The three readings with their own delta/carry-forward bookkeeping (the
/// process list, CPU time, memory pressure) keep their own typed fields;
/// every other reading is configured as already-encoded record blobs in
/// [`Self::records`], since the sampler's fetch path treats them uniformly —
/// one entry per record for a paged reading, a single entry for a
/// fixed-size one. That is what lets one fixture answer seventeen queries
/// without seventeen pairs of fields.
struct Fixture {
    processes: RefCell<Vec<ProcessRecord>>,
    cpu: RefCell<Vec<CpuTimeRecord>>,
    pressure: RefCell<MemoryPressureStats>,
    process_answer: Answer,
    cpu_answer: Answer,
    memory_answer: Answer,
    records: RefCell<BTreeMap<SysinfoQueryId, Vec<Vec<u8>>>>,
    answers: RefCell<BTreeMap<SysinfoQueryId, Answer>>,
    seen: RefCell<Vec<SysinfoQueryId>>,
}

impl Fixture {
    /// A fixture that answers every reading plausibly: the fixed-size
    /// readings with a default-valued record, the paged ones with an empty
    /// list (a real, honest "nothing to report" the sampler must not treat
    /// as a failure). A test then overrides only the reading it is about.
    fn new() -> Self {
        let mut records: BTreeMap<SysinfoQueryId, Vec<Vec<u8>>> = BTreeMap::new();
        records.insert(
            SysinfoQueryId::SYSTEM_IDENTITY,
            alloc::vec![identity(b"host").to_le_bytes().to_vec()],
        );
        records.insert(
            SysinfoQueryId::UPTIME,
            alloc::vec![Uptime::default().to_le_bytes().to_vec()],
        );
        records.insert(
            SysinfoQueryId::LOAD_AVERAGE,
            alloc::vec![LoadAverage::default().to_le_bytes().to_vec()],
        );
        records.insert(
            SysinfoQueryId::MEMORY_TOTAL,
            alloc::vec![MemoryTotal::default().to_le_bytes().to_vec()],
        );
        records.insert(
            SysinfoQueryId::KERNEL_MEMORY_STATS,
            alloc::vec![KernelMemoryStats::default().to_le_bytes().to_vec()],
        );
        records.insert(SysinfoQueryId::RESOURCE_LIMITS, alloc::vec![limit_report()]);
        records.insert(
            SysinfoQueryId::MEMORY_PRESSURE_BAND,
            alloc::vec![MemoryPressureBand::default().to_le_bytes().to_vec()],
        );
        records.insert(
            SysinfoQueryId::RAMZIP_STATS,
            alloc::vec![RamzipStats::default().to_le_bytes().to_vec()],
        );
        records.insert(
            SysinfoQueryId::NET_STACK_DEFENCE,
            alloc::vec![NetStackDefenceCounters::default().to_le_bytes().to_vec()],
        );
        // The hardware tree is the one reading whose reply carries a header
        // ahead of its records, so an empty snapshot is that header alone.
        records.insert(
            SysinfoQueryId::HARDWARE_TREE,
            alloc::vec![HwTreeHeader::new(1, 0).to_le_bytes().to_vec()],
        );
        for paged in [
            SysinfoQueryId::CPU_INFO,
            SysinfoQueryId::CPU_LOAD,
            SysinfoQueryId::MOUNT_LIST,
            SysinfoQueryId::VOLUME_IO_HEALTH,
            SysinfoQueryId::NET_INTERFACE_FACTS,
            SysinfoQueryId::NET_INTERFACE_STATE,
            SysinfoQueryId::NET_INTERFACE_RATES,
            SysinfoQueryId::NET_INTERFACE_COUNTERS,
            SysinfoQueryId::NET_SOCKETS,
            SysinfoQueryId::NET_RESOLVER_SERVERS,
            SysinfoQueryId::NET_TIME_SERVERS,
            SysinfoQueryId::RECLAIM_STATS,
            SysinfoQueryId::CACHE_LEDGERS,
            SysinfoQueryId::SEAT_LIST,
            SysinfoQueryId::CRASH_RECORD,
        ] {
            records.insert(paged, Vec::new());
        }
        Self {
            processes: RefCell::new(Vec::new()),
            cpu: RefCell::new(Vec::new()),
            pressure: RefCell::new(MemoryPressureStats::default()),
            process_answer: Answer::Serve,
            cpu_answer: Answer::Serve,
            memory_answer: Answer::Serve,
            records: RefCell::new(records),
            answers: RefCell::new(BTreeMap::new()),
            seen: RefCell::new(Vec::new()),
        }
    }

    fn set_processes(&self, records: Vec<ProcessRecord>) {
        *self.processes.borrow_mut() = records;
    }

    fn set_cpu(&self, records: Vec<CpuTimeRecord>) {
        *self.cpu.borrow_mut() = records;
    }

    fn set_pressure(&self, stats: MemoryPressureStats) {
        *self.pressure.borrow_mut() = stats;
    }

    /// Serve `blobs` for `query`: one entry per record for a paged reading,
    /// a single whole-reply entry for a fixed-size one.
    fn serve(&self, query: SysinfoQueryId, blobs: Vec<Vec<u8>>) {
        self.records.borrow_mut().insert(query, blobs);
    }

    /// Answer `query` with `answer` instead of its records.
    fn answer(&self, query: SysinfoQueryId, answer: Answer) {
        self.answers.borrow_mut().insert(query, answer);
    }

    /// How many times `query` was issued.
    fn count_of(&self, query: SysinfoQueryId) -> usize {
        self.seen.borrow().iter().filter(|q| **q == query).count()
    }

    /// Whether `query` was issued at all.
    fn saw(&self, query: SysinfoQueryId) -> bool {
        self.count_of(query) > 0
    }
}

impl Transport for Fixture {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        let header = SysinfoRequestHeader::from_bytes(request)?;
        self.seen.borrow_mut().push(header.query);
        let payload = &request[SysinfoRequestHeader::WIRE_LEN
            ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
        match header.query {
            SysinfoQueryId::SELF_PROCESS_LIST | SysinfoQueryId::GLOBAL_PROCESS_LIST => {
                if header.query == SysinfoQueryId::GLOBAL_PROCESS_LIST
                    && self.process_answer == Answer::Deny
                {
                    return Err(Errno::PermissionDenied);
                }
                if self.process_answer == Answer::Fail {
                    return Err(Errno::NotFound);
                }
                let req = ProcessListRequest::from_bytes(payload)?;
                Ok(page(&self.processes.borrow(), req.offset, req.limit, |r| {
                    r.to_le_bytes().to_vec()
                }))
            }
            SysinfoQueryId::CPU_TIME_STATS => match self.cpu_answer {
                Answer::Deny => Err(Errno::PermissionDenied),
                Answer::Fail => Err(Errno::NotFound),
                Answer::Serve => {
                    let req = CpuTimeListRequest::from_bytes(payload)?;
                    Ok(page(&self.cpu.borrow(), req.offset, req.limit, |r| {
                        r.to_le_bytes().to_vec()
                    }))
                }
            },
            SysinfoQueryId::MEMORY_PRESSURE => match self.memory_answer {
                Answer::Deny => Err(Errno::PermissionDenied),
                Answer::Fail => Err(Errno::NotFound),
                Answer::Serve => Ok(self.pressure.borrow().to_le_bytes().to_vec()),
            },
            other => match self
                .answers
                .borrow()
                .get(&other)
                .copied()
                .unwrap_or_default()
            {
                Answer::Deny => Err(Errno::PermissionDenied),
                Answer::Fail => Err(Errno::NotFound),
                Answer::Serve => {
                    let records = self.records.borrow();
                    let blobs = records.get(&other).ok_or(Errno::NotFound)?;
                    if payload.is_empty() {
                        // A fixed-size reading takes no payload, so the
                        // whole configured reply is the answer.
                        Ok(blobs.concat())
                    } else {
                        // Every paged list request shares the same
                        // `{offset, limit}` header, exactly as the sampler
                        // relies on.
                        let req = MountListRequest::from_bytes(payload)?;
                        Ok(page(blobs, req.offset, req.limit, Clone::clone))
                    }
                }
            },
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

fn process(
    pid: u64,
    proc_id: ProcId,
    state: ProcessState,
    cpu_time_ns: u64,
    name: &[u8],
) -> ProcessRecord {
    ProcessRecord::new(
        pid,
        1,
        proc_id,
        ProcId::KERNEL,
        1000,
        1000,
        state,
        0,
        SchedPriority::Normal,
        cpu_time_ns,
        0,
        0,
        0,
        name,
    )
    .expect("valid record")
}

fn granted() -> ScopeVerdicts {
    ScopeVerdicts {
        global_process_scope: true,
        memory_pressure: true,
        hardware_scope: true,
    }
}

const NS: u64 = 1_000_000_000;

/// A system identity carrying `hostname`, otherwise unremarkable.
fn identity(hostname: &[u8]) -> SystemIdentity {
    SystemIdentity::new([7u8; MACHINE_ID_LEN], 1, 2, 3, hostname).expect("a valid identity")
}

/// The fixed resource-limit report: one record per limit kind, in
/// discriminant order, exactly as the service always answers it.
fn limit_report() -> Vec<u8> {
    let mut out = Vec::new();
    for (index, kind) in LimitKind::ALL.iter().enumerate() {
        let usage = u64::try_from(index).unwrap_or(0);
        let limit = ResourceLimit::new(1_000, 2_000).expect("soft below hard");
        out.extend_from_slice(&ResourceLimitRecord::new(*kind, limit, usage).to_le_bytes());
    }
    out
}

/// An interface name, NUL-padded as the wire carries it.
fn if_name(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

/// One per-CPU load record, distinguishable by its CPU index.
fn cpu_load(cpu: u32) -> CpuLoadRecord {
    CpuLoadRecord {
        cpu,
        ..CpuLoadRecord::default()
    }
}

/// One CPU record whose clock is flagged as genuinely measured, so a
/// consumer may trust the figure rather than discard it.
fn measured_clock(cpu: u32, hz: u64) -> CpuInfoRecord {
    CpuInfoRecord::new(
        cpu,
        CpuCoreClass::Performance,
        CPU_INFO_FLAG_FREQ_MEASURED,
        0,
        0,
        hz,
        1_000_000,
        b"Test Core",
    )
    .expect("a valid CPU record")
}

/// Encode `records` as one blob per record, the shape [`Fixture::serve`]
/// takes for a paged reading.
fn blobs<T>(records: &[T], encode: impl Fn(&T) -> Vec<u8>) -> Vec<Vec<u8>> {
    records.iter().map(encode).collect()
}

/// The last of `ticks` samples over `fixture`, one second apart, with
/// every scope granted.
fn sampled(fixture: &Fixture, ticks: u64) -> super::Sample {
    let mut sampler = Sampler::new(granted());
    let mut last = None;
    for i in 0..ticks {
        last = Some(sampler.sample(fixture, i * NS));
    }
    last.expect("at least one sample")
}

/// One CPU inventory record naming `model`.
fn cpu_info(cpu: u32, model: &[u8]) -> CpuInfoRecord {
    CpuInfoRecord::new(
        cpu,
        CpuCoreClass::Performance,
        0,
        0,
        0,
        2_000_000_000,
        1_000_000,
        model,
    )
    .expect("a valid CPU record")
}

/// One mount whose volume reports the given capacity.
fn mount_record(block_size: u32, total_blocks: u64, free_blocks: u64) -> MountRecord {
    MountRecord::new(
        b"id::1",
        b"/Storage/disk",
        b"arxfs",
        MountFlags::from_bits(0).expect("no mount flags set"),
        MountVolumeState {
            usage: VolumeStats {
                block_size,
                total_blocks,
                free_blocks,
                avail_blocks: free_blocks,
                ..VolumeStats::default()
            },
            availability: MountAvailability::Available,
            medium: None,
        },
        [0u8; MOUNT_VOLUME_ID_LEN],
    )
    .expect("a valid mount record")
}

/// One per-volume health record for device number `dev`.
fn volume_health(dev: u64) -> VolumeIoHealthRecord {
    VolumeIoHealthRecord::new(
        [0u8; 16],
        dev,
        MountAvailability::Available,
        BlkHealthCounters::default(),
    )
}

/// One interface's static facts.
fn net_facts(name: &[u8], mtu: u32) -> NetInterfaceFactsRecord {
    NetInterfaceFactsRecord {
        name: if_name(name),
        kind: NetIfKind::Ethernet,
        mac: [0x02, 0, 0, 0, 0, 1],
        mtu,
        offloads: 0,
        rx_queues: 1,
    }
}

/// One interface's live state, with no bound addresses.
fn net_state(name: &[u8], link_up: bool) -> NetInterfaceStateRecord {
    NetInterfaceStateRecord {
        name: if_name(name),
        link_up,
        addr_count: 0,
        addrs: [NetInterfaceStateRecord::EMPTY_ADDR; NET_IF_MAX_ADDRS],
    }
}

/// One interface's throughput over a one-second window.
fn net_rates(name: &[u8], rx_bps: u64) -> NetInterfaceRatesRecord {
    NetInterfaceRatesRecord {
        name: if_name(name),
        window: Duration64::from_secs(1),
        rx_pps: 1,
        rx_bps,
        tx_pps: 0,
        tx_bps: 0,
    }
}

/// One CPU's execution-time record.
fn cpu_time(cpu: u32, busy_ns: u64, idle_ns: u64) -> CpuTimeRecord {
    CpuTimeRecord {
        cpu,
        reserved: 0,
        busy_ns,
        idle_ns,
    }
}

/// One interface's cumulative counters, distinguishable by `rx_bytes`.
fn net_counters(name: &[u8], rx_bytes: u64) -> NetInterfaceCountersRecord {
    NetInterfaceCountersRecord {
        name: if_name(name),
        counters: NetCounters {
            rx_bytes,
            ..NetCounters::default()
        },
    }
}

/// One socket record in `state`; the census reads nothing else off it.
fn socket(state: NetSockState) -> NetSocketRecord {
    NetSocketRecord {
        proto: NetSockProto::Tcp,
        state,
        family: NetAddrFamily::V4,
        local_addr: [0; 16],
        local_port: 80,
        peer_addr: [0; 16],
        peer_port: 0,
        owner: 0,
        recv_q: 0,
        send_q: 0,
    }
}

/// One configured server at IPv4 `octets`.
fn server(octets: [u8; 4]) -> NetServerAddr {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&octets);
    NetServerAddr {
        family: NetAddrFamily::V4,
        addr,
    }
}

/// One reclaim-ledger row for reclaim class `class`, holding
/// `payload_bytes`.
fn reclaim_class(class: u8, payload_bytes: u64) -> ReclaimClassRecord {
    ReclaimClassRecord {
        class,
        payload_bytes,
        ..ReclaimClassRecord::default()
    }
}

/// One bounded-cache ledger row named `label`, holding `payload_bytes`.
fn cache_ledger(label: &[u8], payload_bytes: u64) -> CacheLedgerRecord {
    let mut record = CacheLedgerRecord::new(label, CacheOwnerKind::KernelSubsystem, 0, 0)
        .expect("a valid ledger row");
    record.payload_bytes = payload_bytes;
    record
}

/// A whole hardware-tree snapshot reply: its header, then `nodes`.
fn hw_snapshot(nodes: &[HwNode]) -> Vec<u8> {
    let count = u64::try_from(nodes.len()).expect("a plausible node count");
    let mut out = HwTreeHeader::new(1, count).to_le_bytes().to_vec();
    for node in nodes {
        out.extend_from_slice(&node.to_le_bytes());
    }
    out
}

/// One user-fault crash record for a process called `name`.
fn crash_record(name: &[u8]) -> CrashRecord {
    CrashRecord::new(
        ProcId::from_raw([9; 16]),
        99,
        1000,
        1000,
        false,
        CrashFaultClass::Wild,
        CrashFaultBucket::Wild,
        0,
        name,
    )
    .expect("a valid crash record")
}

/// One process record carrying the storage I/O counters under test.
fn process_with_io(pid: u64, proc_id: ProcId, read: u64, written: u64) -> ProcessRecord {
    ProcessRecord::new(
        pid,
        1,
        proc_id,
        ProcId::KERNEL,
        1000,
        1000,
        ProcessState::Running,
        0,
        SchedPriority::Normal,
        0,
        0,
        read,
        written,
        b"io",
    )
    .expect("valid record")
}

/// The readings the cadence policy issues on every sample.
const EVERY_SAMPLE: [SysinfoQueryId; 8] = [
    SysinfoQueryId::GLOBAL_PROCESS_LIST,
    SysinfoQueryId::CPU_TIME_STATS,
    SysinfoQueryId::UPTIME,
    SysinfoQueryId::LOAD_AVERAGE,
    SysinfoQueryId::CPU_INFO,
    SysinfoQueryId::CPU_LOAD,
    SysinfoQueryId::NET_INTERFACE_STATE,
    SysinfoQueryId::NET_INTERFACE_RATES,
];

/// The readings on the audited memory cadence.
const MEMORY: [SysinfoQueryId; 2] = [
    SysinfoQueryId::MEMORY_PRESSURE,
    SysinfoQueryId::KERNEL_MEMORY_STATS,
];

/// The readings on the slow-moving inventory cadence.
const INVENTORY: [SysinfoQueryId; 5] = [
    SysinfoQueryId::MOUNT_LIST,
    SysinfoQueryId::VOLUME_IO_HEALTH,
    SysinfoQueryId::SEAT_LIST,
    SysinfoQueryId::RESOURCE_LIMITS,
    SysinfoQueryId::CRASH_RECORD,
];

/// The readings fetched once and cached for the sampler's life.
const STATIC: [SysinfoQueryId; 3] = [
    SysinfoQueryId::SYSTEM_IDENTITY,
    SysinfoQueryId::MEMORY_TOTAL,
    SysinfoQueryId::NET_INTERFACE_FACTS,
];

#[test]
fn probe_scopes_reports_granted_when_not_denied() {
    let fixture = Fixture::new();
    let verdicts = probe_scopes(&fixture);
    assert!(verdicts.global_process_scope);
    assert!(verdicts.memory_pressure);
}

#[test]
fn probe_scopes_falls_back_to_self_scope_on_denial() {
    let mut fixture = Fixture::new();
    fixture.process_answer = Answer::Deny;
    fixture.memory_answer = Answer::Deny;
    let verdicts = probe_scopes(&fixture);
    assert!(!verdicts.global_process_scope);
    assert!(!verdicts.memory_pressure);
}

#[test]
fn probe_scopes_treats_a_transient_failure_as_granted() {
    // A non-`PermissionDenied` failure at probe time must not permanently
    // condemn the field to self-scope: only a real capability refusal does.
    let mut fixture = Fixture::new();
    fixture.process_answer = Answer::Fail;
    fixture.memory_answer = Answer::Fail;
    let verdicts = probe_scopes(&fixture);
    assert!(verdicts.global_process_scope);
    assert!(verdicts.memory_pressure);
}

#[test]
fn first_sample_has_no_top_task_but_counts_stopped() {
    let fixture = Fixture::new();
    fixture.set_processes(alloc::vec![
        process(
            1,
            ProcId::from_raw([1; 16]),
            ProcessState::Running,
            500,
            b"init"
        ),
        process(
            2,
            ProcId::from_raw([2; 16]),
            ProcessState::Stopped,
            10,
            b"job"
        ),
    ]);
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);
    assert_eq!(sample.stopped_count, 1);
    assert!(sample.top_task.is_none());
}

#[test]
fn second_sample_picks_the_highest_delta_task() {
    let fixture = Fixture::new();
    let a = ProcId::from_raw([1; 16]);
    let b = ProcId::from_raw([2; 16]);
    fixture.set_processes(alloc::vec![
        process(1, a, ProcessState::Running, 100, b"alpha"),
        process(2, b, ProcessState::Running, 100, b"beta"),
    ]);
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);

    fixture.set_processes(alloc::vec![
        process(1, a, ProcessState::Running, 100 + 200_000_000, b"alpha"),
        process(2, b, ProcessState::Running, 100 + 900_000_000, b"beta"),
    ]);
    let sample = sampler.sample(&fixture, NS);
    let top = sample.top_task.expect("a top task after two samples");
    assert_eq!(top.name.as_slice(), b"beta");
    // 0.9s of CPU-time delta over a 1s interval -> 900 permille.
    assert_eq!(top.cpu_permille, 900);
}

#[test]
fn a_pid_reused_across_lifetimes_is_not_confused_via_proc_id() {
    let fixture = Fixture::new();
    let old_owner = ProcId::from_raw([1; 16]);
    let new_owner = ProcId::from_raw([2; 16]);
    // pid 7 belonged to `old_owner` with a large cumulative time...
    fixture.set_processes(alloc::vec![process(
        7,
        old_owner,
        ProcessState::Running,
        1_000_000,
        b"old"
    )]);
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);

    // ...then pid 7 is reused by a brand-new process instance with a tiny
    // cumulative time. Keying on `proc_id` must treat this as a first
    // sighting (zero delta), never as a fabricated huge negative-turned-zero
    // delta from the old owner's history.
    fixture.set_processes(alloc::vec![process(
        7,
        new_owner,
        ProcessState::Running,
        5,
        b"new"
    )]);
    let sample = sampler.sample(&fixture, NS);
    // The only candidate has a zero delta (first sight), so it is still the
    // (uninteresting) top task at 0 permille.
    let top = sample.top_task.expect("a candidate exists");
    assert_eq!(top.name.as_slice(), b"new");
    assert_eq!(top.cpu_permille, 0);
}

#[test]
fn process_list_failure_degrades_once_and_leaves_history_intact() {
    let mut fixture = Fixture::new();
    let a = ProcId::from_raw([1; 16]);
    fixture.set_processes(alloc::vec![process(1, a, ProcessState::Running, 100, b"a")]);
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);

    fixture.process_answer = Answer::Fail;
    let sample = sampler.sample(&fixture, NS);
    assert_eq!(sample.stopped_count, 0);
    assert!(sample.top_task.is_none());
    assert_eq!(sample.degradations, alloc::vec![DegradedField::ProcessList]);

    // A second consecutive failure does not repeat the notice.
    let sample = sampler.sample(&fixture, 2 * NS);
    assert!(sample.degradations.is_empty());
}

#[test]
fn stopped_count_saturates_rather_than_overflowing() {
    let fixture = Fixture::new();
    let mut records = Vec::new();
    for i in 0..300u32 {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&i.to_le_bytes());
        records.push(process(
            u64::from(i),
            ProcId::from_raw(bytes),
            ProcessState::Stopped,
            0,
            b"p",
        ));
    }
    fixture.set_processes(records);
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);
    assert_eq!(sample.stopped_count, 300);
}

#[test]
fn cpu_busy_permille_uses_the_all_zero_default_on_the_first_sample() {
    let fixture = Fixture::new();
    fixture.set_cpu(alloc::vec![CpuTimeRecord {
        cpu: 0,
        reserved: 0,
        busy_ns: 750,
        idle_ns: 250,
    }]);
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);
    assert_eq!(sample.cpu_busy_permille, Some(750));
}

#[test]
fn cpu_busy_permille_is_a_delta_on_later_samples() {
    let fixture = Fixture::new();
    fixture.set_cpu(alloc::vec![CpuTimeRecord {
        cpu: 0,
        reserved: 0,
        busy_ns: 100,
        idle_ns: 100,
    }]);
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);

    fixture.set_cpu(alloc::vec![CpuTimeRecord {
        cpu: 0,
        reserved: 0,
        busy_ns: 100 + 750,
        idle_ns: 100 + 250,
    }]);
    let sample = sampler.sample(&fixture, NS);
    assert_eq!(sample.cpu_busy_permille, Some(750));
}

#[test]
fn cpu_time_failure_degrades_once() {
    let mut fixture = Fixture::new();
    fixture.cpu_answer = Answer::Fail;
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);
    assert_eq!(sample.cpu_busy_permille, None);
    assert_eq!(sample.degradations, alloc::vec![DegradedField::CpuTime]);
    let sample = sampler.sample(&fixture, NS);
    assert!(sample.degradations.is_empty());
}

#[test]
fn memory_pressure_is_never_queried_when_not_granted() {
    let fixture = Fixture::new();
    let scopes = ScopeVerdicts {
        global_process_scope: true,
        memory_pressure: false,
        hardware_scope: true,
    };
    let mut sampler = Sampler::new(scopes);
    let sample = sampler.sample(&fixture, 0);
    assert!(sample.memory_pressure.is_none());
    assert!(!fixture
        .seen
        .borrow()
        .contains(&SysinfoQueryId::MEMORY_PRESSURE));
}

#[test]
fn memory_pressure_is_queried_only_every_fifth_sample() {
    let fixture = Fixture::new();
    fixture.set_pressure(MemoryPressureStats {
        band: 2,
        total_bytes: 1000,
        free_bytes: 250,
        ..MemoryPressureStats::default()
    });
    let mut sampler = Sampler::new(granted());
    for i in 0..7u64 {
        let _ = sampler.sample(&fixture, i * NS);
    }
    let memory_queries = fixture
        .seen
        .borrow()
        .iter()
        .filter(|q| **q == SysinfoQueryId::MEMORY_PRESSURE)
        .count();
    // Samples 0 and 5 (indices divisible by the divider) out of 7 samples.
    assert_eq!(memory_queries, 2);
}

#[test]
fn memory_pressure_reading_is_carried_forward_between_queries() {
    let fixture = Fixture::new();
    fixture.set_pressure(MemoryPressureStats {
        band: 3,
        total_bytes: 1000,
        free_bytes: 100,
        ..MemoryPressureStats::default()
    });
    let mut sampler = Sampler::new(granted());
    let first = sampler.sample(&fixture, 0).memory_pressure.expect("read");
    assert_eq!(first.band, 3);
    assert_eq!(first.used_permille, 900);

    // Change the backing data; the next four samples do not re-query, so
    // the carried-forward reading is unchanged.
    fixture.set_pressure(MemoryPressureStats {
        band: 0,
        total_bytes: 1000,
        free_bytes: 1000,
        ..MemoryPressureStats::default()
    });
    for i in 1..5u64 {
        let sample = sampler.sample(&fixture, i * NS);
        let reading = sample.memory_pressure.expect("carried forward");
        assert_eq!(reading.band, 3);
    }
}

#[test]
fn memory_pressure_failure_degrades_once_and_keeps_the_last_reading() {
    let mut fixture = Fixture::new();
    fixture.set_pressure(MemoryPressureStats {
        band: 1,
        total_bytes: 1000,
        free_bytes: 500,
        ..MemoryPressureStats::default()
    });
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);

    fixture.memory_answer = Answer::Fail;
    // Advance to the next memory-query cycle (sample index 5).
    let mut sample = None;
    for i in 1..6u64 {
        sample = Some(sampler.sample(&fixture, i * NS));
    }
    let sample = sample.expect("at least one sample");
    assert_eq!(
        sample.degradations,
        alloc::vec![DegradedField::MemoryPressure]
    );
    let reading = sample.memory_pressure.expect("the last known reading");
    assert_eq!(reading.band, 1);
}

#[test]
fn the_fixed_size_readings_are_present_and_decoded() {
    let fixture = Fixture::new();
    fixture.serve(
        SysinfoQueryId::UPTIME,
        alloc::vec![Uptime {
            since_boot: Duration64::from_secs(3_600),
            boot_time: Time64::from_secs(1_000),
        }
        .to_le_bytes()
        .to_vec()],
    );
    fixture.serve(
        SysinfoQueryId::SYSTEM_IDENTITY,
        alloc::vec![identity(b"tairix-box").to_le_bytes().to_vec()],
    );
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);

    let uptime = sample.uptime.expect("uptime read");
    assert_eq!(uptime.since_boot, Duration64::from_secs(3_600));
    assert_eq!(uptime.boot_time, Time64::from_secs(1_000));
    let identity = sample.identity.expect("identity read");
    assert_eq!(identity.hostname_bytes(), b"tairix-box");
    assert_eq!(identity.version_major, 1);
    assert_eq!(sample.load_average, Some(LoadAverage::default()));
    assert_eq!(sample.memory_total, Some(MemoryTotal::default()));
    assert_eq!(sample.kernel_memory, Some(KernelMemoryStats::default()));
    assert_eq!(sample.pressure_band, Some(MemoryPressureBand::default()));
    assert_eq!(sample.ramzip, Some(RamzipStats::default()));
    assert_eq!(
        sample.stack_defence,
        Some(NetStackDefenceCounters::default())
    );

    // The limit report is decoded positionally: one record per kind, in
    // discriminant order.
    let limits = sample.resource_limits.expect("limits read");
    assert_eq!(limits.len(), LimitKind::COUNT);
    for (index, kind) in LimitKind::ALL.iter().enumerate() {
        assert_eq!(limits[index].kind, *kind);
        assert_eq!(limits[index].limit.soft, 1_000);
    }
    assert!(sample.degradations.is_empty());
}

#[test]
fn the_paged_readings_are_present_and_decoded() {
    let fixture = Fixture::new();
    fixture.serve(
        SysinfoQueryId::CPU_INFO,
        blobs(&[cpu_info(0, b"Test Core")], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::CPU_LOAD,
        blobs(&[cpu_load(0), cpu_load(1)], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::MOUNT_LIST,
        blobs(&[mount_record(4_096, 100, 40)], |r| {
            r.to_le_bytes().to_vec()
        }),
    );
    fixture.serve(
        SysinfoQueryId::VOLUME_IO_HEALTH,
        blobs(&[volume_health(7)], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::NET_INTERFACE_FACTS,
        blobs(&[net_facts(b"eth0", 1_500)], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::NET_INTERFACE_STATE,
        blobs(&[net_state(b"eth0", true)], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::NET_INTERFACE_RATES,
        blobs(&[net_rates(b"eth0", 8_000)], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::SEAT_LIST,
        blobs(&[SeatRecord::default()], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::CRASH_RECORD,
        blobs(&[crash_record(b"wild")], |r| r.to_le_bytes().to_vec()),
    );

    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);

    let cpus = sample.cpu_info.expect("cpu inventory read");
    assert_eq!(cpus.len(), 1);
    assert_eq!(cpus[0].model_bytes(), b"Test Core");
    assert_eq!(
        sample.cpu_load.expect("cpu load read"),
        alloc::vec![cpu_load(0), cpu_load(1)]
    );

    // The mount reading is what carries a volume's real capacity.
    let mounts = sample.mounts.expect("mount table read");
    assert_eq!(mounts[0].usage().block_size, 4_096);
    assert_eq!(mounts[0].usage().total_blocks, 100);
    assert_eq!(mounts[0].usage().free_blocks, 40);

    assert_eq!(sample.volume_health.expect("health read")[0].dev(), 7);
    let facts = sample.net_facts.expect("interface facts read");
    assert_eq!(facts[0].mtu, 1_500);
    assert!(sample.net_state.expect("interface state read")[0].link_up);
    assert_eq!(sample.net_rates.expect("rates read")[0].rx_bps, 8_000);
    assert_eq!(sample.seats.expect("seats read").len(), 1);
    let crashes = sample.crashes.expect("crash records read");
    assert_eq!(crashes[0].name_bytes(), b"wild");
    assert!(sample.degradations.is_empty());
}

#[test]
fn the_resource_pane_readings_are_present_and_decoded() {
    let fixture = Fixture::new();
    fixture.serve(
        SysinfoQueryId::NET_INTERFACE_COUNTERS,
        blobs(&[net_counters(b"eth0", 4_096)], |r| {
            r.to_le_bytes().to_vec()
        }),
    );
    fixture.serve(
        SysinfoQueryId::NET_SOCKETS,
        blobs(
            &[
                socket(NetSockState::Established),
                socket(NetSockState::Established),
                socket(NetSockState::Listen),
                socket(NetSockState::Closed),
            ],
            |r| r.to_le_bytes().to_vec(),
        ),
    );
    fixture.serve(
        SysinfoQueryId::NET_RESOLVER_SERVERS,
        blobs(&[server([1, 1, 1, 1])], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::NET_TIME_SERVERS,
        blobs(&[server([2, 2, 2, 2]), server([3, 3, 3, 3])], |r| {
            r.to_le_bytes().to_vec()
        }),
    );
    fixture.serve(
        SysinfoQueryId::RECLAIM_STATS,
        blobs(&[reclaim_class(0, 2_048)], |r| r.to_le_bytes().to_vec()),
    );
    fixture.serve(
        SysinfoQueryId::CACHE_LEDGERS,
        blobs(&[cache_ledger(b"glyph atlas", 1_024)], |r| {
            r.to_le_bytes().to_vec()
        }),
    );
    fixture.serve(
        SysinfoQueryId::HARDWARE_TREE,
        alloc::vec![hw_snapshot(&[HwNode::new(
            1,
            HW_NODE_ROOT,
            HwDeviceClass::Display
        )])],
    );

    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);

    assert_eq!(
        sample.net_counters.expect("counters read")[0]
            .counters
            .rx_bytes,
        4_096
    );
    // The socket table is folded into the two counts the pane states as it
    // is walked, so a closed socket is counted in neither.
    assert_eq!(
        sample.sockets,
        Some(super::SocketCensus {
            established: 2,
            listening: 1,
        })
    );
    assert_eq!(sample.resolver_servers.expect("resolvers read").len(), 1);
    assert_eq!(sample.time_servers.expect("time servers read").len(), 2);
    assert_eq!(
        sample.reclaim.expect("reclaim ledger read")[0].payload_bytes,
        2_048
    );
    let ledgers = sample.cache_ledgers.expect("cache ledgers read");
    assert_eq!(ledgers[0].label_bytes(), b"glyph atlas");
    let nodes = sample.hardware.expect("hardware tree read");
    assert_eq!(nodes[0].class(), Some(HwDeviceClass::Display));
    assert!(sample.degradations.is_empty());
}

#[test]
fn a_core_first_seen_this_sample_reports_no_busy_share() {
    let fixture = Fixture::new();
    fixture.set_cpu(alloc::vec![cpu_time(0, 750, 250)]);
    let mut sampler = Sampler::new(granted());

    // A cumulative total is not a share: the first sighting has nothing to
    // delta against, so it contributes no reading rather than the whole of
    // boot dressed as this interval.
    let first = sampler.sample(&fixture, 0);
    assert_eq!(first.core_busy.len(), 1);
    assert_eq!(first.core_busy[0].cpu, 0);
    assert_eq!(first.core_busy[0].permille, None);

    fixture.set_cpu(alloc::vec![cpu_time(0, 1_500, 500)]);
    let second = sampler.sample(&fixture, NS);
    assert_eq!(second.core_busy[0].permille, Some(750));
}

#[test]
fn a_cores_share_follows_its_own_cpu_index_not_its_position() {
    let fixture = Fixture::new();
    fixture.set_cpu(alloc::vec![
        cpu_time(0, 1_000, 1_000),
        cpu_time(1, 1_000, 1_000)
    ]);
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);

    // The service reports its CPUs in the other order, and only CPU 1 moved.
    // Keying on the index the record names is what stops one core's delta
    // being attributed to the other.
    fixture.set_cpu(alloc::vec![
        cpu_time(1, 1_750, 1_250),
        cpu_time(0, 1_000, 1_000)
    ]);
    let second = sampler.sample(&fixture, NS);
    let share = |cpu: u32| {
        second
            .core_busy
            .iter()
            .find(|core| core.cpu == cpu)
            .and_then(|core| core.permille)
    };
    assert_eq!(share(1), Some(750));
    assert_eq!(share(0), None, "an interval with no work measured no share");
}

#[test]
fn an_empty_paged_reading_is_present_and_empty_not_absent() {
    // A machine with no crashes and no seats has read those lists
    // successfully; the surface must not report that as unavailable.
    let fixture = Fixture::new();
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);
    assert_eq!(sample.crashes, Some(Vec::new()));
    assert_eq!(sample.seats, Some(Vec::new()));
    assert!(sample.degradations.is_empty());
}

#[test]
fn the_per_process_io_counters_are_carried_through() {
    let fixture = Fixture::new();
    fixture.set_processes(alloc::vec![process_with_io(
        1,
        ProcId::from_raw([1; 16]),
        4_096,
        8_192
    )]);
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);
    let process = &sample.processes[0];
    assert_eq!(process.io_bytes_read, 4_096);
    assert_eq!(process.io_bytes_written, 8_192);
}

#[test]
fn a_kernel_scope_denial_leaves_only_the_kernel_readings_absent_as_not_permitted() {
    let fixture = Fixture::new();
    let scopes = ScopeVerdicts {
        memory_pressure: false,
        ..granted()
    };
    let mut sampler = Sampler::new(scopes);
    let sample = sampler.sample(&fixture, 0);

    for (field, absent) in [
        (DegradedField::CpuLoad, sample.cpu_load.is_none()),
        (DegradedField::KernelMemory, sample.kernel_memory.is_none()),
        (DegradedField::VolumeHealth, sample.volume_health.is_none()),
        (DegradedField::CrashRecords, sample.crashes.is_none()),
    ] {
        assert!(absent, "{field:?} must be absent without the scope");
        // The honest statement: refused, not broken.
        assert_eq!(sample.absence(field), Absence::NotPermitted);
    }
    // Never issued, so never audited and never reported as a degradation.
    for query in [
        SysinfoQueryId::CPU_LOAD,
        SysinfoQueryId::KERNEL_MEMORY_STATS,
        SysinfoQueryId::VOLUME_IO_HEALTH,
        SysinfoQueryId::CRASH_RECORD,
    ] {
        assert!(!fixture.saw(query), "{query:?} must not be issued");
    }
    assert!(sample.degradations.is_empty());

    // Only those readings are affected: everything else still arrived.
    assert!(sample.uptime.is_some());
    assert!(sample.mounts.is_some());
    assert!(sample.seats.is_some());
    assert!(sample.net_state.is_some());
}

#[test]
fn a_hardware_scope_denial_leaves_only_the_hardware_readings_absent() {
    let fixture = Fixture::new();
    let scopes = ScopeVerdicts {
        hardware_scope: false,
        ..granted()
    };
    let mut sampler = Sampler::new(scopes);
    let sample = sampler.sample(&fixture, 0);

    assert!(sample.net_facts.is_none());
    assert!(sample.seats.is_none());
    assert_eq!(
        sample.absence(DegradedField::NetInterfaceFacts),
        Absence::NotPermitted
    );
    assert_eq!(sample.absence(DegradedField::Seats), Absence::NotPermitted);
    assert!(!fixture.saw(SysinfoQueryId::SEAT_LIST));
    assert!(!fixture.saw(SysinfoQueryId::NET_INTERFACE_FACTS));
    // The live interface readings share a different capability, so they are
    // unaffected.
    assert!(sample.net_state.is_some());
    assert!(sample.net_rates.is_some());
    assert!(sample.degradations.is_empty());
}

#[test]
fn a_global_scope_denial_leaves_only_the_global_readings_absent() {
    let fixture = Fixture::new();
    let scopes = ScopeVerdicts {
        global_process_scope: false,
        ..granted()
    };
    let mut sampler = Sampler::new(scopes);
    let sample = sampler.sample(&fixture, 0);

    assert!(sample.net_state.is_none());
    assert!(sample.net_rates.is_none());
    assert_eq!(
        sample.absence(DegradedField::NetInterfaceState),
        Absence::NotPermitted
    );
    assert!(!fixture.saw(SysinfoQueryId::NET_INTERFACE_STATE));
    assert!(!fixture.saw(SysinfoQueryId::NET_INTERFACE_RATES));
    // The interface inventory is a hardware-scope reading and still arrives.
    assert!(sample.net_facts.is_some());
    assert!(sample.degradations.is_empty());
}

#[test]
fn a_transport_failure_is_distinguished_from_a_denial() {
    let fixture = Fixture::new();
    fixture.answer(SysinfoQueryId::UPTIME, Answer::Fail);
    fixture.answer(SysinfoQueryId::SEAT_LIST, Answer::Fail);
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);

    // Permitted but unanswered: the query *was* issued, the field is
    // absent, and the reason is a failure rather than a refusal.
    assert!(sample.uptime.is_none());
    assert!(sample.seats.is_none());
    assert!(fixture.saw(SysinfoQueryId::UPTIME));
    assert!(fixture.saw(SysinfoQueryId::SEAT_LIST));
    assert_eq!(sample.absence(DegradedField::Uptime), Absence::Unavailable);
    assert_eq!(sample.absence(DegradedField::Seats), Absence::Unavailable);
    assert_eq!(
        sample.degradations,
        alloc::vec![DegradedField::Uptime, DegradedField::Seats]
    );

    // And nothing else degraded with them.
    assert!(sample.load_average.is_some());
    assert!(sample.mounts.is_some());
}

#[test]
fn a_reading_the_service_answers_with_nonsense_degrades_rather_than_being_half_believed() {
    let fixture = Fixture::new();
    // A reply too short to hold the fixed report is not partially decoded.
    fixture.serve(
        SysinfoQueryId::RESOURCE_LIMITS,
        alloc::vec![alloc::vec![0; 4]],
    );
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);
    assert!(sample.resource_limits.is_none());
    assert_eq!(
        sample.absence(DegradedField::ResourceLimits),
        Absence::Unavailable
    );
    assert_eq!(
        sample.degradations,
        alloc::vec![DegradedField::ResourceLimits]
    );
}

#[test]
fn a_paged_reading_spanning_several_pages_is_assembled_in_order() {
    let fixture = Fixture::new();
    let records: Vec<CpuLoadRecord> = (0..130).map(cpu_load).collect();
    fixture.serve(
        SysinfoQueryId::CPU_LOAD,
        blobs(&records, |r| r.to_le_bytes().to_vec()),
    );
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);

    let read = sample.cpu_load.expect("cpu load read");
    assert_eq!(read, records);
    // 130 records at 64 per page: two full pages and a short third that
    // ends the walk.
    assert_eq!(fixture.count_of(SysinfoQueryId::CPU_LOAD), 3);
}

#[test]
fn a_paged_reading_beyond_the_cap_truncates_deterministically() {
    let fixture = Fixture::new();
    let records: Vec<CpuLoadRecord> = (0..600).map(cpu_load).collect();
    fixture.serve(
        SysinfoQueryId::CPU_LOAD,
        blobs(&records, |r| r.to_le_bytes().to_vec()),
    );
    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);

    let read = sample.cpu_load.expect("cpu load read");
    // Exactly the cap, and exactly the *first* cap records in the
    // service's own order — truncation is deterministic, not a subset.
    assert_eq!(read.len(), super::CPU_RECORD_CAP);
    assert_eq!(read, records[..super::CPU_RECORD_CAP]);
    // It stopped there rather than paging the rest of the 600.
    assert_eq!(fixture.count_of(SysinfoQueryId::CPU_LOAD), 8);
    // Truncation is not a failure: nothing is reported as degraded.
    assert!(sample.degradations.is_empty());
}

#[test]
fn a_static_reading_is_read_once_and_reused() {
    let fixture = Fixture::new();
    let sample = sampled(&fixture, 4);
    assert_eq!(
        sample.identity.expect("identity read").hostname_bytes(),
        b"host"
    );
    // Read on the first sample and never again: the fact cannot change
    // while the process runs.
    assert_eq!(fixture.count_of(SysinfoQueryId::SYSTEM_IDENTITY), 1);
    assert_eq!(fixture.count_of(SysinfoQueryId::MEMORY_TOTAL), 1);
    assert_eq!(fixture.count_of(SysinfoQueryId::NET_INTERFACE_FACTS), 1);
    // The CPU inventory is deliberately not among them: its `current_freq_hz`
    // is a live clock, so it is re-read every sample and a retained record
    // could only report a stale one as live.
    assert_eq!(fixture.count_of(SysinfoQueryId::CPU_INFO), 4);
}

#[test]
fn the_cpu_inventorys_clock_is_a_live_reading_not_a_boot_fact() {
    let fixture = Fixture::new();
    fixture.serve(
        SysinfoQueryId::CPU_INFO,
        blobs(&[measured_clock(0, 2_000_000_000)], |r| {
            r.to_le_bytes().to_vec()
        }),
    );
    let mut sampler = Sampler::new(granted());
    let first = sampler.sample(&fixture, 0);
    assert_eq!(
        first.cpu_info.expect("inventory read")[0].current_freq_hz,
        2_000_000_000
    );

    // The clock moves. A reading cached for the boot would still report the
    // first figure, which is the defect this cadence exists to prevent.
    fixture.serve(
        SysinfoQueryId::CPU_INFO,
        blobs(&[measured_clock(0, 3_900_000_000)], |r| {
            r.to_le_bytes().to_vec()
        }),
    );
    let second = sampler.sample(&fixture, NS);
    let cpus = second.cpu_info.expect("inventory re-read");
    assert_eq!(cpus[0].current_freq_hz, 3_900_000_000);
    assert!(cpus[0].freq_measured());
}

#[test]
fn an_unreadable_cpu_inventory_is_absent_rather_than_a_stale_clock() {
    let fixture = Fixture::new();
    fixture.serve(
        SysinfoQueryId::CPU_INFO,
        blobs(&[measured_clock(0, 2_000_000_000)], |r| {
            r.to_le_bytes().to_vec()
        }),
    );
    let mut sampler = Sampler::new(granted());
    assert!(sampler.sample(&fixture, 0).cpu_info.is_some());

    // A failed re-read leaves no reading at all rather than carrying the
    // previous clock forward: a stale live figure is worse than an honest
    // absence, which the surface states as one.
    fixture.answer(SysinfoQueryId::CPU_INFO, Answer::Fail);
    let second = sampler.sample(&fixture, NS);
    assert!(second.cpu_info.is_none());
    assert_eq!(second.degradations, alloc::vec![DegradedField::CpuInfo]);
}

#[test]
fn a_static_reading_that_was_unavailable_is_retried_until_it_arrives() {
    let fixture = Fixture::new();
    fixture.answer(SysinfoQueryId::SYSTEM_IDENTITY, Answer::Fail);
    let mut sampler = Sampler::new(granted());

    let first = sampler.sample(&fixture, 0);
    assert!(first.identity.is_none());
    assert_eq!(first.degradations, alloc::vec![DegradedField::Identity]);

    // Still unavailable: retried (it is not cached), and not re-announced.
    let second = sampler.sample(&fixture, NS);
    assert!(second.identity.is_none());
    assert!(second.degradations.is_empty());
    assert_eq!(fixture.count_of(SysinfoQueryId::SYSTEM_IDENTITY), 2);

    // Once the service answers, the retry lands it and it is then cached.
    fixture.answer(SysinfoQueryId::SYSTEM_IDENTITY, Answer::Serve);
    let third = sampler.sample(&fixture, 2 * NS);
    assert!(third.identity.is_some());
    assert_eq!(fixture.count_of(SysinfoQueryId::SYSTEM_IDENTITY), 3);
    let fourth = sampler.sample(&fixture, 3 * NS);
    assert!(fourth.identity.is_some());
    assert_eq!(fixture.count_of(SysinfoQueryId::SYSTEM_IDENTITY), 3);
}

#[test]
fn the_cadence_issues_every_reading_on_the_first_sample() {
    let fixture = Fixture::new();
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);
    for query in EVERY_SAMPLE
        .iter()
        .chain(MEMORY.iter())
        .chain(INVENTORY.iter())
        .chain(STATIC.iter())
    {
        assert_eq!(
            fixture.count_of(*query),
            1,
            "{query:?} must be read on the first sample"
        );
    }
}

#[test]
fn the_cadence_issues_only_the_fast_readings_on_the_second_sample() {
    let fixture = Fixture::new();
    let mut sampler = Sampler::new(granted());
    let _ = sampler.sample(&fixture, 0);
    let _ = sampler.sample(&fixture, NS);

    for query in EVERY_SAMPLE {
        assert_eq!(fixture.count_of(query), 2, "{query:?} is a per-sample read");
    }
    for query in MEMORY.iter().chain(INVENTORY.iter()).chain(STATIC.iter()) {
        assert_eq!(
            fixture.count_of(*query),
            1,
            "{query:?} must not be re-read on the second sample"
        );
    }
}

#[test]
fn the_cadence_brings_the_slower_readings_round_again_on_their_own_cycles() {
    let fixture = Fixture::new();
    let mut sampler = Sampler::new(granted());
    // Sixteen samples: indices 0..=15, so the memory cadence comes round at
    // 5, 10 and 15, and the inventory cadence at 15.
    for i in 0..16u64 {
        let _ = sampler.sample(&fixture, i * NS);
    }

    for query in EVERY_SAMPLE {
        assert_eq!(fixture.count_of(query), 16);
    }
    for query in MEMORY {
        assert_eq!(fixture.count_of(query), 4, "{query:?} on 0, 5, 10 and 15");
    }
    for query in INVENTORY {
        assert_eq!(fixture.count_of(query), 2, "{query:?} on 0 and 15");
    }
    for query in STATIC {
        assert_eq!(fixture.count_of(query), 1, "{query:?} is read once");
    }
}

#[test]
fn a_process_list_beyond_the_cap_stops_the_walk_rather_than_paging_on() {
    // One page more than the cap: a walk that ran to exhaustion would ask
    // for a further page and hold the extra records.
    let over = super::PROCESS_RECORD_CAP + usize::from(tairix_procinfo::PROCESS_PAGE);
    let mut records = Vec::with_capacity(over);
    for pid in 0..over {
        let raw = u64::try_from(pid).expect("a pid that fits");
        records.push(process_with_io(raw, ProcId::from_raw([1; 16]), 0, 0));
    }
    let fixture = Fixture::new();
    fixture.set_processes(records);

    let mut sampler = Sampler::new(granted());
    let sample = sampler.sample(&fixture, 0);

    // Exactly the cap, in the service's own order, and no further page: the
    // sampler bounds the work as well as the memory.
    assert_eq!(sample.processes.len(), super::PROCESS_RECORD_CAP);
    assert_eq!(sample.processes[0].pid, 0);
    assert_eq!(
        fixture.count_of(SysinfoQueryId::GLOBAL_PROCESS_LIST),
        super::PROCESS_RECORD_CAP / usize::from(tairix_procinfo::PROCESS_PAGE)
    );
    // Stopping is the caller's own choice, never a failure to report.
    assert!(sample.degradations.is_empty());
}
