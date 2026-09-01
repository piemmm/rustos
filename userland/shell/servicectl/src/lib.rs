//! The `servicectl` tool's behaviour: drive one service-manager
//! runtime-lifecycle operation over the capability-gated control endpoint
//! (`plans/NEW-SERVICEMANAGER.md` SVC-8).
//!
//! This library is the whole behaviour — the command-line grammar
//! ([`command`]) and the request/reply round trip with its rendering
//! ([`session`]) — behind two seams ([`session::ControlChannel`],
//! [`session::ToolIo`]) so every path, including each refusal, is
//! host-testable without a kernel. The freestanding `Run` binary
//! (`src/run.rs`) merely binds the seams to `ipc_call` and the inherited
//! standard streams.
//!
//! # Security shape
//!
//! The tool adds no authority of its own. Reaching the endpoint *is* the
//! authority: the manager binds it restricted-sender, so the kernel refuses
//! a call from a task without `CAP_SERVICE_CONTROL` and the tool never
//! checks a capability itself. An account whose ceiling does not carry it
//! gets a refusal from the kernel, not a partial effect.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are `lib/abi` and the
//! shared help engine, so this userland tool never links a kernel or driver
//! crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths: every refusal renders as a terse line and sets the exit status.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

pub mod command;
pub mod session;

#[cfg(test)]
mod session_tests;

pub use command::{parse, Command, UsageError, USAGE};
pub use session::{dispatch, report_usage, run, run_enrol, ControlChannel, Exit, ToolIo};
