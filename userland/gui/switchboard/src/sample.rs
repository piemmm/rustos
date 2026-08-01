//! System sampling: gathers a compact [`Sample`] of the live system state
//! through the System Information API, tracking the prior-sample state a
//! delta computation needs.
//!
//! Every field degrades to its honest empty form on a query failure or a
//! refused capability — never a fabricated value. `sysinfo` refusals are
//! observed as the typed [`CallError::PermissionDenied`] returned by
//! `lib/procinfo`'s helpers; the optional global-process and
//! memory-pressure scopes are probed exactly once, at startup
//! ([`probe_scopes`]), since a process's capability set is fixed at spawn
//! and re-probing per sample would only spam the audited memory-pressure
//! query for an answer that can never change.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tairix_abi::sysinfo::{ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId};
use tairix_abi::{ProcId, SchedPriority};
use tairix_procinfo::{call, for_each_process, memory_pressure, CallError, CpuTotals, Transport};

use crate::schedule::MEMORY_SAMPLE_DIVIDER;

/// The busiest task observed over the last sample interval, before its
/// display name has been validated against the wire's bounded-text rules
/// ([`crate::derive::derive_summary`] performs that validation).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopTask {
    /// The process's raw name bytes, exactly as `sysinfod` reported them
    /// (at most [`tairix_abi::sysinfo::PROCESS_NAME_MAX`] bytes; may not be
    /// valid UTF-8 or may contain control characters — [`crate::derive`] is
    /// what turns this into a wire-valid name, or drops it honestly).
    pub name: Vec<u8>,
    /// Its CPU share over the sample interval, in permille (`0..=1000`).
    pub cpu_permille: u16,
}

/// One process observed in a sample, carrying enough of its state for the
/// live panel's task and recovery rows ([`crate::model`]).
///
/// `pid` is [`ProcessRecord::pid`] — the scheduler task id — which is the
/// same numeric identity the desktop session's [`SeatReport`] and the
/// window/command mailbox rendezvous address a process by
/// ([`tairix_abi::switchboard_ipc`]); it is display/convenience only and is
/// reused across process lifetimes, so it is never used as a map key across
/// samples (that is [`proc_id`](Self::proc_id)'s job).
///
/// [`SeatReport`]: tairix_abi::switchboard_ipc::SeatReport
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSummary {
    /// The scheduler task id.
    pub pid: u64,
    /// The stable, never-reused process-instance identity.
    pub proc_id: ProcId,
    /// The process's raw name bytes, exactly as `sysinfod` reported them.
    pub name: Vec<u8>,
    /// Lifecycle state.
    pub state: ProcessState,
    /// The owning user id, kernel-attested through the record.
    ///
    /// The pressure and activity actions compare it against the service's
    /// own uid so a control renders denied exactly where the kernel's
    /// same-principal rule would refuse it.
    pub uid: u32,
    /// Bytes of memory currently mapped in the process's address space
    /// ([`ProcessRecord::mem_bytes`]) — what names the memory-pressure
    /// culprit.
    pub mem_bytes: u64,
    /// The process's time-shared scheduling service level, read from the
    /// scheduler's own record — what lets an already-lowered process render
    /// its "lower priority" action spent instead of re-offering it.
    pub priority: SchedPriority,
    /// Its CPU share over the sample interval, in permille. `None` on the
    /// very first sample or an unmeasurable interval, exactly like
    /// [`TopTask::cpu_permille`] for the busiest task.
    pub cpu_permille: Option<u16>,
}

/// A memory-pressure reading, carried forward between samples since the
/// audited query is issued only every [`MEMORY_SAMPLE_DIVIDER`]th sample.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemoryPressureSample {
    /// The current pressure band depth (`0` = normal).
    pub band: u8,
    /// The honest used-memory fraction at the sample, in permille:
    /// `(total_bytes - free_bytes) * 1000 / total_bytes`, clamped to
    /// `1000`, or `0` when `total_bytes` is zero.
    pub used_permille: u16,
    /// The machine's total usable memory in bytes, as the same reading
    /// reported it — what a per-process `mem_bytes` is a fraction *of* in
    /// the pressure card's cause text.
    pub total_bytes: u64,
}

