//! TAIRiX kernel syscall dispatch (Stage 2.7 of `PLAN.md`).
//!
//! This crate owns the *generated dispatch table* the kernel hands every
//! syscall to. It is the **only** place the kernel performs the five
//! steps the charter demands of every privileged entry point:
//!
//! 1. Identify the caller (the per-CPU current task; supplied via the
//!    [`CallerContext`] handed in by the architecture entry stub).
//! 2. Check the syscall's `required_capability` against the caller's
//!    effective set (via [`tairix_kernel_sec::TaskCapabilities`]).
//! 3. Validate every register-passed argument against the
//!    [`tairix_abi::AbiType`] declared in the `abi-v1` table.
//! 4. Dispatch to the owning subsystem through the [`SyscallHandlers`]
//!    trait. Concrete wiring lives in `kernel/core` so this crate stays
//!    decoupled from `kernel/ipc`, `kernel/sched`, …
//! 5. Emit one structured audit record per security-relevant decision.
//!
//! # ABI cross-check
//!
//! [`SYSCALL_TABLE_HASH`] holds the SHA-256 fingerprint of
//! [`tairix_abi::ENCODED_TABLE`]. [`verify_table_hash`] re-computes it
//! and returns `Err(Errno::AbiVersionUnsupported)` if the two diverge —
//! a build-time `cargo xtask abi-check` runs the same comparison, so the
//! kernel never boots against a stale ABI mirror.
//!
//! # Per-arch entry stubs
//!
//! Out of scope for Stage 2.7 (per the issue brief): the userland-facing
//! trampolines that marshal the architecture's syscall registers into
//! [`RawArgs`]. Stage 3 ports add those for each Tier-1 target and call
//! [`Dispatcher::dispatch`] verbatim.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod audit;
pub mod table;

pub use audit::AuditEvent;
pub use table::{
    sandbox_allows, verify_table_hash, CallerContext, Dispatcher, RawArgs, SyscallHandlers,
    SyscallResult, SYSCALL_TABLE_HASH,
};
