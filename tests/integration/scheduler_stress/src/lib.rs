//! Stage-2 cross-crate scheduler stress test.
//!
//! routes cross-crate end-to-end tests to the top-level
//! `tests/` tree. This crate is the workspace-level integration test
//! for the Stage-2 deliverable "Stress test for the scheduler under
//! load on ≥ 4 emulated cores", lifting the in-crate test in
//! `kernel/sched/tests/stress.rs` to a *consumer*-side perspective:
//! it links against `rustos-kernel-sched-mlfq` exactly the way Stage 3a's
//! kernel entry will, and exercises the public API only.
//!
//! ## Scope
//!
//! * `N = 20 000` tasks across `4` simulated cores using the
//!   `TestArch` host double exposed by the `test-arch` feature
//!   (no hacks; the double is gated behind a feature
//!   and never linked into production builds).
//! * Asserts deadlock-freedom (every task eventually exits, bounded
//!   round count).
//! * Asserts bounded first-run latency.
//! * Asserts exact execution count (no task is dropped or doubled).
//!
//! ## Why not "just" re-run `kernel/sched/tests/stress.rs`?
//!
//! That test exists at the `kernel/sched` *unit* scope (unit tests live next to the code). The Stage-2 deliverable
//! lists both a per-crate stress test *and* a workspace-level
//! integration test under `tests/`. This crate is the latter: by
//! running across crate boundaries it would catch a regression that a
//! `kernel/sched`-internal test cannot see (e.g. a public-API change
//! that breaks `Scheduler::spawn`'s `Send` bounds).
//!
//! ## QEMU variant
//!
//! Running this exact scenario *under QEMU* requires Stage-3a SMP
//! (per-CPU APIC timer, AP startup, IPI-driven preemption). Stage 3a
//! is out of scope for this commit; the QEMU runner under
//! `tools/qemu` is in place and `PLAN.md`'s Stage 3a checklist
//! lists the pieces needed to migrate this test from host execution
//! to a QEMU `-smp 4` boot.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Default number of tasks the stress test spawns when the integration
/// harness in `tests/` doesn't override it.
pub const DEFAULT_TASKS: u64 = 20_000;

/// Default number of simulated cores.
pub const DEFAULT_CPUS: u32 = 4;