/// Which kind of measurement degraded to its honest empty value this
/// sample — used to log a one-time stderr notice per field kind rather
/// than spamming one on every subsequent failure of the same kind.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DegradedField {
    /// The process list could not be read (denied, or a transport
    /// failure).
    ProcessList,
    /// The aggregate CPU-time totals could not be read.
    CpuTime,
    /// The memory-pressure query could not be read.
    MemoryPressure,
}

/// One sample of the live system, gathered by [`Sampler::sample`].
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct Sample {
    /// Count of processes observed in the [`ProcessState::Stopped`] state
    /// (recovery candidates).
    pub stopped_count: u16,
    /// Overall CPU busy fraction, in permille. `None` when the aggregate
    /// CPU-time query failed or reported no CPUs.
    pub cpu_busy_permille: Option<u16>,
    /// The task with the highest CPU-time delta since the previous sample.
    /// `None` on the very first sample (nothing to delta against), when
    /// the process list could not be read, or when the interval since the
    /// previous sample is unmeasurable.
    pub top_task: Option<TopTask>,
    /// The most recently known memory-pressure reading, carried forward
    /// between the sparser memory-pressure queries. `None` when the
    /// capability was never granted or no attempt has yet succeeded.
    pub memory_pressure: Option<MemoryPressureSample>,
    /// The whole process list observed this sample, in the order the
    /// System Information API reported it. Empty when the process list
    /// could not be read this sample (the same honest-empty rule as
    /// [`Self::stopped_count`] and [`Self::top_task`]).
    pub processes: Vec<ProcessSummary>,
    /// Field kinds that degraded to their honest empty value *for the
    /// first time* this sample (for a one-time stderr notice).
    pub degradations: Vec<DegradedField>,
}

/// Which optional System Information API scopes this Switchboard
/// instance's ceiling grants, established once at startup
/// ([`probe_scopes`]) and held for the process's life.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ScopeVerdicts {
    /// Whether the system-wide process list
    /// ([`SysinfoQueryId::GLOBAL_PROCESS_LIST`]) is available
    /// (`CAP_SYSINFO_GLOBAL` granted).
    pub global_process_scope: bool,
    /// Whether the memory-pressure gauge
    /// ([`SysinfoQueryId::MEMORY_PRESSURE`]) is available
    /// (`CAP_SYSINFO_KERNEL` granted).
    pub memory_pressure: bool,
}

/// Probe, once, whether the granted ceiling includes the global process
/// scope and the memory-pressure gauge.
///
/// Issued exactly once, at startup: repeating a denied, audited query
/// (memory pressure) every sample would spam the audit log for an answer
/// that cannot change mid-process — capability sets are fixed at spawn. A
/// verdict of "granted" on any outcome other than an explicit
/// [`CallError::PermissionDenied`] is deliberate: a transient service
/// failure at probe time does not condemn a field to its degraded form for
/// the rest of the process's life — a later real failure degrades that one
/// sample honestly instead.
#[must_use]
pub fn probe_scopes(transport: &dyn Transport) -> ScopeVerdicts {
    let probe_payload = ProcessListRequest {
        offset: 0,
        limit: 1,
        flags: 0,
    }
    .to_le_bytes();
    let global_process_scope = !matches!(
        call(
            transport,
            SysinfoQueryId::GLOBAL_PROCESS_LIST,
            &probe_payload,
        ),
        Err(CallError::PermissionDenied)
    );
    let memory_pressure_scope =
        !matches!(memory_pressure(transport), Err(CallError::PermissionDenied));
    ScopeVerdicts {
        global_process_scope,
        memory_pressure: memory_pressure_scope,
    }
}

