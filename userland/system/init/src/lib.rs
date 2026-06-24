//! RustOS PID 1 — the service manager (`init`), Stage 6.
//!
//! `init` is the first user-space process the kernel starts. It owns the
//! lifecycle of every long-running system service (
//! `/System/Services`): it brings them up in dependency order, grants each
//! one exactly the capability set its signed manifest requests intersected
//! with the authority init was itself granted, and reaps children — both
//! its own services and the orphaned zombies that any PID 1 inherits.
//!
//! # What this crate is
//!
//! This crate is the **orchestrator**, not a loader. It owns four
//! responsibilities and holds no privileged state of its own beyond the
//! capability authority handed to it at construction:
//!
//! 1. Maintain a registry of [`ServiceSpec`]s, rejecting a duplicate name.
//! 2. Order the registered services by their declared dependencies and
//!    **fail closed** ([`InitError`]) on a missing dependency or a cycle —
//!    a malformed service graph never boots a partial, surprising system.
//! 3. For each service, decode its signed manifest into a requested
//!    capability set and grant the intersection of that request with the
//!    [`InitConfig::authority`]. A service whose manifest
//!    requests authority init does not hold is refused, never silently
//!    widened.
//! 4. Reap exited children, distinguishing a known service's exit from an
//!    inherited orphan, and audit both.
//!
//! The two operations that touch the outside world — actually launching a
//! verified binary and learning that a child has exited — are injected as
//! the [`Spawner`] and [`Reaper`] seams. The binary that ships as
//! `/System/Services/init` wires the real kernel-backed implementations;
//! tests wire in-memory fixtures. This keeps the security-relevant
//! ordering and capability code independent of kernel plumbing and
//! exhaustively testable.
//!
//! Verifying a service binary's signature, syscall-table hash, and `rxe`
//! envelope is the loader's job (the same pipeline `drvhost` runs for
//! drivers); init computes the capability ceiling and hands
//! it to the [`Spawner`], which performs that verification before it
//! executes anything.
//!
//! # Module map
//!
//! * [`events`] — stable [`rustos_log::EventId`] constants (`9000` range).
//! * [`error`] — [`InitError`] (graph-level, fail-closed) and the
//!   per-service [`StartFailure`] recorded in a [`StartReport`].
//! * [`service`] — [`ServiceSpec`], the [`Pid`] newtype, and the
//!   [`Spawner`] / [`Reaper`] seams.
//! * [`manager`] — the [`Init`] state machine: `register`, `start_all`,
//!   and `reap`.
//!
//! The package also builds the `init` `Run` entry-point binary (`src/run.rs`,
//! `plans/PI.md` P6b). That binary is a lean, **pure-Rust** freestanding
//! program linking only the pure-Rust userland runtime `rustos-rt`
//! (RustOS code never uses the C ABI), **not** this
//! orchestrator library, so its tiny startup-config parser lives alongside it
//! (`src/startup.rs`) rather than here — pulling this crate's `alloc` + crypto
//! dependency chain into a banner-printing program would be the bloat
//! the charter forbids.
//!
//! # Layering
//!
//! The crate is `no_std` (with `alloc`) and depends only on
//! the audited `lib/*` crates `rustos-abi`, `rustos-caps`, and `rustos-log`,
//! so a userland service never links a kernel or driver crate.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod error;
pub mod events;
pub mod manager;
pub mod service;

pub use error::{InitError, StartFailure};
pub use manager::{FailedService, Init, InitConfig, StartReport, StartedService};
pub use service::{Pid, ReapedChild, Reaper, ServiceSpec, Spawner};
