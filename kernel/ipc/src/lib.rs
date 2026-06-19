//! RustOS kernel IPC subsystem (Stage 2.5 of `PLAN.md`).
//!
//! This crate ships the capability-checked primitives every higher-level
//! kernel and userland component uses to talk to another address space:
//!
//! 1. [`port`] — typed message ports declared via [`rustos_abi::ipc`].
//!    Sender and receiver capabilities are checked **at port creation**
//!    and on **every** send. Per `AGENTS.md` §5.2 the receiver does
//!    not re-check; the kernel enforces.
//! 2. [`registry`] — the named-port registry mapping an
//!    [`port::EndpointId`] to the kernel-owned [`port::Port`] bound to
//!    it, so the syscall dispatcher can resolve the endpoint carried in
//!    an [`rustos_abi::ipc::IpcMessageHeader`] to a live port. It owns
//!    no lock of its own (the synchronisation policy lives with
//!    `kernel/core`'s `KernelState`, mirroring `kernel/sec`'s
//!    `CapTable`).
//! 3. [`shmem`] — explicit, capability-gated shared-memory objects
//!    backed by kernel/mem-tracked allocations. Revocation tears down
//!    every live mapping atomically.
//! 4. [`notify`] — asynchronous, signal-like notifications with the
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
//! the *in-kernel* API the dispatcher plugs into; nothing in
//! `kernel/ipc` performs ABI marshalling or user-memory copy-in. The
//! named-port [`registry`] resolves an [`port::EndpointId`] to a live
//! [`port::Port`]; wiring it into the `ipc_send` / `ipc_recv` handlers
//! still awaits the user-memory copy-in path (Stage 5 / Stage 6). Out
//! of scope here: userland driver host, syscall ABI marshalling.
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
pub mod call;
mod loom_compat;
pub mod notify;
pub mod port;
pub mod registry;
pub mod shmem;

pub use audit::AuditEvent;
pub use call::{CallEndpoint, CallEndpointLimits, CallTicket, ReceivedCall, ReplyOutcome};
pub use notify::{NotificationChannel, NotificationFlags};
pub use port::{EndpointId, Message, Port};
pub use registry::PortRegistry;
pub use shmem::{SharedMemory, ShmemHandle, ShmemId, ShmemMapping};