/// The busy share of `delta_ns` over `interval_ns`, in permille
/// (`0..=1000`), or `None` when `interval_ns` is zero (an unmeasurable
/// interval — the honest absence, never a fabricated rate).
fn permille_of(delta_ns: u64, interval_ns: u64) -> Option<u16> {
    if interval_ns == 0 {
        return None;
    }
    let permille = (u128::from(delta_ns) * 1000 / u128::from(interval_ns)).min(1000);
    Some(u16::try_from(permille).unwrap_or(1000))
}

/// The honest used-memory fraction, in permille, given the reported totals.
fn used_permille(total_bytes: u64, free_bytes: u64) -> u16 {
    if total_bytes == 0 {
        return 0;
    }
    let used = total_bytes.saturating_sub(free_bytes);
    let permille = (u128::from(used) * 1000 / u128::from(total_bytes)).min(1000);
    u16::try_from(permille).unwrap_or(1000)
}

/// Gathers successive [`Sample`]s of the live system, tracking the prior
/// per-process CPU time, the prior aggregate CPU totals, and the
/// carried-forward memory-pressure reading needed to compute each sample's
/// deltas.
#[derive(Debug)]
pub struct Sampler {
    scopes: ScopeVerdicts,
    prev_proc_times: BTreeMap<ProcId, u64>,
    prev_sample_ns: Option<u64>,
    prev_totals: Option<CpuTotals>,
    last_memory: Option<MemoryPressureSample>,
    sample_index: u64,
    warned_process_list: bool,
    warned_cpu_time: bool,
    warned_memory_pressure: bool,
}

impl Sampler {
    /// Build a sampler with no prior state, remembering `scopes` (probed
    /// once by [`probe_scopes`]) for the life of the process.
    #[must_use]
    pub fn new(scopes: ScopeVerdicts) -> Self {
        Self {
            scopes,
            prev_proc_times: BTreeMap::new(),
            prev_sample_ns: None,
            prev_totals: None,
            last_memory: None,
            sample_index: 0,
            warned_process_list: false,
            warned_cpu_time: false,
            warned_memory_pressure: false,
        }
    }

    /// Gather one [`Sample`] of the live system through `transport`.
    ///
    /// `now_ns` is the caller's monotonic clock reading for this sample,
    /// used only to measure the actual elapsed interval since the previous
    /// call (never assumed to be exactly the nominal sample period).
    pub fn sample(&mut self, transport: &dyn Transport, now_ns: u64) -> Sample {
        let elapsed_ns = self.prev_sample_ns.map(|prev| now_ns.saturating_sub(prev));
        let mut degradations = Vec::new();

        let (stopped_count, top_task, processes) =
            self.sample_processes(transport, elapsed_ns, &mut degradations);
        let cpu_busy_permille = self.sample_cpu_totals(transport, &mut degradations);
        self.sample_memory_pressure(transport, &mut degradations);

        self.prev_sample_ns = Some(now_ns);
        self.sample_index = self.sample_index.wrapping_add(1);

        Sample {
            stopped_count,
            cpu_busy_permille,
            top_task,
            memory_pressure: self.last_memory,
            processes,
            degradations,
        }
    }

    /// Walk the process list, counting stopped processes, picking the task
    /// with the highest CPU-time delta since the previous sample, and
    /// building each process's [`ProcessSummary`] for the live panel.
    fn sample_processes(
        &mut self,
        transport: &dyn Transport,
        elapsed_ns: Option<u64>,
        degradations: &mut Vec<DegradedField>,
    ) -> (u16, Option<TopTask>, Vec<ProcessSummary>) {
        let mut stopped_count: u16 = 0;
        let mut records: Vec<ProcessRecord> = Vec::new();
        let outcome = for_each_process(transport, self.scopes.global_process_scope, |record| {
            if record.state == ProcessState::Stopped {
                stopped_count = stopped_count.saturating_add(1);
            }
            records.push(*record);
            Ok(())
        });

        if outcome.is_err() {
            if !self.warned_process_list {
                degradations.push(DegradedField::ProcessList);
                self.warned_process_list = true;
            }
            // Prior-sample state is left untouched: a transient failure
            // must not erase history a later successful sample could still
            // use to compute an honest delta.
            return (0, None, Vec::new());
        }
        self.warned_process_list = false;

        // Keyed on the stable, never-reused `proc_id`, so a numeric-pid
        // reuse across two process lifetimes can never be mistaken for one
        // continuously-running task. A process not seen in the previous
        // sample (first sight) contributes an honest zero delta rather than
        // a fabricated rate over an interval it was never observed across.
        let mut current = BTreeMap::new();
        let mut top: Option<(usize, u64)> = None;
        let mut deltas: Vec<u64> = Vec::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            let prev_time = self.prev_proc_times.get(&record.proc_id).copied();
            current.insert(record.proc_id, record.cpu_time_ns);
            let delta = prev_time.map_or(0, |prev| record.cpu_time_ns.saturating_sub(prev));
            deltas.push(delta);
            let is_new_best = match top {
                Some((_, best_delta)) => delta > best_delta,
                None => true,
            };
            if is_new_best {
                top = Some((index, delta));
            }
        }
        self.prev_proc_times = current;

