//! TAIRiX `stress` — a comprehensive load-generation command app
//! (`plans/STRESSTEST.md` ST5).
//!
//! `stress` follows the established `stress`/`stress-ng` command surface: one
//! **controller** process — pinned via `mem_pin`, opted into `signal_intake` so
//! `^C` tears the run down cleanly — spawns N swappable **worker** children
//! (the same binary re-entered in worker mode through the kernel's attested
//! `@self` spawn token), one per requested load unit, that load the CPU,
//! memory, filesystem write path, disk throughput, and the kernel caches,
//! separately or combined, with a configurable overcommit level, a run timeout,
//! quiet and background modes, and an option to run `sysmon` alongside.
//!
//! # What this crate is
//!
//! The fully host-testable substance of the tool:
//!
//! * [`command`] — the closed option grammar and its fail-closed
//!   [`parse`]r.
//! * [`worker`] — the worker role's argv codec: the closed spelling a
//!   controller hands the `@self` re-entry, decoded fail-closed.
//! * [`sizing`] — the byte-target policy over discovered RAM and free
//!   space (never a frozen scalar), with documented fallbacks.
//! * [`load`] — the five bounded, restartable load units, disk ones over
//!   the injected [`Scratch`] seam.
//! * [`ctrl`] — the controller's event-driven state machine: child
//!   exits, observed signals, timeout, grace escalation, and the
//!   0/1/130/143 exit-status decision, all provable on the host.
//! * [`report`] — the GNU-`stress`-shaped progress/summary lines and the
//!   fd-3 `summary` record.
//!
//! The program half (`src/run.rs`) wires these to the real syscalls:
//! spawn/wait/waitset/signal/`signal_intake`/`mem_pin`/`fs_*`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — the audited
//! `lib/abi`, the shared `lib/procinfo` mount walk, and the one `lib/help`
//! engine — never a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths. Every figure the sizing
//! reads travels through the System Information API; the workers hold no
//! authority beyond the caller's own.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod command;
pub mod ctrl;
pub mod error;
pub mod load;
pub mod report;
pub mod sizing;
pub mod worker;

#[cfg(test)]
mod tests;

pub use command::{parse, Command, RunSpec, Workers, USAGE};
pub use ctrl::{Action, Controller, Event, Tally};
pub use error::StressError;
pub use load::{
    cache_file, cache_unit, cpu_unit, hdd_unit, io_unit, run_scratch_paths, scratch_file, vm_unit,
    Scratch, ScratchError, UnitOutcome,
};
pub use report::{completion_line, dispatch_line, refusal_line, summary_record};
pub use sizing::{size_targets, Discovered, Targets};
pub use worker::{WorkerKind, WorkerSpec, REFUSED_EXIT, WORKER_FLAG};
