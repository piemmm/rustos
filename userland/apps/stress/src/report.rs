//! The run's progress and summary reporting (`plans/STRESSTEST.md` §7.1,
//! `AGENTS.md` §20.1).
//!
//! Line shapes track GNU `stress` closely (`AGENTS.md` §16.7): the
//! dispatch line names the hog counts, the completion line reports the
//! elapsed run and its verdict. `--quiet` suppresses these stdout lines;
//! stderr diagnostics are never silenced. The fd-3 `summary` record is
//! additive advisory metadata and never changes stdout, the exit status,
//! or pipeline semantics.

use alloc::format;
use alloc::string::String;

use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};

use crate::command::Workers;
use crate::ctrl::Tally;

/// The producer word every report names.
const OWN_WORD: &str = "stress";

/// The dispatch line printed as the run starts (GNU `stress`'s
/// "dispatching hogs" shape, extended with the TAIRiX-only cache kind).
#[must_use]
pub fn dispatch_line(pid: u64, workers: &Workers) -> String {
    format!(
        "stress: info: [{pid}] dispatching hogs: {} cpu, {} io, {} vm, {} hdd, {} cache\n",
        workers.cpu, workers.io, workers.vm, workers.hdd, workers.cache
    )
}

/// The completion line: verdict plus elapsed seconds (GNU `stress`'s
/// "successful run completed in Ns" shape). A run ended by a signal or
/// with failed workers reports itself failed; typed refusals alone keep
/// the run successful (they are expected outcomes, reported separately by
/// [`refusal_line`]).
#[must_use]
pub fn completion_line(pid: u64, exit_code: i32, elapsed_secs: u64) -> String {
    if exit_code == 0 {
        format!("stress: info: [{pid}] successful run completed in {elapsed_secs}s\n")
    } else {
        format!("stress: info: [{pid}] failed run completed in {elapsed_secs}s\n")
    }
}

/// The refusal line, printed only when workers reported typed refusals:
/// under `--overcommit` these are the expected way a run discovers the
/// machine's limits.
#[must_use]
pub fn refusal_line(pid: u64, refused: u32) -> Option<String> {
    if refused == 0 {
        return None;
    }
    Some(format!(
        "stress: info: [{pid}] {refused} worker(s) refused by resource limits (expected outcome)\n"
    ))
}

/// Encode the fd-3 `summary` record for a finished run into `buf`,
/// returning the encoded length. Advisory only: emitted best-effort after
/// the stdout summary, carrying the machine-readable tally.
#[must_use]
pub fn summary_record(tally: &Tally, exit_code: i32, elapsed_secs: u64, buf: &mut [u8]) -> usize {
    let message = format!(
        "{} clean, {} refused, {} failed in {elapsed_secs}s.",
        tally.clean, tally.refused, tally.failed
    );
    let ai = format!(
        "{{\"subject\":\"stress_run\",\
         \"result\":{{\"clean\":{},\"refused\":{},\"failed\":{},\
         \"elapsed_secs\":{elapsed_secs},\"exit_code\":{exit_code}}}}}",
        tally.clean, tally.refused, tally.failed
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Summary,
        "stress.run_summary",
        Severity::Info,
        Human::message(&message),
    )
    .with_ai(&ai);
    record.write_jsonl(buf).unwrap_or(0)
}