        let top_task = match elapsed_ns {
            // No prior sample time to delta against: honestly no top task,
            // never one measured over a fabricated interval.
            None => None,
            Some(interval) => top.and_then(|(index, delta)| {
                let record = &records[index];
                let cpu_permille = permille_of(delta, interval)?;
                Some(TopTask {
                    name: record.name_bytes().to_vec(),
                    cpu_permille,
                })
            }),
        };

        let processes = records
            .iter()
            .zip(deltas)
            .map(|(record, delta)| ProcessSummary {
                pid: record.pid,
                proc_id: record.proc_id,
                name: record.name_bytes().to_vec(),
                state: record.state,
                uid: record.uid,
                mem_bytes: record.mem_bytes,
                priority: record.priority,
                cpu_permille: elapsed_ns.and_then(|interval| permille_of(delta, interval)),
            })
            .collect();

        (stopped_count, top_task, processes)
    }

    /// Fetch the aggregate CPU-time totals and derive the busy-fraction
    /// delta against the previous sample (an all-zero previous total on
    /// the first sample yields the honest cumulative since-boot ratio).
    fn sample_cpu_totals(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) -> Option<u16> {
        match CpuTotals::fetch_all(transport) {
            Ok(Some(totals)) => {
                self.warned_cpu_time = false;
                let prev = self.prev_totals.unwrap_or_default();
                self.prev_totals = Some(totals);
                CpuTotals::busy_permille(prev, totals)
            }
            // No CPUs reported: an honest empty, never a failure to warn
            // about.
            Ok(None) => None,
            Err(_) => {
                if !self.warned_cpu_time {
                    degradations.push(DegradedField::CpuTime);
                    self.warned_cpu_time = true;
                }
                None
            }
        }
    }

    /// On the memory-pressure query's own slower cadence (and only when the
    /// capability was granted), refresh the carried-forward reading.
    fn sample_memory_pressure(
        &mut self,
        transport: &dyn Transport,
        degradations: &mut Vec<DegradedField>,
    ) {
        if !self.scopes.memory_pressure {
            // Never granted: `last_memory` stays `None` for the process's
            // life; probed once, so no per-sample warning to spam.
            return;
        }
        if !self.sample_index.is_multiple_of(MEMORY_SAMPLE_DIVIDER) {
            // Not this cycle: carry forward whatever reading is already
            // known.
            return;
        }
        match memory_pressure(transport) {
            Ok(stats) => {
                self.warned_memory_pressure = false;
                self.last_memory = Some(MemoryPressureSample {
                    band: stats.band,
                    used_permille: used_permille(stats.total_bytes, stats.free_bytes),
                    total_bytes: stats.total_bytes,
                });
            }
            Err(_) => {
                if !self.warned_memory_pressure {
                    degradations.push(DegradedField::MemoryPressure);
                    self.warned_memory_pressure = true;
                }
                // Leave `last_memory` at its last known value: a single
                // transient failure does not erase a recent honest reading.
            }
        }
    }
}

#[cfg(test)]
#[path = "sample_tests.rs"]
mod tests;
