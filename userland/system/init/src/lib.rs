//! TAIRiX PID 1 — the service manager (`init`), Stage 6.
//!
//! `init` is the first user-space process the kernel starts. It owns the
//! lifecycle of every long-running system service (
//! `/System/Services`): it brings them up in dependency order, launches each
//! one as its own **service account**, and reaps children — both its own
//! services and the orphaned zombies that any PID 1 inherits. The **kernel**
//! is the single authority over a service's capabilities: it grants
//! `manifest ∩ account-ceiling` from the signed bundle at load time; init
//! names the binary and the account, never a capability set.
//!
//! # What this crate is
//!
//! This crate is the **orchestrator**, not a loader, and it is **not** the
//! capability authority. It owns four responsibilities and holds no
//! privileged state of its own:
//!
//! 1. Maintain a registry of [`ServiceSpec`]s, rejecting a duplicate name.
//! 2. Order the registered services by their declared dependencies and
//!    **fail closed** ([`InitError`]) on a missing dependency or a cycle —
//!    a malformed service graph never boots a partial, surprising system —
//!    then bring them up through a readiness-gated admission engine: a
//!    dependent is released only once every dependency it names has
//!    actually reported *ready* (and every named readiness condition it
//!    requires is satisfied), never merely spawned.
//! 3. Launch each admitted service as the **service account** its
//!    [`ServiceSpec`] names; the kernel derives and enforces its capability
//!    grant (`manifest ∩ account-ceiling`) from the signed bundle at load
//!    time. init passes no capability set, so no init-side derivation can
//!    drift from the kernel's authoritative one.
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
//! envelope — and granting `manifest ∩ account-ceiling` — is the
//! loader/kernel's job (the same pipeline `drvhost` runs for drivers); init
//! hands the binary and the account to the [`Spawner`], which performs that
//! verification and the kernel's capability derivation before it executes
//! anything.
//!
//! # Module map
//!
//! * [`events`] — stable [`tairix_log::EventId`] constants (`9000` range).
//! * [`error`] — [`InitError`] (graph-level, fail-closed) and the
//!   per-service [`StartFailure`] recorded in a [`StartReport`].
//! * [`service`] — [`ServiceSpec`], the [`Pid`] newtype, the
//!   [`Spawner`] / [`Reaper`] seams, and the shared manifest →
//!   capability-set decode.
//! * [`registry`] — service **discovery** and the fail-closed **enrolment
//!   registry**: which discovered `/System/Services` bundles are *eligible*
//!   to be brought up ([`Enrolment`]), and the ceiling-checked `enable` /
//!   `disable` operations ([`enrol`] / [`unenrol`]) that record that
//!   decision without ever widening authority.
//! * [`scope`] — the [`AuthorityScope`] a manager instance wields (the
//!   single system manager versus a per-user manager), the fixed security
//!   boundary that confines a per-user manager to services running as its
//!   own user.
//! * [`manager`] — the [`Init`] state machine: `register`,
//!   `register_enrolled`, `start_all`, the readiness admission engine
//!   (`notify`, `satisfy_condition`), on-demand endpoint activation
//!   (`connect` / `disconnect`, the idle-linger `expire_linger` /
//!   `expire_grace` one-shot callbacks, and `take_ready_clients`), and
//!   `reap`.
//!
//! The package also builds the `init` `Run` entry-point binary (`src/run.rs`,
//! `plans/PI.md` P6b). That binary is a lean, **pure-Rust** freestanding
//! program linking only the pure-Rust userland runtime `tairix-rt`
//! (TAIRiX code never uses the C ABI), **not** this
//! orchestrator library, so its tiny startup-config parser lives alongside it
//! (`src/startup.rs`) rather than here — pulling this crate's `alloc` + crypto
//! dependency chain into a banner-printing program would be the bloat
//! the charter forbids.
//!
//! # Layering
//!
//! The crate is `no_std` (with `alloc`) and depends only on
//! the audited `lib/*` crates `tairix-abi`, `tairix-caps`, and `tairix-log`,
//! so a userland service never links a kernel or driver crate.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod error;
pub mod events;
pub mod manager;
pub mod registry;
pub mod scope;
pub mod service;

pub use error::{ActivateError, InitError, NotifyError, StartFailure};
pub use manager::{
    ActivationOutcome, FailedService, Init, InitConfig, ReadyClient, StartReport, StartedService,
};
pub use registry::{enrol, unenrol, EnrolError, Enrolment};
pub use scope::AuthorityScope;
pub use service::{
    decode_manifest_capabilities, ClientId, LoopReaper, Pid, ReapedChild, Reaper, ServiceSpec,
    Spawner, Stopper, DEFAULT_STOP_GRACE,
};
