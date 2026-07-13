//! The worker role's argv codec (`plans/STRESSTEST.md` §7.1).
//!
//! A worker is the same `stress` binary re-entered in worker mode: the
//! controller spawns the kernel's reserved `@self` path token — the
//! kernel substitutes the caller's *attested* program path and runs the
//! full load gate, so a worker is provably the controller's own verified
//! binary, never an `argv[0]` guess — handing it the closed argv block
//! this module encodes. The decode is fail-closed: anything outside the
//! exact spelling is a usage error, never a guessed load.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A worker that hits a typed refusal (a resource limit, `ENOSPC`, a
/// capability denial) exits with this status: the controller counts it as
/// an expected outcome — under `--overcommit` refusals are the *point* —
/// while any other non-zero worker exit fails the run (GNU `stress`
/// convention: exit 1 on worker failure).
pub const REFUSED_EXIT: i32 = 3;

/// The argv word introducing a worker re-entry (`stress --worker …`).
pub const WORKER_FLAG: &str = "--worker";

/// One load subsystem a worker can drive (`plans/STRESSTEST.md` §7.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerKind {
    /// Tight syscall-free arithmetic loops: exercises preemption.
    Cpu,
    /// Allocate/touch/re-touch anonymous memory in a rotating pattern:
    /// drives allocation, fault, and the compressed tier once live.
    Vm,
    /// Small-buffer write/`fs_sync`/read cycles: exercises the write path
    /// and the block cache.
    Io,
    /// Large sequential write/verify/delete cycles: exercises throughput
    /// and free-space accounting.
    Hdd,
    /// Repeated cold directory walks and file re-reads: churns the
    /// filesystem/block caches so their ledgers move.
    Cache,
}

impl WorkerKind {
    /// The argv spelling of this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Vm => "vm",
            Self::Io => "io",
            Self::Hdd => "hdd",
            Self::Cache => "cache",
        }
    }

    /// Recover a kind from its argv spelling; anything else is refused.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "cpu" => Some(Self::Cpu),
            "vm" => Some(Self::Vm),
            "io" => Some(Self::Io),
            "hdd" => Some(Self::Hdd),
            "cache" => Some(Self::Cache),
            _ => None,
        }
    }

    /// Whether this kind writes beneath the scratch directory.
    #[must_use]
    pub const fn uses_scratch(self) -> bool {
        matches!(self, Self::Io | Self::Hdd | Self::Cache)
    }
}

/// One worker's complete instruction block, carried on its argv.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSpec {
    /// The load subsystem this worker drives.
    pub kind: WorkerKind,
    /// The kind's byte target: the vm worker's allocation size, the io/hdd
    /// workers' file-size budget, the cache worker's tree budget. Zero for
    /// the cpu worker (it has no byte dimension).
    pub bytes: u64,
    /// The worker's index within its kind (names its scratch files, so two
    /// workers never contend on one file).
    pub index: u32,
    /// The scratch directory for a disk-touching kind; empty for cpu/vm.
    pub scratch: String,
}

impl WorkerSpec {
    /// Encode this spec as the argument vector the controller hands the
    /// `@self` spawn: `["stress", "--worker", kind, bytes, index, scratch]`.
    #[must_use]
    pub fn encode_argv(&self) -> Vec<String> {
        alloc::vec![
            "stress".to_string(),
            WORKER_FLAG.to_string(),
            self.kind.as_str().to_string(),
            self.bytes.to_string(),
            self.index.to_string(),
            self.scratch.clone(),
        ]
    }

    /// Decode the tokens following `--worker`, fail-closed: exactly four
    /// tokens (kind, decimal bytes, decimal index, scratch — possibly
    /// empty), a scratch that is present exactly when the kind needs one,
    /// and a byte target only for the kinds that have one.
    #[must_use]
    pub fn decode_argv(tokens: &[&str]) -> Option<Self> {
        let [kind, bytes, index, scratch] = tokens else {
            return None;
        };
        let kind = WorkerKind::from_word(kind)?;
        if !bytes.bytes().all(|b| b.is_ascii_digit()) || bytes.is_empty() {
            return None;
        }
        let bytes: u64 = bytes.parse().ok()?;
        if !index.bytes().all(|b| b.is_ascii_digit()) || index.is_empty() {
            return None;
        }
        let index: u32 = index.parse().ok()?;
        // A disk-touching worker without a scratch directory has nowhere
        // legal to write; a cpu/vm worker with one would be authority it
        // does not need. Both shapes are refused.
        if kind.uses_scratch() == scratch.is_empty() {
            return None;
        }
        // The cpu worker has no byte dimension; every other kind needs one.
        if (kind == WorkerKind::Cpu) != (bytes == 0) {
            return None;
        }
        Some(Self {
            kind,
            bytes,
            index,
            scratch: (*scratch).to_string(),
        })
    }
}
