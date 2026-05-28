//! RustOS kernel IPC subsystem (Stage 2.5 of `PLAN.md`).
//!
//! This crate ships the capability-checked primitives every higher-level
//! kernel and userland component uses to talk to another address space:
//!
//! 1. [`port`] — typed message ports declared via [`rustos_abi::ipc`].
//!    Sender and receiver capabilities are checked **at port creation**
//!    and on **every** send. Per `AGENTS.md` §5.2 the receiver does
//!    not re-check; the kernel enforces.
//! 2. [`shmem`] — explicit, capability-gated shared-memory objects
//!    backed by kernel/mem-tracked allocations. Revocation tears down
//!    every live mapping atomically.
//! 3. [`notify`] — asynchronous, signal-like notifications with the
//!    same capability gates as ports.
//!
//! Every refused operation emits exactly one structured audit event
//! through [`audit`] before returning a stable [`rustos_abi::Errno`]
//! to the caller ("fail-closed everywhere"). Audit IDs live in the
//! `kernel/ipc` reserved range `3_000..4_000`; the catalogue is in
//! [`audit`].
//!
//! # Dispatcher boundary
//!
//! The syscall dispatcher arrives in Stage 2.7. This crate exposes
//! the *in-kernel* API the dispatcher will plug into; nothing in
//! `kernel/ipc` performs ABI marshalling or names a registry of
//! ports. Out of scope (issue brief): userland driver host, syscall
//! ABI plumbing, named services / registry.
//!
//! # Documentation
//!
//! See `docs/src/architecture/ipc.md` for the architecture-level
//! description.

#![no_std]
#![cfg_attr(loom, allow(dead_code))]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod audit;
mod loom_compat;
pub mod notify;
pub mod port;
pub mod shmem;

pub use audit::AuditEvent;
pub use notify::{NotificationChannel, NotificationFlags};
pub use port::{EndpointId, Message, Port};
pub use shmem::{SharedMemory, ShmemHandle, ShmemId, ShmemMapping};
