//! Unit tests for the [`Sampler`] and [`probe_scopes`].

use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::sysinfo::{
    CpuTimeListRequest, CpuTimeRecord, MemoryPressureStats, ProcessListRequest, ProcessRecord,
    ProcessState, SysinfoQueryId, SysinfoRequestHeader,
};
use tairix_abi::{Errno, ProcId, SchedPriority};
use tairix_procinfo::Transport;

use super::{probe_scopes, DegradedField, Sampler, ScopeVerdicts};

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
#[derive(Default)]
struct Fixture {
    processes: RefCell<Vec<ProcessRecord>>,
    cpu: RefCell<Vec<CpuTimeRecord>>,
    pressure: RefCell<MemoryPressureStats>,
    process_answer: Answer,
    cpu_answer: Answer,
    memory_answer: Answer,
    seen: RefCell<Vec<SysinfoQueryId>>,
}

impl Fixture {
    fn new() -> Self {
        Self::default()
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
    }
}

const NS: u64 = 1_000_000_000;

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
